//! Catalog objects (PLAN.md, D2 and D11).
//!
//! The identity of every object includes its schema: [`QName`]. Names are
//! kept with their exact case (principle 8) — case is fixed at parse time,
//! the catalog no longer touches it. Expressions (defaults, `CHECK`,
//! generated columns) are kept as emitted SQL: the catalog describes
//! objects, it does not analyze their contents.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Qualified name: `(schema, name)`. Serialized in its SQL form
/// (`pg_catalog.int4`, `public."Menu"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QName {
    pub schema: String,
    pub name: String,
}

impl QName {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
        }
    }

    pub fn pg_catalog(name: impl Into<String>) -> Self {
        Self::new("pg_catalog", name)
    }
}

/// SQL rendering: each part quoted if PostgreSQL would not read it back
/// identically.
impl fmt::Display for QName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}",
            quote_ident(&self.schema),
            quote_ident(&self.name)
        )
    }
}

/// Textual form of a catalog name: quoted if its shape requires it.
/// (For SQL to execute, see [`quote_sql_ident`], which also quotes
/// keywords.)
pub fn quote_ident(s: &str) -> String {
    if pgzest_ast::emit::has_plain_shape(s) {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// Identifier for executable SQL: quoted if its shape requires it or if it
/// is a keyword (`default`, `user`), like PostgreSQL's `quote_ident`.
pub fn quote_sql_ident(s: &str) -> String {
    if pgzest_ast::emit::is_plain_identifier(s) {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Reference to a type, as an object uses it. Serialized in its SQL form:
/// `pg_catalog.varchar(20)`, `pg_catalog.int4[]`, and `?written` for an
/// unresolved name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TypeRef {
    /// Resolved type, with its modifiers as written (`varchar(20)`:
    /// `["20"]`).
    Named {
        name: QName,
        modifiers: Vec<String>,
    },
    Array(Box<TypeRef>),
    /// Written name that resolution did not find. Never guessed: it is kept
    /// as written, and reported.
    Unresolved(String),
    /// Type no analysis has established yet (view column before semantic
    /// analysis): unknown, not assumed.
    Unknown,
}

impl TypeRef {
    pub fn named(name: QName) -> Self {
        TypeRef::Named {
            name,
            modifiers: Vec::new(),
        }
    }
}

impl fmt::Display for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeRef::Named { name, modifiers } if modifiers.is_empty() => write!(f, "{name}"),
            TypeRef::Named { name, modifiers } => write!(f, "{name}({})", modifiers.join(",")),
            TypeRef::Array(inner) => write!(f, "{inner}[]"),
            TypeRef::Unresolved(s) => write!(f, "?{s}"),
            TypeRef::Unknown => write!(f, "?"),
        }
    }
}

// ── Textual forms ─────────────────────────────────────────────────────────────

/// Reads an identifier (quoted or not) at the start of `s`; returns the rest.
fn read_ident(s: &str) -> Option<(String, &str)> {
    if let Some(rest) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                if chars.peek().map(|(_, c)| *c) == Some('"') {
                    chars.next();
                    out.push('"');
                } else {
                    return Some((out, &rest[i + 1..]));
                }
            } else {
                out.push(c);
            }
        }
        None
    } else {
        let end = s.find(['.', '(', '[']).unwrap_or(s.len());
        (end > 0).then(|| (s[..end].to_string(), &s[end..]))
    }
}

/// `schema.name` at the start of `s`.
fn read_qname(s: &str) -> Option<(QName, &str)> {
    let (schema, rest) = read_ident(s)?;
    let rest = rest.strip_prefix('.')?;
    let (name, rest) = read_ident(rest)?;
    Some((QName::new(schema, name), rest))
}

impl std::str::FromStr for QName {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match read_qname(s) {
            Some((q, "")) => Ok(q),
            _ => Err(format!("invalid qualified name: {s}")),
        }
    }
}

impl std::str::FromStr for TypeRef {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if s == "?" {
            return Ok(TypeRef::Unknown);
        }
        if let Some(written) = s.strip_prefix('?') {
            return Ok(TypeRef::Unresolved(written.to_string()));
        }
        let bad = || format!("invalid type reference: {s}");
        let (name, mut rest) = read_qname(s).ok_or_else(bad)?;
        let mut modifiers = Vec::new();
        if let Some(r) = rest.strip_prefix('(') {
            let close = r.find(')').ok_or_else(bad)?;
            modifiers = r[..close].split(',').map(|m| m.to_string()).collect();
            rest = &r[close + 1..];
        }
        let mut ty = TypeRef::Named { name, modifiers };
        while let Some(r) = rest.strip_prefix("[]") {
            ty = TypeRef::Array(Box::new(ty));
            rest = r;
        }
        if rest.is_empty() { Ok(ty) } else { Err(bad()) }
    }
}

