use pgweave_ast::types::DataType;
use pgweave_ast::{DdlStatement, NodeId, SelectItem, Span, Statement};
use pgweave_schema::provider::{SchemaProvider, object_name_to_string};
use std::collections::HashMap;

use crate::infer::{OutputColumn, infer_expr_type};
use crate::scope::{BoundRelation, ColumnBinding, ScopeId, ScopeTree};
use crate::symbol::{Symbol, SymbolKind, SymbolTable};
use pgweave_ast::expr::ExprKind;

/// Full result of semantic analysis on a set of SQL statements.
#[derive(Debug)]
pub struct SemanticResult {
    pub symbols: SymbolTable,
    pub scopes: ScopeTree,
    /// Inferred type of each Expr node (NodeId → DataType).
    pub expr_types: HashMap<NodeId, DataType>,
    /// Output columns of each SELECT statement (NodeId → columns).
    pub select_columns: HashMap<NodeId, Vec<OutputColumn>>,
}

/// Analyze a list of statements against a schema provider.
/// Returns a SemanticResult usable by checker, linter, LSP, and codegen.
pub fn analyze(stmts: &[Statement], schema: &dyn SchemaProvider) -> SemanticResult {
    let mut analyzer = Analyzer::new(schema);
    analyzer.analyze_statements(stmts);
    SemanticResult {
        symbols: analyzer.symbols,
        scopes: analyzer.scopes,
        expr_types: analyzer.expr_types,
        select_columns: analyzer.select_columns,
    }
}

// ── Internal analyzer ─────────────────────────────────────────────────────────

struct Analyzer<'a> {
    schema: &'a dyn SchemaProvider,
    symbols: SymbolTable,
    scopes: ScopeTree,
    expr_types: HashMap<NodeId, DataType>,
    select_columns: HashMap<NodeId, Vec<OutputColumn>>,
    /// Root scope (global)
    root_scope: ScopeId,
}

impl<'a> Analyzer<'a> {
    fn new(schema: &'a dyn SchemaProvider) -> Self {
        let mut scopes = ScopeTree::new();
        let root = scopes.new_scope(None, Span::DUMMY);

        let mut symbols = SymbolTable::new();

        // Populate symbol table from schema provider (tables + functions)
        for table_name in schema.all_table_names() {
            symbols.add_table(
                table_name,
                Symbol {
                    name: table_name.to_string(),
                    kind: SymbolKind::Table,
                    data_type: DataType::Custom(table_name.to_string()),
                    node_id: NodeId::DUMMY,
                    span: Span::DUMMY,
                },
            );
        }

        Self {
            schema,
            symbols,
            scopes,
            expr_types: HashMap::new(),
            select_columns: HashMap::new(),
            root_scope: root,
        }
    }

    fn analyze_statements(&mut self, stmts: &[Statement]) {
        for stmt in stmts {
            self.analyze_statement(stmt);
        }
    }

