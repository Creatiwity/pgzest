//! `query!`: inline SQL query, analyzed at compile time against the
//! project catalog (`pgzest.toml`), without a database connection.
//!
//! ```ignore
//! let page = pgzest_macros::query!(
//!     "SELECT id, title FROM pages WHERE id = $id",
//!     id = page_id,
//! )
//! .fetch(&client) // cardinality optional → Option<Row>
//! .await?;
//! ```
//!
//! - Parameters are named (`$name`) and bound with `name = expr`; each
//!   expression is checked against the expected type.
//! - `fetch` returns what the inferred cardinality promises (`T`,
//!   `Option<T>`, `Vec<T>`); `execute` for a query returning no rows.
//! - An SQL error is a compile error on the string.
//! - Project files are included (`include_bytes!`): changing the schema
//!   recompiles the queries that depend on it.
//!
//! `queries!("queries")`: the code of the named queries of a directory (one
//! module per file: `queries/order/book.sql` → `order::book`) or of a file,
//! at the call site — for example the only content of `src/queries.rs`.
//! Same generation as `pgzest-build`, without `build.rs`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::SystemTime;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Attribute, Expr, Ident, LitStr, Path, Token, Visibility};

struct Input {
    sql: LitStr,
    binds: Vec<(Ident, Expr)>,
}

impl Parse for Input {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let sql: LitStr = input.parse()?;
        let mut binds = Vec::new();
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let name: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let value: Expr = input.parse()?;
            binds.push((name, value));
        }
        Ok(Input { sql, binds })
    }
}

#[proc_macro]
pub fn query(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as Input);
    match expand(&input, false) {
        Ok(tokens) => tokens.into(),
        Err(message) => syn::Error::new(input.sql.span(), message)
            .to_compile_error()
            .into(),
    }
}

/// `query_file!("queries/x.sql", name = value, …)`: like [`query!`], the
/// query being read from a file of the crate (path relative to its
/// `Cargo.toml`), which is included for recompilation.
#[proc_macro]
pub fn query_file(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as Input);
    match expand(&input, true) {
        Ok(tokens) => tokens.into(),
        Err(message) => syn::Error::new(input.sql.span(), message)
            .to_compile_error()
            .into(),
    }
}

/// `queries!("path")`: functions, structs and enums of the named queries
/// of `path` (relative to the crate's `Cargo.toml`), directory or file.
///
/// ```ignore
/// // src/queries.rs
/// pgzest_macros::queries!("queries");
/// // elsewhere: crate::queries::order::book::get(&client, …)
/// ```
///
/// Each file read is included for recompilation. A file **added** to the
/// directory is only seen at the next compilation of the module calling the
/// macro (Cargo does not watch a directory for a macro; `pgzest-build`
/// from `build.rs` does).
#[proc_macro]
pub fn queries(input: TokenStream) -> TokenStream {
    let path = syn::parse_macro_input!(input as LitStr);
    let root = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(r) => PathBuf::from(r),
        None => {
            return syn::Error::new(path.span(), "CARGO_MANIFEST_DIR not set")
                .to_compile_error()
                .into();
        }
    };
    match expand_queries(root, &path.value()) {
        // Each generated token carries the position of the path: "go to
        // definition" leads to the `queries!("…")` call. A token reparsed
        // from a string has no position in the file, and rust-analyzer
        // derives an offset outside the file from it (panic).
        Ok(tokens) => respan(tokens, path.span()).into(),
        Err(message) => syn::Error::new(path.span(), message)
            .to_compile_error()
            .into(),
    }
}

/// `tokens`, each one (groups included) at position `span`.
fn respan(tokens: Tokens, span: proc_macro2::Span) -> Tokens {
    use proc_macro2::TokenTree;
    tokens
        .into_iter()
        .map(|t| match t {
            TokenTree::Group(g) => {
                let mut out = proc_macro2::Group::new(g.delimiter(), respan(g.stream(), span));
                out.set_span(span);
                TokenTree::Group(out)
            }
            mut other => {
                other.set_span(span);
                other
            }
        })
        .collect()
}

