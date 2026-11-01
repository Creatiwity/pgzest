//! Catalogue d'un projet : les fichiers de schéma et les migrations rejoués
//! dans l'ordre des sources, sur le catalogue de la version de PostgreSQL.
//!
//! Remplace `SchemaResolver` et `DdlSchemaProvider` : un seul moteur
//! d'application des DDL (D2). La vue [`SchemaProvider`] sert les
//! consommateurs pas encore migrés.
//!
//! [`ProjectBuilder`] porte le rejeu fichier par fichier : la CLI l'alimente
//! en parsant les fragments ([`ProjectCatalog::from_chunks_with_macros`]),
//! la base incrémentale (`pgweave-project`) avec ses parses mémorisés. Les
//! deux suivent donc exactement les mêmes règles.

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

/// Version de PostgreSQL par défaut : la seule prise en charge aujourd'hui
/// (`[postgres] version` de `pgweave.toml`).
pub const DEFAULT_POSTGRES: u32 = 18;

pub struct ProjectCatalog {
    pub catalog: Catalog,
    /// `search_path` en fin de rejeu.
    pub search_path: SearchPath,
    /// Statements écartés pendant le parse, avec leur fichier : l'état
    /// reconstruit peut être incomplet.
    pub skipped: Vec<(PathBuf, SkippedStatement)>,
    /// Diagnostics du rejeu, avec leur fichier.
    pub diagnostics: Vec<(PathBuf, Diagnostic)>,
    view: CatalogSchema,
}

fn base() -> Replay {
    Replay::new(
        Catalog::for_postgres(DEFAULT_POSTGRES).expect("pack postgres@18 livré avec pgweave"),
    )
}

/// Parse d'un fragment du projet, macros désucrées. `offset` est la position
/// du fragment dans son document : les spans sont exprimés dans le repère du
/// document hôte.
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

/// Rejeu ordonné des fichiers d'un projet (schéma, puis migrations par
/// séquence), chacun déjà parsé.
pub struct ProjectBuilder {
    replay: Replay,
    skipped: Vec<(PathBuf, SkippedStatement)>,
    diagnostics: Vec<(PathBuf, Diagnostic)>,
    /// Première migration sans en-tête, et leur nombre.
    without_header: Option<(PathBuf, usize)>,
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
            skipped: Vec::new(),
            diagnostics: Vec::new(),
            without_header: None,
        }
    }

    /// Les colonnes des vues sont typées par `typer` (voir [`QueryTyper`]).
    pub fn with_typer(mut self, typer: Option<std::sync::Arc<dyn QueryTyper>>) -> Self {
        self.replay.typer = typer;
        self
    }

    /// Applique un fichier : `sql` est son texte (en-tête de migration),
    /// `parsed` son parse. Un fichier illisible n'apporte rien au catalogue ;
    /// l'erreur de parse est signalée par la vérification du fichier.
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
        let mut diags = Vec::new();
        for s in &outcome.statements {
            self.replay.apply(s, &mut diags);
        }
        if is_migration {
            check_header(sql, &self.replay.catalog, &mut diags);
        }
        self.diagnostics
            .extend(diags.into_iter().map(|d| (path.to_path_buf(), d)));
    }

    pub fn finish(mut self) -> ProjectCatalog {
        if let Some((first, n)) = self.without_header {
            self.diagnostics.push((
                first,
                Diagnostic::info(
                    codes::MIGRATION_WITHOUT_HEADER,
                    format!(
                        "{n} migration(s) sans en-tête `-- pgweave:` : analysées avec postgres@{DEFAULT_POSTGRES}"
                    ),
                ),
            ));
        }
        let view = CatalogSchema::new(&self.replay.catalog, &self.replay.search_path);
        ProjectCatalog {
            catalog: self.replay.catalog,
            search_path: self.replay.search_path,
            skipped: self.skipped,
            diagnostics: self.diagnostics,
            view,
        }
    }
}

impl ProjectCatalog {
    /// Catalogue de base, sans objet du projet.
    pub fn empty() -> Self {
        ProjectBuilder::new().finish()
    }

    /// Rejeu de statements déjà parsés, dans l'ordre.
    pub fn from_statements(stmts: &[Statement]) -> Self {
        let mut b = ProjectBuilder::new();
        let outcome = ParseOutcome {
            statements: stmts.to_vec(),
            skipped: Vec::new(),
        };
        b.add_file(Path::new(""), "", &FileRole::Schema, Ok(&outcome));
        b.finish()
    }

    /// Rejeu d'un texte SQL (tests, fichier isolé).
    pub fn from_sql(sql: &str) -> Result<Self, String> {
        let stmts = pgweave_parser::parse(sql).map_err(|e| e.to_string())?;
        Ok(Self::from_statements(&stmts))
    }

    pub fn from_chunks(chunks: &[SqlChunk]) -> Self {
        Self::from_chunks_with_macros(chunks, &pgweave_macro::MacroSet::builtin())
    }

    /// Rejeu des fragments du projet, dans l'ordre où la source les donne
    /// (fichiers de schéma, puis migrations par séquence). Les macros sont
    /// désucrées d'abord : une table déclarée par une macro existe.
    pub fn from_chunks_with_macros(chunks: &[SqlChunk], macros: &pgweave_macro::MacroSet) -> Self {
        Self::from_chunks_typed(chunks, macros, None)
    }

    /// Comme [`Self::from_chunks_with_macros`], les colonnes des vues typées
    /// par `typer`.
    pub fn from_chunks_typed(
        chunks: &[SqlChunk],
        macros: &pgweave_macro::MacroSet,
        typer: Option<std::sync::Arc<dyn QueryTyper>>,
    ) -> Self {
        let mut b = ProjectBuilder::new().with_typer(typer);
        for chunk in chunks {
            let parsed = parse_source(&chunk.sql, chunk.origin.byte_range.start, macros);
            b.add_file(&chunk.origin.path, &chunk.sql, &chunk.role, parsed.as_ref());
        }
        b.finish()
    }
}

/// Confronte l'en-tête d'une migration à l'état rejoué après elle (D9).
fn check_header(sql: &str, cat: &Catalog, out: &mut Vec<Diagnostic>) {
    let Some(Ok(h)) = pgweave_schema::MigrationHeader::find(sql) else {
        return;
    };
    if h.postgres != DEFAULT_POSTGRES {
        out.push(Diagnostic::info(
            codes::MIGRATION_HEADER_MISMATCH,
            format!(
                "migration écrite pour postgres@{}, analysée avec postgres@{DEFAULT_POSTGRES} (seule version prise en charge)",
                h.postgres
            ),
        ));
    }
    for e in &h.extensions {
        match cat.extensions.get(&e.name) {
            None => out.push(Diagnostic::error(
                codes::MIGRATION_HEADER_MISMATCH,
                format!(
                    "l'en-tête annonce l'extension {}@{}, qui n'est pas créée à ce point des migrations",
                    e.name, e.version
                ),
            )),
            Some(active) if active.version.as_deref() != Some(e.version.as_str()) => {
                out.push(Diagnostic::error(
                    codes::MIGRATION_HEADER_MISMATCH,
                    format!(
                        "l'en-tête annonce {}@{}, l'extension active est en version {}",
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
                format!("l'extension {name} est active mais absente de l'en-tête"),
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
