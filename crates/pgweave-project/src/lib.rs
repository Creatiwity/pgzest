//! Base de projet incrémentale (PLAN.md, D3 ; décision 0001).
//!
//! ```text
//! SourceFile ─┐
//!             ├► parse_file ──┬──────────────────────────► check_file(f)
//! Project ────┘ (macros)      │                                  ▲
//!   └─────────► project_replay ─► project_catalog ───────────────┘
//!               (rejeu ordonné et   (comparé par contenu)
//!                vérifié, diagnostics)
//! ```
//!
//! - `parse_file` relit un fichier dont le texte change, ou tous les
//!   fichiers si les macros du projet changent (`no_eq` : l'AST n'est pas
//!   comparable) ;
//! - `project_replay` ne lit que les fichiers de schéma et les migrations, et
//!   garde les diagnostics du rejeu avec leurs spans ;
//! - `project_catalog` n'en retient que le catalogue et le `search_path` :
//!   s'il retrouve les mêmes (un commentaire modifié), salsa s'arrête là et
//!   aucune vérification n'est relancée ;
//! - `check_file` d'un fichier de requêtes ne dépend que de son fichier et
//!   du catalogue ; un fichier de schéma ou de migration est vérifié pendant
//!   le rejeu (chaque statement contre l'état du catalogue à ce point), et
//!   `check_file` en rend les diagnostics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pgweave_ast::Statement;
use pgweave_catalog::ProjectCatalog;
use pgweave_catalog::project::{ProjectBuilder, parse_source};
use pgweave_diagnostics::Diagnostic;
use pgweave_macro::MacroSet;
use pgweave_parser::{ParseError, ParseOutcome};
use pgweave_schema::{FileRole, SqlChunk};
use salsa::Setter;

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
    /// Position du texte dans son document (fragment SQL d'un `.ts`, d'un
    /// Markdown…) ; 0 pour un fichier SQL.
    pub offset: usize,
}

/// Le projet : ses fichiers, dans l'ordre d'application (schéma, puis
/// migrations par séquence), et les macros reconnues.
#[salsa::input]
pub struct Project {
    #[returns(ref)]
    pub files: Vec<SourceFile>,
    #[returns(ref)]
    pub macros: Arc<MacroSet>,
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

/// Parse d'un fichier, macros désucrées : statements compris et écartés, ou
/// l'erreur qui a empêché toute lecture.
#[derive(Clone)]
pub struct ParsedFile(pub Arc<Result<ParseOutcome, ParseError>>);

impl ParsedFile {
    /// Statements compris (aucun si le fichier est illisible).
    pub fn statements(&self) -> &[Statement] {
        match &*self.0 {
            Ok(o) => &o.statements,
            Err(_) => &[],
        }
    }
}

#[salsa::tracked(returns(ref), no_eq)]
pub fn parse_file(db: &dyn Db, project: Project, file: SourceFile) -> ParsedFile {
    db.log_execution(format!("parse_file({})", label(db, file)));
    let parsed = parse_source(file.text(db), *file.offset(db), project.macros(db));
    ParsedFile(Arc::new(parsed))
}

fn defines_schema(role: &FileRole) -> bool {
    matches!(role, FileRole::Schema | FileRole::Migration { .. })
}

// ── Catalogue ─────────────────────────────────────────────────────────────────

/// Rejeu des fichiers de schéma et des migrations, dans l'ordre du projet :
/// catalogue, statements écartés et diagnostics de rejeu par fichier.
#[salsa::tracked(returns(ref), no_eq)]
pub fn project_replay(db: &dyn Db, project: Project) -> Arc<ProjectCatalog> {
    db.log_execution("project_replay".into());
    let mut builder = ProjectBuilder::new()
        .with_typer(Some(pgweave_sema::ViewTyper::shared()))
        .with_checker(Some(pgweave_sema::check::SemaChecker::shared()));
    for &file in project.files(db) {
        if !defines_schema(file.role(db)) {
            continue;
        }
        let parsed = parse_file(db, project, file);
        builder.add_file(
            file.path(db),
            file.text(db),
            file.role(db),
            parsed.0.as_ref().as_ref(),
        );
    }
    Arc::new(builder.finish())
}

/// Catalogue du projet, comparé par contenu (catalogue et `search_path`) :
/// un rejeu qui retrouve le même catalogue n'invalide rien en aval, même si
/// les spans de ses diagnostics ont bougé.
#[derive(Clone)]
pub struct CatalogState(pub Arc<ProjectCatalog>);

impl PartialEq for CatalogState {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || (self.0.catalog == other.0.catalog && self.0.search_path == other.0.search_path)
    }
}