fn expand_queries(root: PathBuf, rel: &str) -> Result<Tokens, String> {
    let project = project(root.clone())?;
    let opts = rust_options(&project);
    let mut watched: Vec<String> = project
        .files
        .iter()
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .collect();
    let code = pgzest_build::generate_path(&project, &root.join(rel), &opts, &mut |p| {
        if p.is_file() {
            watched.push(p.display().to_string());
        }
    })?;
    let code = parse(&code)?;
    Ok(quote!(
        #( const _: &[u8] = include_bytes!(#watched); )*
        #code
    ))
}

/// Project generation options: runtime as an absolute path, enum module
/// (`[rust] types`).
fn rust_options(project: &pgzest_build::Project) -> pgzest_codegen::rust::RustOptions {
    pgzest_codegen::rust::RustOptions {
        runtime: "::pgzest_runtime".into(),
        types_path: project.config.rust.types.clone(),
        ..Default::default()
    }
}

/// Project files to include for recompilation.
fn watched(project: &pgzest_build::Project) -> Vec<String> {
    project
        .files
        .iter()
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .collect()
}

// ── sql! ──────────────────────────────────────────────────────────────────────

/// `sql! { /// doc  pub name: cardinality -> Path, "SELECT …" }`.
struct SqlDef {
    attrs: Vec<Attribute>,
    vis: Visibility,
    name: Ident,
    cardinality: Option<Ident>,
    row: Option<Path>,
    sql: LitStr,
}

impl Parse for SqlDef {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let vis: Visibility = input.parse()?;
        let name: Ident = input.parse()?;
        let cardinality = if input.peek(Token![:]) {
            input.parse::<Token![:]>()?;
            let c: Ident = input.parse()?;
            if !matches!(c.to_string().as_str(), "one" | "optional" | "many" | "none") {
                return Err(syn::Error::new(
                    c.span(),
                    "cardinality: one, optional, many or none",
                ));
            }
            Some(c)
        } else {
            None
        };
        let row = if input.peek(Token![->]) {
            input.parse::<Token![->]>()?;
            Some(input.parse::<Path>()?)
        } else {
            None
        };
        input.parse::<Token![,]>()?;
        let sql: LitStr = input.parse()?;
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
        Ok(SqlDef {
            attrs,
            vis,
            name,
            cardinality,
            row,
            sql,
        })
    }
}

/// A query defined in Rust code: function, parameter struct and row
/// struct, like a named query from a file.
///
/// ```ignore
/// pgzest_macros::sql! {
///     /// The card of a dish.
///     pub dish_card: optional,
///     r#"SELECT d.id, d.name FROM dishes d WHERE d.id = $id"#
/// }
///
/// pgzest_macros::sql! {
///     pub vegetarian: many -> DishCardRow,
///     r#"SELECT d.id, d.name FROM dishes d WHERE d.vegetarian"#
/// }
/// ```
///
/// The string only contains the query: name, cardinality (optional,
/// inferred otherwise) and reused row are parameters of the macro.
/// `-> Path` reuses the row struct of another query (`sql!` or file);
/// column compatibility (names, order, types, nullability not relaxed) is
/// checked at compile time. Editing the string reruns the analysis, in the
/// editor too.
#[proc_macro]
pub fn sql(input: TokenStream) -> TokenStream {
    let def = syn::parse_macro_input!(input as SqlDef);
    let root = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(r) => PathBuf::from(r),
        None => {
            return syn::Error::new(def.sql.span(), "CARGO_MANIFEST_DIR not set")
                .to_compile_error()
                .into();
        }
    };
    match expand_sql(root, &def) {
        Ok(tokens) => tokens.into(),
        Err(message) => syn::Error::new(def.sql.span(), message)
            .to_compile_error()
            .into(),
    }
}

