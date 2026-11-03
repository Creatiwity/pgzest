//! Catalog of a project: schema files and migrations replayed in source
//! order, on top of the catalog of the PostgreSQL version.
//!
//! Replaces `SchemaResolver` and `DdlSchemaProvider`: a single DDL
//! application engine (D2). The [`SchemaProvider`] view serves consumers
//! that have not migrated yet.
//!
//! [`ProjectBuilder`] carries the file-by-file replay: the CLI feeds it by
//! parsing fragments ([`ProjectCatalog::from_chunks_with_macros`]), the
//! incremental database (`pgweave-project`) with its memoized parses. Both
//! therefore follow exactly the same rules.

use std::path::{Path, PathBuf};

use pgweave_ast::Statement;
use pgweave_diagnostics::{Diagnostic, codes};
use pgweave_parser::{ParseError, ParseOutcome, SkippedStatement};
use pgweave_schema::FileRole;
use pgweave_schema::provider::{
    FunctionSignature, RlsAwareSchemaProvider, SchemaProvider, TableSchema,
};
use pgweave_schema::source::SqlChunk;

use crate::apply::{QueryTyper, Replay};
use crate::catalog::{Catalog, SearchPath};
use crate::compat::CatalogSchema;

/// Default PostgreSQL version: the only one supported today
/// (`[postgres] version` in `pgweave.toml`).
pub const DEFAULT_POSTGRES: u32 = 18;

pub struct ProjectCatalog {
    pub catalog: Catalog,
    /// Declarative mode (D10): state reached by the migrations alone,
    /// replayed separately from the schema. `catalog` is then the target
    /// state.
    pub history: Option<Catalog>,
    /// `search_path` at the end of the replay.
    pub search_path: SearchPath,
    /// Statements skipped during parsing, with their file: the rebuilt state
    /// may be incomplete.
    pub skipped: Vec<(PathBuf, SkippedStatement)>,
    /// Replay diagnostics, with their file.
    pub diagnostics: Vec<(PathBuf, Diagnostic)>,
    view: CatalogSchema,
}

fn base() -> Replay {
    Replay::new(
        Catalog::for_postgres(DEFAULT_POSTGRES).expect("postgres@18 pack shipped with pgweave"),
    )
}

/// Parse of a project fragment, macros desugared. `offset` is the position of
/// the fragment in its document: spans are expressed in the host document's
/// coordinates.
pub fn parse_source(
    sql: &str,
    offset: usize,
    macros: &pgweave_macro::MacroSet,
) -> Result<ParseOutcome, ParseError> {
    let expansion = pgweave_macro::expand_source(sql, macros);
    if expansion.is_passthrough() {
        pgweave_parser::parse_at_with_outcome(sql, offset)
    } else {
        pgweave_parser::parse_mapped_with_outcome(&expansion.sql, &expansion.map)
    }
}

/// Statement checking during the replay.
///
/// Provided by semantic analysis (`pgweave-sema`), which depends on the
/// catalog. A statement is checked when PostgreSQL would check it: when it is
/// applied, against the catalog state at that point (a migration query sees
/// the columns of that moment), or at the end of the replay for a body
/// executed later (PL/pgSQL), if it is still in force.
pub trait StatementChecker: Send + Sync {
    /// `stmt` was just applied to the catalog.
    fn check_applied(
        &self,
        catalog: &Catalog,
        path: &SearchPath,
        stmt: &Statement,
        out: &mut Vec<Diagnostic>,
    );
    /// Part of `stmt` can only be checked against the final state.
    fn is_deferred(&self, stmt: &Statement) -> bool;
    /// Deferred part of `stmt`, against the final catalog.
    fn check_deferred(
        &self,
        catalog: &Catalog,
        path: &SearchPath,
        stmt: &Statement,
        out: &mut Vec<Diagnostic>,
    );
}

/// Ordered replay of a project's files (schema, then migrations by
/// sequence), each one already parsed.
///
/// In declarative mode (D10), the schema alone describes the target state:
/// migrations are replayed into a separate catalog (their history), and the
/// gap between the two is reported (`schema-drift`).
pub struct ProjectBuilder {
    replay: Replay,
    /// Migration replay, in declarative mode.
    history: Option<History>,
    skipped: Vec<(PathBuf, SkippedStatement)>,
    diagnostics: Vec<(PathBuf, Diagnostic)>,
    /// First migration without a header, and how many there are.
    without_header: Option<(PathBuf, usize)>,
    checker: Option<std::sync::Arc<dyn StatementChecker>>,
    /// Statements to check at the end of the replay: (file, path at that
    /// point, statement, full check). Full for a query file, which runs
    /// against the final state; otherwise, only the deferred part (PL/pgSQL
    /// body).
    deferred: Vec<Deferred>,
    /// First schema file, last migration: where to attach the drift.
    first_schema: Option<PathBuf>,
    last_migration: Option<PathBuf>,
    /// Keep the source text of routines (`Routine::source`).
    keep_sources: bool,
}

