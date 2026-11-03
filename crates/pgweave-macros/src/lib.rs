//! `query!` : requête SQL inline, analysée à la compilation contre le
//! catalogue du projet (`pgweave.toml`), sans connexion à une base.
//!
//! ```ignore
//! let page = pgweave_macros::query!(
//!     "SELECT id, title FROM pages WHERE id = $id",
//!     id = page_id,
//! )
//! .fetch(&client) // cardinalité optional → Option<Row>
//! .await?;
//! ```
//!
//! - Les paramètres sont nommés (`$nom`) et liés par `nom = expr` ; chaque
//!   expression est vérifiée contre le type attendu.
//! - `fetch` renvoie ce que promet la cardinalité inférée (`T`,
//!   `Option<T>`, `Vec<T>`) ; `execute` pour une requête sans ligne.
//! - Une erreur SQL est une erreur de compilation sur la chaîne.
//! - Les fichiers du projet sont inclus (`include_bytes!`) : modifier le
//!   schéma recompile les requêtes qui en dépendent.
//!
//! `queries!("queries")` : le code des requêtes nommées d'un répertoire (un
//! module par fichier : `queries/order/book.sql` → `order::book`) ou d'un
//! fichier, à l'endroit de l'appel — par exemple seul contenu de
//! `src/queries.rs`. Même génération que `pgweave-build`, sans `build.rs`.

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

/// `query_file!("queries/x.sql", nom = valeur, …)` : comme [`query!`], la
/// requête lue dans un fichier du crate (chemin relatif à son
/// `Cargo.toml`), qui est inclus pour la recompilation.
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

/// `queries!("chemin")` : fonctions, structs et enums des requêtes nommées
/// de `chemin` (relatif au `Cargo.toml` du crate), répertoire ou fichier.
///
/// ```ignore
/// // src/queries.rs
/// pgweave_macros::queries!("queries");
/// // ailleurs : crate::queries::order::book::get(&client, …)
/// ```
///
/// Chaque fichier lu est inclus pour la recompilation. Un fichier **ajouté**
/// au répertoire n'est vu qu'à la prochaine compilation du module qui
/// appelle la macro (Cargo ne surveille pas un répertoire pour une macro ;
/// `pgweave-build` depuis `build.rs`, lui, le fait).
#[proc_macro]
pub fn queries(input: TokenStream) -> TokenStream {
    let path = syn::parse_macro_input!(input as LitStr);
    let root = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(r) => PathBuf::from(r),
        None => {
            return syn::Error::new(path.span(), "CARGO_MANIFEST_DIR absent")
                .to_compile_error()
                .into();
        }
    };
    match expand_queries(root, &path.value()) {
        // Chaque token généré porte la position du chemin : « aller à la
        // définition » mène à l'appel `queries!("…")`. Un token reparsé
        // depuis une chaîne n'a pas de position dans le fichier, et
        // rust-analyzer en déduit un décalage hors du fichier (panique).
        Ok(tokens) => respan(tokens, path.span()).into(),
        Err(message) => syn::Error::new(path.span(), message)
            .to_compile_error()
            .into(),
    }
}