fn expand_sql(root: PathBuf, def: &SqlDef) -> Result<Tokens, String> {
    let project = project(root)?;
    let opts = rust_options(&project);
    let name = def.name.to_string();
    let header = format!(
        "-- name: {name}{}\n",
        def.cardinality
            .as_ref()
            .map(|c| format!(" :{c}"))
            .unwrap_or_default()
    );
    let sql = def.sql.value();
    let text = format!("{header}{sql}");
    // Positions relative to the string (without the added header).
    let located = |d: &pgzest_diagnostics::Diagnostic| {
        let at = d
            .span
            .as_ref()
            .map_or(0, |s| s.start.saturating_sub(header.len()));
        let before = &sql[..at.min(sql.len())];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!("{line}:{col}: [{}] {}", d.code, d.message)
    };
    let file = pgzest_query::parse_query_file_with(&text, &project.macros);
    let errors: Vec<String> = file
        .diagnostics
        .iter()
        .filter(|d| d.severity == pgzest_diagnostics::Severity::Error)
        .map(located)
        .collect();
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    let [q] = file.queries.as_slice() else {
        return Err("one query per `sql!`".into());
    };
    let d = pgzest_query::describe_named(&project.catalog, q)
        .map_err(|ds| ds.iter().map(located).collect::<Vec<_>>().join("\n"))?;

    let row = def
        .row
        .as_ref()
        .map(|p| quote!(#p).to_string().replace(' ', ""));
    let code = pgzest_codegen::rust::generate_query(&d, &project.catalog, &opts, row.clone())?;
    // Requested visibility for generated items (fields: public).
    let vis = {
        let v = &def.vis;
        quote!(#v).to_string()
    };
    let doc: String = def
        .attrs
        .iter()
        .map(|a| quote!(#a).to_string() + "\n")
        .collect();
    let mut out = String::new();
    for l in code.split_inclusive('\n') {
        if let Some(rest) = l.strip_prefix("pub async fn ") {
            if rest.starts_with(&format!("{name}<")) {
                out.push_str(&doc);
            }
            out.push_str(&format!("{vis} async fn {rest}"));
        } else if let Some(rest) = l.strip_prefix("pub struct ") {
            out.push_str(&format!("{vis} struct {rest}"));
        } else if let Some(rest) = l.strip_prefix("pub const ") {
            out.push_str(&format!("{vis} const {rest}"));
        } else {
            out.push_str(l);
        }
    }
    let code = parse(&out)?;
    // Code at the position of the string; the name of the function and of
    // the structs at that of the written name: "go to definition" leads there.
    let pascal: String = name
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect();
    let code = respan_names(
        respan(code, def.sql.span()),
        &name,
        &pascal,
        def.name.span(),
    );

    let check = match (&def.row, &row) {
        (Some(path), Some(shown)) => row_check(path, shown, &name, &d),
        _ => quote!(),
    };
    let watched = watched(&project);
    Ok(quote!(
        #( const _: &[u8] = include_bytes!(#watched); )*
        #code
        #check
    ))
}

/// Constant assertions: the query's columns are those of the reused row
/// (`Path::PGZEST_COLUMNS`), without relaxed nullability.
fn row_check(path: &Path, shown: &str, name: &str, d: &pgzest_query::Descriptor) -> Tokens {
    use pgzest_query::descriptor::NullabilityDesc;
    let n = d.columns.len();
    let cols = quote!(<#path>::PGZEST_COLUMNS);
    let count = format!(
        "`{name}` -> {shown}: {n} column(s) here ({}), a different count in {shown}",
        d.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut checks = vec![quote!(
        const _: () = assert!(#cols.len() == #n, #count);
    )];
    for (i, c) in d.columns.iter().enumerate() {
        let col = &c.name;
        let pg = &c.ty.pg;
        let named = format!(
            "`{name}` -> {shown}: column {} is named `{col}` here, not in {shown}",
            i + 1
        );
        let typed =
            format!("`{name}` -> {shown}: `{col}` is {pg} here, of another type in {shown}");
        checks.push(quote!(
            const _: () = assert!(
                #cols.len() <= #i || ::pgzest_runtime::str_eq(#cols[#i].0, #col),
                #named
            );
            const _: () = assert!(
                #cols.len() <= #i || ::pgzest_runtime::str_eq(#cols[#i].1, #pg),
                #typed
            );
        ));
        if c.nullable != NullabilityDesc::NonNull {
            let nullable = format!("`{name}` -> {shown}: `{col}` can be null here, not in {shown}");
            checks.push(quote!(
                const _: () = assert!(#cols.len() <= #i || !#cols[#i].2, #nullable);
            ));
        }
    }
    quote!(#(#checks)*)
}

/// The identifiers `name` and `Name…` (function, structs) at `span`.
fn respan_names(tokens: Tokens, name: &str, pascal: &str, span: proc_macro2::Span) -> Tokens {
    use proc_macro2::TokenTree;
    tokens
        .into_iter()
        .map(|t| match t {
            TokenTree::Group(g) => {
                let mut out = proc_macro2::Group::new(
                    g.delimiter(),
                    respan_names(g.stream(), name, pascal, span),
                );
                out.set_span(g.span());
                TokenTree::Group(out)
            }
            TokenTree::Ident(mut i) => {
                let s = i.to_string();
                if s == name || s == format!("{name}_stream") || s.starts_with(pascal) {
                    i.set_span(span);
                }
                TokenTree::Ident(i)
            }
            other => other,
        })
        .collect()
}

// ── types! ────────────────────────────────────────────────────────────────────

/// `#[types] mod db {}`: the enums of all the project's enumerated types,
/// in this module (the one declared by `[rust] types` in `pgzest.toml`).
///
/// ```ignore
/// #[pgzest_macros::types]
/// pub mod db {}
/// ```
///
/// An attribute rather than an argument-less macro: the generated code is
/// located on the module name, where "go to definition" leads.
#[proc_macro_attribute]
pub fn types(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(proc_macro2::Span::call_site(), "#[types] takes no argument")
            .to_compile_error()
            .into();
    }
    let mut module = syn::parse_macro_input!(item as syn::ItemMod);
    let span = module.ident.span();
    let result = (|| {
        let root = std::env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .ok_or("CARGO_MANIFEST_DIR not set")?;
        let project = project(root)?;
        let code = pgzest_codegen::rust::generate_enums(&project.catalog, &rust_options(&project))
            .map_err(|e| e.join("\n"))?;
        let code = respan(parse(&code)?, span);
        let watched = watched(&project);
        Ok::<_, String>(quote!(
            #( const _: &[u8] = include_bytes!(#watched); )*
            #code
        ))
    })();
    let generated = match result {
        Ok(t) => t,
        Err(e) => return syn::Error::new(span, e).to_compile_error().into(),
    };
    let items: syn::File = match syn::parse2(generated) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error().into(),
    };
    match &mut module.content {
        Some((_, content)) => content.extend(items.items),
        None => {
            return syn::Error::new(span, "#[types] applies to an inline module: `mod db {}`")
                .to_compile_error()
                .into();
        }
    }
    quote!(#module).into()
}

/// Loaded project, with the modification time of each file read: an
/// editor's macro server lives longer than a compilation.
struct Cached {
    project: Rc<pgzest_build::Project>,
    stamps: Vec<(PathBuf, Option<SystemTime>)>,
}

thread_local! {
    static PROJECTS: RefCell<HashMap<PathBuf, Cached>> = RefCell::new(HashMap::new());
}

fn modified(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn project(root: PathBuf) -> Result<Rc<pgzest_build::Project>, String> {
    PROJECTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(c) = cache.get(&root)
            && c.stamps.iter().all(|(p, t)| modified(p) == *t)
        {
            return Ok(c.project.clone());
        }
        let project = Rc::new(pgzest_build::load_project(&root)?);
        let stamps = project
            .files
            .iter()
            .map(|p| (p.clone(), modified(p)))
            .collect();
        cache.insert(
            root,
            Cached {
                project: project.clone(),
                stamps,
            },
        );
        Ok(project)
    })
}

fn parse(code: &str) -> Result<Tokens, String> {
    code.parse::<Tokens>()
        .map_err(|e| format!("invalid generated code: {e}"))
}

fn expand(input: &Input, from_file: bool) -> Result<Tokens, String> {
    let root = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok_or("CARGO_MANIFEST_DIR not set")?;
    expand_in(root, input, from_file)
}

fn expand_in(root: PathBuf, input: &Input, from_file: bool) -> Result<Tokens, String> {
    let project = project(root.clone())?;
    let (sql, file) = if from_file {
        let path = root.join(input.sql.value());
        let sql = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        (sql, Some(path))
    } else {
        (input.sql.value(), None)
    };
    let d = pgzest_query::describe_sql_with(&project.catalog, "query", &sql, &project.macros)
        .map_err(|diags| {
            diags
                .iter()
                .map(|d| {
                    let at = match (&file, &d.span) {
                        (Some(f), Some(s)) => {
                            let before = &sql[..s.start.min(sql.len())];
                            let line = before.matches('\n').count() + 1;
                            let col =
                                before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
                            format!("{}:{line}:{col}: ", f.display())
                        }
                        _ => String::new(),
                    };
                    match &d.hint {
                        Some(h) => format!("{at}[{}] {} ({h})", d.code, d.message),
                        None => format!("{at}[{}] {}", d.code, d.message),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })?;

    // Bindings: exactly the query's parameters, by name.
    let mut binds = Vec::new();
    for p in &d.params {
        if p.name.parse::<u32>().is_ok() {
            return Err(format!(
                "positional parameter ${}: name the parameters (`$name`) to bind them with `name = value`",
                p.name
            ));
        }
        let Some((_, e)) = input.binds.iter().find(|(n, _)| *n == p.name) else {
            return Err(format!(
                "parameter ${} has no value: add `{} = …`",
                p.name, p.name
            ));
        };
        binds.push(e);
    }
    if let Some((n, _)) = input
        .binds
        .iter()
        .find(|(n, _)| !d.params.iter().any(|p| *n == p.name))
    {
        return Err(format!("`{n}` matches no parameter ${n} of the query"));
    }

    let opts = pgzest_codegen::rust::RustOptions {
        runtime: "::pgzest_runtime".into(),
        ..Default::default()
    };
    let q = pgzest_codegen::rust::inline_query(&d, &project.catalog, &opts)?;
    // Generated code at the position of the SQL string (see `queries!`);
    // bound expressions keep their own.
    let span = input.sql.span();
    let parse = |code: &str| parse(code).map(|t| respan(t, span));
    let query_const = parse(&q.query_const)?;
    let (row_def, row_ty, decode) = match &q.row {
        Some(code) => (parse(code)?, quote!(Row), quote!(Row::from_row)),
        None => (quote!(), quote!(()), quote!(|_| Ok(()))),
    };
    let cardinality: Tokens = parse(&format!("::pgzest_runtime::cardinality::{}", q.cardinality))?;
    let mut params = Vec::new();
    for ((_, ty, domain), e) in q.params.iter().zip(&binds) {
        let ty = parse(ty)?;
        params.push(if *domain {
            quote!(::pgzest_runtime::bind(::pgzest_runtime::Domain({ let v: #ty = #e; v })))
        } else {
            quote!(::pgzest_runtime::bind({ let v: #ty = #e; v }))
        });
    }
    let watched: Vec<String> = project
        .files
        .iter()
        .chain(&file)
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .collect();
    Ok(quote!({
        #( const _: &[u8] = include_bytes!(#watched); )*
        #query_const
        #row_def
        ::pgzest_runtime::Inline::<#row_ty, #cardinality>::new(&QUERY, vec![#(#params),*], #decode)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temporary project: a schema, a configuration.
    fn project_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pgzest-macros-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("schema")).unwrap();
        std::fs::write(
            dir.join("pgzest.toml"),
            "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("schema/s.sql"),
            "CREATE TYPE mood AS ENUM ('a', 'b');\n\
             CREATE TABLE notes (id BIGINT PRIMARY KEY, body TEXT NOT NULL, mood mood);\n",
        )
        .unwrap();
        dir
    }

    fn expand_str(dir: &std::path::Path, input: &str) -> Result<String, String> {
        let input: Input = syn::parse_str(input).expect("input");
        expand_in(dir.to_path_buf(), &input, false).map(|t| t.to_string())
    }

    #[test]
    fn expansion_and_rejections() {
        let dir = project_dir("expansion");
        let code = expand_str(&dir, r#""SELECT body FROM notes WHERE id = $id", id = 7"#)
            .expect("expansion");
        assert!(code.contains("cardinality :: Optional"), "{code}");
        assert!(code.contains("let v : i64 = 7"), "{code}");
        assert!(code.contains("include_bytes !"), "{code}");

        let err = |input: &str| expand_str(&dir, input).expect_err(input);
        assert_eq!(
            err(r#""SELECT nope FROM notes""#),
            "[column-not-found] column \"nope\" does not exist"
        );
        assert_eq!(
            err(r#""SELECT body FROM notes WHERE id = $id""#),
            "parameter $id has no value: add `id = …`"
        );
        assert_eq!(
            err(r#""SELECT body FROM notes", id = 1"#),
            "`id` matches no parameter $id of the query"
        );
        assert_eq!(
            err(r#""SELECT body FROM notes WHERE id = $1""#),
            "positional parameter $1: name the parameters (`$name`) to bind them with `name = value`"
        );
        assert!(err(r#""SELECT mood FROM notes""#).contains("enum public.mood has no Rust type"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn one_module_per_file_and_reused_rows() {
        let dir = project_dir("queries");
        std::fs::create_dir_all(dir.join("queries/notes")).unwrap();
        std::fs::write(
            dir.join("queries/notes/read.sql"),
            "-- name: card :optional\nSELECT id, body FROM notes WHERE id = $id;\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("queries/search.sql"),
            "-- name: all_cards :many -> notes::read::card\nSELECT id, body FROM notes;\n",
        )
        .unwrap();
        let code = expand_queries(dir.clone(), "queries")
            .expect("expansion")
            .to_string();
        assert!(code.contains("pub mod notes"), "{code}");
        assert!(code.contains("pub mod read"), "{code}");
        assert!(code.contains("pub mod search"), "{code}");
        assert!(
            code.contains("Vec < super :: notes :: read :: CardRow >"),
            "{code}"
        );
        let file = expand_queries(dir.clone(), "queries/notes/read.sql")
            .expect("file")
            .to_string();
        assert!(!file.contains("pub mod"), "{file}");
        let err = expand_queries(dir.clone(), "nope").expect_err("missing");
        assert!(err.contains("neither a file nor a directory"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn expand_sql_str(dir: &std::path::Path, input: &str) -> Result<String, String> {
        let def: SqlDef = syn::parse_str(input).expect("input");
        expand_sql(dir.to_path_buf(), &def).map(|t| t.to_string())
    }

    #[test]
    fn sql_one_query_per_macro() {
        let dir = project_dir("sql");
        let code = expand_sql_str(
            &dir,
            r#"/// The note.
               pub note: optional, "SELECT id, body FROM notes WHERE id = $id""#,
        )
        .expect("expansion");
        assert!(code.contains("pub async fn note"), "{code}");
        assert!(code.contains("NoteParams"), "{code}");
        assert!(code.contains("PGZEST_COLUMNS"), "{code}");
        assert!(code.contains("The note."), "{code}");

        // Reused row: one constant assertion per compared property.
        let code = expand_sql_str(
            &dir,
            r#"pub all: many -> super::NoteRow, "SELECT id, body FROM notes""#,
        )
        .expect("expansion");
        assert!(
            code.contains("< super :: NoteRow > :: PGZEST_COLUMNS"),
            "{code}"
        );
        assert!(code.contains("str_eq"), "{code}");
        assert!(!code.contains("pub struct AllRow"), "{code}");

        // Inherited visibility: items private to the module.
        let code =
            expand_sql_str(&dir, r#"private_q, "SELECT body FROM notes""#).expect("expansion");
        assert!(
            code.contains("async fn private_q") && !code.contains("pub async fn private_q"),
            "{code}"
        );

        let err = expand_sql_str(&dir, r#"pub m, "SELECT mood FROM notes""#).expect_err("enum");
        assert!(err.contains("[rust] types"), "{err}");
        let err = expand_sql_str(&dir, r#"pub q, "SELECT nope FROM notes""#).expect_err("column");
        assert!(err.starts_with("1:8: [column-not-found]"), "{err}");
        assert!(syn::parse_str::<SqlDef>(r#"pub q: several, "SELECT 1""#).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