type Deferred = (PathBuf, SearchPath, Statement, bool);

struct History {
    replay: Replay,
    deferred: Vec<Deferred>,
}

impl Default for ProjectBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectBuilder {
    pub fn new() -> Self {
        ProjectBuilder {
            replay: base(),
            history: None,
            skipped: Vec::new(),
            diagnostics: Vec::new(),
            without_header: None,
            checker: None,
            deferred: Vec::new(),
            first_schema: None,
            last_migration: None,
            keep_sources: false,
        }
    }

    /// Keeps the written text of each routine (migration generation:
    /// PostgreSQL keeps a body verbatim, comments included).
    pub fn keep_sources(mut self, on: bool) -> Self {
        self.keep_sources = on;
        self
    }

    /// Declarative mode (`[schema] declarative`, D10): migrations are
    /// replayed separately, and compared to the schema at the end.
    pub fn declarative(mut self, on: bool) -> Self {
        self.history = on.then(|| {
            let mut replay = base();
            replay.typer = self.replay.typer.clone();
            History {
                replay,
                deferred: Vec::new(),
            }
        });
        self
    }

    /// Statements are checked by `checker` (see [`StatementChecker`]).
    pub fn with_checker(mut self, checker: Option<std::sync::Arc<dyn StatementChecker>>) -> Self {
        self.checker = checker;
        self
    }

    /// View columns are typed by `typer` (see [`QueryTyper`]).
    pub fn with_typer(mut self, typer: Option<std::sync::Arc<dyn QueryTyper>>) -> Self {
        if let Some(h) = &mut self.history {
            h.replay.typer = typer.clone();
        }
        self.replay.typer = typer;
        self
    }

    /// Applies a file: `sql` is its text (migration header), `parsed` its
    /// parse. An unreadable file brings nothing to the catalog; the parse
    /// error is reported by the file's check.
    pub fn add_file(
        &mut self,
        path: &Path,
        sql: &str,
        role: &FileRole,
        parsed: Result<&ParseOutcome, &ParseError>,
    ) {
        let is_migration = matches!(role, FileRole::Migration { .. });
        if is_migration && pgweave_schema::MigrationHeader::find(sql).is_none() {
            let e = self.without_header.get_or_insert((path.to_path_buf(), 0));
            e.1 += 1;
        }
        let Ok(outcome) = parsed else {
            return;
        };
        self.skipped.extend(
            outcome
                .skipped
                .iter()
                .map(|sk| (path.to_path_buf(), sk.clone())),
        );
        match role {
            FileRole::Schema if self.first_schema.is_none() => {
                self.first_schema = Some(path.to_path_buf());
            }
            FileRole::Migration { .. } => self.last_migration = Some(path.to_path_buf()),
            _ => {}
        }
        let mut diags = Vec::new();
        let (replay, deferred) = match &mut self.history {
            Some(h) if is_migration => (&mut h.replay, &mut h.deferred),
            _ => (&mut self.replay, &mut self.deferred),
        };
        for s in &outcome.statements {
            // A query file runs against the final schema.
            if *role == FileRole::Query {
                if self.checker.is_some() {
                    deferred.push((
                        path.to_path_buf(),
                        replay.search_path.clone(),
                        s.clone(),
                        true,
                    ));
                }
                continue;
            }
            replay.apply(s, &mut diags);
            if self.keep_sources {
                attach_source(&mut replay.catalog, s, sql);
            }
            if let Some(c) = &self.checker {
                c.check_applied(&replay.catalog, &replay.search_path, s, &mut diags);
                if c.is_deferred(s) {
                    deferred.push((
                        path.to_path_buf(),
                        replay.search_path.clone(),
                        s.clone(),
                        false,
                    ));
                }
            }
        }
        if is_migration {
            check_header(sql, &replay.catalog, &mut diags);
        }
        self.diagnostics
            .extend(diags.into_iter().map(|d| (path.to_path_buf(), d)));
    }

