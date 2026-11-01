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

/// Quantificateur d'une comparaison à un ensemble.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quantifier {
    /// `ANY` (synonyme : `SOME`).
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
    /// `||` — concaténation de chaînes.
    ///
    /// Variante à part et non `Custom("||")` : c'est l'opérateur non standard le
    /// plus fréquent de PostgreSQL, et son type de résultat est connu (texte),
    /// ce dont l'inférence a besoin.
    Concat,
    /// Tout opérateur non standard ou d'extension (PostGIS, JSONB, défini par
    /// l'utilisateur). La chaîne est le symbole : `<->`, `&&`, `@>`, `#>>`.
    ///
    /// C'est le point d'extensibilité de cet enum : un opérateur inconnu se
    /// range ici plutôt que de faire échouer la conversion.
    Custom(String),
    /// `IS DISTINCT FROM` : différence où NULL est une valeur comparable.
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

    // ── Ajouts : constructions PostgreSQL courantes ────────────────────────────
    /// `EXISTS (subquery)` / `NOT EXISTS (…)` — résultat booléen.
    Exists {
        subquery: Box<SelectStmt>,
        negated: bool,
    },
    /// `INTERVAL '1 day'`. Le littéral est conservé tel quel : sa décomposition
    /// en champs n'intéresse pas encore l'analyse, son type suffit.
    Interval(String),
    /// `timestamp AT TIME ZONE zone`.
    AtTimeZone {
        timestamp: Box<Expr>,
        time_zone: Box<Expr>,
    },
    /// `expr COLLATE "C"` — même type que `expr`.
    Collate {
        expr: Box<Expr>,
        collation: String,
    },
    /// `ROW(a, b)` ou `(a, b)` — constructeur de ligne.
    Tuple(Vec<Expr>),
    /// Accès composé : `arr[1]`, `(rec).champ`, `payload['clé']`.
    ///
    /// `path` conserve les accesseurs sous forme textuelle, dans l'ordre. Le
    /// type de l'élément n'est pas résolu ici.
    FieldAccess {
        root: Box<Expr>,
        path: Vec<String>,
    },
    /// `expr op ANY (tableau | sous-requête)` / `ALL (…)` : comparaison de
    /// `left` à chaque élément de `right`. Le résultat est booléen ; `right`
    /// est un tableau (ou une sous-requête), pas une valeur du type de `left`.
    Quantified {
        left: Box<Expr>,
        op: BinaryOp,
        quantifier: Quantifier,
        right: Box<Expr>,
    },
    /// Paramètre lié : `$1`, `:nom`.
    ///
    /// Son type dépend du contexte d'appel, donc il n'est pas décidé ici — mais
    /// le nœud existe, là où il faisait auparavant échouer tout le statement.
    Parameter(String),

    /// Expression que cet AST ne modélise pas.
    ///
    /// Remplace un ancien `Literal(Null)` attrape-tout : affirmer `NULL` était
    /// un mensonge sur le contenu de la requête — un `WHERE EXISTS (…)` devenait
    /// `WHERE NULL`, ce qui veut dire « aucune ligne ». Ne rien affirmer permet
    /// aux consommateurs de passer leur tour sans conclure de travers, et au
    /// checker de signaler qu'il n'a pas tout compris.
    ///
    /// La chaîne décrit la construction, pour le diagnostic.
    Unsupported(String),
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
