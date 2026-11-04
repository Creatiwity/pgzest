//! Comparison of the catalog rebuilt by pgzest (migration replay) with the
//! real catalog of a PostgreSQL on which the same migrations were applied:
//! the P2 exit criterion.
//!
//! The scope is the project's: user schemas, objects that are not members
//! of an extension (those come from packs, compared separately by name and
//! version). Expressions (defaults, `CHECK`) are compared by presence: the
//! text PostgreSQL gives back is not the one pgzest emits.

use std::collections::{BTreeMap, BTreeSet};

use pgzest_catalog::model::{
    ConstraintKind, Extension, Index, Policy, Schema, Sequence, Trigger, TypeKind,
};
use pgzest_catalog::{Catalog, Contributor, QName};
use tokio_postgres::Client;

use crate::catalog_gen::Snapshot;

fn err(e: tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db.message().to_string(),
        None => e.to_string(),
    }
}

fn is_system(schema: &str) -> bool {
    schema == "pg_catalog"
        || schema == "information_schema"
        || schema == "pg_temp"
        || schema.starts_with("pg_toast")
        || schema.starts_with("pg_temp_")
}

/// Project catalog as PostgreSQL holds it.
pub(crate) async fn real_catalog(client: &Client) -> Result<Catalog, String> {
    let schemas: Vec<String> = client
        .query("SELECT nspname::text FROM pg_namespace ORDER BY 1", &[])
        .await
        .map_err(err)?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .filter(|s| !is_system(s))
        .collect();
    let refs: Vec<&str> = schemas.iter().map(String::as_str).collect();
    let snap = Snapshot::take_user(client, &refs).await?;
    let pack = snap.into_pack(
        Contributor {
            name: "postgres".into(),
            version: "real".into(),
        },
        None,
        &Default::default(),
    );
    let mut cat = Catalog::new();
    cat.load_pack(&pack);
    for s in &schemas {
        cat.schemas.insert(
            s.clone(),
            Schema {
                name: s.clone(),
                owner: None,
            },
        );
    }

    let q = |s: String, n: String| QName::new(s, n);

    for r in client
        .query(
            "SELECT n.nspname::text, c.relname::text, a.attname::text, pg_get_expr(d.adbin, d.adrelid)
               FROM pg_attrdef d
               JOIN pg_class c ON c.oid = d.adrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
               JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum",
            &[],
        )
        .await
        .map_err(err)?
    {
        if let Some(rel) = cat.relations.get_mut(&q(r.get(0), r.get(1)))
            && let Some(col) = rel.columns.iter_mut().find(|c| c.name == r.get::<_, String>(2))
        {
            col.default = Some(r.get(3));
        }
    }

    for r in client
        .query(
            "SELECT n.nspname::text, c.relname::text, a.attname::text
               FROM pg_attribute a
               JOIN pg_class c ON c.oid = a.attrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE a.attidentity <> ''",
            &[],
        )
        .await
        .map_err(err)?
    {
        if let Some(rel) = cat.relations.get_mut(&q(r.get(0), r.get(1)))
            && let Some(col) = rel
                .columns
                .iter_mut()
                .find(|c| c.name == r.get::<_, String>(2))
        {
            col.identity = Some(pgzest_catalog::model::IdentityKind::Always);
        }
    }

    for r in client
        .query(
            "SELECT n.nspname::text, c.relname::text, k.conname::text, k.contype::text,
                    pg_get_constraintdef(k.oid), NOT k.convalidated
               FROM pg_constraint k
               JOIN pg_class c ON c.oid = k.conrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE k.contype IN ('p', 'u', 'f', 'c', 'x')",
            &[],
        )
        .await
        .map_err(err)?
    {
        let Some(rel) = cat.relations.get_mut(&q(r.get(0), r.get(1))) else {
            continue;
        };
        let def: String = r.get(4);
        let kind = match r.get::<_, String>(3).as_str() {
            "p" => ConstraintKind::PrimaryKey { columns: vec![] },
            "u" => ConstraintKind::Unique { columns: vec![] },
            "f" => ConstraintKind::ForeignKey {
                columns: vec![],
                references: QName::new("", ""),
                ref_columns: vec![],
            },
            "c" => ConstraintKind::Check {
                expr: def,
                columns: vec![],
            },
            _ => ConstraintKind::Exclude {
                definition: def,
                columns: vec![],
            },
        };
        rel.constraints.push(pgzest_catalog::model::Constraint {
            name: r.get(2),
            kind,
            definition: None,
            not_valid: r.get(5),
        });
    }

    for r in client
        .query(
            "SELECT n.nspname::text, i.relname::text, tn.nspname::text, t.relname::text, x.indisunique,
                    pg_get_indexdef(x.indexrelid)
               FROM pg_index x
               JOIN pg_class i ON i.oid = x.indexrelid
               JOIN pg_namespace n ON n.oid = i.relnamespace
               JOIN pg_class t ON t.oid = x.indrelid
               JOIN pg_namespace tn ON tn.oid = t.relnamespace
              WHERE NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = x.indexrelid
                                  AND k.conrelid = x.indrelid AND k.contype IN ('p', 'u', 'x'))",
            &[],
        )
        .await
        .map_err(err)?
    {
        let schema: String = r.get(0);
        if is_system(&schema) {
            continue;
        }
        let name = q(schema, r.get(1));
        cat.indexes.insert(
            name.clone(),
            Index {
                name,
                table: q(r.get(2), r.get(3)),
                unique: r.get(4),
                definition: r.get(5),
                columns: vec![],
            },
        );
    }

    for r in client
        .query(
            "SELECT n.nspname::text, s.relname::text, tn.nspname::text, t.relname::text, a.attname::text
               FROM pg_class s
               JOIN pg_namespace n ON n.oid = s.relnamespace
               LEFT JOIN pg_depend d ON d.objid = s.oid AND d.classid = 'pg_class'::regclass
                                    AND d.deptype IN ('a', 'i') AND d.refobjsubid > 0
               LEFT JOIN pg_class t ON t.oid = d.refobjid
               LEFT JOIN pg_namespace tn ON tn.oid = t.relnamespace
               LEFT JOIN pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid
              WHERE s.relkind = 'S'
                AND NOT EXISTS (SELECT 1 FROM pg_depend e WHERE e.objid = s.oid AND e.deptype = 'e')",
            &[],
        )
        .await
        .map_err(err)?
    {
        let schema: String = r.get(0);
        if is_system(&schema) {
            continue;
        }
        let name = q(schema, r.get(1));
        let owned_by = match (r.get::<_, Option<String>>(2), r.get::<_, Option<String>>(3), r.get::<_, Option<String>>(4)) {
            (Some(s), Some(t), Some(c)) => Some((q(s, t), c)),
            _ => None,
        };
        cat.sequences.insert(
            name.clone(),
            Sequence {
                name,
                owned_by,
                definition: None,
            },
        );
    }

    for r in client
        .query(
            "SELECT n.nspname::text, c.relname::text, t.tgname::text, pn.nspname::text, p.proname::text,
                    (t.tgtype & 1) = 1
               FROM pg_trigger t
               JOIN pg_class c ON c.oid = t.tgrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
               JOIN pg_proc p ON p.oid = t.tgfoid
               JOIN pg_namespace pn ON pn.oid = p.pronamespace
              WHERE NOT t.tgisinternal",
            &[],
        )
        .await
        .map_err(err)?
    {
        let table = q(r.get(0), r.get(1));
        let name: String = r.get(2);
        cat.triggers.entry(table.clone()).or_default().insert(
            name.clone(),
            Trigger {
                name,
                table,
                function: q(r.get(3), r.get(4)),
                timing: String::new(),
                events: vec![],
                for_each_row: r.get(5),
                old_table: None,
                new_table: None,
                definition: None,
            },
        );
    }

    for r in client
        .query(
            "SELECT n.nspname::text, c.relname::text, p.polname::text
               FROM pg_policy p JOIN pg_class c ON c.oid = p.polrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace",
            &[],
        )
        .await
        .map_err(err)?
    {
        let table = q(r.get(0), r.get(1));
        let name: String = r.get(2);
        cat.policies.entry(table.clone()).or_default().insert(
            name.clone(),
            Policy {
                name,
                table,
                command: String::new(),
                permissive: true,
                roles: vec![],
                using: None,
                with_check: None,
                definition: None,
            },
        );
    }

    for r in client
        .query(
            "SELECT e.extname::text, e.extversion, n.nspname::text
               FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
              WHERE e.extname <> 'plpgsql'",
            &[],
        )
        .await
        .map_err(err)?
    {
        let name: String = r.get(0);
        cat.extensions.insert(
            name.clone(),
            Extension {
                name,
                version: Some(r.get(1)),
                schema: r.get(2),
            },
        );
    }
    Ok(cat)
}