    pub fn finish(mut self) -> ProjectCatalog {
        if let Some(c) = &self.checker {
            let deferred = std::mem::take(&mut self.deferred);
            run_deferred(
                c.as_ref(),
                &self.replay.catalog,
                deferred,
                &mut self.diagnostics,
            );
            if let Some(h) = &mut self.history {
                let deferred = std::mem::take(&mut h.deferred);
                run_deferred(
                    c.as_ref(),
                    &h.replay.catalog,
                    deferred,
                    &mut self.diagnostics,
                );
            }
        }
        if let Some((first, n)) = self.without_header {
            self.diagnostics.push((
                first,
                Diagnostic::info(
                    codes::MIGRATION_WITHOUT_HEADER,
                    format!(
                        "{n} migration(s) without a `-- pgweave:` header: analyzed with postgres@{DEFAULT_POSTGRES}"
                    ),
                ),
            ));
        }
        let history = self.history.map(|h| h.replay.catalog);
        if let Some(h) = &history
            && let Some(d) = drift(h, &self.replay.catalog, self.last_migration.is_some())
        {
            let file = self
                .last_migration
                .or(self.first_schema)
                .unwrap_or_default();
            self.diagnostics.push((file, d));
        }
        let view = CatalogSchema::new(&self.replay.catalog, &self.replay.search_path);
        ProjectCatalog {
            catalog: self.replay.catalog,
            history,
            search_path: self.replay.search_path,
            skipped: self.skipped,
            diagnostics: self.diagnostics,
            view,
        }
    }
}

/// Attaches to the routine created by `stmt` its text in `text`, if it reads
/// back as that same statement (a text rewritten by a macro does not, the
/// emitted definition then remains the only source).
pub fn attach_source(cat: &mut Catalog, stmt: &Statement, text: &str) {
    use pgweave_ast::DdlStatement;
    let span = match stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => f.span,
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => p.span,
        _ => return,
    };
    let Some(slice) = text.get(span.start as usize..span.end as usize) else {
        return;
    };
    let definition = pgweave_ast::emit::emit_statement(stmt);
    let same = pgweave_parser::parse(slice)
        .is_ok_and(|v| v.len() == 1 && pgweave_ast::emit::emit_statement(&v[0]) == definition);
    if !same {
        return;
    }
    let source = slice.trim().trim_end_matches(';').trim_end().to_string();
    for r in cat.routines.values_mut().flatten() {
        if r.definition.as_deref() == Some(definition.as_str()) {
            r.source = Some(source.clone());
        }
    }
}

fn run_deferred(
    c: &dyn StatementChecker,
    cat: &Catalog,
    deferred: Vec<Deferred>,
    out: &mut Vec<(PathBuf, Diagnostic)>,
) {
    for (file, path, stmt, full) in deferred {
        let mut diags = Vec::new();
        if full {
            c.check_applied(cat, &path, &stmt, &mut diags);
            c.check_deferred(cat, &path, &stmt, &mut diags);
        } else if routine_in_force(cat, &stmt) {
            c.check_deferred(cat, &path, &stmt, &mut diags);
        }
        out.extend(diags.into_iter().map(|d| (file.clone(), d)));
    }
}

/// Gap between the migrations' state and the declarative schema.
fn drift(history: &Catalog, target: &Catalog, has_migrations: bool) -> Option<Diagnostic> {
    let m = crate::diff::diff(history, target);
    if m.is_empty() {
        return None;
    }
    let mut message = if has_migrations {
        format!(
            "the declarative schema differs from the state reached by the migrations ({} statement(s) to generate",
            m.steps.len()
        )
    } else {
        format!(
            "no migration establishes the declarative schema ({} statement(s) to generate",
            m.steps.len()
        )
    };
    if !m.unsupported.is_empty() {
        message.push_str(&format!(
            ", {} change(s) to write by hand",
            m.unsupported.len()
        ));
    }
    message.push(')');
    let mut d = Diagnostic::info(codes::SCHEMA_DRIFT, message);
    let first: Vec<String> = m
        .steps
        .iter()
        .take(3)
        .map(|s| s.sql.lines().next().unwrap_or_default().to_string())
        .chain(
            m.unsupported
                .iter()
                .take(3)
                .map(|u| format!("{}: {}", u.object, u.change)),
        )
        .collect();
    d = d.with_hint(format!(
        "`pgweave migrate create <name>` generates the migration; first: {}",
        first.join(" · ")
    ));
    Some(d)
}

impl ProjectCatalog {
    /// Base catalog, with no project object.
    pub fn empty() -> Self {
        ProjectBuilder::new().finish()
    }

    /// Replay of already parsed statements, in order.
    pub fn from_statements(stmts: &[Statement]) -> Self {
        let mut b = ProjectBuilder::new();
        let outcome = ParseOutcome {
            statements: stmts.to_vec(),
            skipped: Vec::new(),
        };
        b.add_file(Path::new(""), "", &FileRole::Schema, Ok(&outcome));
        b.finish()
    }

