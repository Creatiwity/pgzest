//! Catalogue d'un projet : les fichiers de schéma et les migrations rejoués
//! dans l'ordre des sources, sur le catalogue de la version de PostgreSQL.
//!
//! Remplace `SchemaResolver` et `DdlSchemaProvider` : un seul moteur
//! d'application des DDL (D2). La vue [`SchemaProvider`] sert les
//! consommateurs pas encore migrés ; l'index de navigation (LSP) est
//! conservé tel quel.

use std::path::PathBuf;

use pgweave_ast::Statement;
use pgweave_diagnostics::Diagnostic;
use pgweave_parser::SkippedStatement;
use pgweave_schema::SchemaIndex;
use pgweave_schema::provider::{
    FunctionSignature, RlsAwareSchemaProvider, SchemaProvider, TableSchema,
};
use pgweave_schema::source::SqlChunk;

use crate::apply::Replay;
use crate::catalog::{Catalog, SearchPath};
use crate::compat::CatalogSchema;

/// Version de PostgreSQL par défaut : la seule prise en charge aujourd'hui
/// (`[postgres] version` de `pgweave.toml`).
pub const DEFAULT_POSTGRES: u32 = 18;

pub struct ProjectCatalog {
    pub catalog: Catalog,
    /// `search_path` en fin de rejeu.
    pub search_path: SearchPath,
    /// Index des définitions par fichier, pour la navigation.
    pub index: SchemaIndex,
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

impl ProjectCatalog {
    fn finish(
        replay: Replay,
        index: SchemaIndex,
        skipped: Vec<(PathBuf, SkippedStatement)>,
        diagnostics: Vec<(PathBuf, Diagnostic)>,
    ) -> Self {
        let view = CatalogSchema::new(&replay.catalog, &replay.search_path);
        ProjectCatalog {
            catalog: replay.catalog,
            search_path: replay.search_path,
            index,
            skipped,
            diagnostics,
            view,
        }
    }

    /// Catalogue de base, sans objet du projet.
    pub fn empty() -> Self {
        Self::finish(base(), SchemaIndex::default(), Vec::new(), Vec::new())
    }

    /// Rejeu de statements déjà parsés, dans l'ordre.
    pub fn from_statements(stmts: &[Statement]) -> Self {
        let mut replay = base();
        let mut diags = Vec::new();
        for s in stmts {
            replay.apply(s, &mut diags);
        }
        let diagnostics = diags.into_iter().map(|d| (PathBuf::new(), d)).collect();
        Self::finish(replay, SchemaIndex::default(), Vec::new(), diagnostics)
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
        let mut replay = base();
        let mut skipped = Vec::new();
        let mut diagnostics = Vec::new();
        for chunk in chunks {
            let expansion = pgweave_macro::expand_source(&chunk.sql, macros);
            let parsed = if expansion.is_passthrough() {
                pgweave_parser::parse_at_with_outcome(&chunk.sql, chunk.origin.byte_range.start)
            } else {
                pgweave_parser::parse_mapped_with_outcome(&expansion.sql, &expansion.map)
            };
            let Ok(outcome) = parsed else {
                continue;
            };
            for sk in outcome.skipped {
                skipped.push((chunk.origin.path.clone(), sk));
            }
            let mut diags = Vec::new();
            for s in &outcome.statements {
                replay.apply(s, &mut diags);
            }
            diagnostics.extend(diags.into_iter().map(|d| (chunk.origin.path.clone(), d)));
        }
        Self::finish(
            replay,
            SchemaIndex::from_chunks(chunks),
            skipped,
            diagnostics,
        )
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
