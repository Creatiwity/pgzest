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
    pub table: Option<Ident>,
    pub column: Ident,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    Number(String),
    String(String),
    Boolean(bool),
    Null,
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
    /// Any non-standard or extension operator (PostGIS, JSONB, custom, etc.).
    /// The string is the operator symbol, e.g. "<->", "&&", "@>", "#>>".
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UnaryOp {
    Not,
    Neg,
    IsNull,
    IsNotNull,
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
        filter: Option<Box<Expr>>,
        distinct: bool,
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
}

// Forward-declare SelectStmt so Expr can reference it
use crate::stmt::SelectStmt;

/// Une expression, avec sa position dans la source.
///
/// `Expr` enveloppe [`ExprKind`] plutôt que d'être l'énumération elle-même :
/// on ne peut pas ajouter un champ à un enum, et c'est le découpage retenu par
/// rustc et oxc pour la même raison. Le motif se lit `match &expr.kind`.
///
/// Le span est en offsets octets dans le document, comme celui des statements.
/// Il vaut [`Span::DUMMY`] quand la position n'est pas connue de façon fiable :
/// expression synthétisée (codegen, desugaring, tests), ou statement dont le
/// texte a été réécrit avant le parse — mieux vaut pas de span qu'un span faux.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    /// Expression avec un span connu.
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// Expression sans span — synthétisée, ou position indisponible.
    pub fn bare(kind: ExprKind) -> Self {
        Self {
            kind,
            span: Span::DUMMY,
        }
    }

    /// Le span, ou `None` s'il est resté à [`Span::DUMMY`].
    ///
    /// À préférer à la lecture directe de `span` pour émettre un diagnostic :
    /// un span nul pointerait en 1:1.
    pub fn real_span(&self) -> Option<Span> {
        if self.span == Span::DUMMY {
            None
        } else {
            Some(self.span)
        }
    }

    /// Remplace le span, en conservant le `kind`.
    pub fn with_span(mut self, span: Span) -> Self {
        self.span = span;
        self
    }
}

/// Permet `ExprKind::Wildcard.into()` là où la position n'importe pas.
impl From<ExprKind> for Expr {
    fn from(kind: ExprKind) -> Self {
        Self::bare(kind)
    }
}
