//! The in-memory catalog: global contributors (packs) and project objects,
//! indexed by qualified name, with resolution of unqualified names through a
//! `search_path` (D11).

use std::collections::BTreeMap;

use crate::model::{
    Cast, Extension, Index, OpClass, Operator, Policy, QName, Relation, Routine, Schema, Sequence,
    Trigger, TypeDef,
};
use crate::pack::{Contributor, Pack, signature};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    /// Loaded contributors, in order.
    pub contributors: Vec<Contributor>,
    pub schemas: BTreeMap<String, Schema>,
    pub types: BTreeMap<QName, TypeDef>,
    /// Overloads per name.
    pub routines: BTreeMap<QName, Vec<Routine>>,
    pub operators: BTreeMap<QName, Vec<Operator>>,
    pub casts: Vec<Cast>,
    pub opclasses: Vec<OpClass>,
    pub relations: BTreeMap<QName, Relation>,
    pub indexes: BTreeMap<QName, Index>,
    pub sequences: BTreeMap<QName, Sequence>,
    /// Per table, then per trigger name.
    pub triggers: BTreeMap<QName, BTreeMap<String, Trigger>>,
    pub policies: BTreeMap<QName, BTreeMap<String, Policy>>,
    pub extensions: BTreeMap<String, Extension>,
    /// Pack loaded by each active extension (in its schema), to unload it on
    /// `DROP EXTENSION` or on an update.
    pub extension_packs: BTreeMap<String, Pack>,
    /// Creation counter (rank of routines and views, for export).
    pub next_order: u64,
    /// `COMMENT ON …` in effect, per target (`TABLE public.t`), as the
    /// emitted statement. No effect on analysis, restored on export.
    pub comments: BTreeMap<String, String>,
}

/// Effective `search_path`: schemas looked up in order for an unqualified
/// name. `pg_temp` (temporary tables) comes first, and `pg_catalog` is looked
/// up first if it is not listed — as in PostgreSQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPath(Vec<String>);

impl SearchPath {
    pub fn new<I, S>(schemas: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        SearchPath(schemas.into_iter().map(Into::into).collect())
    }

    /// Creation schema for unqualified objects: the first one listed
    /// (`pg_catalog` and `pg_temp` excluded).
    pub fn creation_schema(&self) -> Option<&str> {
        self.0
            .iter()
            .map(String::as_str)
            .find(|s| *s != "pg_catalog" && *s != "pg_temp")
    }

    /// Full lookup order.
    pub fn lookup_order(&self) -> Vec<&str> {
        let mut out = vec!["pg_temp"];
        if !self.0.iter().any(|s| s == "pg_catalog") {
            out.push("pg_catalog");
        }
        out.extend(
            self.0
                .iter()
                .map(String::as_str)
                .filter(|s| *s != "pg_temp"),
        );
        out
    }

    pub fn schemas(&self) -> &[String] {
        &self.0
    }
}