/// Result for one object category.
#[derive(Default)]
pub(crate) struct Category {
    /// Objects present in PostgreSQL.
    pub real: usize,
    /// Identical.
    pub matched: usize,
    /// Missing from the rebuilt catalog.
    pub missing: usize,
    /// Extra in the rebuilt catalog.
    pub extra: usize,
    /// Present on both sides, but different.
    pub different: usize,
    /// Present on both sides, value not yet established by pgzest (type of
    /// a view column before semantic analysis, P3).
    pub unknown: usize,
}

pub(crate) struct Comparison {
    pub categories: BTreeMap<&'static str, Category>,
    /// Detail of each gap, names included: for `--details` only.
    pub details: Vec<String>,
}

/// Compares key → description sets.
fn compare_maps(
    out: &mut Comparison,
    category: &'static str,
    real: BTreeMap<String, String>,
    ours: BTreeMap<String, String>,
) {
    let c = out.categories.entry(category).or_default();
    c.real = real.len();
    for (k, v) in &real {
        match ours.get(k) {
            None => {
                c.missing += 1;
                out.details.push(format!("{category} missing: {k}"));
            }
            Some(o) if o == v => c.matched += 1,
            Some(o) if o.starts_with('?') => c.unknown += 1,
            Some(o) => {
                c.different += 1;
                out.details.push(format!(
                    "{category} different: {k}\n  real: {v}\n  pgzest: {o}"
                ));
            }
        }
    }
    for k in ours.keys() {
        if !real.contains_key(k) {
            c.extra += 1;
            out.details.push(format!("{category} extra: {k}"));
        }
    }
}

