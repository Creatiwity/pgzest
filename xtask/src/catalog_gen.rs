//! `cargo xtask catalog-gen` : génère un pack de catalogue (PLAN.md, D9)
//! depuis un PostgreSQL réel.
//!
//! - Sans `--extension` : le pack `postgres@<majeure>` — schémas
//!   `pg_catalog` et `information_schema` d'une base vierge.
//! - Avec `--extension nom` : le delta apporté par `CREATE EXTENSION nom`,
//!   créée dans un schéma dédié remplacé par `@extschema@`.
//!
//! Une base temporaire est créée puis supprimée ; rien n'est modifié
//! ailleurs sur le serveur.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use pgweave_catalog::model::{
    Arg, ArgMode, Attribute, Cast, CastContext, CastMethod, CheckConstraint, Column, OpClass,
    Operator, Persistence, Relation, RelationKind, Routine, RoutineKind, Schema, TypeDef, TypeKind,
    Volatility,
};
use pgweave_catalog::pack::{EXTSCHEMA, PACK_FORMAT};
use pgweave_catalog::{Contributor, Pack, QName, TypeRef};
use tokio_postgres::{Client, NoTls};

/// Schéma de travail où l'extension est créée.
const WORK_SCHEMA: &str = "pgweave_ext";

pub fn run(
    url: Option<String>,
    extension: Option<String>,
    version: Option<String>,
    out: Option<PathBuf>,
) -> Result<(), String> {
    let url = url
        .or_else(|| std::env::var("PGWEAVE_ORACLE_URL").ok())
        .ok_or("--pg-url ou PGWEAVE_ORACLE_URL requis")?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let pack = rt.block_on(generate(&url, extension.as_deref(), version.as_deref()))?;
    let json = pack.to_json();
    let path = out.unwrap_or_else(|| {
        PathBuf::from(format!(
            "crates/pgweave-catalog/packs/{}-{}.json",
            pack.contributor.name, pack.contributor.version
        ))
    });
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!(
        "{} : {} types, {} routines, {} opérateurs, {} conversions, {} classes d'opérateurs, {} relations → {}",
        pack.contributor,
        pack.types.len(),
        pack.routines.len(),
        pack.operators.len(),
        pack.casts.len(),
        pack.opclasses.len(),
        pack.relations.len(),
        path.display()
    );
    Ok(())
}

fn err(e: tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db.message().to_string(),
        None => e.to_string(),
    }
}

async fn connect(config: &tokio_postgres::Config) -> Result<Client, String> {
    let (client, conn) = config.connect(NoTls).await.map_err(err)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(client)
}

