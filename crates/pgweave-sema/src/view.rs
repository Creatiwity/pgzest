//! Typage des vues pour le rejeu du catalogue ([`QueryTyper`]).

use std::sync::Arc;

use pgweave_ast::stmt::SelectStmt;
use pgweave_catalog::{Catalog, QueryTyper, SearchPath, TypeRef};

use crate::expr::Analyzer;
use crate::types::{Ty, TypeSystem};

/// Typeur des requêtes de vues par l'analyse sémantique.
pub struct ViewTyper;

impl ViewTyper {
    /// À passer au rejeu (`ProjectBuilder::with_typer`).
    pub fn shared() -> Arc<dyn QueryTyper> {
        Arc::new(ViewTyper)
    }
}

/// Référence de catalogue d'un type : `élément[]` pour le tableau attitré
/// d'un élément, comme dans les packs.
fn type_ref(ts: &TypeSystem, ty: &Ty, modifiers: Vec<String>) -> TypeRef {
    match ts.element(ty) {
        Some(e) if ts.array_of(&e).as_ref() == Some(ty) => {
            TypeRef::Array(Box::new(TypeRef::Named { name: e, modifiers }))
        }
        _ => TypeRef::Named {
            name: ty.clone(),
            modifiers,
        },
    }
}

impl QueryTyper for ViewTyper {
    fn output_columns(
        &self,
        catalog: &Catalog,
        path: &SearchPath,
        query: &SelectStmt,
    ) -> Result<Vec<(String, TypeRef)>, String> {
        let mut analyzer = Analyzer::new(TypeSystem::new(catalog), path.clone());
        let cols = analyzer
            .select(query, None, true)
            .map_err(|e| format!("[{}] {}", e.code, e.message))?;
        if !analyzer.params.is_empty() {
            return Err("une vue ne peut pas contenir de paramètre".into());
        }
        let ts = TypeSystem::new(catalog);
        Ok(cols
            .into_iter()
            .map(|c| {
                let r = type_ref(&ts, &c.ty, c.modifiers);
                (c.name, r)
            })
            .collect())
    }
}