    fn analyze_statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::Select(sel) => {
                let _ = self.analyze_select(sel, self.root_scope);
            }
            Statement::Ddl(ddl) => {
                match ddl {
                    DdlStatement::CreateFunction(cf) => {
                        // Register function in symbol table
                        let name = object_name_to_string(&cf.name);
                        let ret_type = return_type_to_data_type(&cf.return_type);
                        self.symbols.add_function(
                            name.clone(),
                            Symbol {
                                name,
                                kind: SymbolKind::Function,
                                data_type: ret_type,
                                node_id: cf.id,
                                span: cf.span,
                            },
                        );
                    }
                    DdlStatement::CreateTable(ct) => {
                        // Register table in symbol table (from DDL, not just schema)
                        let name = object_name_to_string(&ct.name);
                        self.symbols.add_table(
                            name.clone(),
                            Symbol {
                                name,
                                kind: SymbolKind::Table,
                                data_type: DataType::Custom("table".into()),
                                node_id: ct.id,
                                span: ct.span,
                            },
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Analyze a SELECT and return its output columns.
    fn analyze_select(
        &mut self,
        sel: &pgweave_ast::stmt::SelectStmt,
        parent_scope: ScopeId,
    ) -> Vec<OutputColumn> {
        // Create a new scope for this SELECT
        let scope_id = self.scopes.new_scope(Some(parent_scope), sel.span);

        // Bind FROM + JOIN tables into the scope
        for table_ref in &sel.from {
            self.bind_table_ref(table_ref, scope_id);
        }
        for join in &sel.joins {
            self.bind_table_ref(&join.table, scope_id);
        }

        // Analyze WHERE / GROUP BY / HAVING / ORDER BY expressions
        if let Some(w) = &sel.where_clause {
            self.infer_and_store(w, scope_id);
        }
        for e in &sel.group_by {
            self.infer_and_store(e, scope_id);
        }
        if let Some(h) = &sel.having {
            self.infer_and_store(h, scope_id);
        }
        for o in &sel.order_by {
            self.infer_and_store(&o.expr, scope_id);
        }

        // Build output columns from SELECT list
        let mut output = Vec::new();
        for item in sel.columns.iter() {
            match item {
                // Expansion de `*` et `t.*` : relations dans l'ordre du FROM,
                // colonnes dans l'ordre de déclaration — l'ordre de PostgreSQL.
                SelectItem::Wildcard => output.extend(self.expand_star(scope_id, None)),
                SelectItem::QualifiedWildcard(alias) => {
                    output.extend(self.expand_star(scope_id, Some(&alias.value)))
                }
                SelectItem::Expr { expr, alias } => {
                    let ty = self
                        .infer_and_store(expr, scope_id)
                        .unwrap_or(DataType::Text);
                    let name = alias
                        .as_ref()
                        .map(|a| a.value.clone())
                        .or_else(|| expr_to_column_name(expr));
                    output.push(OutputColumn {
                        name,
                        data_type: ty,
                        node_id: NodeId::DUMMY,
                    });
                }
            }
        }

        // Opérandes d'UNION/INTERSECT/EXCEPT : analysés au même niveau. Les
        // colonnes du résultat sont nommées par le premier SELECT ; l'unification
        // de leurs types relève de l'analyse sémantique complète (P3).
        for op in &sel.set_ops {
            self.analyze_select(&op.select, parent_scope);
        }

        // Store output columns indexed by this SELECT's NodeId
        self.select_columns.insert(sel.id, output.clone());
        output
    }

    /// Colonnes produites par `*` (`qualifier` absent) ou `qualifier.*`.
    ///
    /// Une relation inconnue ne produit aucune colonne : l'erreur est
    /// signalée par le checker, pas devinée ici.
    fn expand_star(&self, scope_id: ScopeId, qualifier: Option<&str>) -> Vec<OutputColumn> {
        let Some(scope) = self.scopes.get(scope_id) else {
            return Vec::new();
        };
        scope
            .relations
            .iter()
            .filter(|r| qualifier.is_none_or(|q| r.alias.eq_ignore_ascii_case(q)))
            .flat_map(|r| &r.columns)
            .map(|c| OutputColumn {
                name: Some(c.name.clone()),
                data_type: c.data_type.clone(),
                node_id: NodeId::DUMMY,
            })
            .collect()
    }

    /// Bind a table reference into the current scope (adds its columns).
    fn bind_table_ref(&mut self, table_ref: &pgweave_ast::stmt::TableRef, scope_id: ScopeId) {
        use pgweave_ast::stmt::TableSource;
        let Some(visible) = table_ref.visible_name().map(str::to_string) else {
            // Sous-requête sans alias : PostgreSQL l'interdit, rien à lier.
            return;
        };
        let columns: Option<Vec<(String, DataType)>> = match &table_ref.source {
            TableSource::Table(name) => {
                self.schema
                    .get_table(&object_name_to_string(name))
                    .map(|ts| {
                        ts.columns
                            .iter()
                            .map(|col| (col.name.clone(), col.data_type.clone()))
                            .collect()
                    })
            }
            // Colonnes de sortie de la sous-requête, analysée comme telle.
            TableSource::Subquery { query, .. } => {
                let out = self.analyze_select(query, scope_id);
                Some(
                    out.into_iter()
                        .map(|c| (c.name.unwrap_or_else(|| "?column?".into()), c.data_type))
                        .collect(),
                )
            }
            // Le type des colonnes d'une fonction n'est pas encore connu du
            // catalogue (P2) : la source reste sans colonnes plutôt que d'en
            // inventer.
            TableSource::Function { .. } => None,
        };
        let alias = visible;

        if let Some(mut col_data) = columns {
            for (col, renamed) in col_data.iter_mut().zip(&table_ref.column_aliases) {
                col.0 = renamed.value.clone();
            }

            let scope = self.scopes.get_mut(scope_id).unwrap();
            scope.relations.push(BoundRelation {
                alias: alias.clone(),
                columns: col_data
                    .iter()
                    .map(|(name, data_type)| ColumnBinding {
                        name: name.clone(),
                        table_alias: Some(alias.clone()),
                        data_type: data_type.clone(),
                        source_node: NodeId::DUMMY,
                    })
                    .collect(),
            });
            for (col_name, col_type) in col_data {
                let key = format!("{}.{}", alias.to_lowercase(), col_name.to_lowercase());
                scope.columns.insert(
                    key,
                    ColumnBinding {
                        name: col_name.clone(),
                        table_alias: Some(alias.clone()),
                        data_type: col_type.clone(),
                        source_node: NodeId::DUMMY,
                    },
                );
                // Also register unqualified (first one wins)
                scope
                    .columns
                    .entry(col_name.to_lowercase())
                    .or_insert(ColumnBinding {
                        name: col_name,
                        table_alias: Some(alias.clone()),
                        data_type: col_type,
                        source_node: NodeId::DUMMY,
                    });
            }
        }
    }

    /// Infer the type of an expression and store it in expr_types.
    fn infer_and_store(
        &mut self,
        expr: &pgweave_ast::expr::Expr,
        scope_id: ScopeId,
    ) -> Option<DataType> {
        let ty = infer_expr_type(expr, scope_id, &self.scopes, &self.symbols, self.schema);
        // For subqueries, recurse
        if let ExprKind::Subquery(sub) = &expr.kind {
            self.analyze_select(sub, scope_id);
        }
        ty
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn return_type_to_data_type(rt: &pgweave_ast::ddl::ReturnType) -> DataType {
    use pgweave_ast::ddl::ReturnType;
    match rt {
        ReturnType::Scalar(dt) | ReturnType::SetOf(dt) => dt.clone(),
        ReturnType::Table(_) => DataType::Custom("table".into()),
        ReturnType::Void => DataType::Custom("void".into()),
        ReturnType::Trigger => DataType::Custom("trigger".into()),
    }
}

fn expr_to_column_name(expr: &pgweave_ast::expr::Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Column(col_ref) => Some(col_ref.column.value.clone()),
        ExprKind::FunctionCall { name, .. } => Some(name.0.last()?.value.clone()),
        _ => None,
    }
}