/// `tokens`, chacun (groupes compris) à la position `span`.
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
    let code = pgweave_build::generate_path(&project, &root.join(rel), &opts, &mut |p| {
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

/// Options de génération du projet : runtime en chemin absolu, module des
/// enums (`[rust] types`).
fn rust_options(project: &pgweave_build::Project) -> pgweave_codegen::rust::RustOptions {
    pgweave_codegen::rust::RustOptions {
        runtime: "::pgweave_runtime".into(),
        types_path: project.config.rust.types.clone(),
        ..Default::default()
    }
}

/// Fichiers du projet à inclure pour la recompilation.
fn watched(project: &pgweave_build::Project) -> Vec<String> {
    project
        .files
        .iter()
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .collect()
}

// ── sql! ──────────────────────────────────────────────────────────────────────

/// `sql! { /// doc  pub nom: cardinalité -> Chemin, "SELECT …" }`.
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
                    "cardinalité : one, optional, many ou none",
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

/// Une requête définie dans le code Rust : fonction, struct de paramètres et
/// struct de ligne, comme une requête nommée d'un fichier.
///
/// ```ignore
/// pgweave_macros::sql! {
///     /// La fiche d'un plat.
///     pub dish_card: optional,
///     r#"SELECT d.id, d.name FROM dishes d WHERE d.id = $id"#
/// }
///
/// pgweave_macros::sql! {
///     pub vegetarian: many -> DishCardRow,
///     r#"SELECT d.id, d.name FROM dishes d WHERE d.vegetarian"#
/// }
/// ```
///
/// La chaîne ne contient que la requête : nom, cardinalité (optionnelle,
/// inférée sinon) et ligne reprise sont des paramètres de la macro.
/// `-> Chemin` reprend la struct de ligne d'une autre requête (`sql!` ou
/// fichier) ; la compatibilité des colonnes (noms, ordre, types, nullabilité
/// non relâchée) est vérifiée à la compilation. Modifier la chaîne relance
/// l'analyse, dans l'éditeur aussi.
#[proc_macro]
pub fn sql(input: TokenStream) -> TokenStream {
    let def = syn::parse_macro_input!(input as SqlDef);
    let root = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(r) => PathBuf::from(r),
        None => {
            return syn::Error::new(def.sql.span(), "CARGO_MANIFEST_DIR absent")
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
    // Positions rapportées à la chaîne (sans l'en-tête ajouté).
    let located = |d: &pgweave_diagnostics::Diagnostic| {
        let at = d
            .span
            .as_ref()
            .map_or(0, |s| s.start.saturating_sub(header.len()));
        let before = &sql[..at.min(sql.len())];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!("{line}:{col}: [{}] {}", d.code, d.message)
    };
    let file = pgweave_query::parse_query_file_with(&text, &project.macros);
    let errors: Vec<String> = file
        .diagnostics
        .iter()
        .filter(|d| d.severity == pgweave_diagnostics::Severity::Error)
        .map(located)
        .collect();
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    let [q] = file.queries.as_slice() else {
        return Err("une requête par `sql!`".into());
    };
    let d = pgweave_query::describe_named(&project.catalog, q)
        .map_err(|ds| ds.iter().map(located).collect::<Vec<_>>().join("\n"))?;

    let row = def
        .row
        .as_ref()
        .map(|p| quote!(#p).to_string().replace(' ', ""));
    let code = pgweave_codegen::rust::generate_query(&d, &project.catalog, &opts, row.clone())?;
    // Visibilité demandée pour les éléments générés (champs : publics).
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
    // Code à la position de la chaîne ; le nom de la fonction et des structs
    // à celle du nom écrit : « aller à la définition » y mène.
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

/// Assertions constantes : les colonnes de la requête sont celles de la
/// ligne reprise (`Chemin::PGWEAVE_COLUMNS`), sans nullabilité relâchée.
fn row_check(path: &Path, shown: &str, name: &str, d: &pgweave_query::Descriptor) -> Tokens {
    use pgweave_query::descriptor::NullabilityDesc;
    let n = d.columns.len();
    let cols = quote!(<#path>::PGWEAVE_COLUMNS);
    let count = format!(
        "`{name}` -> {shown} : {n} colonne(s) ici ({}), pas le même nombre dans {shown}",
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
            "`{name}` -> {shown} : la colonne {} s'appelle `{col}` ici, pas dans {shown}",
            i + 1
        );
        let typed =
            format!("`{name}` -> {shown} : `{col}` est {pg} ici, d'un autre type dans {shown}");
        checks.push(quote!(
            const _: () = assert!(
                #cols.len() <= #i || ::pgweave_runtime::str_eq(#cols[#i].0, #col),
                #named
            );
            const _: () = assert!(
                #cols.len() <= #i || ::pgweave_runtime::str_eq(#cols[#i].1, #pg),
                #typed
            );
        ));
        if c.nullable != NullabilityDesc::NonNull {
            let nullable =
                format!("`{name}` -> {shown} : `{col}` peut être nulle ici, pas dans {shown}");
            checks.push(quote!(
                const _: () = assert!(#cols.len() <= #i || !#cols[#i].2, #nullable);
            ));
        }
    }
    quote!(#(#checks)*)
}

/// Les identifiants `name` et `Name…` (fonction, structs) à `span`.
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

/// `#[types] mod db {}` : les enums de tous les types énumérés du projet,
/// dans ce module (celui que déclare `[rust] types` de `pgweave.toml`).
///
/// ```ignore
/// #[pgweave_macros::types]
/// pub mod db {}
/// ```
///
/// Un attribut plutôt qu'une macro sans argument : le code généré est situé
/// sur le nom du module, où mène « aller à la définition ».
#[proc_macro_attribute]
pub fn types(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[types] ne prend pas d'argument",
        )
        .to_compile_error()
        .into();
    }
    let mut module = syn::parse_macro_input!(item as syn::ItemMod);
    let span = module.ident.span();
    let result = (|| {
        let root = std::env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .ok_or("CARGO_MANIFEST_DIR absent")?;
        let project = project(root)?;
        let code = pgweave_codegen::rust::generate_enums(&project.catalog, &rust_options(&project))
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
            return syn::Error::new(
                span,
                "#[types] s'applique à un module en ligne : `mod db {}`",
            )
            .to_compile_error()
            .into();
        }
    }
    quote!(#module).into()
}

/// Projet chargé, avec la date de modification de chaque fichier lu : le
/// serveur de macros d'un éditeur vit plus longtemps qu'une compilation.
struct Cached {
    project: Rc<pgweave_build::Project>,
    stamps: Vec<(PathBuf, Option<SystemTime>)>,
}

thread_local! {
    static PROJECTS: RefCell<HashMap<PathBuf, Cached>> = RefCell::new(HashMap::new());
}

fn modified(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn project(root: PathBuf) -> Result<Rc<pgweave_build::Project>, String> {
    PROJECTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(c) = cache.get(&root)
            && c.stamps.iter().all(|(p, t)| modified(p) == *t)
        {
            return Ok(c.project.clone());
        }
        let project = Rc::new(pgweave_build::load_project(&root)?);
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
        .map_err(|e| format!("code généré invalide : {e}"))
}

fn expand(input: &Input, from_file: bool) -> Result<Tokens, String> {
    let root = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok_or("CARGO_MANIFEST_DIR absent")?;
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
    let d = pgweave_query::describe_sql_with(&project.catalog, "query", &sql, &project.macros)
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

    // Liaisons : exactement les paramètres de la requête, par leur nom.
    let mut binds = Vec::new();
    for p in &d.params {
        if p.name.parse::<u32>().is_ok() {
            return Err(format!(
                "paramètre positionnel ${} : nommer les paramètres (`$nom`) pour les lier par `nom = valeur`",
                p.name
            ));
        }
        let Some((_, e)) = input.binds.iter().find(|(n, _)| *n == p.name) else {
            return Err(format!(
                "paramètre ${} sans valeur : ajouter `{} = …`",
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
        return Err(format!(
            "`{n}` ne correspond à aucun paramètre ${n} de la requête"
        ));
    }

    let opts = pgweave_codegen::rust::RustOptions {
        runtime: "::pgweave_runtime".into(),
        ..Default::default()
    };
    let q = pgweave_codegen::rust::inline_query(&d, &project.catalog, &opts)?;
    // Code généré à la position de la chaîne SQL (voir `queries!`) ; les
    // expressions liées gardent la leur.
    let span = input.sql.span();
    let parse = |code: &str| parse(code).map(|t| respan(t, span));
    let query_const = parse(&q.query_const)?;
    let (row_def, row_ty, decode) = match &q.row {
        Some(code) => (parse(code)?, quote!(Row), quote!(Row::from_row)),
        None => (quote!(), quote!(()), quote!(|_| Ok(()))),
    };
    let cardinality: Tokens = parse(&format!(
        "::pgweave_runtime::cardinality::{}",
        q.cardinality
    ))?;
    let mut params = Vec::new();
    for ((_, ty, domain), e) in q.params.iter().zip(&binds) {
        let ty = parse(ty)?;
        params.push(if *domain {
            quote!(::pgweave_runtime::bind(::pgweave_runtime::Domain({ let v: #ty = #e; v })))
        } else {
            quote!(::pgweave_runtime::bind({ let v: #ty = #e; v }))
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
        ::pgweave_runtime::Inline::<#row_ty, #cardinality>::new(&QUERY, vec![#(#params),*], #decode)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Projet temporaire : un schéma, une configuration.
    fn project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pgweave-macros-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("schema")).unwrap();
        std::fs::write(
            dir.join("pgweave.toml"),
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
        let input: Input = syn::parse_str(input).expect("entrée");
        expand_in(dir.to_path_buf(), &input, false).map(|t| t.to_string())
    }

    #[test]
    fn expansion_et_refus() {
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
            "paramètre $id sans valeur : ajouter `id = …`"
        );
        assert_eq!(
            err(r#""SELECT body FROM notes", id = 1"#),
            "`id` ne correspond à aucun paramètre $id de la requête"
        );
        assert_eq!(
            err(r#""SELECT body FROM notes WHERE id = $1""#),
            "paramètre positionnel $1 : nommer les paramètres (`$nom`) pour les lier par `nom = valeur`"
        );
        assert!(
            err(r#""SELECT mood FROM notes""#).contains("l'enum public.mood n'a pas de type Rust")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn un_module_par_fichier_et_lignes_reprises() {
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
            .expect("fichier")
            .to_string();
        assert!(!file.contains("pub mod"), "{file}");
        let err = expand_queries(dir.clone(), "nope").expect_err("absent");
        assert!(err.contains("ni fichier ni répertoire"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn expand_sql_str(dir: &std::path::Path, input: &str) -> Result<String, String> {
        let def: SqlDef = syn::parse_str(input).expect("entrée");
        expand_sql(dir.to_path_buf(), &def).map(|t| t.to_string())
    }

    #[test]
    fn sql_une_requete_par_macro() {
        let dir = project_dir("sql");
        let code = expand_sql_str(
            &dir,
            r#"/// La note.
               pub note: optional, "SELECT id, body FROM notes WHERE id = $id""#,
        )
        .expect("expansion");
        assert!(code.contains("pub async fn note"), "{code}");
        assert!(code.contains("NoteParams"), "{code}");
        assert!(code.contains("PGWEAVE_COLUMNS"), "{code}");
        assert!(code.contains("La note."), "{code}");

        // Ligne reprise : une assertion constante par propriété comparée.
        let code = expand_sql_str(
            &dir,
            r#"pub all: many -> super::NoteRow, "SELECT id, body FROM notes""#,
        )
        .expect("expansion");
        assert!(
            code.contains("< super :: NoteRow > :: PGWEAVE_COLUMNS"),
            "{code}"
        );
        assert!(code.contains("str_eq"), "{code}");
        assert!(!code.contains("pub struct AllRow"), "{code}");

        // Visibilité héritée : éléments privés au module.
        let code = expand_sql_str(&dir, r#"privee, "SELECT body FROM notes""#).expect("expansion");
        assert!(
            code.contains("async fn privee") && !code.contains("pub async fn privee"),
            "{code}"
        );

        let err = expand_sql_str(&dir, r#"pub m, "SELECT mood FROM notes""#).expect_err("enum");
        assert!(err.contains("[rust] types"), "{err}");
        let err = expand_sql_str(&dir, r#"pub q, "SELECT nope FROM notes""#).expect_err("colonne");
        assert!(err.starts_with("1:8: [column-not-found]"), "{err}");
        assert!(syn::parse_str::<SqlDef>(r#"pub q: plusieurs, "SELECT 1""#).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