impl Default for SearchPath {
    /// `"$user", public`: with no known role, `public`.
    fn default() -> Self {
        SearchPath::new(["public"])
    }
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the objects of a pack. An overload already present (same name,
    /// same signature) is replaced: an extension pack does not redefine the
    /// base catalog, but two loads of the same pack are idempotent.
    pub fn load_pack(&mut self, pack: &Pack) {
        self.contributors.push(pack.contributor.clone());
        for s in &pack.schemas {
            self.schemas.insert(s.name.clone(), s.clone());
        }
        for t in &pack.types {
            self.types.insert(t.name.clone(), t.clone());
        }
        for r in &pack.routines {
            let overloads = self.routines.entry(r.name.clone()).or_default();
            let sig = signature(r);
            overloads.retain(|o| signature(o) != sig);
            overloads.push(r.clone());
        }
        for o in &pack.operators {
            let list = self.operators.entry(o.name.clone()).or_default();
            list.retain(|x| (&x.left, &x.right) != (&o.left, &o.right));
            list.push(o.clone());
        }
        for c in &pack.casts {
            self.casts
                .retain(|x| (&x.source, &x.target) != (&c.source, &c.target));
            self.casts.push(c.clone());
        }
        for r in &pack.relations {
            self.relations.insert(r.name.clone(), r.clone());
        }
        for oc in &pack.opclasses {
            self.opclasses.retain(|x| {
                (&x.access_method, &x.name, &x.input) != (&oc.access_method, &oc.name, &oc.input)
            });
            self.opclasses.push(oc.clone());
        }
    }

    /// First schema of the path that contains `name` according to `exists`.
    fn resolve(
        &self,
        path: &SearchPath,
        name: &str,
        exists: impl Fn(&QName) -> bool,
    ) -> Option<QName> {
        path.lookup_order()
            .into_iter()
            .map(|schema| QName::new(schema, name))
            .find(|q| exists(q))
    }

    /// Relation designated by a written name: qualified, it is exact;
    /// otherwise resolved through the `search_path`.
    pub fn resolve_relation(
        &self,
        schema: Option<&str>,
        name: &str,
        path: &SearchPath,
    ) -> Option<&Relation> {
        match schema {
            Some(s) => self.relations.get(&QName::new(s, name)),
            None => self
                .resolve(path, name, |q| self.relations.contains_key(q))
                .and_then(|q| self.relations.get(&q)),
        }
    }

    pub fn resolve_type(
        &self,
        schema: Option<&str>,
        name: &str,
        path: &SearchPath,
    ) -> Option<&TypeDef> {
        match schema {
            Some(s) => self.types.get(&QName::new(s, name)),
            None => self
                .resolve(path, name, |q| self.types.contains_key(q))
                .and_then(|q| self.types.get(&q)),
        }
    }

    /// Every overload visible under this name: the first schema of the path
    /// that defines some wins for an unqualified name.
    ///
    /// (PostgreSQL actually considers the overloads of every schema in the
    /// path, then breaks ties; that fine-grained choice belongs to function
    /// resolution, P3.)
    pub fn resolve_routines(
        &self,
        schema: Option<&str>,
        name: &str,
        path: &SearchPath,
    ) -> Vec<&Routine> {
        match schema {
            Some(s) => self
                .routines
                .get(&QName::new(s, name))
                .map(|v| v.iter().collect())
                .unwrap_or_default(),
            None => path
                .lookup_order()
                .into_iter()
                .filter_map(|s| self.routines.get(&QName::new(s, name)))
                .flatten()
                .collect(),
        }
    }

    /// Removes the objects brought by an extension.
    pub fn unload_extension(&mut self, name: &str) {
        self.extensions.remove(name);
        let Some(pack) = self.extension_packs.remove(name) else {
            return;
        };
        self.contributors.retain(|c| *c != pack.contributor);
        for t in &pack.types {
            self.types.remove(&t.name);
        }
        for r in &pack.routines {
            if let Some(list) = self.routines.get_mut(&r.name) {
                let sig = signature(r);
                list.retain(|o| signature(o) != sig);
                if list.is_empty() {
                    self.routines.remove(&r.name);
                }
            }
        }
        for o in &pack.operators {
            if let Some(list) = self.operators.get_mut(&o.name) {
                list.retain(|x| (&x.left, &x.right) != (&o.left, &o.right));
                if list.is_empty() {
                    self.operators.remove(&o.name);
                }
            }
        }
        self.casts.retain(|c| !pack.casts.contains(c));
        self.opclasses.retain(|c| !pack.opclasses.contains(c));
        for r in &pack.relations {
            self.relations.remove(&r.name);
        }
    }

    /// `DROP SCHEMA … CASCADE`: removes everything the schema contains.
    pub fn drop_schema_objects(&mut self, schema: &str) {
        self.types.retain(|q, _| q.schema != schema);
        self.routines.retain(|q, _| q.schema != schema);
        self.operators.retain(|q, _| q.schema != schema);
        self.relations.retain(|q, _| q.schema != schema);
        self.indexes.retain(|q, _| q.schema != schema);
        self.sequences.retain(|q, _| q.schema != schema);
        self.triggers.retain(|q, _| q.schema != schema);
        self.policies.retain(|q, _| q.schema != schema);
        let gone: Vec<String> = self
            .extensions
            .values()
            .filter(|e| e.schema == schema)
            .map(|e| e.name.clone())
            .collect();
        for e in gone {
            self.unload_extension(&e);
        }
    }

    pub fn schema_exists(&self, name: &str) -> bool {
        self.schemas.contains_key(name)
    }
}