#[salsa::tracked(returns(ref))]
pub fn project_catalog(db: &dyn Db, project: Project) -> CatalogState {
    db.log_execution("project_catalog".into());
    CatalogState(project_replay(db, project).clone())
}

// ── Vérification ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct FileDiagnostics(pub Arc<Vec<Diagnostic>>);

/// Diagnostics de vérification d'un fichier : ceux de son rejeu pour un
/// fichier de schéma ou de migration, sinon ses statements vérifiés contre
/// le catalogue du projet.
#[salsa::tracked(returns(ref), no_eq)]
pub fn check_file(db: &dyn Db, project: Project, file: SourceFile) -> FileDiagnostics {
    db.log_execution(format!("check_file({})", label(db, file)));
    if defines_schema(file.role(db)) {
        let path = file.path(db);
        return FileDiagnostics(Arc::new(
            project_replay(db, project)
                .diagnostics
                .iter()
                .filter(|(p, _)| p == path)
                .map(|(_, d)| d.clone())
                .collect(),
        ));
    }
    let state = project_catalog(db, project);
    let stmts = parse_file(db, project, file).statements();
    FileDiagnostics(Arc::new(pgweave_checker::check_statements(
        stmts,
        state.0.as_ref(),
    )))
}

// ── Espace de travail ─────────────────────────────────────────────────────────

/// Projet chargé en mémoire : la base et ses entrées, indexées par chemin.
///
/// C'est ce que tient le serveur de langage : les fragments du projet lus
/// sur disque au chargement, puis le texte des documents ouverts à mesure
/// qu'ils sont modifiés. Seul ce qui dépend d'un fichier modifié est
/// recalculé.
pub struct Workspace {
    db: Database,
    project: Project,
    /// Fragments par document. Un document hôte (`.ts`, Markdown) peut en
    /// porter plusieurs : son texte n'est alors pas un fragment, et il n'est
    /// pas mis à jour depuis l'éditeur.
    by_path: HashMap<PathBuf, Vec<SourceFile>>,
}

impl Workspace {
    /// Projet formé des fragments `chunks`, dans l'ordre d'application.
    pub fn new(chunks: &[SqlChunk], macros: Arc<MacroSet>) -> Self {
        let db = Database::default();
        let mut by_path: HashMap<PathBuf, Vec<SourceFile>> = HashMap::new();
        let files: Vec<SourceFile> = chunks
            .iter()
            .map(|c| {
                let f = SourceFile::new(
                    &db,
                    c.origin.path.clone(),
                    c.sql.clone(),
                    c.role.clone(),
                    c.origin.byte_range.start,
                );
                by_path.entry(c.origin.path.clone()).or_default().push(f);
                f
            })
            .collect();
        let project = Project::new(&db, files, macros);
        Workspace {
            db,
            project,
            by_path,
        }
    }

    /// Remplace le texte d'un fichier du projet par celui de l'éditeur.
    /// Renvoie `false` si `path` n'est pas un fichier SQL du projet (inconnu,
    /// ou document hôte de plusieurs fragments).
    pub fn set_text(&mut self, path: &Path, text: &str) -> bool {
        let Some(&[file]) = self.by_path.get(path).map(Vec::as_slice) else {
            return false;
        };
        if *file.offset(&self.db) != 0 {
            return false;
        }
        if file.text(&self.db) != text {
            file.set_text(&mut self.db).to(text.to_string());
        }
        true
    }

    /// Remplace les macros du projet : tous les fichiers sont relus.
    pub fn set_macros(&mut self, macros: Arc<MacroSet>) {
        self.project.set_macros(&mut self.db).to(macros);
    }

    /// `path` est un fichier du projet.
    pub fn contains(&self, path: &Path) -> bool {
        self.by_path.contains_key(path)
    }

    /// Catalogue du projet à jour (rejeu mémorisé).
    pub fn catalog(&self) -> Arc<ProjectCatalog> {
        project_replay(&self.db, self.project).clone()
    }

    /// Diagnostics du rejeu rattachés à `path`.
    pub fn replay_diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        project_replay(&self.db, self.project)
            .diagnostics
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, d)| d.clone())
            .collect()
    }

    /// Requêtes exécutées depuis le dernier appel.
    pub fn take_executions(&self) -> Vec<String> {
        self.db.take_executions()
    }
}
