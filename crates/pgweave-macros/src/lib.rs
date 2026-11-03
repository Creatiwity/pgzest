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
use syn::{Expr, Ident, LitStr, Token};

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
        Ok(tokens) => tokens.into(),
        Err(message) => syn::Error::new(path.span(), message)
            .to_compile_error()
            .into(),
    }
}

fn expand_queries(root: PathBuf, rel: &str) -> Result<Tokens, String> {
    let project = project(root.clone())?;
    let opts = pgweave_codegen::rust::RustOptions {
        runtime: "::pgweave_runtime".into(),
        ..Default::default()
    };
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
}
