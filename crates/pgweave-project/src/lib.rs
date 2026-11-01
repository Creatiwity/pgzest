//! Base de projet incrémentale (PLAN.md, D3 ; décision 0001).
//!
//! ```text
//! SourceFile (entrée) ──► parse_file ──┬──────────────► check_file(f)
//!                                      │                      ▲
//! Project (entrée) ─► project_catalog ─┴─ fichiers de schéma │
//!                     (rejeu ordonné)      et migrations ─────┘
//!                  └► trigger_definitions ───────────────────┘
//! ```
//!
//! - `parse_file` relit un fichier dont le texte change (`no_eq` : l'AST
//!   n'est pas comparable) ;
//! - `project_catalog` ne lit que les fichiers de schéma et les migrations ;
//!   s'il retrouve le même catalogue (un commentaire modifié), salsa s'arrête
//!   là et aucune vérification n'est relancée ;
//! - `check_file` ne dépend que de son fichier, du catalogue et des
//!   définitions de triggers (contexte des fonctions trigger).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pgweave_ast::Statement;
use pgweave_catalog::{Catalog, ProjectCatalog, SearchPath};
use pgweave_diagnostics::Diagnostic;
use pgweave_schema::FileRole;

// ── Entrées ───────────────────────────────────────────────────────────────────

/// Un fichier du projet : son texte est ce qui change.
#[salsa::input(debug)]
pub struct SourceFile {
    #[returns(ref)]
    pub path: PathBuf,
    #[returns(ref)]
    pub text: String,
    #[returns(ref)]
    pub role: FileRole,
}

/// Le projet : ses fichiers, dans l'ordre d'application (schéma, puis
/// migrations par séquence).
#[salsa::input]
pub struct Project {
    #[returns(ref)]
    pub files: Vec<SourceFile>,
}

// ── Base ──────────────────────────────────────────────────────────────────────

#[salsa::db]
pub trait Db: salsa::Database {
    /// Trace des requêtes exécutées (tests d'incrémentalité, diagnostic).
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
        std::mem::take(&mut *self.executions.lock().expect("journal"))
    }
}

#[salsa::db]
impl salsa::Database for Database {}

#[salsa::db]
impl Db for Database {
    fn log_execution(&self, what: String) {
        self.executions.lock().expect("journal").push(what);
    }
}

fn label(db: &dyn Db, file: SourceFile) -> String {
    file.path(db)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ── Parse ─────────────────────────────────────────────────────────────────────

/// Statements d'un fichier, macros désucrées.
#[derive(Clone)]
pub struct ParsedFile(pub Arc<Vec<Statement>>);

#[salsa::tracked(returns(ref), no_eq)]
pub fn parse_file(db: &dyn Db, file: SourceFile) -> ParsedFile {
    db.log_execution(format!("parse_file({})", label(db, file)));
    let text = file.text(db);
    let expansion = pgweave_macro::expand_source(text, &pgweave_macro::MacroSet::builtin());
    let parsed = if expansion.is_passthrough() {
        pgweave_parser::parse_with_outcome(text)
    } else {
        pgweave_parser::parse_mapped_with_outcome(&expansion.sql, &expansion.map)
    };
    ParsedFile(Arc::new(parsed.map(|o| o.statements).unwrap_or_default()))
}

fn defines_schema(role: &FileRole) -> bool {
    matches!(role, FileRole::Schema | FileRole::Migration { .. })
}

// ── Catalogue ─────────────────────────────────────────────────────────────────

/// Catalogue du projet et `search_path` final. Comparé par contenu : un
/// rejeu qui retrouve le même catalogue n'invalide rien en aval.
#[derive(Clone, PartialEq)]
pub struct CatalogState {
    pub catalog: Arc<Catalog>,
    pub search_path: SearchPath,
}

#[salsa::tracked(returns(ref))]
pub fn project_catalog(db: &dyn Db, project: Project) -> CatalogState {
    db.log_execution("project_catalog".into());
    let stmts: Vec<Statement> = project
        .files(db)
        .iter()
        .filter(|f| defines_schema(f.role(db)))
        .flat_map(|f| parse_file(db, *f).0.iter().cloned())
        .collect();
    let p = ProjectCatalog::from_statements(&stmts);
    CatalogState {
        catalog: Arc::new(p.catalog),
        search_path: p.search_path,
    }
}

/// `CREATE TRIGGER` du projet, émis : le contexte des fonctions trigger
/// (table, événements). Texte comparable, donc une modification qui ne
/// touche aucun trigger n'invalide pas les vérifications.
#[salsa::tracked(returns(ref))]
pub fn trigger_definitions(db: &dyn Db, project: Project) -> Vec<String> {
    db.log_execution("trigger_definitions".into());
    project
        .files(db)
        .iter()
        .flat_map(|f| parse_file(db, *f).0.iter())
        .filter(|s| {
            matches!(
                s,
                Statement::Ddl(pgweave_ast::DdlStatement::CreateTrigger(_))
            )
        })
        .map(pgweave_ast::emit::emit_statement)
        .collect()
}

// ── Vérification ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct FileDiagnostics(pub Arc<Vec<Diagnostic>>);

#[salsa::tracked(returns(ref), no_eq)]
pub fn check_file(db: &dyn Db, project: Project, file: SourceFile) -> FileDiagnostics {
    db.log_execution(format!("check_file({})", label(db, file)));
    let state = project_catalog(db, project);
    let schema = pgweave_catalog::compat::CatalogSchema::new(&state.catalog, &state.search_path);
    let triggers: Vec<Statement> = trigger_definitions(db, project)
        .iter()
        .filter_map(|sql| pgweave_parser::parse(sql).ok())
        .flatten()
        .collect();
    let stmts = &parse_file(db, file).0;
    let checker = pgweave_checker::Checker::with_builtin_rules();
    FileDiagnostics(Arc::new(
        checker.run_with_context(stmts, &triggers, &schema),
    ))
}
