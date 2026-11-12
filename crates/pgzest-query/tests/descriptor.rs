//! Query descriptor (PLAN.md §4): types, nullability, cardinality,
//! dependencies, fingerprint.

use pgzest_catalog::ProjectCatalog;
use pgzest_query::descriptor::{Command, NullabilityDesc};
use pgzest_query::{Cardinality, Descriptor, describe_named, parse_query_file};

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
    ProjectCatalog::from_sql(SCHEMA).expect("schema")
}

/// Descriptors of the queries of a file, or the error messages.
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
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn cardinality(sql: &str) -> Cardinality {
    one(sql).cardinality
}

#[test]
fn inferred_cardinality() {
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
fn nullability_of_parameters_and_columns() {
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
    // Outside an assignment, a parameter's nullability is not established.
    let d = one("SELECT id FROM pages WHERE site = $site");
    assert_eq!(d.params[0].nullable, NullabilityDesc::Unknown);
    assert_eq!(d.command, Command::Select);
}

#[test]
fn nullability_of_calls() {
    let cat = ProjectCatalog::from_sql(
        "CREATE TABLE docs (data JSONB NOT NULL, ids INT[] NOT NULL, name TEXT NOT NULL,
             during TSTZRANGE NOT NULL);
         CREATE FUNCTION slug(t TEXT) RETURNS TEXT LANGUAGE sql STRICT IMMUTABLE AS 'SELECT lower(t)';",
    )
    .expect("schema");
    let parsed = parse_query_file(
        "-- name: q
         SELECT lower(name) AS lowered, name || '!' AS shouted, data ->> 'k' AS field,
                data -> 'k' AS node, data #>> '{a,b}' AS path, data @? '$.a' AS matches,
                array_position(ids, 1) AS position, regexp_match(name, 'x') AS matched,
                substring(name, 2) AS rest, substring(name, 'x+') AS run, lower(during) AS since,
                current_setting('app.user_id', true) AS setting, lag(name) OVER () AS previous,
                coalesce(data ->> 'k', '') AS defaulted, slug(name) AS project
         FROM docs;\n",
    );
    let d = describe_named(&cat, &parsed.queries[0]).expect("described");
    let columns: Vec<(&str, NullabilityDesc)> = d
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    use NullabilityDesc::*;
    assert_eq!(
        columns,
        [
            ("lowered", NonNull),
            ("shouted", NonNull),
            // `STRICT` only says NULL in, NULL out: these return NULL by
            // themselves (missing key, no match, empty range…).
            ("field", Nullable),
            ("node", Nullable),
            ("path", Nullable),
            ("matches", Nullable),
            ("position", Nullable),
            ("matched", Nullable),
            ("rest", NonNull),
            ("run", Nullable),
            ("since", Nullable),
            ("setting", Nullable),
            ("previous", Nullable),
            ("defaulted", NonNull),
            // A project function: nothing establishes its result.
            ("project", Unknown),
        ]
    );
}

#[test]
fn project_dependencies_only() {
    let d = one(
        "SELECT p.id, p.status, t.tag FROM pages p JOIN tags t ON t.page_id = p.id WHERE p.slug = lower($slug)",
    );
    assert_eq!(
        d.depends_on,
        ["public.pages", "public.status", "public.tags"]
    );
}

#[test]
fn fingerprint_stable_and_schema_sensitive() {
    let a = one("SELECT id, title FROM pages WHERE id = $id");
    // Same query, other formatting and comment: same fingerprint.
    let b = one("SELECT id,\n  title -- title\nFROM pages\nWHERE id = $id");
    assert_eq!(a.fingerprint, b.fingerprint);
    assert_eq!(a.fingerprint.len(), 64);
    // A dependency's schema changes: so does the fingerprint.
    let other =
        ProjectCatalog::from_sql(&SCHEMA.replace("title         TEXT NOT NULL", "title TEXT"))
            .expect("schema");
    let q =
        &parse_query_file("-- name: q\nSELECT id, title FROM pages WHERE id = $id;\n").queries[0];
    let c = describe_named(&other, q).expect("descriptor");
    assert_ne!(a.fingerprint, c.fingerprint);
}

#[test]
fn explicit_refusals() {
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
            "query-parameter-untyped: the type of parameter $x is not established by its usage",
            "column-not-found: column \"nope\" does not exist",
            "ok one",
            "query-cardinality-mismatch: declared `none`, but the query returns rows (inferred: `many`)",
            "ok one",
            "ok one",
            "ok many",
        ]
    );
    // Parameters in positional-rewrite order (first appearance), not in
    // analysis order (WHERE before SELECT).
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
    // The fingerprint depends on the PostgreSQL pack: checked separately.
    json["fingerprint"] = serde_json::json!("…");
    insta::assert_snapshot!(serde_json::to_string_pretty(&json).expect("json"));
}

