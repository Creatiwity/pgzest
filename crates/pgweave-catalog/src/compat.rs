//! Transitional bridge: the catalog seen as a [`SchemaProvider`], for
//! consumers that have not migrated yet (checker, LSP, CLI). Goes away with
//! `SchemaProvider` (PLAN.md, P2).
//!
//! Names are exact (principle 8): a qualified name designates `schema.name`,
//! a simple name the object visible through the `search_path`.

use std::collections::{HashMap, HashSet};

use pgweave_ast::types::DataType;
use pgweave_schema::provider::{
    ColumnInfo, FunctionSignature, RlsAwareSchemaProvider, SchemaProvider, TableSchema,
};

use crate::catalog::{Catalog, SearchPath};
use crate::model::{QName, TypeRef};

pub struct CatalogSchema {
    /// Project objects (excluding `pg_catalog` and `information_schema`), by
    /// qualified name.
    tables: HashMap<String, TableSchema>,
    functions: HashMap<String, FunctionSignature>,
    /// Simple project names: temporary tables (looked up before
    /// `pg_catalog`), then objects visible through the rest of the path.
    temp_tables: HashMap<String, TableSchema>,
    path_tables: HashMap<String, TableSchema>,
    path_functions: HashMap<String, FunctionSignature>,
    rls: HashSet<String>,
    /// View of the built-in catalog, computed once per process.
    base: &'static BaseView,
}

/// Built-in objects (`pg_catalog`, `information_schema`).
struct BaseView {
    qualified_tables: HashMap<String, TableSchema>,
    qualified_functions: HashMap<String, FunctionSignature>,
    /// `pg_catalog`, looked up for a simple name.
    tables: HashMap<String, TableSchema>,
    functions: HashMap<String, FunctionSignature>,
}

fn is_builtin_schema(s: &str) -> bool {
    s == "pg_catalog" || s == "information_schema"
}

fn base_view() -> &'static BaseView {
    static BASE: std::sync::OnceLock<BaseView> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let cat = Catalog::for_postgres(18).expect("postgres@18 pack shipped with pgweave");
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
        // Rank of a schema in the path: smaller = higher priority.
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
    /// Qualified name: exact. Simple name: temporary table, then
    /// `pg_catalog`, then the rest of the `search_path` — PostgreSQL's order.
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
