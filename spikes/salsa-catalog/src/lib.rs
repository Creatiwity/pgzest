//! Spike D3 (PLAN.md) : `salsa` comme moteur incrémental du catalogue.
//!
//! **Jetable.** Ce crate répond à une question — salsa convient-il pour
//! « index léger + résolution paresseuse + portée de routine + LSP
//! incrémental » ? — et sera supprimé quand `pgweave-project` le remplacera
//! (P2). Décision et mesures : `docs/decisions/0001-salsa.md`.
//!
//! Graphe de requêtes :
//!
//! ```text
//! SourceFile (input) ──► file_declarations ──► table_index ─┐
//!        │               (scan de tokens,       (nom → fichiers,
//!        │                pas de parse)          dans l'ordre)
//!        └──────────► parse_file ◄──────────────────────────┴─ resolve_table(nom)
//!                     (parse complet,                         (DDL appliqués dans
//!                      seulement si demandé)                   l'ordre des fichiers)
//!
//! routine_scope(nom) : paramètres, variables, tables temporaires du corps
//! resolve_in_routine(routine, nom) : table temporaire d'abord, puis globale
//! ```
//!
//! Volontairement, la sémantique d'application des DDL reproduit celle de
//! `SchemaResolver` (noms en minuscules, `public.` par défaut, `ALTER TABLE`
//! ADD/DROP/ALTER COLUMN) pour pouvoir comparer les deux à l'identique.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pgweave_ast::Statement;
use pgweave_ast::ddl::{AlterColumnAction, AlterTableOp, DdlStatement, FunctionBody};
use pgweave_ast::plpgsql::{Block, Declaration, PlStatement, PlStatementKind};
use pgweave_schema::provider::object_name_to_string;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, Tokenizer, Word};

// ── Entrées ───────────────────────────────────────────────────────────────────

/// Un fichier SQL du projet. Son texte est la seule chose qui change.
#[salsa::input(debug)]
pub struct SourceFile {
    #[returns(ref)]
    pub path: PathBuf,
    #[returns(ref)]
    pub text: String,
}

/// Le projet : ses fichiers, dans l'ordre d'application.
#[salsa::input]
pub struct Project {
    #[returns(ref)]
    pub files: Vec<SourceFile>,
}

// ── Base ──────────────────────────────────────────────────────────────────────

#[salsa::db]
pub trait Db: salsa::Database {
    /// Trace d'exécution des requêtes, pour vérifier ce qui est recalculé.
    fn log_execution(&self, what: String);
}

#[salsa::db]
#[derive(Clone, Default)]
pub struct Database {
    storage: salsa::Storage<Self>,
    executions: Arc<Mutex<Vec<String>>>,
}

impl Database {
    /// Requêtes exécutées depuis le dernier appel, dans l'ordre.
    pub fn take_executions(&self) -> Vec<String> {
        std::mem::take(&mut *self.executions.lock().unwrap())
    }
}

#[salsa::db]
impl salsa::Database for Database {}

#[salsa::db]
impl Db for Database {
    fn log_execution(&self, what: String) {
        self.executions.lock().unwrap().push(what);
    }
}

// ── Noms ──────────────────────────────────────────────────────────────────────

#[salsa::interned(debug)]
pub struct TableName<'db> {
    #[returns(ref)]
    pub text: String,
}

#[salsa::interned(debug)]
pub struct RoutineName<'db> {
    #[returns(ref)]
    pub text: String,
}

/// Même normalisation que `SchemaResolver` : minuscules, `public.` par défaut.
pub fn qualify(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains('.') {
        lower
    } else {
        format!("public.{lower}")
    }
}

// ── Parse (complet, à la demande) ─────────────────────────────────────────────

/// Résultat de parse partagé. `no_eq` : l'AST n'implémente pas `Eq`, donc
/// tout changement de texte invalide ce nœud — les nœuds en aval, eux,
/// comparent leurs résultats et s'arrêtent si rien n'a changé.
#[derive(Clone)]
pub struct Parsed(pub Arc<Vec<Statement>>);

