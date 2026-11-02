//! Descripteur de requête (PLAN.md §4) : types, nullabilité, cardinalité,
//! dépendances, empreinte.

use pgweave_catalog::ProjectCatalog;
use pgweave_query::descriptor::{Command, NullabilityDesc};
use pgweave_query::{Cardinality, Descriptor, describe_named, parse_query_file};

const SCHEMA: &str = "
CREATE TYPE status AS ENUM ('draft', 'live');
CREATE TABLE pages (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    slug          VARCHAR(80) NOT NULL UNIQUE,
    title         TEXT NOT NULL,
    status        status NOT NULL DEFAULT 'draft',
    creation_date TIMESTAMPTZ,
    site          INT NOT NULL,
    path          TEXT NOT NULL,
    UNIQUE (site, path)
);
CREATE TABLE tags (page_id BIGINT REFERENCES pages, tag TEXT NOT NULL);
";

fn catalog() -> ProjectCatalog {
    ProjectCatalog::from_sql(SCHEMA).expect("schéma")
}

/// Descripteurs des requêtes d'un fichier, ou les messages d'erreur.
fn describe(file: &str) -> Vec<Result<Descriptor, Vec<String>>> {
    let cat = catalog();
    let parsed = parse_query_file(file);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    parsed
        .queries
        .iter()
        .map(|q| {
            describe_named(&cat, q).map_err(|ds| {
                ds.into_iter()
                    .map(|d| format!("{}: {}", d.code, d.message))
                    .collect()
            })
        })
        .collect()
}

fn one(sql: &str) -> Descriptor {
    describe(&format!("-- name: q\n{sql};\n"))
        .remove(0)
        .unwrap_or_else(|e| panic!("{sql} : {e:?}"))
}

fn cardinality(sql: &str) -> Cardinality {
    one(sql).cardinality
}

#[test]
fn cardinalite_inferee() {
    use Cardinality::*;
    for (sql, expected) in [
        ("SELECT id, title FROM pages WHERE id = $id", Optional),
        (
            "SELECT id FROM pages WHERE slug = $slug AND status = 'live'",
            Optional,
        ),
        (
            "SELECT id FROM pages p WHERE p.site = $site AND p.path = $path",
            Optional,
        ),
        ("SELECT id FROM pages WHERE site = $site", Many),
        ("SELECT id FROM pages WHERE id = $id OR slug = $slug", Many),
        ("SELECT id FROM pages WHERE id = id", Many),
        ("SELECT count(*) FROM pages", One),
        (
            "SELECT max(creation_date) FROM pages WHERE site = $site",
            One,
        ),
        ("SELECT count(*) FROM pages HAVING count(*) > 1", Optional),
        ("SELECT site, count(*) FROM pages GROUP BY site", Many),
        ("SELECT id FROM pages ORDER BY id LIMIT 1", Optional),
        ("SELECT now()", One),
        ("SELECT generate_series(1, 3)", Many),
        ("SELECT id FROM pages UNION SELECT page_id FROM tags", Many),
        (
            "INSERT INTO pages (slug, title, site, path) VALUES ($slug, $title, 1, '/') RETURNING id",
            One,
        ),
        ("INSERT INTO tags (page_id, tag) VALUES ($p, $t)", None),
        (
            "INSERT INTO tags (page_id, tag) SELECT id, 'x' FROM pages RETURNING tag",
            Many,
        ),
        (
            "UPDATE pages SET title = $title WHERE id = $id RETURNING title",
            Optional,
        ),
        (
            "UPDATE pages SET title = $title WHERE site = $site RETURNING id",
            Many,
        ),
        (
            "DELETE FROM pages WHERE slug = $slug RETURNING id",
            Optional,
        ),
        ("DELETE FROM tags WHERE tag = $tag", None),
    ] {
        assert_eq!(cardinality(sql), expected, "{sql}");
    }
}

