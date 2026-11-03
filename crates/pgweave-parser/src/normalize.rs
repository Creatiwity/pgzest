//! AST normalizer: converts a syntactic `SelectStmt` into a `NormalizedSelect`
//! whose field ordering reflects the logical evaluation order of a SQL query:
//!
//! ```text
//! WITH (CTEs)
//!   → FROM
//!   → JOINs
//!   → WHERE
//!   → GROUP BY
//!   → HAVING
//!   → WINDOW
//!   → SELECT columns   (projection)
//!   → DISTINCT
//!   → ORDER BY         (may reference SELECT aliases, resolved here)
//!   → LIMIT / OFFSET
//! ```
//!
//! No deep expression transformation is performed at this stage. The normalizer
//! only re-orders the semantic representation and builds a `resolved_aliases` map
//! so that subsequent passes (type-checker, codegen) can resolve ORDER BY aliases
//! without re-scanning the SELECT list.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use pgweave_ast::expr::{Expr, Ident};
use pgweave_ast::stmt::{
    Cte, Join, NamedWindow, OrderByExpr, SelectItem, SelectStmt, SetOperation, TableRef,
};

/// A `SelectStmt` re-expressed in logical evaluation order.
///
/// Field order in this struct matches the pipeline:
///   with → from → joins → where_clause → group_by → having
///   → columns → distinct → order_by → limit → offset
///
/// The additional `resolved_aliases` field maps every alias name defined in the
/// SELECT list to the expression it names. This lets downstream passes resolve
/// ORDER BY references such as `ORDER BY total` where `total` was defined as
/// `SUM(amount) AS total` in the SELECT clause.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedSelect {
    // ── Phase 1: source ──────────────────────────────────────────────────────
    /// Common Table Expressions (WITH clause), evaluated first.
    pub with: Vec<Cte>,

    /// `WITH RECURSIVE`: CTEs may reference themselves.
    pub with_recursive: bool,

    /// Primary table references in the FROM clause.
    pub from: Vec<TableRef>,

    /// JOIN clauses, applied in order after FROM.
    pub joins: Vec<Join>,

    // ── Phase 2: filtering ───────────────────────────────────────────────────
    /// Row-level filter evaluated before grouping (WHERE).
    pub where_clause: Option<Expr>,

    // ── Phase 3: grouping ────────────────────────────────────────────────────
    /// GROUP BY expressions.
    pub group_by: Vec<Expr>,

    /// Group-level filter evaluated after grouping (HAVING).
    pub having: Option<Expr>,

    /// Named windows (`WINDOW`), evaluated after grouping.
    pub windows: Vec<NamedWindow>,

    // ── Phase 4: projection ──────────────────────────────────────────────────
    /// SELECT column list (projection), evaluated after grouping.
    pub columns: Vec<SelectItem>,

    /// Whether DISTINCT de-duplication is applied after projection.
    pub distinct: bool,

    /// `DISTINCT ON (…)`: one row per value of these expressions.
    pub distinct_on: Vec<Expr>,

    // ── Phase 5: ordering & pagination ───────────────────────────────────────
    /// ORDER BY expressions. Aliases defined in `columns` are resolved in
    /// `resolved_aliases`; the type-checker should consult that map first.
    pub order_by: Vec<OrderByExpr>,

    /// LIMIT clause.
    pub limit: Option<Expr>,

    /// OFFSET clause.
    pub offset: Option<Expr>,

    // ── Phase 6: set operations ──────────────────────────────────────────────
    /// UNION / INTERSECT / EXCEPT, applied after this SELECT; ORDER BY, LIMIT
    /// and OFFSET then apply to their result.
    pub set_ops: Vec<SetOperation>,

    /// `VALUES`: rows of values, in place of the projection.
    pub values: Vec<Vec<Expr>>,

    // ── Derived metadata ─────────────────────────────────────────────────────
    /// Alias → Expr map built from the SELECT list.
    ///
    /// For every `SelectItem::Expr { expr, alias: Some(name) }` entry in
    /// `columns`, this map contains `name.value → expr.clone()`.
    ///
    /// Downstream passes (type-checker, ORDER BY resolver) must look up alias
    /// names here before treating an identifier as a column reference.
    pub resolved_aliases: HashMap<String, Expr>,
}