async fn generate(
    url: &str,
    extension: Option<&str>,
    version: Option<&str>,
) -> Result<Pack, String> {
    let base: tokio_postgres::Config = url.parse().map_err(|e| format!("URL : {e}"))?;
    let admin = connect(&base).await?;
    let db = format!("pgweave_catalog_{}", std::process::id());
    admin
        .batch_execute(&format!("CREATE DATABASE \"{db}\" TEMPLATE template0"))
        .await
        .map_err(err)?;
    let mut config = base.clone();
    config.dbname(&db);
    let result = async {
        let client = connect(&config).await?;
        let major: i32 = client
            .query_one(
                "SELECT current_setting('server_version_num')::int / 10000",
                &[],
            )
            .await
            .map_err(err)?
            .get(0);
        match extension {
            None => {
                let snap = Snapshot::take(&client, &["pg_catalog", "information_schema"]).await?;
                let mut pack = snap.into_pack(
                    Contributor {
                        name: "postgres".into(),
                        version: major.to_string(),
                    },
                    None,
                    &HashSet::new(),
                );
                pack.schemas.push(Schema {
                    name: "public".into(),
                    owner: None,
                });
                pack.normalize();
                Ok(pack)
            }
            Some(ext) => {
                let casts_before = Snapshot::casts(&client).await?;
                client
                    .batch_execute(&format!("CREATE SCHEMA {WORK_SCHEMA}"))
                    .await
                    .map_err(err)?;
                let version_clause = version
                    .map(|v| format!(" VERSION '{}'", v.replace('\'', "''")))
                    .unwrap_or_default();
                client
                    .batch_execute(&format!(
                        "CREATE EXTENSION \"{ext}\" SCHEMA {WORK_SCHEMA}{version_clause}"
                    ))
                    .await
                    .map_err(err)?;
                let installed: String = client
                    .query_one(
                        "SELECT extversion FROM pg_extension WHERE extname = $1",
                        &[&ext],
                    )
                    .await
                    .map_err(err)?
                    .get(0);
                let snap = Snapshot::take(&client, &[WORK_SCHEMA]).await?;
                let mut pack = snap.into_pack(
                    Contributor {
                        name: ext.to_string(),
                        version: installed,
                    },
                    Some(major.to_string()),
                    &casts_before,
                );
                // Les OID d'une extension changent à chaque installation : ils
                // ne décrivent rien de stable.
                for t in &mut pack.types {
                    t.oid = None;
                }
                for r in &mut pack.routines {
                    r.oid = None;
                }
                for o in &mut pack.operators {
                    o.oid = None;
                }
                pack.normalize();
                Ok(pack)
            }
        }
    }
    .await;
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .await
        .map_err(err)?;
    result
}

/// Colonne d'une relation : nom, type, rendu `format_type` si modificateur,
/// `NOT NULL`.
type AttrRow = (String, i64, Option<String>, bool);

/// Clé d'une conversion : (source, cible) en OID.
type CastKey = (i64, i64);

#[derive(Clone)]
struct TypeRow {
    oid: i64,
    schema: String,
    name: String,
    typtype: String,
    category: String,
    preferred: bool,
    elem: i64,
    array: i64,
    basetype: i64,
    base_format: Option<String>,
    notnull: bool,
    default: Option<String>,
    relid: i64,
    relkind: Option<String>,
}

/// Ce qu'un PostgreSQL contient dans un ensemble de schémas.
pub(crate) struct Snapshot {
    /// Tous les types du serveur, pour nommer n'importe quel OID.
    all_types: HashMap<i64, TypeRow>,
    schemas: Vec<String>,
    types: Vec<TypeRow>,
    enum_labels: BTreeMap<i64, Vec<String>>,
    domain_checks: BTreeMap<i64, Vec<CheckConstraint>>,
    range_subtypes: HashMap<i64, i64>,
    multirange_ranges: HashMap<i64, i64>,
    attributes: BTreeMap<i64, Vec<AttrRow>>,
    routines: Vec<tokio_postgres::Row>,
    operators: Vec<tokio_postgres::Row>,
    /// (source, cible) → (`castcontext`, `castmethod`).
    casts: Vec<(CastKey, (String, String))>,
    opclasses: Vec<tokio_postgres::Row>,
    relations: Vec<tokio_postgres::Row>,
}

impl Snapshot {
    async fn casts(client: &Client) -> Result<HashSet<CastKey>, String> {
        Ok(client
            .query(
                "SELECT castsource::int8, casttarget::int8 FROM pg_cast",
                &[],
            )
            .await
            .map_err(err)?
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect())
    }

    /// Objets des schémas `schemas`. `user` : catalogue d'un projet — les
    /// membres d'extensions (fournis par leurs packs) sont exclus.
    pub(crate) async fn take_user(client: &Client, schemas: &[&str]) -> Result<Snapshot, String> {
        let mut snap = Self::take(client, schemas).await?;
        let members: HashSet<i64> = client
            .query("SELECT objid::int8 FROM pg_depend WHERE deptype = 'e'", &[])
            .await
            .map_err(err)?
            .iter()
            .map(|r| r.get(0))
            .collect();
        snap.types.retain(|t| !members.contains(&t.oid));
        let keep = |r: &tokio_postgres::Row| !members.contains(&r.get::<_, i64>(0));
        snap.routines.retain(keep);
        snap.operators.retain(keep);
        snap.relations.retain(keep);
        snap.casts.clear();
        snap.opclasses.clear();
        Ok(snap)
    }

