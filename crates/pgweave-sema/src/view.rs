//! View typing for the catalog replay ([`QueryTyper`]).

use std::sync::Arc;

use pgweave_ast::stmt::SelectStmt;
use pgweave_catalog::{Catalog, QueryTyper, SearchPath, TypeRef};

use crate::expr::Analyzer;
use crate::types::{Ty, TypeSystem};

/// Typer of view queries through semantic analysis.
pub struct ViewTyper;

impl ViewTyper {
    /// To pass to the replay (`ProjectBuilder::with_typer`).
    pub fn shared() -> Arc<dyn QueryTyper> {
        Arc::new(ViewTyper)
    }
}

/// Catalog reference of a type: `element[]` for the designated array type
/// of an element, as in the packs.
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
            return Err("a view cannot contain a parameter".into());
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