#[salsa::tracked(returns(ref), no_eq)]
pub fn parse_file(db: &dyn Db, file: SourceFile) -> Parsed {
    db.log_execution(format!("parse_file({})", file_label(db, file)));
    let stmts = pgweave_parser::parse_with_outcome(file.text(db))
        .map(|o| o.statements)
        .unwrap_or_default();
    Parsed(Arc::new(stmts))
}

// ── Déclarations (scan léger, sans parse) ─────────────────────────────────────

/// Noms déclarés ou modifiés par un fichier, extraits des tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, salsa::SalsaValue)]
pub struct FileDeclarations {
    /// Tables créées ou modifiées (`CREATE TABLE`, `ALTER TABLE`).
    pub tables: Vec<String>,
    /// Fonctions et procédures créées.
    pub routines: Vec<String>,
}

#[salsa::tracked(returns(ref))]
pub fn file_declarations(db: &dyn Db, file: SourceFile) -> FileDeclarations {
    db.log_execution(format!("file_declarations({})", file_label(db, file)));
    scan_declarations(file.text(db))
}

/// Balayage des tokens en tête de statement. Un corps `$$…$$` est un seul
/// token : ce qu'il contient (tables temporaires…) n'est pas global et
/// n'apparaît donc pas ici.
pub fn scan_declarations(sql: &str) -> FileDeclarations {
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize() else {
        return FileDeclarations::default();
    };
    let words: Vec<Option<&Word>> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .map(|t| match t {
            Token::Word(w) => Some(w),
            _ => None,
        })
        .collect();
    let semis: Vec<bool> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .map(|t| *t == Token::SemiColon)
        .collect();
    // Un nom qualifié a besoin des points : on garde les tokens significatifs.
    let significant: Vec<&Token> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();

    let mut decls = FileDeclarations::default();
    let mut at_start = true;
    let mut i = 0;
    while i < significant.len() {
        if semis[i] {
            at_start = true;
            i += 1;
            continue;
        }
        if at_start {
            at_start = false;
            let kw = |j: usize| words.get(j).copied().flatten().map(|w| w.keyword);
            let mut j = i + 1;
            match kw(i) {
                Some(Keyword::CREATE) => {
                    if kw(j) == Some(Keyword::OR) && kw(j + 1) == Some(Keyword::REPLACE) {
                        j += 2;
                    }
                    while matches!(
                        kw(j),
                        Some(
                            Keyword::GLOBAL
                                | Keyword::LOCAL
                                | Keyword::TEMP
                                | Keyword::TEMPORARY
                                | Keyword::UNLOGGED
                        )
                    ) {
                        j += 1;
                    }
                    match kw(j) {
                        Some(Keyword::TABLE) => {
                            j += 1;
                            if kw(j) == Some(Keyword::IF) {
                                j += 3; // IF NOT EXISTS
                            }
                            if let Some(name) = object_name(&significant, j) {
                                decls.tables.push(qualify(&name));
                            }
                        }
                        Some(Keyword::FUNCTION | Keyword::PROCEDURE) => {
                            if let Some(name) = object_name(&significant, j + 1) {
                                decls.routines.push(qualify(&name));
                            }
                        }
                        _ => {}
                    }
                }
                Some(Keyword::ALTER) if kw(j) == Some(Keyword::TABLE) => {
                    j += 1;
                    if kw(j) == Some(Keyword::IF) {
                        j += 2; // IF EXISTS
                    }
                    if kw(j) == Some(Keyword::ONLY) {
                        j += 1;
                    }
                    if let Some(name) = object_name(&significant, j) {
                        decls.tables.push(qualify(&name));
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    decls.tables.dedup();
    decls
}

/// `a` ou `a.b` à partir de l'indice `start`.
fn object_name(tokens: &[&Token], start: usize) -> Option<String> {
    let Some(Token::Word(first)) = tokens.get(start) else {
        return None;
    };
    let mut name = first.value.clone();
    if let (Some(Token::Period), Some(Token::Word(second))) =
        (tokens.get(start + 1), tokens.get(start + 2))
    {
        name = format!("{name}.{}", second.value);
    }
    Some(name)
}

// ── Index du projet ───────────────────────────────────────────────────────────

/// Pour chaque table, les fichiers qui la créent ou la modifient, dans
/// l'ordre du projet. Ne dépend que des déclarations : modifier le corps d'un
/// fichier sans changer ses déclarations ne recalcule rien en aval.
#[salsa::tracked(returns(ref))]
pub fn table_index(db: &dyn Db, project: Project) -> BTreeMap<String, Vec<SourceFile>> {
    db.log_execution("table_index".into());
    let mut index: BTreeMap<String, Vec<SourceFile>> = BTreeMap::new();
    for &file in project.files(db) {
        for table in &file_declarations(db, file).tables {
            index.entry(table.clone()).or_default().push(file);
        }
    }
    index
}

#[salsa::tracked(returns(ref))]
pub fn routine_index(db: &dyn Db, project: Project) -> BTreeMap<String, Vec<SourceFile>> {
    db.log_execution("routine_index".into());
    let mut index: BTreeMap<String, Vec<SourceFile>> = BTreeMap::new();
    for &file in project.files(db) {
        for routine in &file_declarations(db, file).routines {
            index.entry(routine.clone()).or_default().push(file);
        }
    }
    index
}

// ── Résolution paresseuse d'une table ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, salsa::SalsaValue)]
pub struct Column {
    pub name: String,
    /// Type, sous forme textuelle stable (Debug de l'AST) : suffisant pour
    /// comparer, ce n'est pas le modèle de types de P2.
    pub data_type: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, salsa::SalsaValue)]
pub struct TableDef {
    pub columns: Vec<Column>,
}

/// La table telle qu'elle est après application, dans l'ordre, de tous les
/// DDL qui la concernent. Seuls les fichiers qui la touchent sont parsés.
#[salsa::tracked(returns(ref))]
pub fn resolve_table<'db>(
    db: &'db dyn Db,
    project: Project,
    name: TableName<'db>,
) -> Option<TableDef> {
    let key = name.text(db).clone();
    db.log_execution(format!("resolve_table({key})"));
    let files = table_index(db, project).get(&key)?;

    let mut table: Option<TableDef> = None;
    for &file in files {
        for stmt in parse_file(db, file).0.iter() {
            apply_to_table(stmt, &key, &mut table);
        }
    }
    table
}

fn apply_to_table(stmt: &Statement, key: &str, table: &mut Option<TableDef>) {
    let Statement::Ddl(ddl) = stmt else { return };
    match ddl {
        DdlStatement::CreateTable(ct) if qualify(&object_name_to_string(&ct.name)) == key => {
            *table = Some(TableDef {
                columns: ct
                    .columns
                    .iter()
                    .map(|c| Column {
                        name: c.name.value.clone(),
                        data_type: format!("{:?}", c.data_type),
                        nullable: c.nullable,
                    })
                    .collect(),
            });
        }
        DdlStatement::AlterTable(at) if qualify(&object_name_to_string(&at.name)) == key => {
            let Some(t) = table.as_mut() else { return };
            for op in &at.operations {
                match op {
                    AlterTableOp::AddColumn(c) => {
                        let lower = c.name.value.to_lowercase();
                        if !t.columns.iter().any(|x| x.name.to_lowercase() == lower) {
                            t.columns.push(Column {
                                name: c.name.value.clone(),
                                data_type: format!("{:?}", c.data_type),
                                nullable: c.nullable,
                            });
                        }
                    }
                    AlterTableOp::DropColumn { name, .. } => {
                        let lower = name.value.to_lowercase();
                        t.columns.retain(|x| x.name.to_lowercase() != lower);
                    }
                    AlterTableOp::AlterColumn { name, action } => {
                        let lower = name.value.to_lowercase();
                        if let Some(col) = t
                            .columns
                            .iter_mut()
                            .find(|x| x.name.to_lowercase() == lower)
                        {
                            match action {
                                AlterColumnAction::SetNotNull => col.nullable = false,
                                AlterColumnAction::DropNotNull => col.nullable = true,
                                AlterColumnAction::SetDataType(dt) => {
                                    col.data_type = format!("{dt:?}")
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

// ── Portée de routine ─────────────────────────────────────────────────────────

/// Ce qu'une fonction ou procédure ajoute au contexte de son corps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, salsa::SalsaValue)]
pub struct RoutineScope {
    pub params: Vec<(String, String)>,
    pub variables: Vec<(String, String)>,
    /// Tables temporaires créées dans le corps, visibles du corps seulement.
    pub temp_tables: BTreeMap<String, TableDef>,
}

#[salsa::tracked(returns(ref))]
pub fn routine_scope<'db>(
    db: &'db dyn Db,
    project: Project,
    name: RoutineName<'db>,
) -> Option<RoutineScope> {
    let key = name.text(db).clone();
    db.log_execution(format!("routine_scope({key})"));
    // La dernière définition gagne (CREATE OR REPLACE).
    let file = *routine_index(db, project).get(&key)?.last()?;
    let parsed = parse_file(db, file);
    let (params, body) = parsed.0.iter().rev().find_map(|s| match s {
        Statement::Ddl(DdlStatement::CreateFunction(f))
            if qualify(&object_name_to_string(&f.name)) == key =>
        {
            Some((&f.params, &f.body))
        }
        Statement::Ddl(DdlStatement::CreateProcedure(p))
            if qualify(&object_name_to_string(&p.name)) == key =>
        {
            Some((&p.params, &p.body))
        }
        _ => None,
    })?;

    let mut scope = RoutineScope {
        params: params
            .iter()
            .filter_map(|p| {
                let n = p.name.as_ref()?;
                Some((n.value.to_lowercase(), format!("{:?}", p.data_type)))
            })
            .collect(),
        ..Default::default()
    };
    if let FunctionBody::Parsed(body) = body {
        scope.variables = body
            .block
            .declarations
            .iter()
            .filter_map(|d| match d {
                Declaration::Variable(v) => {
                    Some((v.name.value.to_lowercase(), format!("{:?}", v.var_type)))
                }
                _ => None,
            })
            .collect();
        collect_temp_tables(&body.block, &mut scope.temp_tables);
    }
    Some(scope)
}

fn collect_temp_tables(block: &Block, out: &mut BTreeMap<String, TableDef>) {
    collect_in_statements(&block.statements, out);
}

fn collect_in_statements(stmts: &[PlStatement], out: &mut BTreeMap<String, TableDef>) {
    for stmt in stmts {
        match &stmt.kind {
            PlStatementKind::Sql {
                statement: s @ Statement::Ddl(DdlStatement::CreateTable(ct)),
                ..
            } if ct.temporary => {
                let key = object_name_to_string(&ct.name).to_lowercase();
                let mut def = None;
                apply_to_table(s, &qualify(&key), &mut def);
                if let Some(def) = def {
                    out.insert(key, def);
                }
            }
            PlStatementKind::If {
                then_branch,
                elsif_branches,
                else_branch,
                ..
            } => {
                collect_in_statements(then_branch, out);
                for (_, b) in elsif_branches {
                    collect_in_statements(b, out);
                }
                if let Some(b) = else_branch {
                    collect_in_statements(b, out);
                }
            }
            PlStatementKind::Block(b) => collect_temp_tables(b, out),
            PlStatementKind::Loop { body, .. }
            | PlStatementKind::While { body, .. }
            | PlStatementKind::ForRange { body, .. }
            | PlStatementKind::ForQuery { body, .. }
            | PlStatementKind::ForExecute { body, .. } => collect_in_statements(body, out),
            _ => {}
        }
    }
}

/// D'où vient une table résolue depuis le corps d'une routine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    TempTable(TableDef),
    Global(TableDef),
}

/// Résolution d'un nom de table dans le corps d'une routine : les tables
/// temporaires du corps masquent les tables globales (comme `pg_temp` en
/// tête du `search_path`).
pub fn resolve_in_routine(
    db: &dyn Db,
    project: Project,
    routine: &str,
    table: &str,
) -> Option<Resolved> {
    let routine = RoutineName::new(db, qualify(routine));
    if let Some(scope) = routine_scope(db, project, routine)
        && let Some(def) = scope.temp_tables.get(&table.to_lowercase())
    {
        return Some(Resolved::TempTable(def.clone()));
    }
    let name = TableName::new(db, qualify(table));
    resolve_table(db, project, name)
        .clone()
        .map(Resolved::Global)
}

fn file_label(db: &dyn Db, file: SourceFile) -> String {
    file.path(db)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