    async fn take(client: &Client, schemas: &[&str]) -> Result<Snapshot, String> {
        let schemas: Vec<String> = schemas.iter().map(|s| s.to_string()).collect();
        let type_rows = client
            .query(
                "SELECT t.oid::int8, n.nspname::text, t.typname::text, t.typtype::text,
                        t.typcategory::text, t.typispreferred, t.typelem::int8, t.typarray::int8,
                        t.typbasetype::int8,
                        CASE WHEN t.typbasetype <> 0 THEN format_type(t.typbasetype, t.typtypmod) END,
                        t.typnotnull, t.typdefault, t.typrelid::int8, c.relkind::text
                   FROM pg_type t
                   JOIN pg_namespace n ON n.oid = t.typnamespace
                   LEFT JOIN pg_class c ON c.oid = t.typrelid",
                &[],
            )
            .await
            .map_err(err)?;
        let mut all_types = HashMap::new();
        let mut types = Vec::new();
        for r in type_rows {
            let row = TypeRow {
                oid: r.get(0),
                schema: r.get(1),
                name: r.get(2),
                typtype: r.get(3),
                category: r.get(4),
                preferred: r.get(5),
                elem: r.get(6),
                array: r.get(7),
                basetype: r.get(8),
                base_format: r.get(9),
                notnull: r.get(10),
                default: r.get(11),
                relid: r.get(12),
                relkind: r.get(13),
            };
            if schemas.contains(&row.schema) {
                types.push(row.clone());
            }
            all_types.insert(row.oid, row);
        }

        let mut enum_labels: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        for r in client
            .query(
                "SELECT enumtypid::int8, enumlabel::text FROM pg_enum ORDER BY enumtypid, enumsortorder",
                &[],
            )
            .await
            .map_err(err)?
        {
            enum_labels.entry(r.get(0)).or_default().push(r.get(1));
        }

        let mut domain_checks: BTreeMap<i64, Vec<CheckConstraint>> = BTreeMap::new();
        for r in client
            .query(
                "SELECT contypid::int8, conname::text, pg_get_constraintdef(oid)
                   FROM pg_constraint WHERE contypid <> 0 AND contype = 'c' ORDER BY conname",
                &[],
            )
            .await
            .map_err(err)?
        {
            domain_checks
                .entry(r.get(0))
                .or_default()
                .push(CheckConstraint {
                    name: Some(r.get(1)),
                    expr: r.get(2),
                });
        }

        let mut range_subtypes = HashMap::new();
        let mut multirange_ranges = HashMap::new();
        for r in client
            .query(
                "SELECT rngtypid::int8, rngsubtype::int8, rngmultitypid::int8 FROM pg_range",
                &[],
            )
            .await
            .map_err(err)?
        {
            range_subtypes.insert(r.get::<_, i64>(0), r.get::<_, i64>(1));
            multirange_ranges.insert(r.get::<_, i64>(2), r.get::<_, i64>(0));
        }

        let mut attributes: BTreeMap<i64, Vec<AttrRow>> = BTreeMap::new();
        for r in client
            .query(
                "SELECT a.attrelid::int8, a.attname::text, a.atttypid::int8,
                        CASE WHEN a.atttypmod >= 0 THEN format_type(a.atttypid, a.atttypmod) END,
                        a.attnotnull
                   FROM pg_attribute a
                   JOIN pg_class c ON c.oid = a.attrelid
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE a.attnum > 0 AND NOT a.attisdropped AND n.nspname = ANY($1)
                  ORDER BY a.attrelid, a.attnum",
                &[&schemas],
            )
            .await
            .map_err(err)?
        {
            attributes
                .entry(r.get(0))
                .or_default()
                .push((r.get(1), r.get(2), r.get(3), r.get(4)));
        }

        let routines = client
            .query(
                "SELECT p.oid::int8, n.nspname::text, p.proname::text, p.prokind::text,
                        p.proargtypes::oid[]::int8[], p.proallargtypes::int8[],
                        p.proargmodes::text[], p.proargnames, p.pronargdefaults::int4,
                        p.prorettype::int8, p.proretset, p.provolatile::text, p.proisstrict,
                        p.prosecdef, l.lanname::text
                   FROM pg_proc p
                   JOIN pg_namespace n ON n.oid = p.pronamespace
                   JOIN pg_language l ON l.oid = p.prolang
                  WHERE n.nspname = ANY($1)",
                &[&schemas],
            )
            .await
            .map_err(err)?;
        let operators = client
            .query(
                "SELECT o.oid::int8, n.nspname::text, o.oprname::text, o.oprleft::int8,
                        o.oprright::int8, o.oprresult::int8
                   FROM pg_operator o JOIN pg_namespace n ON n.oid = o.oprnamespace
                  WHERE n.nspname = ANY($1)",
                &[&schemas],
            )
            .await
            .map_err(err)?;
        let casts = client
            .query(
                "SELECT castsource::int8, casttarget::int8, castcontext::text, castmethod::text
                   FROM pg_cast",
                &[],
            )
            .await
            .map_err(err)?
            .iter()
            .map(|r| ((r.get(0), r.get(1)), (r.get(2), r.get(3))))
            .collect();
        let opclasses = client
            .query(
                "SELECT n.nspname::text, c.opcname::text, a.amname::text, c.opcintype::int8, c.opcdefault
                   FROM pg_opclass c
                   JOIN pg_namespace n ON n.oid = c.opcnamespace
                   JOIN pg_am a ON a.oid = c.opcmethod
                  WHERE n.nspname = ANY($1)",
                &[&schemas],
            )
            .await
            .map_err(err)?;
        let relations = client
            .query(
                "SELECT c.oid::int8, n.nspname::text, c.relname::text, c.relkind::text,
                        c.relpersistence::text, c.relrowsecurity
                   FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = ANY($1) AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
                &[&schemas],
            )
            .await
            .map_err(err)?;

        Ok(Snapshot {
            all_types,
            schemas,
            types,
            enum_labels,
            domain_checks,
            range_subtypes,
            multirange_ranges,
            attributes,
            routines,
            operators,
            casts,
            opclasses,
            relations,
        })
    }

