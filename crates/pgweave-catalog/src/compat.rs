//! Pont transitoire : le catalogue vu comme un [`SchemaProvider`], pour les
//! consommateurs qui n'ont pas encore migré (checker, LSP, CLI). Disparaît
//! avec `SchemaProvider` (PLAN.md, P2).
//!
//! Les noms sont exacts (principe 8) : un nom qualifié désigne `schéma.nom`,
//! un nom simple l'objet visible par le `search_path`.

use std::collections::{HashMap, HashSet};

use pgweave_ast::types::DataType;
use pgweave_schema::provider::{
    ColumnInfo, FunctionSignature, RlsAwareSchemaProvider, SchemaProvider, TableSchema,
};

use crate::catalog::{Catalog, SearchPath};
use crate::model::{QName, TypeRef};

pub struct CatalogSchema {
    /// Objets du projet (hors `pg_catalog` et `information_schema`), par nom
    /// qualifié.
    tables: HashMap<String, TableSchema>,
    functions: HashMap<String, FunctionSignature>,
    /// Noms simples du projet : tables temporaires (consultées avant
    /// `pg_catalog`), puis objets visibles par le reste du chemin.
    temp_tables: HashMap<String, TableSchema>,
    path_tables: HashMap<String, TableSchema>,
    path_functions: HashMap<String, FunctionSignature>,
    rls: HashSet<String>,
    /// Vue du catalogue intégré, calculée une fois par processus.
    base: &'static BaseView,
}

/// Objets intégrés (`pg_catalog`, `information_schema`).
struct BaseView {
    qualified_tables: HashMap<String, TableSchema>,
    qualified_functions: HashMap<String, FunctionSignature>,
    /// `pg_catalog`, consulté pour un nom simple.
    tables: HashMap<String, TableSchema>,
    functions: HashMap<String, FunctionSignature>,
}

fn is_builtin_schema(s: &str) -> bool {
    s == "pg_catalog" || s == "information_schema"
}

fn base_view() -> &'static BaseView {
    static BASE: std::sync::OnceLock<BaseView> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let cat = Catalog::for_postgres(18).expect("pack postgres@18 livré avec pgweave");
        let mut v = BaseView {
            qualified_tables: HashMap::new(),
            qualified_functions: HashMap::new(),
            tables: HashMap::new(),
            functions: HashMap::new(),
        };
        for r in cat
            .relations
            .values()
            .filter(|r| is_builtin_schema(&r.name.schema))
        {
            let t = table(r);
            if r.name.schema == "pg_catalog" {
                v.tables.insert(r.name.name.clone(), t.clone());
            }
            v.qualified_tables
                .insert(format!("{}.{}", r.name.schema, r.name.name), t);
        }
        for (q, overloads) in cat
            .routines
            .iter()
            .filter(|(q, _)| is_builtin_schema(&q.schema))
        {
            if let Some(r) = overloads.first() {
                if q.schema == "pg_catalog" {
                    v.functions.insert(q.name.clone(), signature(q, r));
                }
                v.qualified_functions
                    .insert(format!("{}.{}", q.schema, q.name), signature(q, r));
            }
        }
        v
    })
}

fn table(r: &crate::model::Relation) -> TableSchema {
    TableSchema {
        name: r.name.name.clone(),
        columns: r
            .columns
            .iter()
            .map(|c| ColumnInfo {
                name: c.name.clone(),
                data_type: data_type(&c.ty),
                nullable: !c.not_null,
            })
            .collect(),
    }
}

pub fn data_type(t: &TypeRef) -> DataType {
    match t {
        TypeRef::Array(inner) => DataType::Array(Box::new(data_type(inner))),
        TypeRef::Named { name, modifiers } => {
            let num = |i: usize| modifiers.get(i).and_then(|m| m.parse::<u64>().ok());
            if name.schema == "pg_catalog" {
                let dt = match name.name.as_str() {
                    "int2" => Some(DataType::SmallInt),
                    "int4" => Some(DataType::Integer),
                    "int8" => Some(DataType::BigInt),
                    "numeric" => Some(DataType::Numeric {
                        precision: num(0),
                        scale: num(1),
                    }),
                    "float4" => Some(DataType::Real),
                    "float8" => Some(DataType::DoublePrecision),
                    "text" => Some(DataType::Text),
                    "varchar" => Some(DataType::Varchar(num(0))),
                    "bpchar" => Some(DataType::Char(num(0))),
                    "date" => Some(DataType::Date),
                    "time" => Some(DataType::Time(num(0))),
                    "timetz" => Some(DataType::TimeTz(num(0))),
                    "timestamp" => Some(DataType::Timestamp(num(0))),
                    "timestamptz" => Some(DataType::TimestampTz(num(0))),
                    "interval" => Some(DataType::Interval),
                    "bool" => Some(DataType::Boolean),
                    "bytea" => Some(DataType::Bytea),
                    "json" => Some(DataType::Json),
                    "jsonb" => Some(DataType::Jsonb),
                    "uuid" => Some(DataType::Uuid),
                    _ => None,
                };
                if let Some(dt) = dt {
                    return dt;
                }
            }
            let n = if name.schema == "pg_catalog" || name.schema == "public" {
                name.name.clone()
            } else {
                format!("{}.{}", name.schema, name.name)
            };
            if modifiers.is_empty() {
                DataType::Custom(n)
            } else {
                DataType::Modified {
                    name: n,
                    modifiers: modifiers.clone(),
                }
            }
        }
        TypeRef::Unresolved(s) => DataType::Custom(s.clone()),
        TypeRef::Unknown => DataType::Custom("unknown".into()),
    }
}