#[test]
fn nullabilite_des_parametres_et_colonnes() {
    let d = one(
        "INSERT INTO pages (slug, title, site, path, creation_date) VALUES ($slug, $title, $site, '/', $at) RETURNING id, creation_date",
    );
    let params: Vec<(&str, NullabilityDesc)> = d
        .params
        .iter()
        .map(|p| (p.name.as_str(), p.nullable))
        .collect();
    assert_eq!(
        params,
        [
            ("slug", NullabilityDesc::NonNull),
            ("title", NullabilityDesc::NonNull),
            ("site", NullabilityDesc::NonNull),
            ("at", NullabilityDesc::Nullable),
        ]
    );
    // Hors affectation, la nullabilité d'un paramètre n'est pas établie.
    let d = one("SELECT id FROM pages WHERE site = $site");
    assert_eq!(d.params[0].nullable, NullabilityDesc::Unknown);
    assert_eq!(d.command, Command::Select);
}

#[test]
fn dependances_du_projet_seulement() {
    let d = one(
        "SELECT p.id, p.status, t.tag FROM pages p JOIN tags t ON t.page_id = p.id WHERE p.slug = lower($slug)",
    );
    assert_eq!(
        d.depends_on,
        ["public.pages", "public.status", "public.tags"]
    );
}

#[test]
fn empreinte_stable_et_sensible_au_schema() {
    let a = one("SELECT id, title FROM pages WHERE id = $id");
    // Même requête, autre mise en forme et commentaire : même empreinte.
    let b = one("SELECT id,\n  title -- titre\nFROM pages\nWHERE id = $id");
    assert_eq!(a.fingerprint, b.fingerprint);
    assert_eq!(a.fingerprint.len(), 64);
    // Le schéma d'une dépendance change : l'empreinte aussi.
    let other =
        ProjectCatalog::from_sql(&SCHEMA.replace("title         TEXT NOT NULL", "title TEXT"))
            .expect("schéma");
    let q =
        &parse_query_file("-- name: q\nSELECT id, title FROM pages WHERE id = $id;\n").queries[0];
    let c = describe_named(&other, q).expect("descripteur");
    assert_ne!(a.fingerprint, c.fingerprint);
}

#[test]
fn refus_explicites() {
    let results = describe(
        "-- name: untyped\nSELECT $x;\n\n\
         -- name: unknown_column\nSELECT nope FROM pages;\n\n\
         -- name: declared_none\n-- :none\nSELECT 1;\n\n\
         -- name: wrong :none\nSELECT id FROM pages;\n\n\
         -- name: narrowed :one\nSELECT id FROM pages WHERE site = $site;\n\n\
         -- name: narrowed_optional :one\nSELECT id FROM pages WHERE id = $id;\n\n\
         -- name: order\nINSERT INTO tags (page_id, tag) SELECT id, $tag FROM pages WHERE site = $site RETURNING tag;\n",
    );
    let errs: Vec<String> = results
        .iter()
        .map(|r| match r {
            Ok(d) => format!("ok {}", d.cardinality.as_str()),
            Err(e) => e.join(" | "),
        })
        .collect();
    assert_eq!(
        errs,
        [
            "query-parameter-untyped: le type du paramètre $x n'est pas établi par son usage",
            "column-not-found: column \"nope\" does not exist",
            "ok one",
            "query-cardinality-mismatch: déclarée `none`, mais la requête renvoie des lignes (inférée : `many`)",
            "ok one",
            "ok one",
            "ok many",
        ]
    );
    // Paramètres dans l'ordre de la réécriture positionnelle (première
    // apparition), pas dans l'ordre d'analyse (WHERE avant SELECT).
    let order = results.last().unwrap().as_ref().unwrap();
    assert_eq!(
        order.sql,
        "INSERT INTO tags (page_id, tag) SELECT id, $1 FROM pages WHERE site = $2 RETURNING tag"
    );
    let names: Vec<&str> = order.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["tag", "site"]);
}

#[test]
fn format_json() {
    let d = one("SELECT id, slug, status, creation_date FROM pages WHERE id = $id");
    let mut json = serde_json::to_value(&d).expect("json");
    // L'empreinte dépend du pack PostgreSQL : vérifiée à part.
    json["fingerprint"] = serde_json::json!("…");
    insta::assert_snapshot!(serde_json::to_string_pretty(&json).expect("json"));
}