    fn qname(&self, schema: &str, name: &str) -> QName {
        let schema = if schema == WORK_SCHEMA {
            EXTSCHEMA
        } else {
            schema
        };
        QName::new(schema, name)
    }

    fn type_qname(&self, oid: i64) -> Option<QName> {
        self.all_types
            .get(&oid)
            .map(|t| self.qname(&t.schema, &t.name))
    }

    /// Référence au type `oid`, avec les modificateurs extraits de son rendu
    /// `format_type` (`character varying(20)` → `["20"]`).
    fn type_ref(&self, oid: i64, formatted: Option<&str>) -> TypeRef {
        let Some(t) = self.all_types.get(&oid) else {
            return TypeRef::Unresolved(format!("oid {oid}"));
        };
        // `élément[]` seulement pour le tableau attitré de l'élément
        // (`typarray`) : `oidvector` a `oid` pour élément sans être `oid[]`.
        let canonical_array = self.all_types.get(&t.elem).is_some_and(|e| e.array == oid);
        if t.typtype == "b" && t.category == "A" && t.elem != 0 && canonical_array {
            let inner = formatted.map(|f| f.trim_end_matches("[]"));
            return TypeRef::Array(Box::new(self.type_ref(t.elem, inner)));
        }
        let modifiers = formatted
            .and_then(|f| {
                let open = f.rfind('(')?;
                let close = f.rfind(')')?;
                (close > open).then(|| {
                    f[open + 1..close]
                        .split(',')
                        .map(|m| m.trim().to_string())
                        .collect()
                })
            })
            .unwrap_or_default();
        TypeRef::Named {
            name: self.qname(&t.schema, &t.name),
            modifiers,
        }
    }