impl CatalogSchema {
    pub fn new(cat: &Catalog, path: &SearchPath) -> Self {
        let mut v = CatalogSchema {
            tables: HashMap::new(),
            functions: HashMap::new(),
            temp_tables: HashMap::new(),
            path_tables: HashMap::new(),
            path_functions: HashMap::new(),
            rls: HashSet::new(),
            base: base_view(),
        };
        let schemas: Vec<&str> = path
            .lookup_order()
            .into_iter()
            .filter(|s| !is_builtin_schema(s))
            .collect();
        // Rang d'un schéma dans le chemin : plus petit = prioritaire.
        let rank = |s: &str| schemas.iter().position(|x| *x == s);
        let mut best_table: HashMap<String, usize> = HashMap::new();
        for r in cat
            .relations
            .values()
            .filter(|r| !is_builtin_schema(&r.name.schema))
        {
            let t = table(r);
            let qualified = format!("{}.{}", r.name.schema, r.name.name);
            if r.row_level_security {
                v.rls.insert(qualified.clone());
            }
            if r.name.schema == "pg_temp" {
                v.temp_tables.insert(r.name.name.clone(), t.clone());
            } else if let Some(k) = rank(&r.name.schema)
                && best_table.get(&r.name.name).is_none_or(|b| k < *b)
            {
                best_table.insert(r.name.name.clone(), k);
                if r.row_level_security {
                    v.rls.insert(r.name.name.clone());
                }
                v.path_tables.insert(r.name.name.clone(), t.clone());
            }
            v.tables.insert(qualified, t);
        }
        let mut best_fn: HashMap<String, usize> = HashMap::new();
        for (q, overloads) in cat
            .routines
            .iter()
            .filter(|(q, _)| !is_builtin_schema(&q.schema))
        {
            let Some(r) = overloads.first() else {
                continue;
            };
            if let Some(k) = rank(&q.schema)
                && best_fn.get(&q.name).is_none_or(|b| k < *b)
            {
                best_fn.insert(q.name.clone(), k);
                v.path_functions.insert(q.name.clone(), signature(q, r));
            }
            v.functions
                .insert(format!("{}.{}", q.schema, q.name), signature(q, r));
        }
        v
    }
}

fn signature(q: &QName, r: &crate::model::Routine) -> FunctionSignature {
    FunctionSignature {
        name: q.name.clone(),
        params: r.input_types().map(data_type).collect(),
        return_type: r
            .returns
            .as_ref()
            .map(data_type)
            .unwrap_or_else(|| DataType::Custom("void".into())),
    }
}

impl SchemaProvider for CatalogSchema {
    /// Nom qualifié : exact. Nom simple : table temporaire, puis
    /// `pg_catalog`, puis le reste du `search_path` — l'ordre de PostgreSQL.
    fn get_table(&self, name: &str) -> Option<&TableSchema> {
        if name.contains('.') {
            return self
                .tables
                .get(name)
                .or_else(|| self.base.qualified_tables.get(name));
        }
        self.temp_tables
            .get(name)
            .or_else(|| self.base.tables.get(name))
            .or_else(|| self.path_tables.get(name))
    }

    fn get_function(&self, name: &str) -> Option<&FunctionSignature> {
        if name.contains('.') {
            return self
                .functions
                .get(name)
                .or_else(|| self.base.qualified_functions.get(name));
        }
        self.base
            .functions
            .get(name)
            .or_else(|| self.path_functions.get(name))
    }

    fn all_table_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self
            .temp_tables
            .keys()
            .chain(self.path_tables.keys())
            .chain(self.tables.keys())
            .map(String::as_str)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn as_rls_aware(&self) -> Option<&dyn RlsAwareSchemaProvider> {
        Some(self)
    }
}

impl RlsAwareSchemaProvider for CatalogSchema {
    fn table_has_rls(&self, table_name: &str) -> bool {
        self.rls.contains(table_name)
    }
}