#[test]
fn copy_from_stdin() {
    let cat = ProjectCatalog::from_sql(
        "CREATE TABLE g (id INT PRIMARY KEY, label TEXT, twice INT GENERATED ALWAYS AS (id * 2) STORED);",
    )
    .expect("schema");
    let file = parse_query_file(
        "-- name: all_columns\nCOPY g FROM STDIN (FORMAT csv, HEADER true);\n\n\
         -- name: generated\nCOPY g (id, twice) FROM STDIN;\n",
    );
    let d = describe_named(&cat, &file.queries[0]).expect("descriptor");
    assert_eq!(d.command, Command::CopyIn);
    assert_eq!(d.cardinality, Cardinality::None);
    let cols: Vec<&str> = d.columns.iter().map(|c| c.name.as_str()).collect();
    // A generated column is not loaded by COPY.
    assert_eq!(cols, ["id", "label"]);
    let copy = d.copy.expect("format");
    assert_eq!(
        (copy.table.as_str(), copy.format.as_str(), copy.header),
        ("public.g", "csv", true)
    );
    let err = describe_named(&cat, &file.queries[1]).expect_err("generated column");
    assert_eq!(err[0].message, "column \"twice\" is a generated column");
}

#[test]
fn unique_index_and_quoted_names() {
    let cat = ProjectCatalog::from_sql(
        "CREATE TABLE \"User\" (id UUID PRIMARY KEY, email TEXT NOT NULL, \"orgId\" UUID, nick TEXT);\n\
         CREATE UNIQUE INDEX \"User_email_key\" ON \"User\"(email);\n\
         CREATE UNIQUE INDEX \"User_nick_key\" ON \"User\"(nick) WHERE nick IS NOT NULL;\n\
         CREATE UNIQUE INDEX \"User_lower_key\" ON \"User\"(lower(email));",
    )
    .expect("schema");
    let file = parse_query_file(
        "-- name: by_email\nSELECT id FROM \"User\" WHERE email = $email;\n\n\
         -- name: by_nick\nSELECT id FROM \"User\" WHERE nick = $nick;\n",
    );
    let by_email = describe_named(&cat, &file.queries[0]).expect("descriptor");
    // Simple unique index: a key (Prisma's `@unique`).
    assert_eq!(by_email.cardinality, Cardinality::Optional);
    assert_eq!(by_email.depends_on, ["public.\"User\""]);
    // Partial index: not a key for every equality.
    let by_nick = describe_named(&cat, &file.queries[1]).expect("descriptor");
    assert_eq!(by_nick.cardinality, Cardinality::Many);
    let q = pgzest_query::descriptor::parse_qualified("public.\"User\"").expect("name");
    assert_eq!((q.schema.as_str(), q.name.as_str()), ("public", "User"));
}

// ── Fragments and row references (P6) ────────────────────────────────────────

fn fragments(defs: &str) -> pgzest_expand::MacroSet {
    let mut set = pgzest_expand::MacroSet::builtin();
    let (defs, errors) = pgzest_expand::parse_fragment_file(defs, "f.sql");
    assert!(errors.is_empty(), "{errors:?}");
    for d in defs {
        set.insert_fragment(d).unwrap();
    }
    set
}