    pub(crate) fn into_pack(
        self,
        contributor: Contributor,
        postgres: Option<String>,
        skip_casts: &HashSet<CastKey>,
    ) -> Pack {
        let mut pack = Pack {
            format: PACK_FORMAT,
            contributor,
            postgres,
            schemas: self
                .schemas
                .iter()
                .filter(|s| s.as_str() != WORK_SCHEMA)
                .map(|s| Schema {
                    name: s.clone(),
                    owner: None,
                })
                .collect(),
            types: Vec::new(),
            routines: Vec::new(),
            operators: Vec::new(),
            casts: Vec::new(),
            opclasses: Vec::new(),
            relations: Vec::new(),
        };

        for t in &self.types {
            let kind = match t.typtype.as_str() {
                "p" => TypeKind::Pseudo,
                "e" => TypeKind::Enum {
                    labels: self.enum_labels.get(&t.oid).cloned().unwrap_or_default(),
                },
                "d" => TypeKind::Domain {
                    base: self.type_ref(t.basetype, t.base_format.as_deref()),
                    not_null: t.notnull,
                    default: t.default.clone(),
                    checks: self.domain_checks.get(&t.oid).cloned().unwrap_or_default(),
                },
                "c" => TypeKind::Composite {
                    attributes: self
                        .attributes
                        .get(&t.relid)
                        .map(|attrs| {
                            attrs
                                .iter()
                                .map(|(name, ty, fmt, _)| Attribute {
                                    name: name.clone(),
                                    ty: self.type_ref(*ty, fmt.as_deref()),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                },
                "r" => match self
                    .range_subtypes
                    .get(&t.oid)
                    .and_then(|s| self.type_qname(*s))
                {
                    Some(subtype) => TypeKind::Range { subtype },
                    None => TypeKind::Base,
                },
                "m" => match self
                    .multirange_ranges
                    .get(&t.oid)
                    .and_then(|r| self.type_qname(*r))
                {
                    Some(range) => TypeKind::Multirange { range },
                    None => TypeKind::Base,
                },
                _ if t.category == "A" && t.elem != 0 => match self.type_qname(t.elem) {
                    Some(element) => TypeKind::Array { element },
                    None => TypeKind::Base,
                },
                _ => TypeKind::Base,
            };
            // Type ligne d'une table ou d'une vue : la relation le décrit.
            if t.typtype == "c" && t.relkind.as_deref() != Some("c") {
                continue;
            }
            pack.types.push(TypeDef {
                name: self.qname(&t.schema, &t.name),
                oid: Some(t.oid as u32),
                kind,
                category: t.category.chars().next().unwrap_or('U'),
                preferred: t.preferred,
                array: (t.array != 0).then(|| self.type_qname(t.array)).flatten(),
            });
        }

        for r in &self.routines {
            pack.routines.push(self.routine(r));
        }

        for r in &self.operators {
            let left: i64 = r.get(3);
            pack.operators.push(Operator {
                name: self.qname(r.get(1), r.get(2)),
                oid: Some(r.get::<_, i64>(0) as u32),
                left: (left != 0).then(|| self.type_ref(left, None)),
                right: self.type_ref(r.get(4), None),
                result: self.type_ref(r.get(5), None),
            });
        }

        for ((source, target), (context, method)) in &self.casts {
            if skip_casts.contains(&(*source, *target)) {
                continue;
            }
            pack.casts.push(Cast {
                source: self.type_ref(*source, None),
                target: self.type_ref(*target, None),
                context: match context.as_str() {
                    "i" => CastContext::Implicit,
                    "a" => CastContext::Assignment,
                    _ => CastContext::Explicit,
                },
                method: match method.as_str() {
                    "f" => CastMethod::Function,
                    "b" => CastMethod::Binary,
                    _ => CastMethod::InOut,
                },
            });
        }

        for r in &self.opclasses {
            pack.opclasses.push(OpClass {
                name: self.qname(r.get(0), r.get(1)),
                access_method: r.get(2),
                input: self.type_ref(r.get(3), None),
                default: r.get(4),
            });
        }

        for r in &self.relations {
            let oid: i64 = r.get(0);
            let kind = match r.get::<_, String>(3).as_str() {
                "p" => RelationKind::PartitionedTable,
                "v" => RelationKind::View,
                "m" => RelationKind::MaterializedView,
                "f" => RelationKind::ForeignTable,
                _ => RelationKind::Table,
            };
            pack.relations.push(Relation {
                name: self.qname(r.get(1), r.get(2)),
                kind,
                persistence: match r.get::<_, String>(4).as_str() {
                    "u" => Persistence::Unlogged,
                    "t" => Persistence::Temporary,
                    _ => Persistence::Permanent,
                },
                columns: self
                    .attributes
                    .get(&oid)
                    .map(|attrs| {
                        attrs
                            .iter()
                            .map(|(name, ty, fmt, not_null)| Column {
                                name: name.clone(),
                                ty: self.type_ref(*ty, fmt.as_deref()),
                                not_null: *not_null,
                                default: None,
                                identity: None,
                                generated: None,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                constraints: Vec::new(),
                row_level_security: r.get(5),
                opaque: false,
                definition: None,
                order: 0,
            });
        }
        pack
    }

    fn routine(&self, r: &tokio_postgres::Row) -> Routine {
        let in_types: Vec<i64> = r.get(4);
        let all_types: Option<Vec<i64>> = r.get(5);
        let modes: Option<Vec<String>> = r.get(6);
        let names: Option<Vec<String>> = r.get(7);
        let n_defaults = r.get::<_, i32>(8) as usize;
        let kind = match r.get::<_, String>(3).as_str() {
            "a" => RoutineKind::Aggregate,
            "w" => RoutineKind::Window,
            "p" => RoutineKind::Procedure,
            _ => RoutineKind::Function,
        };

        let types = all_types.unwrap_or_else(|| in_types.clone());
        let mut args: Vec<Arg> = types
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                let mode = match modes.as_ref().and_then(|m| m.get(i)).map(String::as_str) {
                    Some("o") => ArgMode::Out,
                    Some("b") => ArgMode::InOut,
                    Some("v") => ArgMode::Variadic,
                    Some("t") => ArgMode::Table,
                    _ => ArgMode::In,
                };
                Arg {
                    name: names
                        .as_ref()
                        .and_then(|n| n.get(i))
                        .filter(|n| !n.is_empty())
                        .cloned(),
                    ty: self.type_ref(*ty, None),
                    mode,
                    has_default: false,
                }
            })
            .collect();
        // Les `pronargdefaults` derniers arguments d'entrée ont un défaut.
        let input_positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
            .map(|(i, _)| i)
            .collect();
        for &i in input_positions.iter().rev().take(n_defaults) {
            args[i].has_default = true;
        }

        let ret: i64 = r.get(9);
        Routine {
            name: self.qname(r.get(1), r.get(2)),
            oid: Some(r.get::<_, i64>(0) as u32),
            kind,
            args,
            returns: (kind != RoutineKind::Procedure).then(|| self.type_ref(ret, None)),
            returns_set: r.get(10),
            volatility: match r.get::<_, String>(11).as_str() {
                "i" => Volatility::Immutable,
                "s" => Volatility::Stable,
                _ => Volatility::Volatile,
            },
            strict: r.get(12),
            security_definer: r.get(13),
            language: r.get(14),
            definition: None,
            order: 0,
            catalog_effects: false,
            source: None,
        }
    }
}
