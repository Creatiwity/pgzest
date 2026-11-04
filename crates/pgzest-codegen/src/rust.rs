//! Rust code for named queries, from their descriptors (PLAN.md §5, P5):
//! one function per query, matching its cardinality, a parameter struct
//! and a row struct, executed by `pgzest-runtime`.
//!
//! Type mapping: table below, extensible per project
//! ([`RustOptions::overrides`], D9). A type without a mapping is a
//! refusal, never a fallback.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pgzest_catalog::ProjectCatalog;
use pgzest_catalog::model::TypeKind;
use pgzest_query::Descriptor;
use pgzest_query::descriptor::NullabilityDesc;
pub use pgzest_query::{QueryModule, resolve_row};
use pgzest_sema::{Ty, TypeSystem};

/// Date and time crate used by the project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Temporal {
    #[default]
    Time,
    Chrono,
}

#[derive(Debug, Clone)]
pub struct RustOptions {
    /// Path of the runtime in the user crate.
    pub runtime: String,
    pub temporal: Temporal,
    /// Rust type for a PostgreSQL type (`"public.geometry"` →
    /// `"geo_types::Geometry"`), taking precedence over the built-in table.
    pub overrides: BTreeMap<String, String>,
    /// Module holding the project's enums (`[rust] types`, `crate::db`),
    /// generated separately ([`generate_enums`]): query code refers to it
    /// instead of defining its own.
    pub types_path: Option<String>,
}

impl Default for RustOptions {
    fn default() -> Self {
        RustOptions {
            runtime: "pgzest_runtime".into(),
            temporal: Temporal::default(),
            overrides: BTreeMap::new(),
            types_path: None,
        }
    }
}

/// Rust type of a value: owned (column) and borrowed (parameter).
#[derive(Debug, Clone)]
struct RustTy {
    owned: String,
    borrowed: String,
    /// The borrowed form carries the `'a` lifetime.
    lifetime: bool,
}

impl RustTy {
    fn copy(t: &str) -> Self {
        RustTy {
            owned: t.into(),
            borrowed: t.into(),
            lifetime: false,
        }
    }
}

struct Generator<'c> {
    ts: TypeSystem<'c>,
    opts: &'c RustOptions,
    /// Enums used: qualified name → (Rust type, PostgreSQL name, labels).
    enums: BTreeMap<Ty, (String, String, Vec<String>)>,
}

/// Rust module for the `descriptors` queries, all at the root, typed against
/// `catalog`. Errors say which query cannot be translated, and why.
pub fn generate_rust(
    descriptors: &[Descriptor],
    catalog: &ProjectCatalog,
    opts: &RustOptions,
) -> Result<String, Vec<String>> {
    generate_rust_modules(
        &[QueryModule {
            source: None,
            path: Vec::new(),
            descriptors: descriptors.to_vec(),
        }],
        catalog,
        opts,
    )
}

/// Module segment: snake_case, keyword as a raw identifier; `self`,
/// `super`, `crate` rejected (no raw form).
fn module_ident(segment: &str) -> Result<String, String> {
    let snake = segment
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && segment
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !snake || matches!(segment, "self" | "super" | "crate") {
        return Err(format!(
            "`{segment}` is not a valid Rust module name (snake_case, not self, super or crate)"
        ));
    }
    Ok(if KEYWORDS.contains(&segment) {
        format!("r#{segment}")
    } else {
        segment.to_string()
    })
}

