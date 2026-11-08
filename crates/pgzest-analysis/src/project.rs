//! Incremental project database (PLAN.md, D3; decision 0001).
//!
//! ```text
//! SourceFile ─┐
//!             ├► parse_file ──┬──────────────────────────► check_file(f)
//! Project ────┘ (macros)      │                                  ▲
//!   └─────────► project_replay ─► project_catalog ───────────────┘
//!               (ordered, checked   (compared by content)
//!                replay, diagnostics)
//! ```
//!
//! - `parse_file` re-reads a file whose text changes, or every file when
//!   the project macros change (`no_eq`: the AST is not comparable);
//! - `project_replay` only reads schema files and migrations, and keeps the
//!   replay diagnostics with their spans;
//! - `project_catalog` keeps only the catalog and the `search_path`: if it
//!   finds the same ones again (an edited comment), salsa stops there and no
//!   check is rerun;
//! - `check_file` of a query file only depends on that file and the catalog;
//!   a schema or migration file is checked during the replay (each statement
//!   against the catalog state at that point), and `check_file` returns those
//!   diagnostics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pgzest_ast::Statement;
use pgzest_catalog::project::{ProjectBuilder, parse_source};
use pgzest_catalog::{ProjectCatalog, ProjectPacks};
use pgzest_diagnostics::Diagnostic;
use pgzest_expand::MacroSet;
use pgzest_parser::{ParseError, ParseOutcome};
use pgzest_schema::{FileRole, SqlChunk};
use salsa::Setter;

// ── Inputs ────────────────────────────────────────────────────────────────────

/// A project file: its text is what changes.
#[salsa::input(debug)]
pub struct SourceFile {
    #[returns(ref)]
    pub path: PathBuf,
    #[returns(ref)]
    pub text: String,
    #[returns(ref)]
    pub role: FileRole,
    /// Position of the text in its document (SQL fragment of a `.ts`, of a
    /// Markdown file…); 0 for a SQL file.
    pub offset: usize,
}

/// The project: its files, in application order (schema, then migrations by
/// sequence), and the recognized macros.
#[salsa::input]
pub struct Project {
    #[returns(ref)]
    pub files: Vec<SourceFile>,
    #[returns(ref)]
    pub macros: Arc<MacroSet>,
    /// `[schema] declarative`: migrations are replayed separately (D10).
    pub declarative: bool,
    /// Extension packs of the project (`[postgres] packs`).
    #[returns(ref)]
    pub packs: Arc<ProjectPacks>,
}

// ── Base ──────────────────────────────────────────────────────────────────────

#[salsa::db]
pub trait Db: salsa::Database {
    /// Trace of executed queries (incrementality tests, diagnostics).
    fn log_execution(&self, what: String);
}

#[salsa::db]
#[derive(Clone, Default)]
pub struct Database {
    storage: salsa::Storage<Self>,
    executions: Arc<Mutex<Vec<String>>>,
}

impl Database {
    /// Queries executed since the last call, in order.
    pub fn take_executions(&self) -> Vec<String> {
        std::mem::take(&mut *self.executions.lock().expect("execution log"))
    }
}

#[salsa::db]
impl salsa::Database for Database {}

#[salsa::db]
impl Db for Database {
    fn log_execution(&self, what: String) {
        self.executions.lock().expect("execution log").push(what);
    }
}

fn label(db: &dyn Db, file: SourceFile) -> String {
    file.path(db)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ── Parse ─────────────────────────────────────────────────────────────────────

/// Parse of a file, macros desugared: understood and skipped statements, or
/// the error that prevented any reading.
#[derive(Clone)]
pub struct ParsedFile(pub Arc<Result<ParseOutcome, ParseError>>);

impl ParsedFile {
    /// Understood statements (none if the file is unreadable).
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

// ── Catalog ───────────────────────────────────────────────────────────────────

/// Replay of the schema files and migrations, in project order: catalog,
/// skipped statements and replay diagnostics per file.
#[salsa::tracked(returns(ref), no_eq)]
pub fn project_replay(db: &dyn Db, project: Project) -> Arc<ProjectCatalog> {
    db.log_execution("project_replay".into());
    let mut builder = ProjectBuilder::new()
        .with_typer(Some(pgzest_sema::ViewTyper::shared()))
        .with_checker(Some(pgzest_sema::check::SemaChecker::shared()))
        .with_instrumentation(project.macros(db).instrumentation())
        .declarative(*project.declarative(db))
        .with_packs(project.packs(db).clone());
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

/// Project catalog, compared by content (catalog and `search_path`): a replay
/// that yields the same catalog invalidates nothing downstream, even if the
/// spans of its diagnostics moved.
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

// ── Checking ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct FileDiagnostics(pub Arc<Vec<Diagnostic>>);

/// Check diagnostics of a file: those of its replay for a schema or migration
/// file, otherwise its statements checked against the project catalog.
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
    FileDiagnostics(Arc::new(crate::check_statements(stmts, state.0.as_ref())))
}

// ── Workspace ─────────────────────────────────────────────────────────────────

/// Project loaded in memory: the database and its inputs, indexed by path.
///
/// This is what the language server holds: the project fragments read from
/// disk at load time, then the text of open documents as they are edited.
/// Only what depends on an edited file is recomputed.
pub struct Workspace {
    db: Database,
    project: Project,
    /// Fragments per document. A host document (`.ts`, Markdown) may carry
    /// several: its text is then not a fragment, and it is not updated from
    /// the editor.
    by_path: HashMap<PathBuf, Vec<SourceFile>>,
}

impl Workspace {
    /// Project made of the `chunks` fragments, in application order.
    pub fn new(chunks: &[SqlChunk], macros: Arc<MacroSet>) -> Self {
        Self::with_mode(chunks, macros, false)
    }

    /// Like [`Self::new`], in declarative mode if `declarative` (D10).
    pub fn with_mode(chunks: &[SqlChunk], macros: Arc<MacroSet>, declarative: bool) -> Self {
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
        let project = Project::new(&db, files, macros, declarative, Default::default());
        Workspace {
            db,
            project,
            by_path,
        }
    }

    /// Replaces the text of a project file with the editor's one.
    /// Returns `false` if `path` is not a SQL file of the project (unknown,
    /// or host document of several fragments).
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

    /// Replaces the project's extension packs: the catalog is replayed.
    pub fn set_packs(&mut self, packs: Arc<ProjectPacks>) {
        self.project.set_packs(&mut self.db).to(packs);
    }

    /// Replaces the project macros: every file is re-read.
    pub fn set_macros(&mut self, macros: Arc<MacroSet>) {
        self.project.set_macros(&mut self.db).to(macros);
    }

    /// `path` is a project file.
    pub fn contains(&self, path: &Path) -> bool {
        self.by_path.contains_key(path)
    }

    /// Up-to-date project catalog (memoized replay).
    pub fn catalog(&self) -> Arc<ProjectCatalog> {
        project_replay(&self.db, self.project).clone()
    }

    /// Replay diagnostics attached to `path`.
    pub fn replay_diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        project_replay(&self.db, self.project)
            .diagnostics
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, d)| d.clone())
            .collect()
    }

    /// Queries executed since the last call.
    pub fn take_executions(&self) -> Vec<String> {
        self.db.take_executions()
    }
}
