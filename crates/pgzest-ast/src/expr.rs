use crate::node::Span;
use crate::types::DataType;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ident {
    pub value: String,
    pub quoted: bool,
}

impl Ident {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            quoted: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectName(pub Vec<Ident>);

impl ObjectName {
    pub fn unquoted(parts: &[&str]) -> Self {
        Self(parts.iter().map(|s| Ident::new(*s)).collect())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnRef {
    /// First name of a three-part reference: `schema` in
    /// `schema.table.column`, or, in PL/pgSQL, the label in `block.variable.field`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Ident>,
    pub table: Option<Ident>,
    pub column: Ident,
}

impl ColumnRef {
    /// Column `column`, qualified or not by `table`.
    pub fn new(table: Option<Ident>, column: Ident) -> Self {
        ColumnRef {
            schema: None,
            table,
            column,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    Number(String),
    String(String),
    Boolean(bool),
    Null,
    /// Bit string `B'0101'` or `X'1F'`: prefix (`b` or `x`) followed by the
    /// digits, like `T_BitString` in PostgreSQL. Type `bit`.
    BitString(String),
}

/// Form of a multiple grouping in `GROUP BY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupingKind {
    Rollup,
    Cube,
    Sets,
}

/// Quantifier of a comparison against a set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quantifier {
    /// `ANY` (synonym: `SOME`).
    Any,
    All,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BinaryOp {
    Eq,
    NotEq,
    Lt,
    Lte,
    Gt,
    Gte,
    And,
    Or,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    In,
    NotIn,
    Like,
    NotLike,
    ILike,
    Is,
    IsNot,
    /// `||` — string concatenation.
    ///
    /// A separate variant rather than `Custom("||")`: it is PostgreSQL's most
    /// frequent non-standard operator, and its result type is known (text),
    /// which inference needs.
    Concat,
    /// Any non-standard or extension operator (PostGIS, JSONB, user-defined).
    /// The string is the symbol: `<->`, `&&`, `@>`, `#>>`.
    ///
    /// This is the extensibility point of this enum: an unknown operator
    /// lands here rather than making the conversion fail.
    Custom(String),
    /// `IS DISTINCT FROM`: inequality where NULL is a comparable value.
    IsDistinctFrom,
    /// `IS NOT DISTINCT FROM`.
    IsNotDistinctFrom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UnaryOp {
    Not,
    Neg,
    IsNull,
    IsNotNull,
    /// Other prefix operator, by its symbol: `+`, `@`, `~`, `|/`, `||/`.
    Prefix(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExprKind {
    Literal(Literal),
    Column(ColumnRef),
    BinaryOp {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    UnaryOp {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    FunctionCall {
        name: ObjectName,
        args: Vec<Expr>,
        /// `ORDER BY` inside an aggregate's parentheses:
        /// `string_agg(x, ',' ORDER BY y)`.
        order_by: Vec<crate::stmt::OrderByExpr>,
        filter: Option<Box<Expr>>,
        distinct: bool,
        /// `WITHIN GROUP (ORDER BY …)`: ordered-set aggregate
        /// (`percentile_cont(0.5) WITHIN GROUP (ORDER BY x)`).
        within_group: Vec<crate::stmt::OrderByExpr>,
        /// `OVER …`: window function call.
        over: Option<Box<Window>>,
    },
    Cast {
        expr: Box<Expr>,
        data_type: DataType,
    },
    Case {
        operand: Option<Box<Expr>>,
        when_clauses: Vec<(Expr, Expr)>,
        else_clause: Option<Box<Expr>>,
    },
    Subquery(Box<SelectStmt>),
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<SelectStmt>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    Wildcard,
    QualifiedWildcard(Ident),

    // ── Additions: common PostgreSQL constructs ───────────────────────────────
    /// `EXISTS (subquery)` / `NOT EXISTS (…)` — boolean result.
    Exists {
        subquery: Box<SelectStmt>,
        negated: bool,
    },
    /// `INTERVAL '1 day'`. The literal is kept as is: its breakdown into
    /// fields does not matter to the analysis yet, its type is enough.
    Interval(String),
    /// `timestamp AT TIME ZONE zone`.
    AtTimeZone {
        timestamp: Box<Expr>,
        time_zone: Box<Expr>,
    },
    /// `expr COLLATE "C"` — same type as `expr`.
    Collate {
        expr: Box<Expr>,
        collation: String,
    },
    /// `ROW(a, b)` or `(a, b)` — row constructor.
    Tuple(Vec<Expr>),
    /// Composite access: `arr[1]`, `(rec).field`, `payload['key']`.
    ///
    /// `path` keeps the accessors in textual form, in order. The element
    /// type is not resolved here.
    FieldAccess {
        root: Box<Expr>,
        path: Vec<String>,
    },
    /// `expr op ANY (array | subquery)` / `ALL (…)`: comparison of
    /// `left` with each element of `right`. The result is boolean; `right`
    /// is an array (or a subquery), not a value of `left`'s type.
    Quantified {
        left: Box<Expr>,
        op: BinaryOp,
        quantifier: Quantifier,
        right: Box<Expr>,
    },
    /// Named argument of a function call: `name => value`. Only appears
    /// in the arguments of a `FunctionCall` or of a function in `FROM`.
    NamedArgument {
        name: Ident,
        value: Box<Expr>,
    },
    /// Bound parameter: `$1`, `:name`.
    ///
    /// Its type depends on the call context, so it is not decided here — but
    /// the node exists, where it previously made the whole statement fail.
    Parameter(String),

    /// `ROLLUP (…)`, `CUBE (…)`, `GROUPING SETS (…)`: `GROUP BY` element
    /// only. Each set is a list of expressions.
    GroupingSets {
        kind: GroupingKind,
        sets: Vec<Vec<Expr>>,
    },

    /// Expression this AST does not model.
    ///
    /// Replaces a former catch-all `Literal(Null)`: asserting `NULL` was
    /// a lie about the query's content — a `WHERE EXISTS (…)` became
    /// `WHERE NULL`, which means "no rows". Asserting nothing lets
    /// consumers skip it without drawing wrong conclusions, and lets the
    /// checker report that it did not understand everything.
    ///
    /// The string describes the construct, for the diagnostic.
    Unsupported(String),
}

// Forward-declare SelectStmt so Expr can reference it
use crate::stmt::SelectStmt;

/// An expression, with its position in the source.
///
/// `Expr` wraps [`ExprKind`] rather than being the enum itself:
/// a field cannot be added to an enum, and this is the split chosen by
/// rustc and oxc for the same reason. The pattern reads `match &expr.kind`.
///
/// The span is in byte offsets in the document, like that of statements.
/// It is [`Span::DUMMY`] when the position is not reliably known:
/// synthesized expression (codegen, desugaring, tests), or statement whose
/// text was rewritten before parsing — better no span than a wrong span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    /// Expression with a known span.
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// Expression without a span — synthesized, or position unavailable.
    pub fn bare(kind: ExprKind) -> Self {
        Self {
            kind,
            span: Span::DUMMY,
        }
    }

    /// Direct sub-expressions, in writing order, subqueries
    /// excluded (see [`Self::subqueries`]). Single source of truth for
    /// traversals: a variant added here is seen by all.
    pub fn children(&self) -> Vec<&Expr> {
        let mut out: Vec<&Expr> = Vec::new();
        match &self.kind {
            ExprKind::BinaryOp { left, right, .. } | ExprKind::Quantified { left, right, .. } => {
                out.push(left);
                out.push(right);
            }
            ExprKind::UnaryOp { expr, .. }
            | ExprKind::Cast { expr, .. }
            | ExprKind::Collate { expr, .. } => out.push(expr),
            ExprKind::FunctionCall {
                args,
                order_by,
                filter,
                within_group,
                over,
                ..
            } => {
                out.extend(args);
                out.extend(order_by.iter().map(|o| &o.expr));
                out.extend(within_group.iter().map(|o| &o.expr));
                if let Some(f) = filter {
                    out.push(f);
                }
                if let Some(Window::Spec(spec)) = over.as_deref() {
                    out.extend(spec.exprs());
                }
            }
            ExprKind::Case {
                operand,
                when_clauses,
                else_clause,
            } => {
                if let Some(op) = operand {
                    out.push(op);
                }
                for (w, t) in when_clauses {
                    out.push(w);
                    out.push(t);
                }
                if let Some(el) = else_clause {
                    out.push(el);
                }
            }
            ExprKind::InSubquery { expr, .. } => out.push(expr),
            ExprKind::InList { expr, list, .. } => {
                out.push(expr);
                out.extend(list);
            }
            ExprKind::Between {
                expr, low, high, ..
            } => {
                out.push(expr);
                out.push(low);
                out.push(high);
            }
            ExprKind::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                out.push(timestamp);
                out.push(time_zone);
            }
            ExprKind::Tuple(items) => out.extend(items),
            ExprKind::GroupingSets { sets, .. } => out.extend(sets.iter().flatten()),
            ExprKind::FieldAccess { root, .. } => out.push(root),
            ExprKind::NamedArgument { value, .. } => out.push(value),
            ExprKind::Subquery(_)
            | ExprKind::Exists { .. }
            | ExprKind::Column(_)
            | ExprKind::Literal(_)
            | ExprKind::Wildcard
            | ExprKind::QualifiedWildcard(_)
            | ExprKind::Interval(_)
            | ExprKind::Parameter(_)
            | ExprKind::Unsupported(_) => {}
        }
        out
    }

    /// Direct subqueries (`(SELECT …)`, `IN (SELECT …)`, `EXISTS`).
    pub fn subqueries(&self) -> Vec<&SelectStmt> {
        match &self.kind {
            ExprKind::Subquery(q)
            | ExprKind::InSubquery { subquery: q, .. }
            | ExprKind::Exists { subquery: q, .. } => vec![q],
            _ => vec![],
        }
    }

    /// The span, or `None` if it remained [`Span::DUMMY`].
    ///
    /// Prefer this over reading `span` directly to emit a diagnostic:
    /// a null span would point at 1:1.
    pub fn real_span(&self) -> Option<Span> {
        if self.span == Span::DUMMY {
            None
        } else {
            Some(self.span)
        }
    }

    /// Replaces the span, keeping the `kind`.
    pub fn with_span(mut self, span: Span) -> Self {
        self.span = span;
        self
    }
}

/// Allows `ExprKind::Wildcard.into()` where the position does not matter.
impl From<ExprKind> for Expr {
    fn from(kind: ExprKind) -> Self {
        Self::bare(kind)
    }
}

// ── Windows ───────────────────────────────────────────────────────────────────

/// Window of an `OVER` call: named (`OVER w`) or described inline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Window {
    Named(Ident),
    Spec(WindowSpec),
}

/// `([base_window] [PARTITION BY …] [ORDER BY …] [frame])`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSpec {
    /// Named window this one inherits from.
    pub base: Option<Ident>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<crate::stmt::OrderByExpr>,
    pub frame: Option<WindowFrame>,
}

impl WindowSpec {
    /// Window expressions: partition, ordering, frame bounds.
    pub fn exprs(&self) -> Vec<&Expr> {
        let mut out: Vec<&Expr> = self.partition_by.iter().collect();
        out.extend(self.order_by.iter().map(|o| &o.expr));
        if let Some(f) = &self.frame {
            for b in std::iter::once(&f.start).chain(&f.end) {
                if let FrameBound::Preceding(e) | FrameBound::Following(e) = b {
                    out.push(e);
                }
            }
        }
        out
    }
}

/// `{ROWS | RANGE | GROUPS} start` or `… BETWEEN start AND end`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowFrame {
    pub units: FrameUnits,
    pub start: FrameBound,
    /// `None`: form without `BETWEEN` (the end is `CURRENT ROW`).
    pub end: Option<FrameBound>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameUnits {
    Rows,
    Range,
    Groups,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Box<Expr>),
    CurrentRow,
    Following(Box<Expr>),
    UnboundedFollowing,
}
