use pgweave_ast::types::DataType;
use pgweave_ast::{NodeId, Span};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u32);

/// A single lexical scope (one SELECT, one CTE, one PL/pgSQL block).
#[derive(Debug)]
pub struct Scope {
    pub id: ScopeId,
    pub parent: Option<ScopeId>,
    /// Source range covered by this scope (for LSP offset lookup).
    pub span: Span,
    /// Columns available in this scope: alias/table → columns.
    /// Key is "alias.colname" or just "colname".
    pub columns: HashMap<String, ColumnBinding>,
    /// Local variables (CTEs, PL/pgSQL vars/params).
    pub locals: HashMap<String, LocalBinding>,
    /// Relations liées par FROM / JOIN, **dans l'ordre de la requête**, chacune
    /// avec ses colonnes dans l'ordre de déclaration. `columns` est une table
    /// de recherche sans ordre ; l'expansion de `*` doit, elle, suivre l'ordre
    /// de PostgreSQL.
    pub relations: Vec<BoundRelation>,
}

/// Une table liée dans un scope, sous son alias.
#[derive(Debug, Clone)]
pub struct BoundRelation {
    pub alias: String,
    pub columns: Vec<ColumnBinding>,
}

#[derive(Debug, Clone)]
pub struct ColumnBinding {
    pub name: String,
    pub table_alias: Option<String>,
    pub data_type: DataType,
    pub source_node: NodeId,
}

#[derive(Debug, Clone)]
pub struct LocalBinding {
    pub name: String,
    pub data_type: DataType,
    pub source_node: NodeId,
}

/// Tree of scopes built during semantic analysis.
#[derive(Debug, Default)]
pub struct ScopeTree {
    scopes: Vec<Scope>,
    next_id: u32,
}

impl ScopeTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_scope(&mut self, parent: Option<ScopeId>, span: Span) -> ScopeId {
        let id = ScopeId(self.next_id);
        self.next_id += 1;
        self.scopes.push(Scope {
            id,
            parent,
            span,
            columns: HashMap::new(),
            locals: HashMap::new(),
            relations: Vec::new(),
        });
        id
    }

    pub fn get(&self, id: ScopeId) -> Option<&Scope> {
        self.scopes.get(id.0 as usize)
    }

    pub fn get_mut(&mut self, id: ScopeId) -> Option<&mut Scope> {
        self.scopes.get_mut(id.0 as usize)
    }

    /// Find the innermost scope that contains the given byte offset.
    /// Returns the most specific (deepest) scope.
    pub fn scope_at_offset(&self, offset: u32) -> Option<ScopeId> {
        // Find the smallest (most specific) scope containing offset
        self.scopes
            .iter()
            .filter(|s| s.span.contains_offset(offset))
            .min_by_key(|s| s.span.end - s.span.start)
            .map(|s| s.id)
    }

    /// All columns available at a given scope (including parent scopes).
    pub fn available_columns(&self, scope_id: ScopeId) -> Vec<&ColumnBinding> {
        let mut result = Vec::new();
        let mut current = Some(scope_id);
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = current {
            if seen.contains(&id) {
                break;
            }
            seen.insert(id);
            if let Some(scope) = self.get(id) {
                result.extend(scope.columns.values());
                current = scope.parent;
            } else {
                break;
            }
        }
        result
    }
}