/// Module tree of the generated code: one `pub mod` per directory and per
/// query file. Enums are at the root; each module imports its parent
/// (`use super::*`) to see them. A row reused from another module
/// (`-> a::b::name`) is referred to by a relative path.
pub fn generate_rust_modules(
    modules: &[QueryModule],
    catalog: &ProjectCatalog,
    opts: &RustOptions,
) -> Result<String, Vec<String>> {
    let mut g = Generator {
        ts: TypeSystem::new(&catalog.catalog),
        opts,
        enums: BTreeMap::new(),
    };
    let mut errors = Vec::new();
    let mut bodies: BTreeMap<Vec<String>, String> = BTreeMap::new();
    for m in modules {
        let where_ = if m.path.is_empty() {
            String::new()
        } else {
            format!("{}::", m.path.join("::"))
        };
        let mut idents = Vec::new();
        for s in &m.path {
            match module_ident(s) {
                Ok(i) => idents.push(i),
                Err(e) => errors.push(format!("{where_}: {e}")),
            }
        }
        let body = bodies.entry(m.path.clone()).or_default();
        for d in &m.descriptors {
            let row = match &d.row {
                None => None,
                Some(r) => {
                    let target = resolve_row(&m.path, r, modules);
                    if let Err(e) = pgzest_query::check_row_reference(d, target.map(|(_, t)| t)) {
                        errors.push(format!("{where_}{}: {e}", d.name));
                        continue;
                    }
                    let (path, t) = target.expect("checked");
                    let mut ty = "super::".repeat(m.path.len());
                    if path == m.path.as_slice() {
                        ty.clear();
                    } else {
                        for s in path {
                            ty.push_str(&module_ident(s).unwrap_or_default());
                            ty.push_str("::");
                        }
                    }
                    ty.push_str(&format!("{}Row", pascal(&t.name)));
                    Some(ty)
                }
            };
            match g.query(d, row) {
                Ok(code) => body.push_str(&with_source(code, m.source.as_ref(), d)),
                Err(e) => errors.push(format!("{where_}{}: {e}", d.name)),
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let body = render_tree(&bodies, &[]);
    let mut out = String::from("// Generated by pgzest from query descriptors. Do not edit.\n\n");
    let mut names = BTreeMap::new();
    for (ty, (rust, typname, labels)) in &g.enums {
        if let Some(other) = names.insert(rust.clone(), ty.clone()) {
            return Err(vec![format!(
                "enums {other} and {ty} map to the same Rust type `{rust}`"
            )]);
        }
        out.push_str(&enum_code(&g.opts.runtime, rust, typname, labels)?);
    }
    out.push_str(&body);
    Ok(out)
}

/// `code` (that of one query), with each function and the row struct
/// preceded by a link to the query header in its SQL file: a macro can
/// only locate its code in the Rust file that calls it, so the link leads
/// to the SQL (hover, or ⌘-click on the link).
fn with_source(code: String, source: Option<&pgzest_query::QuerySource>, d: &Descriptor) -> String {
    let Some(src) = source else {
        return code;
    };
    let Some(line) = src.lines.get(&d.name) else {
        return code;
    };
    // Attribute rather than `///`: a doc comment reparsed by
    // rust-analyzer's macro server keeps an extra `/`.
    let doc = format!(
        "#[doc = {:?}]\n",
        format!(
            " Query `{}`: [{}:{line}](file://{}#L{line})",
            d.name,
            src.label,
            src.path.display()
        )
    );
    let row = format!("pub struct {}Row ", pascal(&d.name));
    let mut out = String::with_capacity(code.len() + 4 * doc.len());
    for l in code.split_inclusive('\n') {
        if l.starts_with("pub async fn ") || l.starts_with(&row) {
            out.push_str(&doc);
        }
        out.push_str(l);
    }
    out
}

/// Code for a single query (`sql!`), its row possibly reused from a Rust
/// type (`row`, path). A used enum requires `types_path`: two queries in
/// the same module cannot each define their own.
pub fn generate_query(
    d: &Descriptor,
    catalog: &ProjectCatalog,
    opts: &RustOptions,
    row: Option<String>,
) -> Result<String, String> {
    let mut g = Generator {
        ts: TypeSystem::new(&catalog.catalog),
        opts,
        enums: BTreeMap::new(),
    };
    let code = g.query(d, row)?;
    if let Some(ty) = g.enums.keys().next() {
        return Err(format!(
            "the query uses enum {ty}: declare where the project's enums live \
             (`[rust] types = \"crate::db\"` in pgzest.toml) and call \
             `pgzest_macros::types!();` there"
        ));
    }
    Ok(code)
}

/// Enums for all enumerated types of the project (`types!`), excluding the
/// system catalog.
pub fn generate_enums(catalog: &ProjectCatalog, opts: &RustOptions) -> Result<String, Vec<String>> {
    let mut out = String::new();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for t in catalog.catalog.types.values() {
        let TypeKind::Enum { labels } = &t.kind else {
            continue;
        };
        if t.name.schema == "pg_catalog" || t.name.schema == "information_schema" {
            continue;
        }
        let rust = pascal(&t.name.name);
        if let Some(other) = names.insert(rust.clone(), t.name.to_string()) {
            return Err(vec![format!(
                "enums {other} and {} map to the same Rust type `{rust}`",
                t.name
            )]);
        }
        out.push_str(&enum_code(&opts.runtime, &rust, &t.name.name, labels)?);
    }
    Ok(out)
}

/// Code of module `at` and its submodules.
fn render_tree(bodies: &BTreeMap<Vec<String>, String>, at: &[String]) -> String {
    let mut out = bodies.get(at).cloned().unwrap_or_default();
    let children: std::collections::BTreeSet<&String> = bodies
        .keys()
        .filter(|p| p.len() > at.len() && p.starts_with(at))
        .map(|p| &p[at.len()])
        .collect();
    for child in children {
        let mut path = at.to_vec();
        path.push(child.clone());
        let inner = render_tree(bodies, &path);
        let _ = writeln!(
            out,
            "pub mod {} {{\n#[allow(unused_imports)]\nuse super::*;\n\n{inner}}}\n",
            module_ident(child).unwrap_or_default()
        );
    }
    out
}

impl Generator<'_> {
    /// `row`: row type reused from another query (`-> other`).
    fn query(&mut self, d: &Descriptor, row: Option<String>) -> Result<String, String> {
        if let Some(copy) = &d.copy {
            return self.copy(d, copy);
        }
        let rt = self.opts.runtime.clone();
        let fn_name = rust_ident(&d.name)?;
        let pascal = pascal(&d.name);
        let const_name = d.name.to_uppercase();
        let mut o = String::new();

        // OID of each parameter; a domain is passed as its base type (the
        // server converts implicitly), a type without a fixed OID lets the
        // server type the query.
        let mut oids = Some(Vec::new());
        for p in &d.params {
            let base = self.ts.base_type(&qname(&p.ty.pg));
            match (self.ts.def(&base).and_then(|t| t.oid), oids.as_mut()) {
                (Some(oid), Some(v)) => v.push(oid),
                _ => oids = None,
            }
        }
        let oids = match oids {
            Some(v) => format!(
                "Some(&[{}])",
                v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
            ),
            None => "None".into(),
        };
        let _ = writeln!(o, "/// `{}`", d.sql.replace('\n', " "));
        let _ = writeln!(
            o,
            "pub const {const_name}: {rt}::Query = {rt}::Query {{ name: {:?}, sql: {:?}, param_oids: {oids} }};\n",
            d.name, d.sql
        );
        if let Some(dy) = &d.dynamic {
            for (i, v) in dy.variants.iter().enumerate() {
                let mut oids = Some(Vec::new());
                for name in &v.params {
                    let p = d.params.iter().find(|p| &p.name == name).ok_or_else(|| {
                        format!("variant {i}: parameter ${name} unknown to the descriptor")
                    })?;
                    let base = self.ts.base_type(&qname(&p.ty.pg));
                    match (self.ts.def(&base).and_then(|t| t.oid), oids.as_mut()) {
                        (Some(oid), Some(o)) => o.push(oid),
                        _ => oids = None,
                    }
                }
                let oids = match oids {
                    Some(v) => format!(
                        "Some(&[{}])",
                        v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                    ),
                    None => "None".into(),
                };
                let _ = writeln!(
                    o,
                    "#[doc(hidden)]\npub const {const_name}_V{i}: {rt}::Query = {rt}::Query {{ name: {:?}, sql: {:?}, param_oids: {oids} }};",
                    d.name, v.sql
                );
            }
            let _ = writeln!(o);
        }

        // Parameters.
        let mut params = Vec::new();
        let mut lifetime = false;
        for p in &d.params {
            let t = self.rust_type(&p.ty.pg)?;
            lifetime |= t.lifetime;
            let ty = match p.nullable {
                // A parameter whose nullability is not established is
                // expected non-null: NULL only makes sense where the query
                // explicitly accepts it (nullable column).
                NullabilityDesc::Nullable => format!("Option<{}>", t.borrowed),
                NullabilityDesc::NonNull | NullabilityDesc::Unknown => t.borrowed,
            };
            params.push((field_ident(&p.name)?, ty));
        }
        // Query with variants: one enum per choice, as a struct field.
        let choice_types: Vec<(String, String, Vec<String>)> = d
            .dynamic
            .iter()
            .flat_map(|dy| &dy.choices)
            .map(|c| {
                let ty = format!("{pascal}{}", self::pascal(&c.name));
                let variants = c.labels.iter().map(|l| self::pascal(l)).collect();
                (c.name.clone(), ty, variants)
            })
            .collect();
        for (name, ty, variants) in &choice_types {
            let _ = writeln!(
                o,
                "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum {ty} {{\n{}}}\n",
                variants
                    .iter()
                    .map(|v| format!("    {v},\n"))
                    .collect::<String>()
            );
            params.push((field_ident(name)?, ty.clone()));
        }
        let params_ty = if params.is_empty() {
            None
        } else {
            let name = format!("{pascal}Params");
            let lt = if lifetime { "<'a>" } else { "" };
            let _ = writeln!(o, "#[derive(Debug, Clone)]\npub struct {name}{lt} {{");
            for (f, t) in &params {
                let _ = writeln!(o, "    pub {f}: {t},");
            }
            let _ = writeln!(o, "}}\n");
            Some(format!("{name}{}", if lifetime { "<'_>" } else { "" }))
        };

        // Row: that of another query (`-> other`, checked), or its own.
        let row_ty = if let Some(r) = row {
            Some(r)
        } else if d.columns.is_empty() {
            None
        } else {
            let name = format!("{pascal}Row");
            let mut seen = std::collections::BTreeSet::new();
            let mut fields = Vec::new();
            for c in &d.columns {
                let f = field_ident(&c.name)
                    .map_err(|e| format!("{e} (output column: give it an alias)"))?;
                if !seen.insert(f.clone()) {
                    return Err(format!(
                        "two output columns are named `{}`: give them distinct aliases",
                        c.name
                    ));
                }
                let t = self.rust_type(&c.ty.pg)?;
                let ty = match c.nullable {
                    NullabilityDesc::NonNull => t.owned,
                    NullabilityDesc::Nullable | NullabilityDesc::Unknown => {
                        format!("Option<{}>", t.owned)
                    }
                };
                fields.push((f, ty));
            }
            let _ = writeln!(
                o,
                "#[derive(Debug, Clone, PartialEq)]\npub struct {name} {{"
            );
            for (f, t) in &fields {
                let _ = writeln!(o, "    pub {f}: {t},");
            }
            let _ = writeln!(o, "}}\n");
            let _ = writeln!(o, "impl {name} {{");
            // Columns (name, PostgreSQL type, non-null): a query that reuses
            // this row (`-> Path`) is compared against it at compile
            // time.
            let _ = writeln!(
                o,
                "    pub const PGZEST_COLUMNS: &'static [(&'static str, &'static str, bool)] = &[{}];",
                d.columns
                    .iter()
                    .map(|c| format!(
                        "({:?}, {:?}, {})",
                        c.name,
                        c.ty.pg,
                        c.nullable == NullabilityDesc::NonNull
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let _ = writeln!(
                o,
                "    pub(crate) fn from_row(row: &{rt}::Row) -> Result<Self, {rt}::tokio_postgres::Error> {{"
            );
            let _ = writeln!(o, "        Ok(Self {{");
            for (i, (f, _)) in fields.iter().enumerate() {
                let _ = writeln!(o, "            {f}: row.try_get({i})?,");
            }
            let _ = writeln!(o, "        }})\n    }}\n}}\n");
            Some(name)
        };

        // Function.
        let args = match &params_ty {
            Some(t) => format!(", params: {t}"),
            None => String::new(),
        };
        // A domain goes through `Domain`: the server may announce it as such.
        let domains: Vec<bool> = d.params.iter().map(|p| self.is_domain(&p.ty.pg)).collect();
        let bound = match &params_ty {
            Some(_) => format!(
                "&[{}]",
                params
                    .iter()
                    .zip(&domains)
                    .map(|((f, _), dom)| if *dom {
                        format!("&{rt}::Domain(params.{f})")
                    } else {
                        format!("&params.{f}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => "&[]".into(),
        };
        // Query with variants: the variant (checked at compile time) and
        // its values, chosen by an exhaustive `match` on the provided
        // blocks and the choices.
        let (prelude, qref, bound) = match &d.dynamic {
            None => (String::new(), format!("&{const_name}"), bound),
            Some(dy) => self.variant_match(d, dy, &const_name, &choice_types)?,
        };
        use pgzest_query::Cardinality;
        let (ret, call) = match (d.cardinality, &row_ty) {
            (Cardinality::None, _) | (_, None) => (
                "u64".to_string(),
                format!("{rt}::execute(client, {qref}, {bound})"),
            ),
            (Cardinality::One, Some(r)) => (
                r.clone(),
                format!("{rt}::fetch_one(client, {qref}, {bound}, {r}::from_row)"),
            ),
            (Cardinality::Optional, Some(r)) => (
                format!("Option<{r}>"),
                format!("{rt}::fetch_optional(client, {qref}, {bound}, {r}::from_row)"),
            ),
            (Cardinality::Many, Some(r)) => (
                format!("Vec<{r}>"),
                format!("{rt}::fetch_all(client, {qref}, {bound}, {r}::from_row)"),
            ),
        };
        let _ = writeln!(
            o,
            "pub async fn {fn_name}<C: {rt}::GenericClient + ?Sized>(client: &C{args}) -> Result<{ret}, {rt}::Error> {{\n{prelude}    {call}.await\n}}\n"
        );
        // Several rows: also as a stream, decoded as they arrive.
        if let (Cardinality::Many, Some(r)) = (d.cardinality, &row_ty) {
            let _ = writeln!(
                o,
                "/// Like [`{fn_name}`], with rows decoded as they arrive.\npub async fn {fn_name}_stream<C: {rt}::GenericClient + ?Sized>(client: &C{args}) -> Result<impl {rt}::futures_util::Stream<Item = Result<{r}, {rt}::Error>> + Send + use<C>, {rt}::Error> {{\n{prelude}    {rt}::stream(client, {qref}, {bound}, {r}::from_row).await\n}}\n"
            );
        }
        Ok(o)
    }

    /// Prelude of a function with variants: `let (q, binds) = match …;`,
    /// then the query and the values to pass.
    fn variant_match(
        &self,
        d: &Descriptor,
        dy: &pgzest_query::descriptor::DynamicDesc,
        const_name: &str,
        choices: &[(String, String, Vec<String>)],
    ) -> Result<(String, String, String), String> {
        let rt = &self.opts.runtime;
        let mut prelude = String::new();
        // Domains: values prepared outside the `match` (no temporary).
        let mut value = std::collections::BTreeMap::new();
        for p in &d.params {
            let f = field_ident(&p.name)?;
            let cond = dy.conditions.contains(&p.name);
            let dom = self.is_domain(&p.ty.pg);
            let expr = match (cond, dom) {
                (false, false) => format!("&params.{f}"),
                (false, true) => {
                    let _ = writeln!(prelude, "    let __{f} = {rt}::Domain(params.{f});");
                    format!("&__{f}")
                }
                // Bound by the arm's `Some(__c_f)` pattern.
                (true, _) => format!("__c_{f}"),
            };
            if cond && dom {
                let _ = writeln!(prelude, "    let __{f} = params.{f}.map({rt}::Domain);");
            }
            value.insert(p.name.clone(), (expr, cond, dom, f));
        }
        let mut distinct: Vec<&String> = Vec::new();
        for c in &dy.conditions {
            if !distinct.contains(&c) {
                distinct.push(c);
            }
        }
        let mut scrutinee: Vec<String> = distinct
            .iter()
            .map(|c| {
                let (_, _, dom, f) = &value[*c];
                if *dom {
                    format!("__{f}.as_ref()")
                } else {
                    format!("params.{f}.as_ref()")
                }
            })
            .collect();
        for (name, _, _) in choices {
            scrutinee.push(format!("params.{}", field_ident(name)?));
        }
        let n_blocks = dy.conditions.len();
        let mut arms = String::new();
        for (i, v) in dy.variants.iter().enumerate() {
            // Index → combination (see `pgzest_query::dynamic::selections`).
            let bits: Vec<bool> = (0..n_blocks).map(|k| i >> k & 1 == 1).collect();
            let mut rest = i >> n_blocks;
            let mut patterns = Vec::new();
            let mut consistent = true;
            for c in &distinct {
                let states: Vec<bool> = dy
                    .conditions
                    .iter()
                    .zip(&bits)
                    .filter(|(n, _)| n == c)
                    .map(|(_, b)| *b)
                    .collect();
                // One parameter controlling several blocks: they go together.
                if states.iter().any(|b| *b != states[0]) {
                    consistent = false;
                }
                let (_, _, _, f) = &value[*c];
                patterns.push(if states[0] {
                    format!("Some(__c_{f})")
                } else {
                    "None".into()
                });
            }
            for (_, ty, variants) in choices {
                patterns.push(format!("{ty}::{}", variants[rest % variants.len()]));
                rest /= variants.len();
            }
            if !consistent {
                continue;
            }
            let binds: Vec<&str> = v.params.iter().map(|n| value[n].0.as_str()).collect();
            let _ = writeln!(
                arms,
                "        ({}) => (&{const_name}_V{i}, vec![{}]),",
                patterns.join(", "),
                binds.join(", ")
            );
        }
        let _ = writeln!(
            prelude,
            "    let (q, binds): (&'static {rt}::Query, Vec<{rt}::Param<'_>>) = match ({}) {{\n{arms}    }};",
            scrutinee.join(", ")
        );
        Ok((prelude, "q".into(), "&binds".into()))
    }

    /// `COPY … FROM STDIN`: typed records (binary) or byte stream (text,
    /// CSV).
    fn copy(
        &mut self,
        d: &Descriptor,
        copy: &pgzest_query::descriptor::CopyDesc,
    ) -> Result<String, String> {
        let rt = self.opts.runtime.clone();
        let fn_name = rust_ident(&d.name)?;
        let const_name = d.name.to_uppercase();
        let mut oids = Some(Vec::new());
        for c in &d.columns {
            let base = self.ts.base_type(&qname(&c.ty.pg));
            match (self.ts.def(&base).and_then(|t| t.oid), oids.as_mut()) {
                (Some(oid), Some(v)) => v.push(oid),
                _ => oids = None,
            }
        }
        let oids = match oids {
            Some(v) => format!(
                "Some(&[{}])",
                v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
            ),
            None => "None".into(),
        };
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let table = qname(&copy.table);
        let table = format!("{}.{}", quote(&table.schema), quote(&table.name));
        let columns = d
            .columns
            .iter()
            .map(|c| quote(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut o = String::new();
        let _ = writeln!(o, "/// `{}`", d.sql.replace('\n', " "));
        let _ = writeln!(
            o,
            "pub const {const_name}: {rt}::CopyIn = {rt}::CopyIn {{ name: {:?}, sql: {:?}, table: {table:?}, columns: {columns:?}, column_oids: {oids} }};\n",
            d.name, d.sql
        );
        if copy.format != "binary" {
            let _ = writeln!(
                o,
                "/// Data in {} format read from `reader`, as is; returns the number of rows copied.\npub async fn {fn_name}<C: {rt}::CopyClient + ?Sized, R: {rt}::tokio::io::AsyncRead + Unpin>(client: &C, reader: R) -> Result<u64, {rt}::Error> {{\n    {rt}::copy_in_stream(client, &{const_name}, reader).await\n}}\n",
                copy.format
            );
            return Ok(o);
        }
        let record = format!("{}Record", pascal(&d.name));
        let mut fields = Vec::new();
        let mut lifetime = false;
        for c in &d.columns {
            let t = self.rust_type(&c.ty.pg)?;
            lifetime |= t.lifetime;
            let ty = match c.nullable {
                NullabilityDesc::NonNull => t.borrowed,
                _ => format!("Option<{}>", t.borrowed),
            };
            fields.push((field_ident(&c.name)?, ty));
        }
        let lt = if lifetime { "<'a>" } else { "" };
        let _ = writeln!(o, "#[derive(Debug, Clone)]\npub struct {record}{lt} {{");
        for (f, t) in &fields {
            let _ = writeln!(o, "    pub {f}: {t},");
        }
        let _ = writeln!(o, "}}\n");
        let item = if lifetime {
            format!("{record}<'a>")
        } else {
            record.clone()
        };
        let values = fields
            .iter()
            .map(|(f, _)| format!("&r.{f}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            o,
            "/// Binary copy of the records; returns the number of rows copied.\npub async fn {fn_name}<'a, C: {rt}::CopyClient + ?Sized>(client: &C, records: impl IntoIterator<Item = {item}>) -> Result<u64, {rt}::Error> {{\n    let mut copy = {rt}::BinaryCopy::open(client, &{const_name}).await?;\n    for r in records {{\n        copy.write(&[{values}]).await?;\n    }}\n    copy.finish().await\n}}\n"
        );
        Ok(o)
    }

    /// The type (or array element) is a domain.
    fn is_domain(&self, qualified: &str) -> bool {
        matches!(
            self.ts.def(&qname(qualified)).map(|d| &d.kind),
            Some(TypeKind::Domain { .. })
        )
    }

    /// Extension that provides this type, if any.
    fn extension_of(&self, ty: &Ty) -> Option<String> {
        let cat = self.ts.catalog;
        cat.extension_packs.iter().find_map(|(ext, pack)| {
            let schema = &cat.extensions.get(ext)?.schema;
            (*schema == ty.schema && pack.types.iter().any(|t| t.name.name == ty.name))
                .then(|| ext.clone())
        })
    }

    /// Rust type of a PostgreSQL type (qualified name from the descriptor).
    fn rust_type(&mut self, qualified: &str) -> Result<RustTy, String> {
        if let Some(t) = self.opts.overrides.get(qualified) {
            return Ok(RustTy::copy(t));
        }
        let ty = qname(qualified);
        let def = self.ts.def(&ty);
        match def.map(|d| &d.kind) {
            Some(TypeKind::Domain { .. }) => {
                let base = self.ts.base_type(&ty);
                return self.rust_type(&base.to_string());
            }
            Some(TypeKind::Enum { labels }) => {
                let rust = pascal(&ty.name);
                if let Some(path) = &self.opts.types_path {
                    return Ok(RustTy::copy(&format!("{path}::{rust}")));
                }
                self.enums
                    .insert(ty.clone(), (rust.clone(), ty.name.clone(), labels.clone()));
                return Ok(RustTy::copy(&rust));
            }
            Some(TypeKind::Array { element }) => {
                let e = self.rust_type(&element.to_string())?;
                return Ok(RustTy {
                    // An array element may be NULL.
                    owned: format!("Vec<Option<{}>>", e.owned),
                    borrowed: format!("&'a [{}]", e.owned),
                    lifetime: true,
                });
            }
            _ => {}
        }
        if ty.schema != "pg_catalog" {
            // Extension type: mapping by extension and name, whatever
            // schema it is installed in (D9).
            return match (self.extension_of(&ty).as_deref(), ty.name.as_str()) {
                (Some("citext"), "citext") => Ok(RustTy {
                    owned: "String".into(),
                    borrowed: "&'a str".into(),
                    lifetime: true,
                }),
                (Some(ext), _) => Err(format!(
                    "no Rust type for {qualified} (extension {ext}): declare it in the project's type mapping table"
                )),
                (None, _) => Err(format!(
                    "no Rust type for {qualified}: declare it in the project's type mapping table"
                )),
            };
        }
        let time = self.opts.temporal == Temporal::Time;
        Ok(match ty.name.as_str() {
            "bool" => RustTy::copy("bool"),
            "int2" => RustTy::copy("i16"),
            "int4" => RustTy::copy("i32"),
            "int8" => RustTy::copy("i64"),
            "float4" => RustTy::copy("f32"),
            "float8" => RustTy::copy("f64"),
            "oid" => RustTy::copy("u32"),
            "char" => RustTy::copy("i8"),
            "text" | "varchar" | "bpchar" | "name" => RustTy {
                owned: "String".into(),
                borrowed: "&'a str".into(),
                lifetime: true,
            },
            "bytea" => RustTy {
                owned: "Vec<u8>".into(),
                borrowed: "&'a [u8]".into(),
                lifetime: true,
            },
            "uuid" => RustTy::copy("uuid::Uuid"),
            "json" | "jsonb" => RustTy {
                owned: "serde_json::Value".into(),
                borrowed: "&'a serde_json::Value".into(),
                lifetime: true,
            },
            "numeric" => RustTy::copy("rust_decimal::Decimal"),
            "date" if time => RustTy::copy("time::Date"),
            "time" if time => RustTy::copy("time::Time"),
            "timestamp" if time => RustTy::copy("time::PrimitiveDateTime"),
            "timestamptz" if time => RustTy::copy("time::OffsetDateTime"),
            "date" => RustTy::copy("chrono::NaiveDate"),
            "time" => RustTy::copy("chrono::NaiveTime"),
            "timestamp" => RustTy::copy("chrono::NaiveDateTime"),
            "timestamptz" => RustTy::copy("chrono::DateTime<chrono::Utc>"),
            _ => {
                return Err(format!(
                    "no Rust type for {qualified}: declare it in the project's type mapping table"
                ));
            }
        })
    }
}

/// Parts of an inline query (`query!`), to be placed in a block.
pub struct InlineQuery {
    /// `const QUERY: Query = …;`
    pub query_const: String,
    /// `Row` struct and its decoding, if the query returns rows.
    pub row: Option<String>,
    /// Parameters in order: name, expected Rust type (without a named
    /// lifetime: `&str`, `Option<i64>`…), and whether it is a domain (to be
    /// passed through `Domain`).
    pub params: Vec<(String, String, bool)>,
    /// Cardinality marker (`One`, `Optional`, `Many`, `NoRows`).
    pub cardinality: &'static str,
}

/// Inline query: like a named query, without enums (their Rust types must
/// be unique in the crate: named queries, `build.rs`).
pub fn inline_query(
    d: &Descriptor,
    catalog: &ProjectCatalog,
    opts: &RustOptions,
) -> Result<InlineQuery, String> {
    if d.copy.is_some() {
        return Err("COPY: declare a named query (build.rs)".into());
    }
    let mut g = Generator {
        ts: TypeSystem::new(&catalog.catalog),
        opts,
        enums: BTreeMap::new(),
    };
    let rt = &opts.runtime;
    let mut oids = Some(Vec::new());
    let mut params = Vec::new();
    for p in &d.params {
        let base = g.ts.base_type(&qname(&p.ty.pg));
        match (g.ts.def(&base).and_then(|t| t.oid), oids.as_mut()) {
            (Some(oid), Some(v)) => v.push(oid),
            _ => oids = None,
        }
        let t = g.rust_type(&p.ty.pg)?;
        let ty = t.borrowed.replace("&'a ", "&");
        let ty = match p.nullable {
            NullabilityDesc::Nullable => format!("Option<{ty}>"),
            NullabilityDesc::NonNull | NullabilityDesc::Unknown => ty,
        };
        params.push((p.name.clone(), ty, g.is_domain(&p.ty.pg)));
    }
    let oids = match oids {
        Some(v) => format!(
            "Some(&[{}])",
            v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
        ),
        None => "None".into(),
    };
    let query_const = format!(
        "const QUERY: {rt}::Query = {rt}::Query {{ name: {:?}, sql: {:?}, param_oids: {oids} }};",
        d.name, d.sql
    );
    let row = if d.columns.is_empty() {
        None
    } else {
        let mut o = String::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut fields = Vec::new();
        for c in &d.columns {
            let f = field_ident(&c.name)
                .map_err(|e| format!("{e} (output column: give it an alias)"))?;
            if !seen.insert(f.clone()) {
                return Err(format!(
                    "two output columns are named `{}`: give them distinct aliases",
                    c.name
                ));
            }
            let t = g.rust_type(&c.ty.pg)?;
            fields.push((
                f,
                match c.nullable {
                    NullabilityDesc::NonNull => t.owned,
                    _ => format!("Option<{}>", t.owned),
                },
            ));
        }
        let _ = writeln!(o, "#[derive(Debug, Clone, PartialEq)]\npub struct Row {{");
        for (f, t) in &fields {
            let _ = writeln!(o, "    pub {f}: {t},");
        }
        let _ = writeln!(
            o,
            "}}\nimpl Row {{ fn from_row(row: &{rt}::Row) -> Result<Self, {rt}::tokio_postgres::Error> {{ Ok(Self {{"
        );
        for (i, (f, _)) in fields.iter().enumerate() {
            let _ = writeln!(o, "    {f}: row.try_get({i})?,");
        }
        let _ = writeln!(o, "}}) }} }}");
        Some(o)
    };
    if let Some((ty, _)) = g.enums.iter().next() {
        return Err(format!(
            "enum {ty} has no Rust type in an inline query: use a named query (build.rs), which generates its enums"
        ));
    }
    use pgzest_query::Cardinality;
    let cardinality = match (d.cardinality, &row) {
        (Cardinality::None, _) | (_, None) => "NoRows",
        (Cardinality::One, _) => "One",
        (Cardinality::Optional, _) => "Optional",
        (Cardinality::Many, _) => "Many",
    };
    Ok(InlineQuery {
        query_const,
        row,
        params,
        cardinality,
    })
}

/// Rust enum for a PostgreSQL enum, encoded by its labels.
fn enum_code(
    rt: &str,
    rust: &str,
    typname: &str,
    labels: &[String],
) -> Result<String, Vec<String>> {
    let mut variants = Vec::new();
    for l in labels {
        let v = pascal(l);
        if v.is_empty() || !v.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            return Err(vec![format!(
                "label {l:?} of enum {typname}: no Rust variant possible"
            )]);
        }
        if variants.iter().any(|(x, _)| *x == v) {
            return Err(vec![format!(
                "labels of enum {typname}: two map to variant `{v}`"
            )]);
        }
        variants.push((v, l.clone()));
    }
    let mut o = String::new();
    let _ = writeln!(
        o,
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]\npub enum {rust} {{"
    );
    for (v, l) in &variants {
        let _ = writeln!(o, "    /// `{l}`\n    {v},");
    }
    let _ = writeln!(o, "}}\n");
    let _ = writeln!(o, "impl {rt}::PgEnum for {rust} {{");
    let _ = writeln!(o, "    const TYPE_NAME: &'static str = {typname:?};");
    let _ = writeln!(
        o,
        "    fn label(&self) -> &'static str {{\n        match self {{"
    );
    for (v, l) in &variants {
        let _ = writeln!(o, "            {rust}::{v} => {l:?},");
    }
    let _ = writeln!(o, "        }}\n    }}");
    let _ = writeln!(
        o,
        "    fn from_label(label: &str) -> Option<Self> {{\n        match label {{"
    );
    for (v, l) in &variants {
        let _ = writeln!(o, "            {l:?} => Some({rust}::{v}),");
    }
    let _ = writeln!(o, "            _ => None,\n        }}\n    }}\n}}\n");
    let _ = writeln!(
        o,
        "impl<'a> {rt}::FromSql<'a> for {rust} {{
    fn from_sql(_: &{rt}::Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {{
        {rt}::enum_from_sql(raw)
    }}
    fn accepts(ty: &{rt}::Type) -> bool {{
        ty.name() == {typname:?}
    }}
}}

impl {rt}::ToSql for {rust} {{
    fn to_sql(&self, _: &{rt}::Type, out: &mut {rt}::types::private::BytesMut) -> Result<{rt}::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {{
        {rt}::enum_to_sql(self, out)
    }}
    fn accepts(ty: &{rt}::Type) -> bool {{
        ty.name() == {typname:?}
    }}
    {rt}::types::to_sql_checked!();
}}
"
    );
    Ok(o)
}

fn qname(qualified: &str) -> Ty {
    pgzest_query::descriptor::parse_qualified(qualified)
        .unwrap_or_else(|| pgzest_sema::pg(qualified))
}

/// Words of a name, lowercased: split on any non-alphanumeric character
/// and on case changes (`createdAt` → created, at; `HTTPServer` → http,
/// server; `ORDERED` → ordered).
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in s
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
    {
        let chars: Vec<char> = part.chars().collect();
        let mut word = String::new();
        for (i, &c) in chars.iter().enumerate() {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next = chars.get(i + 1);
            let boundary = c.is_ascii_uppercase()
                && prev.is_some_and(|p| {
                    p.is_ascii_lowercase()
                        || p.is_ascii_digit()
                        || (p.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase()))
                });
            if boundary && !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            word.push(c.to_ascii_lowercase());
        }
        if !word.is_empty() {
            out.push(word);
        }
    }
    out
}

/// `page_by_id` → `PageById`, `ORDERED` → `Ordered`.
fn pascal(s: &str) -> String {
    words(s)
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "abstract", "become", "box", "do", "final", "gen", "macro", "override",
    "priv", "try", "typeof", "unsized", "virtual", "yield",
];

/// Rust identifier for a query name (it must be one as is).
fn rust_ident(name: &str) -> Result<String, String> {
    field_ident(name)
}

/// Rust field for a column or parameter name: in snake_case
/// (`createdAt` → `created_at`, `ID` → `id`), keyword as a raw identifier.
fn field_ident(name: &str) -> Result<String, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("`{name}` is not a Rust identifier"));
    }
    let out = words(name).join("_");
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(format!("`{name}` is not a Rust identifier"));
    }
    Ok(if KEYWORDS.contains(&out.as_str()) {
        format!("r#{out}")
    } else {
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_names() {
        for (name, field, ty) in [
            ("createdAt", "created_at", "CreatedAt"),
            ("companyId", "company_id", "CompanyId"),
            ("ID", "id", "Id"),
            ("HTTPServer", "http_server", "HttpServer"),
            ("ORDERED", "ordered", "Ordered"),
            ("IN_PROGRESS", "in_progress", "InProgress"),
            ("page_by_id", "page_by_id", "PageById"),
            ("v2Count", "v2_count", "V2Count"),
            ("type", "r#type", "Type"),
        ] {
            assert_eq!(field_ident(name).unwrap(), field, "{name}");
            assert_eq!(pascal(name), ty, "{name}");
        }
        assert!(field_ident("?column?").is_err());
    }
}