/// Normalize a syntactic `SelectStmt` into a `NormalizedSelect`.
///
/// The transformation is **non-destructive**: every piece of information from
/// the original statement is preserved. The only addition is the
/// `resolved_aliases` map derived from the SELECT list.
pub fn normalize_select(stmt: SelectStmt) -> NormalizedSelect {
    // Build the alias → expr map from the SELECT column list.
    let resolved_aliases = build_alias_map(&stmt.columns);

    NormalizedSelect {
        // Phase 1 – source
        with: stmt.with,
        with_recursive: stmt.with_recursive,
        from: stmt.from,
        joins: stmt.joins,

        // Phase 2 – filtering
        where_clause: stmt.where_clause,

        // Phase 3 – grouping
        group_by: stmt.group_by,
        having: stmt.having,
        windows: stmt.windows,

        // Phase 4 – projection
        columns: stmt.columns,
        distinct: stmt.distinct,
        distinct_on: stmt.distinct_on,

        // Phase 5 – ordering & pagination
        order_by: stmt.order_by,
        limit: stmt.limit,
        offset: stmt.offset,

        set_ops: stmt.set_ops,
        values: stmt.values,

        // Derived
        resolved_aliases,
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Build the alias map from a SELECT column list.
///
/// Only `SelectItem::Expr` variants with an explicit alias contribute an entry.
/// `Wildcard` and `QualifiedWildcard` items are silently skipped.
fn build_alias_map(columns: &[SelectItem]) -> HashMap<String, Expr> {
    let mut map = HashMap::new();
    for item in columns {
        if let SelectItem::Expr {
            expr,
            alias: Some(Ident { value, .. }),
        } = item
        {
            map.insert(value.clone(), expr.clone());
        }
    }
    map
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use pgweave_ast::expr::{BinaryOp, ColumnRef, ExprKind, Literal};
    use pgweave_ast::stmt::{JoinConstraint, JoinKind, OrderByExpr, TableSource};

    fn col(name: &str) -> Expr {
        Expr::bare(ExprKind::Column(ColumnRef {
            schema: None,
            table: None,
            column: Ident::new(name),
        }))
    }

    fn alias(name: &str) -> Ident {
        Ident::new(name)
    }

    fn simple_stmt() -> SelectStmt {
        SelectStmt {
            id: pgweave_ast::NodeId::DUMMY,
            span: pgweave_ast::Span::DUMMY,
            with: vec![],
            with_recursive: false,
            distinct: false,
            distinct_on: vec![],
            columns: vec![
                SelectItem::Expr {
                    expr: col("id"),
                    alias: None,
                },
                SelectItem::Expr {
                    expr: Expr::bare(ExprKind::BinaryOp {
                        left: Box::new(col("price")),
                        op: BinaryOp::Multiply,
                        right: Box::new(col("qty")),
                    }),
                    alias: Some(alias("total")),
                },
            ],
            from: vec![TableRef {
                source: TableSource::Table(pgweave_ast::expr::ObjectName::unquoted(&["orders"])),
                alias: None,
                column_aliases: vec![],
                column_types: vec![],
                span: Default::default(),
            }],
            joins: vec![],
            where_clause: Some(Expr::bare(ExprKind::BinaryOp {
                left: Box::new(col("status")),
                op: BinaryOp::Eq,
                right: Box::new(Expr::bare(ExprKind::Literal(Literal::String(
                    "open".into(),
                )))),
            })),
            group_by: vec![],
            having: None,
            windows: vec![],
            order_by: vec![OrderByExpr {
                expr: col("total"),
                asc: Some(false),
                nulls_first: None,
            }],
            limit: Some(Expr::bare(ExprKind::Literal(Literal::Number("10".into())))),
            offset: None,
            set_ops: vec![],
            values: vec![],
        }
    }

    #[test]
    fn normalized_preserves_all_fields() {
        let stmt = simple_stmt();
        let norm = normalize_select(stmt.clone());

        assert_eq!(norm.from.len(), 1);
        assert_eq!(norm.columns.len(), 2);
        assert_eq!(norm.order_by.len(), 1);
        assert!(norm.where_clause.is_some());
        assert!(norm.limit.is_some());
        assert!(norm.offset.is_none());
        assert!(!norm.distinct);
    }

    #[test]
    fn alias_map_contains_aliased_columns() {
        let stmt = simple_stmt();
        let norm = normalize_select(stmt);

        assert!(
            norm.resolved_aliases.contains_key("total"),
            "expected 'total' in resolved_aliases"
        );
        assert!(
            !norm.resolved_aliases.contains_key("id"),
            "unaliased column should not appear in resolved_aliases"
        );
    }

    #[test]
    fn alias_map_is_empty_for_wildcard_select() {
        let stmt = SelectStmt {
            id: pgweave_ast::NodeId::DUMMY,
            span: pgweave_ast::Span::DUMMY,
            with: vec![],
            with_recursive: false,
            distinct: false,
            distinct_on: vec![],
            columns: vec![SelectItem::Wildcard],
            from: vec![],
            joins: vec![],
            where_clause: None,
            group_by: vec![],
            having: None,
            windows: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
            set_ops: vec![],
            values: vec![],
        };
        let norm = normalize_select(stmt);
        assert!(norm.resolved_aliases.is_empty());
    }

    #[test]
    fn join_preserved() {
        use pgweave_ast::expr::ObjectName;
        let mut stmt = simple_stmt();
        stmt.joins.push(Join {
            kind: JoinKind::Inner,
            table: TableRef {
                source: TableSource::Table(ObjectName::unquoted(&["products"])),
                alias: None,
                column_aliases: vec![],
                column_types: vec![],
                span: Default::default(),
            },
            constraint: JoinConstraint::Using(vec![Ident::new("product_id")]),
        });
        let norm = normalize_select(stmt);
        assert_eq!(norm.joins.len(), 1);
    }
}
