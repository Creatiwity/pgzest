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
            "INSERT INTO pages (slug, title, site, path) VALUES ($s, $t, 1, '/') \
             ON CONFLICT (slug) DO UPDATE SET title = excluded.title RETURNING id",
            One,
        ),
        (
            "INSERT INTO pages (slug, title, site, path) VALUES ($s, $t, 1, '/') \
             ON CONFLICT (slug) DO UPDATE SET title = excluded.title WHERE pages.title <> excluded.title RETURNING id",
            Optional,
        ),
        (
            "INSERT INTO pages (slug, title, site, path) VALUES ($s, $t, 1, '/') ON CONFLICT DO NOTHING RETURNING id",
            Optional,
        ),
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

#[test]
fn copy_from_stdin() {
    let cat = ProjectCatalog::from_sql(
        "CREATE TABLE g (id INT PRIMARY KEY, label TEXT, twice INT GENERATED ALWAYS AS (id * 2) STORED);",
    )
    .expect("schéma");
    let file = parse_query_file(
        "-- name: all_columns\nCOPY g FROM STDIN (FORMAT csv, HEADER true);\n\n\
         -- name: generated\nCOPY g (id, twice) FROM STDIN;\n",
    );
    let d = describe_named(&cat, &file.queries[0]).expect("descripteur");
    assert_eq!(d.command, Command::CopyIn);
    assert_eq!(d.cardinality, Cardinality::None);
    let cols: Vec<&str> = d.columns.iter().map(|c| c.name.as_str()).collect();
    // Une colonne générée n'est pas alimentée par COPY.
    assert_eq!(cols, ["id", "label"]);
    let copy = d.copy.expect("format");
    assert_eq!(
        (copy.table.as_str(), copy.format.as_str(), copy.header),
        ("public.g", "csv", true)
    );
    let err = describe_named(&cat, &file.queries[1]).expect_err("colonne générée");
    assert_eq!(err[0].message, "column \"twice\" is a generated column");
}

#[test]
fn index_unique_et_noms_quotes() {
    let cat = ProjectCatalog::from_sql(
        "CREATE TABLE \"User\" (id UUID PRIMARY KEY, email TEXT NOT NULL, \"orgId\" UUID, nick TEXT);\n\
         CREATE UNIQUE INDEX \"User_email_key\" ON \"User\"(email);\n\
         CREATE UNIQUE INDEX \"User_nick_key\" ON \"User\"(nick) WHERE nick IS NOT NULL;\n\
         CREATE UNIQUE INDEX \"User_lower_key\" ON \"User\"(lower(email));",
    )
    .expect("schéma");
    let file = parse_query_file(
        "-- name: by_email\nSELECT id FROM \"User\" WHERE email = $email;\n\n\
         -- name: by_nick\nSELECT id FROM \"User\" WHERE nick = $nick;\n",
    );
    let by_email = describe_named(&cat, &file.queries[0]).expect("descripteur");
    // Index unique simple : une clé (les `@unique` de Prisma).
    assert_eq!(by_email.cardinality, Cardinality::Optional);
    assert_eq!(by_email.depends_on, ["public.\"User\""]);
    // Index partiel : pas une clé pour toute égalité.
    let by_nick = describe_named(&cat, &file.queries[1]).expect("descripteur");
    assert_eq!(by_nick.cardinality, Cardinality::Many);
    let q = pgweave_query::descriptor::parse_qualified("public.\"User\"").expect("nom");
    assert_eq!((q.schema.as_str(), q.name.as_str()), ("public", "User"));
}

// ── Fragments et lignes de référence (P6) ─────────────────────────────────────

fn fragments(defs: &str) -> pgweave_macro::MacroSet {
    let mut set = pgweave_macro::MacroSet::builtin();
    let (defs, errors) = pgweave_macro::parse_fragment_file(defs, "f.sql");
    assert!(errors.is_empty(), "{errors:?}");
    for d in defs {
        set.insert_fragment(d).unwrap();
    }
    set
}

#[test]
fn fragments_inseres_parametres_compris() {
    let set = fragments(
        "-- fragment: card(p)\np.id, p.title, p.status\n\
         -- fragment: of_site(p)\np.site = $site AND p.status = $status",
    );
    let src = "-- name: cards :many\nSELECT @card(x) FROM pages x WHERE @of_site(x) AND x.title <> $title;\n";
    let file = pgweave_query::parse_query_file_with(src, &set);
    assert!(file.diagnostics.is_empty(), "{:?}", file.diagnostics);
    let d = describe_named(&catalog(), &file.queries[0]).expect("décrite");
    assert_eq!(
        d.sql,
        "SELECT x.id, x.title, x.status FROM pages x WHERE (x.site = $1 AND x.status = $2) AND x.title <> $3"
    );
    let params: Vec<&str> = d.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(params, vec!["site", "status", "title"]);
    let cols: Vec<&str> = d.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, vec!["id", "title", "status"]);
}

#[test]
fn erreur_dans_un_fragment_rapportee_a_l_invocation() {
    let set = fragments("-- fragment: bad(p)\np.nope");
    let src = "-- name: q\nSELECT @bad(x) FROM pages x;\n";
    let file = pgweave_query::parse_query_file_with(src, &set);
    let errs = describe_named(&catalog(), &file.queries[0]).unwrap_err();
    let span = errs[0].span.clone().expect("position");
    assert_eq!(&src[span.start..span.end], "@bad(x)", "{errs:?}");
}

#[test]
fn ligne_de_reference_verifiee() {
    let file = parse_query_file(
        "-- name: card :optional\nSELECT id, title, creation_date FROM pages WHERE id = $id;\n\
         -- name: same :many -> card\nSELECT id, title, creation_date FROM pages;\n\
         -- name: renamed :many -> card\nSELECT id, slug, creation_date FROM pages;\n\
         -- name: widened :many -> card\nSELECT p.id, t.tag AS title, p.creation_date FROM pages p JOIN tags t ON t.page_id = p.id;\n\
         -- name: nullable :many -> card\nSELECT p.id, p.title, p.creation_date FROM tags t LEFT JOIN pages p ON p.id = t.page_id;\n\
         -- name: chained :many -> same\nSELECT id, title, creation_date FROM pages;\n\
         -- name: missing -> nope\nSELECT 1 AS x;\n",
    );
    assert!(file.diagnostics.is_empty(), "{:?}", file.diagnostics);
    assert_eq!(file.queries[1].row.as_ref().unwrap().0, "card");
    let cat = catalog();
    let all: Vec<Descriptor> = file
        .queries
        .iter()
        .map(|q| describe_named(&cat, q).expect("décrite"))
        .collect();
    let check = |name: &str| {
        let d = all.iter().find(|d| d.name == name).unwrap();
        let target = d
            .row
            .as_ref()
            .and_then(|r| all.iter().find(|x| &x.name == r));
        pgweave_query::check_row_reference(d, target)
    };
    assert_eq!(check("same"), Ok(()));
    // `creation_date` est nullable dans la référence : une valeur toujours
    // présente s'y range ; l'inverse non.
    assert_eq!(check("widened"), Ok(()));
    assert!(
        check("renamed")
            .unwrap_err()
            .contains("`slug` là où `card` a `title`")
    );
    assert!(
        check("nullable")
            .unwrap_err()
            .contains("`id` peut être nulle ici")
    );
    assert!(check("chained").unwrap_err().contains("désigner `card`"));
    assert!(check("missing").unwrap_err().contains("introuvable"));
}