#[test]
fn fragments_inserted_including_parameters() {
    let set = fragments(
        "-- fragment: card(p)\np.id, p.title, p.status\n\
         -- fragment: of_site(p)\np.site = $site AND p.status = $status",
    );
    let src = "-- name: cards :many\nSELECT @card(x) FROM pages x WHERE @of_site(x) AND x.title <> $title;\n";
    let file = pgzest_query::parse_query_file_with(src, &set);
    assert!(file.diagnostics.is_empty(), "{:?}", file.diagnostics);
    let d = describe_named(&catalog(), &file.queries[0]).expect("described");
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
fn error_in_a_fragment_reported_at_the_invocation() {
    let set = fragments("-- fragment: bad(p)\np.nope");
    let src = "-- name: q\nSELECT @bad(x) FROM pages x;\n";
    let file = pgzest_query::parse_query_file_with(src, &set);
    let errs = describe_named(&catalog(), &file.queries[0]).unwrap_err();
    let span = errs[0].span.clone().expect("position");
    assert_eq!(&src[span.start..span.end], "@bad(x)", "{errs:?}");
}

#[test]
fn row_reference_checked() {
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
        .map(|q| describe_named(&cat, q).expect("described"))
        .collect();
    let check = |name: &str| {
        let d = all.iter().find(|d| d.name == name).unwrap();
        let target = d
            .row
            .as_ref()
            .and_then(|r| all.iter().find(|x| &x.name == r));
        pgzest_query::check_row_reference(d, target)
    };
    assert_eq!(check("same"), Ok(()));
    // `creation_date` is nullable in the reference: an always-present value
    // fits in it; not the other way around.
    assert_eq!(check("widened"), Ok(()));
    assert!(
        check("renamed")
            .unwrap_err()
            .contains("`slug` where `card` has `title`")
    );
    assert!(
        check("nullable")
            .unwrap_err()
            .contains("`id` may be null here")
    );
    assert!(check("chained").unwrap_err().contains("designate `card`"));
    assert!(check("missing").unwrap_err().contains("not found"));
}

#[test]
fn optional_parameter() {
    let src = "-- name: q :many\nSELECT id FROM pages\n\
               WHERE ($status?::status IS NULL OR status = $status) AND site = $site;\n";
    let file = parse_query_file(src);
    assert!(file.diagnostics.is_empty(), "{:?}", file.diagnostics);
    let d = describe_named(&catalog(), &file.queries[0]).expect("described");
    assert_eq!(
        d.sql,
        "SELECT id FROM pages\nWHERE ($1::status IS NULL OR status = $1) AND site = $2"
    );
    let status = d.params.iter().find(|p| p.name == "status").unwrap();
    assert_eq!(status.nullable, NullabilityDesc::Nullable);
    assert_eq!(status.ty.pg, "public.status");
    let site = d.params.iter().find(|p| p.name == "site").unwrap();
    assert_ne!(site.nullable, NullabilityDesc::Nullable);

    // jsonb `?` operator surrounded by spaces: not a marker.
    let file = parse_query_file("-- name: j\nSELECT $doc::jsonb ? 'k' AS has;\n");
    let pgzest_query::Params::Named(p) = &file.queries[0].params else {
        panic!()
    };
    assert!(!p[0].optional);

    // A positional parameter is not optional.
    let file = parse_query_file("-- name: p\nSELECT $1?::int;\n");
    assert!(
        file.diagnostics
            .iter()
            .any(|d| d.message.contains("optional positional")),
        "{:?}",
        file.diagnostics
    );
    assert_eq!(
        pgzest_query::blank_optional_markers("SELECT $a?, '$b?', $c ? 'k' -- $d?"),
        "SELECT $a , '$b?', $c ? 'k' -- $d?"
    );
}

#[test]
fn query_with_variants() {
    let src = "-- name: page :many\nSELECT id, title FROM pages\nWHERE site = $site\n\
               [[ AND status = $status ]]\n\
               ORDER BY {{ sort: by_title = title | recent = creation_date DESC }}\nLIMIT $limit;\n";
    let file = parse_query_file(src);
    assert!(file.diagnostics.is_empty(), "{:?}", file.diagnostics);
    let d = describe_named(&catalog(), &file.queries[0]).expect("described");
    let dy = d.dynamic.as_ref().expect("variants");
    assert_eq!(dy.conditions, vec!["status"]);
    assert_eq!(dy.choices[0].labels, vec!["by_title", "recent"]);
    assert_eq!(dy.variants.len(), 4);
    let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(
        squash(&dy.variants[0].sql),
        "SELECT id, title FROM pages WHERE site = $1 ORDER BY title LIMIT $2"
    );
    assert_eq!(dy.variants[0].params, vec!["site", "limit"]);
    assert_eq!(
        squash(&dy.variants[3].sql),
        "SELECT id, title FROM pages WHERE site = $1 AND status = $2 ORDER BY creation_date DESC LIMIT $3"
    );
    let status = d.params.iter().find(|p| p.name == "status").unwrap();
    assert_eq!(status.nullable, NullabilityDesc::Nullable);
    assert_eq!(d.cardinality, Cardinality::Many);
}

#[test]
fn rejected_variants() {
    // A choice that changes the projection.
    let file = parse_query_file("-- name: q\nSELECT {{ col: a = id | b = title }} FROM pages;\n");
    let errs = describe_named(&catalog(), &file.queries[0]).unwrap_err();
    assert!(
        errs[0].message.contains("not return the same columns"),
        "{errs:?}"
    );
    // An error in a variant is reported with the variant as a hint.
    let file =
        parse_query_file("-- name: q\nSELECT id FROM pages WHERE true [[ AND nope = $x ]];\n");
    let errs = describe_named(&catalog(), &file.queries[0]).unwrap_err();
    assert!(
        errs[0]
            .hint
            .as_deref()
            .unwrap_or("")
            .contains("$x provided"),
        "{errs:?}"
    );

    for (sql, expected) in [
        (
            "SELECT id FROM pages WHERE true [[ AND site = $a AND status = $b ]]",
            "a single parameter",
        ),
        (
            "SELECT id FROM pages WHERE site = $a [[ AND site = $a ]]",
            "outside blocks",
        ),
        (
            "SELECT id FROM pages WHERE true [[ AND site = $a? ]]",
            "drop the `?`",
        ),
        (
            "SELECT id FROM pages WHERE true [[ AND true ]]",
            "without a parameter",
        ),
    ] {
        let file = parse_query_file(&format!("-- name: q\n{sql};\n"));
        assert!(
            file.diagnostics
                .iter()
                .any(|d| d.message.contains(expected)),
            "{sql}: {:?}",
            file.diagnostics
        );
    }
}