    /// Replay of a SQL text (tests, standalone file).
    pub fn from_sql(sql: &str) -> Result<Self, String> {
        let stmts = pgweave_parser::parse(sql).map_err(|e| e.to_string())?;
        Ok(Self::from_statements(&stmts))
    }

    pub fn from_chunks(chunks: &[SqlChunk]) -> Self {
        Self::from_chunks_with_macros(chunks, &pgweave_macro::MacroSet::builtin())
    }

    /// Replay of the project fragments, in the order the source gives them
    /// (schema files, then migrations by sequence). Macros are desugared
    /// first: a table declared by a macro exists.
    pub fn from_chunks_with_macros(chunks: &[SqlChunk], macros: &pgweave_macro::MacroSet) -> Self {
        Self::from_chunks_typed(chunks, macros, None)
    }

    /// Like [`Self::from_chunks_with_macros`], with view columns typed by
    /// `typer`.
    pub fn from_chunks_typed(
        chunks: &[SqlChunk],
        macros: &pgweave_macro::MacroSet,
        typer: Option<std::sync::Arc<dyn QueryTyper>>,
    ) -> Self {
        Self::from_chunks_checked(chunks, macros, typer, None)
    }

    /// Like [`Self::from_chunks_typed`], with statements checked by
    /// `checker` (diagnostics attached to their file).
    pub fn from_chunks_checked(
        chunks: &[SqlChunk],
        macros: &pgweave_macro::MacroSet,
        typer: Option<std::sync::Arc<dyn QueryTyper>>,
        checker: Option<std::sync::Arc<dyn StatementChecker>>,
    ) -> Self {
        let mut b = ProjectBuilder::new()
            .with_typer(typer)
            .with_checker(checker);
        for chunk in chunks {
            let parsed = parse_source(&chunk.sql, chunk.origin.byte_range.start, macros);
            b.add_file(&chunk.origin.path, &chunk.sql, &chunk.role, parsed.as_ref());
        }
        b.finish()
    }
}

/// Is the routine created by `stmt` still the catalog's one (neither
/// replaced nor dropped)? Its body is only checked in that case: a replaced
/// body will never run.
pub fn routine_in_force(cat: &Catalog, stmt: &Statement) -> bool {
    let definition = pgweave_ast::emit::emit_statement(stmt);
    cat.routines
        .values()
        .flatten()
        .any(|r| r.definition.as_deref() == Some(definition.as_str()))
}

/// Checks a migration header against the state replayed after it (D9).
fn check_header(sql: &str, cat: &Catalog, out: &mut Vec<Diagnostic>) {
    let Some(Ok(h)) = pgweave_schema::MigrationHeader::find(sql) else {
        return;
    };
    if h.postgres != DEFAULT_POSTGRES {
        out.push(Diagnostic::info(
            codes::MIGRATION_HEADER_MISMATCH,
            format!(
                "migration written for postgres@{}, analyzed with postgres@{DEFAULT_POSTGRES} (the only supported version)",
                h.postgres
            ),
        ));
    }
    for e in &h.extensions {
        match cat.extensions.get(&e.name) {
            None => out.push(Diagnostic::error(
                codes::MIGRATION_HEADER_MISMATCH,
                format!(
                    "the header declares extension {}@{}, which is not created at this point of the migrations",
                    e.name, e.version
                ),
            )),
            Some(active) if active.version.as_deref() != Some(e.version.as_str()) => {
                out.push(Diagnostic::error(
                    codes::MIGRATION_HEADER_MISMATCH,
                    format!(
                        "the header declares {}@{}, the active extension is at version {}",
                        e.name,
                        e.version,
                        active.version.as_deref().unwrap_or("?")
                    ),
                ))
            }
            Some(_) => {}
        }
    }
    for name in cat.extensions.keys() {
        if !h.extensions.iter().any(|e| &e.name == name) {
            out.push(Diagnostic::warning(
                codes::MIGRATION_HEADER_MISMATCH,
                format!("extension {name} is active but missing from the header"),
            ));
        }
    }
}

impl SchemaProvider for ProjectCatalog {
    fn get_table(&self, name: &str) -> Option<&TableSchema> {
        self.view.get_table(name)
    }

    fn get_function(&self, name: &str) -> Option<&FunctionSignature> {
        self.view.get_function(name)
    }

    fn all_table_names(&self) -> Vec<&str> {
        self.view.all_table_names()
    }

    fn as_rls_aware(&self) -> Option<&dyn RlsAwareSchemaProvider> {
        Some(self)
    }
}

impl RlsAwareSchemaProvider for ProjectCatalog {
    fn table_has_rls(&self, table_name: &str) -> bool {
        self.view.table_has_rls(table_name)
    }
}