macro_rules! serde_as_string {
    ($t:ty) => {
        impl Serialize for $t {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_string())
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

serde_as_string!(QName);
serde_as_string!(TypeRef);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textual_forms_round_trip() {
        for s in [
            "pg_catalog.int4",
            "public.\"Menu\"",
            "\"we.ird\".\"a\"\"b\"",
            "pg_catalog.varchar(20)",
            "public.geometry(Point,4326)",
            "pg_catalog.int4[][]",
            "\"@extschema@\".citext",
            "?my_type",
            "?",
        ] {
            let t: TypeRef = s.parse().unwrap();
            assert_eq!(t.to_string(), s);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeDef {
    pub name: QName,
    /// OID in the PostgreSQL the pack was generated from; absent for a type
    /// declared by the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<u32>,
    pub kind: TypeKind,
    /// `typcategory` (implicit conversion rules): `N` numeric, `S` string,
    /// `D` date/time, `U` user…
    pub category: char,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preferred: bool,
    /// Associated array type (`pg_catalog._int4` for `int4`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub array: Option<QName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeKind {
    Base,
    Pseudo,
    Enum {
        labels: Vec<String>,
    },
    Domain {
        base: TypeRef,
        not_null: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        checks: Vec<CheckConstraint>,
    },
    Composite {
        attributes: Vec<Attribute>,
    },
    Array {
        element: QName,
    },
    Range {
        subtype: QName,
    },
    Multirange {
        range: QName,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribute {
    pub name: String,
    pub ty: TypeRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckConstraint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub expr: String,
}

// ── Functions, operators, casts ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutineKind {
    Function,
    Aggregate,
    Window,
    Procedure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArgMode {
    In,
    Out,
    InOut,
    Variadic,
    /// Column of a `RETURNS TABLE`.
    Table,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Volatility {
    Immutable,
    Stable,
    Volatile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub ty: TypeRef,
    pub mode: ArgMode,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_default: bool,
}

/// One overload of a function, aggregate or procedure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    pub name: QName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<u32>,
    pub kind: RoutineKind,
    pub args: Vec<Arg>,
    /// Absent for a procedure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<TypeRef>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub returns_set: bool,
    pub volatility: Volatility,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub security_definer: bool,
    pub language: String,
    /// Emitted `CREATE …`, for a project routine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// Creation rank in the replay (dependencies on export).
    #[serde(default, skip_serializing)]
    pub order: u64,
    /// The body may modify the catalog (DDL, dynamic SQL): a call has effects
    /// the replay does not reproduce.
    #[serde(default, skip_serializing)]
    pub catalog_effects: bool,
    /// Text of the `CREATE …` as written, when kept
    /// ([`crate::project::ProjectBuilder::keep_sources`]): PostgreSQL keeps a
    /// routine body verbatim, a migration must reuse it.
    #[serde(default, skip_serializing)]
    pub source: Option<String>,
}

impl Routine {
    /// Input argument types: those that form the signature.
    pub fn input_types(&self) -> impl Iterator<Item = &TypeRef> {
        self.args
            .iter()
            .filter(|a| matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
            .map(|a| &a.ty)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operator {
    pub name: QName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<u32>,
    /// Absent for a prefix operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<TypeRef>,
    pub right: TypeRef,
    pub result: TypeRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CastContext {
    Implicit,
    Assignment,
    Explicit,
}

/// Implementation of a cast (`castmethod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastMethod {
    /// Through a function.
    Function,
    /// Same binary representation: simple relabeling.
    Binary,
    /// Through the input/output functions (text).
    InOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cast {
    pub source: TypeRef,
    pub target: TypeRef,
    pub context: CastContext,
    pub method: CastMethod,
}

/// Operator class of an index method (`gist`, `btree`…): what makes a type
/// indexable by that method (`btree_gist` adds some).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpClass {
    pub name: QName,
    pub access_method: String,
    pub input: TypeRef,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
}

// ── Relations ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Persistence {
    Permanent,
    Unlogged,
    Temporary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub name: QName,
    pub kind: RelationKind,
    pub persistence: Persistence,
    pub columns: Vec<Column>,
    pub constraints: Vec<Constraint>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub row_level_security: bool,
    /// Columns not established (view whose query could not be expanded):
    /// their absence proves nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opaque: bool,
    /// Emitted `CREATE VIEW …`, for a project view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    #[serde(default, skip_serializing)]
    pub order: u64,
}

impl Relation {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentityKind {
    Always,
    ByDefault,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generated {
    pub expr: String,
    /// `STORED`; otherwise `VIRTUAL` (PostgreSQL 18 default).
    pub stored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: TypeRef,
    pub not_null: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentityKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<Generated>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub name: String,
    pub kind: ConstraintKind,
    /// Full emitted text (FK actions included), for export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// `NOT VALID`: existing rows are not guaranteed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_valid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConstraintKind {
    PrimaryKey {
        columns: Vec<String>,
    },
    Unique {
        columns: Vec<String>,
    },
    ForeignKey {
        columns: Vec<String>,
        references: QName,
        ref_columns: Vec<String>,
    },
    Check {
        expr: String,
        /// Referenced columns: the constraint goes away with any of them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        columns: Vec<String>,
    },
    Exclude {
        definition: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        columns: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub name: QName,
    pub table: QName,
    pub unique: bool,
    /// Emitted definition (`CREATE INDEX …`), for the migration diff.
    pub definition: String,
    /// Columns indexed or referenced by an expression or the predicate: the
    /// index goes away with any of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sequence {
    pub name: QName,
    /// Owning column (`OWNED BY`, `serial`, identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<(QName, String)>,
    /// Emitted `CREATE SEQUENCE …` (parameters included), for an explicitly
    /// created sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    pub name: String,
    pub table: QName,
    pub function: QName,
    /// `BEFORE`, `AFTER`, `INSTEAD OF`.
    pub timing: String,
    /// `INSERT`, `UPDATE [OF …]`, `DELETE`, `TRUNCATE`.
    pub events: Vec<String>,
    pub for_each_row: bool,
    /// Transition tables `REFERENCING OLD TABLE AS … NEW TABLE AS …`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub name: String,
    pub table: QName,
    pub command: String,
    pub permissive: bool,
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub using: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with_check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extension {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub schema: String,
}