/// Comparable description of each project object (excluding system and
/// extension members).
fn describe(cat: &Catalog) -> BTreeMap<&'static str, BTreeMap<String, String>> {
    let ext_types: BTreeSet<&QName> = cat
        .extension_packs
        .values()
        .flat_map(|p| p.types.iter().map(|t| &t.name))
        .collect();
    let ext_routines: BTreeSet<(&QName, String)> = cat
        .extension_packs
        .values()
        .flat_map(|p| {
            p.routines.iter().map(|r| {
                (
                    &r.name,
                    r.input_types()
                        .map(|t| t.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                )
            })
        })
        .collect();
    let ext_relations: BTreeSet<&QName> = cat
        .extension_packs
        .values()
        .flat_map(|p| p.relations.iter().map(|r| &r.name))
        .collect();
    let user = |q: &QName| !is_system(&q.schema);

    let mut d: BTreeMap<&'static str, BTreeMap<String, String>> = BTreeMap::new();
    d.insert(
        "schemas",
        cat.schemas
            .keys()
            .filter(|s| !is_system(s))
            .map(|s| (s.clone(), String::new()))
            .collect(),
    );
    d.insert(
        "extensions",
        cat.extensions
            .values()
            .map(|e| {
                (
                    e.name.clone(),
                    format!("{}@{}", e.schema, e.version.clone().unwrap_or_default()),
                )
            })
            .collect(),
    );
    d.insert(
        "types",
        cat.types
            .values()
            .filter(|t| user(&t.name) && !ext_types.contains(&t.name))
            .map(|t| {
                let desc = match &t.kind {
                    TypeKind::Enum { labels } => format!("enum {}", labels.join(",")),
                    TypeKind::Domain { base, not_null, .. } => {
                        format!("domain {base} not_null={not_null}")
                    }
                    TypeKind::Composite { attributes } => format!(
                        "composite {}",
                        attributes
                            .iter()
                            .map(|a| format!("{} {}", a.name, a.ty))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    TypeKind::Array { element } => format!("array of {element}"),
                    other => format!("{other:?}"),
                };
                (t.name.to_string(), desc)
            })
            .collect(),
    );
    let mut routines = BTreeMap::new();
    for list in cat.routines.values() {
        for r in list {
            let sig = r
                .input_types()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(",");
            if !user(&r.name) || ext_routines.contains(&(&r.name, sig.clone())) {
                continue;
            }
            let ret = r
                .returns
                .as_ref()
                .map(|t| format!("{}{t}", if r.returns_set { "setof " } else { "" }))
                .unwrap_or_default();
            routines.insert(
                format!("{}({sig})", r.name),
                format!("{:?} → {ret}", r.kind),
            );
        }
    }
    d.insert("routines", routines);

    let mut relations = BTreeMap::new();
    let mut columns = BTreeMap::new();
    let mut constraints = BTreeMap::new();
    for r in cat
        .relations
        .values()
        .filter(|r| user(&r.name) && !ext_relations.contains(&r.name))
    {
        relations.insert(
            r.name.to_string(),
            format!("{:?} {:?}", r.kind, r.persistence),
        );
        for c in &r.columns {
            columns.insert(
                format!("{}.{}", r.name, c.name),
                format!(
                    "{}{}{}",
                    c.ty,
                    if c.not_null { " not null" } else { "" },
                    if c.default.is_some() || c.identity.is_some() || c.generated.is_some() {
                        " default"
                    } else {
                        ""
                    }
                ),
            );
        }
        for c in &r.constraints {
            let tag = match &c.kind {
                ConstraintKind::PrimaryKey { .. } => "primary key",
                ConstraintKind::Unique { .. } => "unique",
                ConstraintKind::ForeignKey { .. } => "foreign key",
                ConstraintKind::Check { .. } => "check",
                ConstraintKind::Exclude { .. } => "exclude",
            };
            constraints.insert(
                format!("{}.{}", r.name, c.name),
                format!("{tag}{}", if c.not_valid { " not valid" } else { "" }),
            );
        }
    }
    d.insert("relations", relations);
    d.insert("columns", columns);
    d.insert("constraints", constraints);
    d.insert(
        "index",
        cat.indexes
            .values()
            .filter(|i| user(&i.name))
            .map(|i| {
                (
                    i.name.to_string(),
                    format!("{} unique={}", i.table, i.unique),
                )
            })
            .collect(),
    );
    d.insert(
        "sequences",
        cat.sequences
            .values()
            .filter(|s| user(&s.name))
            .map(|s| {
                (
                    s.name.to_string(),
                    s.owned_by
                        .as_ref()
                        .map(|(t, c)| format!("{t}.{c}"))
                        .unwrap_or_default(),
                )
            })
            .collect(),
    );
    d.insert(
        "triggers",
        cat.triggers
            .values()
            .flat_map(|m| m.values())
            .filter(|t| user(&t.table))
            .map(|t| (format!("{}.{}", t.table, t.name), t.function.to_string()))
            .collect(),
    );
    d.insert(
        "policies",
        cat.policies
            .values()
            .flat_map(|m| m.values())
            .map(|p| (format!("{}.{}", p.table, p.name), String::new()))
            .collect(),
    );
    d
}

pub(crate) fn compare(real: &Catalog, ours: &Catalog) -> Comparison {
    let mut out = Comparison {
        categories: BTreeMap::new(),
        details: Vec::new(),
    };
    let r = describe(real);
    let mut o = describe(ours);
    for (cat, real_map) in r {
        let ours_map = o.remove(cat).unwrap_or_default();
        compare_maps(&mut out, cat, real_map, ours_map);
    }
    out
}
