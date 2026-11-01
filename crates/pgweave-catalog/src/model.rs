//! Objets du catalogue (PLAN.md, D2 et D11).
//!
//! L'identité de tout objet comprend son schéma : [`QName`]. Les noms sont
//! conservés avec leur casse exacte (principe 8) — la casse est fixée au
//! parse, le catalogue ne la touche plus. Les expressions (défauts,
//! `CHECK`, colonnes générées) sont gardées sous forme de SQL émis : le
//! catalogue décrit des objets, il n'analyse pas leur contenu.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Nom qualifié : `(schéma, nom)`. Sérialisé sous sa forme SQL
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

/// Rendu SQL : chaque partie quotée si PostgreSQL ne la relirait pas à
/// l'identique.
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

/// Forme textuelle d'un nom du catalogue : quoté si sa forme l'exige.
/// (Pour du SQL à exécuter, voir [`quote_sql_ident`], qui quote aussi les
/// mots-clés.)
pub fn quote_ident(s: &str) -> String {
    if pgweave_ast::emit::has_plain_shape(s) {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// Identifiant pour du SQL exécutable : quoté si sa forme l'exige ou si
/// c'est un mot-clé (`default`, `user`), comme `quote_ident` de PostgreSQL.
pub fn quote_sql_ident(s: &str) -> String {
    if pgweave_ast::emit::is_plain_identifier(s) {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Référence à un type, telle qu'un objet l'utilise. Sérialisée sous sa
/// forme SQL : `pg_catalog.varchar(20)`, `pg_catalog.int4[]`, et `?écrit`
/// pour un nom non résolu.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TypeRef {
    /// Type résolu, avec ses modificateurs tels qu'écrits (`varchar(20)` :
    /// `["20"]`).
    Named {
        name: QName,
        modifiers: Vec<String>,
    },
    Array(Box<TypeRef>),
    /// Nom écrit que la résolution n'a pas trouvé. Jamais deviné : il est
    /// conservé tel qu'écrit, et signalé.
    Unresolved(String),
    /// Type qu'aucune analyse n'a encore établi (colonne de vue avant
    /// l'analyse sémantique) : inconnu, pas supposé.
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

// ── Formes textuelles ─────────────────────────────────────────────────────────

/// Lit un identifiant (quoté ou non) en tête de `s` ; rend le reste.
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

/// `schéma.nom` en tête de `s`.
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
            _ => Err(format!("nom qualifié invalide : {s}")),
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
        let bad = || format!("référence de type invalide : {s}");
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
    fn formes_textuelles_reversibles() {
        for s in [
            "pg_catalog.int4",
            "public.\"Menu\"",
            "\"we.ird\".\"a\"\"b\"",
            "pg_catalog.varchar(20)",
            "public.geometry(Point,4326)",
            "pg_catalog.int4[][]",
            "\"@extschema@\".citext",
            "?mon_type",
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
    /// OID dans le PostgreSQL d'où le pack a été généré ; absent pour un type
    /// déclaré par le projet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<u32>,
    pub kind: TypeKind,
    /// `typcategory` (règles de conversion implicite) : `N` numérique, `S`
    /// chaîne, `D` date/heure, `U` utilisateur…
    pub category: char,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preferred: bool,
    /// Type tableau associé (`pg_catalog._int4` pour `int4`).
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

// ── Fonctions, opérateurs, conversions ────────────────────────────────────────

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
    /// Colonne d'un `RETURNS TABLE`.
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

/// Une surcharge de fonction, d'agrégat ou de procédure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    pub name: QName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<u32>,
    pub kind: RoutineKind,
    pub args: Vec<Arg>,
    /// Absent pour une procédure.
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
    /// `CREATE …` émis, pour une routine du projet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// Rang de création dans le rejeu (dépendances à l'export).
    #[serde(default, skip_serializing)]
    pub order: u64,
    /// Le corps peut modifier le catalogue (DDL, SQL dynamique) : un appel
    /// a des effets que le rejeu ne reproduit pas.
    #[serde(default, skip_serializing)]
    pub catalog_effects: bool,
}

impl Routine {
    /// Types des arguments d'entrée : ceux qui forment la signature.
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
    /// Absent pour un opérateur préfixe.
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

/// Mise en œuvre d'une conversion (`castmethod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastMethod {
    /// Par une fonction.
    Function,
    /// Même représentation binaire : simple ré-étiquetage.
    Binary,
    /// Par les fonctions d'entrée-sortie (texte).
    InOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cast {
    pub source: TypeRef,
    pub target: TypeRef,
    pub context: CastContext,
    pub method: CastMethod,
}

/// Classe d'opérateurs d'une méthode d'index (`gist`, `btree`…) : ce qui
/// rend un type indexable par cette méthode (`btree_gist` en ajoute).
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
    /// Colonnes non établies (vue dont la requête n'a pas pu être
    /// développée) : leur absence ne prouve rien.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opaque: bool,
    /// `CREATE VIEW …` émis, pour une vue du projet.
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
    /// `STORED` ; sinon `VIRTUAL` (défaut de PostgreSQL 18).
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
    /// Texte complet émis (actions de FK comprises), pour l'export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// `NOT VALID` : les lignes existantes ne sont pas garanties.
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
        /// Colonnes citées : la contrainte disparaît avec l'une d'elles.
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
    /// Définition émise (`CREATE INDEX …`), pour le diff de migration.
    pub definition: String,
    /// Colonnes indexées ou citées par une expression ou le prédicat :
    /// l'index disparaît avec l'une d'elles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sequence {
    pub name: QName,
    /// Colonne qui la possède (`OWNED BY`, `serial`, identité).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<(QName, String)>,
    /// `CREATE SEQUENCE …` émis (paramètres compris), pour une séquence créée
    /// explicitement.
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
    /// Tables de transition `REFERENCING OLD TABLE AS … NEW TABLE AS …`.
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
