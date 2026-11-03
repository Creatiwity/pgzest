//! Packs: catalog fragments provided by a contributor of the global context
//! (PLAN.md, D9) — the PostgreSQL major version or an extension.
//!
//! A pack is **generated** from a real PostgreSQL (`cargo xtask
//! catalog-gen`), never written by hand. An extension's pack is the catalog
//! delta before and after `CREATE EXTENSION`.
//!
//! Format: JSON, one object per line in each list, sorted lists — a
//! PostgreSQL change shows up in the diff.

use serde::{Deserialize, Serialize};

use crate::model::{Cast, OpClass, Operator, Relation, Routine, Schema, TypeDef};

/// Pack format version.
pub const PACK_FORMAT: u32 = 1;

/// Contributor: `postgres@18`, `citext@1.6`…
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Contributor {
    pub name: String,
    pub version: String,
}

impl std::fmt::Display for Contributor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pack {
    pub format: u32,
    pub contributor: Contributor,
    /// For an extension: PostgreSQL version against which the delta was
    /// computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postgres: Option<String>,
    pub schemas: Vec<Schema>,
    pub types: Vec<TypeDef>,
    pub routines: Vec<Routine>,
    pub operators: Vec<Operator>,
    pub casts: Vec<Cast>,
    pub opclasses: Vec<OpClass>,
    /// Provided tables and views (system catalogs, `information_schema`).
    pub relations: Vec<Relation>,
}

/// Schema of an extension in a pack: replaced at load time by the schema
/// where the extension is created (`CREATE EXTENSION … SCHEMA s`), like the
/// `@extschema@` of PostgreSQL extension scripts.
pub const EXTSCHEMA: &str = "@extschema@";

#[derive(Debug)]
pub struct PackError(pub String);

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PackError {}

impl Pack {
    pub fn from_json(text: &str) -> Result<Pack, PackError> {
        let pack: Pack =
            serde_json::from_str(text).map_err(|e| PackError(format!("unreadable pack: {e}")))?;
        if pack.format != PACK_FORMAT {
            return Err(PackError(format!(
                "pack {} has format {}, expected format {PACK_FORMAT}",
                pack.contributor, pack.format
            )));
        }
        Ok(pack)
    }

    /// Sorts every list: same catalog, same text.
    pub fn normalize(&mut self) {
        self.schemas.sort_by(|a, b| a.name.cmp(&b.name));
        self.types.sort_by(|a, b| a.name.cmp(&b.name));
        self.routines
            .sort_by(|a, b| (&a.name, signature(a)).cmp(&(&b.name, signature(b))));
        self.operators
            .sort_by(|a, b| (&a.name, &a.left, &a.right).cmp(&(&b.name, &b.left, &b.right)));
        self.casts
            .sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));
        self.opclasses.sort_by(|a, b| {
            (&a.access_method, &a.name, &a.input).cmp(&(&b.access_method, &b.name, &b.input))
        });
        self.relations.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Stable JSON: one element per line.
    pub fn to_json(&self) -> String {
        fn list<T: Serialize>(out: &mut String, key: &str, items: &[T], last: bool) {
            out.push_str(&format!("  \"{key}\": ["));
            for (i, item) in items.iter().enumerate() {
                out.push_str(if i == 0 { "\n    " } else { ",\n    " });
                out.push_str(&serde_json::to_string(item).expect("pack serialization"));
            }
            out.push_str(if items.is_empty() { "]" } else { "\n  ]" });
            out.push_str(if last { "\n" } else { ",\n" });
        }
        let mut out = String::from("{\n");
        out.push_str(&format!("  \"format\": {},\n", self.format));
        out.push_str(&format!(
            "  \"contributor\": {},\n",
            serde_json::to_string(&self.contributor).unwrap()
        ));
        if let Some(pg) = &self.postgres {
            out.push_str(&format!(
                "  \"postgres\": {},\n",
                serde_json::to_string(pg).unwrap()
            ));
        }
        list(&mut out, "schemas", &self.schemas, false);
        list(&mut out, "types", &self.types, false);
        list(&mut out, "routines", &self.routines, false);
        list(&mut out, "operators", &self.operators, false);
        list(&mut out, "casts", &self.casts, false);
        list(&mut out, "opclasses", &self.opclasses, false);
        list(&mut out, "relations", &self.relations, true);
        out.push_str("}\n");
        out
    }
}

/// Input signature of a routine, for ordering and duplicates.
pub(crate) fn signature(r: &Routine) -> Vec<String> {
    r.input_types().map(|t| t.to_string()).collect()
}
