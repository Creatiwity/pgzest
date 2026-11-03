//! DDL replay on the catalog: each case reproduces what PostgreSQL 18 does
//! with the same script (chosen names, columns, constraints, extensions).

use pgweave_catalog::model::{ConstraintKind, TypeKind, TypeRef};
use pgweave_catalog::{Catalog, QName, Replay};
use pgweave_diagnostics::Diagnostic;

fn replay(sql: &str) -> (Replay, Vec<Diagnostic>) {
    let mut r = Replay::new(Catalog::for_postgres(18).unwrap());
    let mut diags = Vec::new();
    for stmt in pgweave_parser::parse(sql).expect("parse") {
        r.apply(&stmt, &mut diags);
    }
    (r, diags)
}

fn q(name: &str) -> QName {
    QName::new("public", name)
}

fn constraint_names(r: &Replay, table: &str) -> Vec<String> {
    r.catalog.relations[&q(table)]
        .constraints
        .iter()
        .map(|c| c.name.clone())
        .collect()
}

#[test]
fn constraint_names_chosen_like_postgres() {
    let (r, d) = replay(
        "CREATE TABLE org (id SERIAL PRIMARY KEY, code TEXT UNIQUE, CHECK (code <> ''));\n\
         CREATE TABLE member (id BIGINT, org_id INT REFERENCES org, email TEXT,\n\
           UNIQUE (org_id, email), CONSTRAINT explicit_name CHECK (id > 0));",
    );
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(
        constraint_names(&r, "org"),
        ["org_pkey", "org_code_key", "org_code_check"]
    );
    assert_eq!(
        constraint_names(&r, "member"),
        [
            "member_org_id_fkey",
            "member_org_id_email_key",
            "explicit_name"
        ]
    );
    // The FK without referenced columns targets the primary key.
    let fk = &r.catalog.relations[&q("member")].constraints[0];
    assert!(
        matches!(&fk.kind, ConstraintKind::ForeignKey { ref_columns, .. } if ref_columns == &["id"])
    );
}

#[test]
fn serial_creates_its_sequence() {
    let (r, _) = replay("CREATE TABLE t (id SERIAL, n BIGSERIAL);");
    let t = &r.catalog.relations[&q("t")];
    assert_eq!(t.columns[0].ty, TypeRef::named(QName::pg_catalog("int4")));
    assert!(t.columns[0].not_null);
    assert_eq!(
        t.columns[0].default.as_deref(),
        Some("nextval('t_id_seq'::regclass)")
    );
    assert!(r.catalog.sequences.contains_key(&q("t_n_seq")));
}

#[test]
fn default_null_stored_only_if_coercion_leaves_a_node() {
    // Measured on PostgreSQL 18 (`pg_attrdef`).
    let (r, d) = replay(
        "CREATE DOMAIN dd AS int;\n\
         CREATE TABLE t (a int DEFAULT NULL, b varchar(1) DEFAULT NULL, c text DEFAULT NULL,\n\
           d varchar DEFAULT NULL, e numeric(5,2) DEFAULT NULL, f varchar(2)[] DEFAULT NULL,\n\
           g dd DEFAULT NULL, h int[] DEFAULT NULL, i varchar(1) DEFAULT NULL::text,\n\
           j int DEFAULT NULL::int, k bigint DEFAULT NULL::int,\n\
           m varchar(1) DEFAULT NULL::varchar(1));\n\
         ALTER TABLE t ADD COLUMN l varchar(1) DEFAULT NULL;\n\
         ALTER TABLE t ALTER COLUMN a SET DEFAULT NULL::bigint;",
    );
    assert!(d.is_empty(), "{d:?}");
    let stored: Vec<&str> = r.catalog.relations[&q("t")]
        .columns
        .iter()
        .filter(|c| c.default.is_some())
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(stored, ["a", "b", "e", "f", "g", "i", "k", "m", "l"]);
}

#[test]
fn alter_table_follows_postgres() {
    let (r, d) = replay(
        "CREATE TABLE t (a INT, b TEXT);\n\
         ALTER TABLE t ADD COLUMN c INT NOT NULL DEFAULT 0, ALTER COLUMN b SET NOT NULL;\n\
         ALTER TABLE t RENAME COLUMN a TO id;\n\
         ALTER TABLE t ADD PRIMARY KEY (id);\n\
         ALTER TABLE t RENAME TO u;\n\
         ALTER TABLE u DROP COLUMN c;",
    );
    assert!(d.is_empty(), "{d:?}");
    let u = &r.catalog.relations[&q("u")];
    let cols: Vec<_> = u
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.not_null))
        .collect();
    assert_eq!(cols, [("id", true), ("b", true)]);
    // The constraint keeps the name chosen at its creation.
    assert_eq!(constraint_names(&r, "u"), ["t_pkey"]);
    assert!(!r.catalog.relations.contains_key(&q("t")));
}

#[test]
fn enum_and_arrays() {
    let (r, d) = replay(
        "CREATE TYPE mood AS ENUM ('sad', 'ok');\n\
         ALTER TYPE mood ADD VALUE 'happy';\n\
         ALTER TYPE mood ADD VALUE 'meh' BEFORE 'ok';\n\
         CREATE TABLE t (m mood, ms mood[]);",
    );
    assert!(d.is_empty(), "{d:?}");
    let TypeKind::Enum { labels } = &r.catalog.types[&q("mood")].kind else {
        panic!()
    };
    assert_eq!(labels, &["sad", "meh", "ok", "happy"]);
    assert!(r.catalog.types.contains_key(&q("_mood")));
    let t = &r.catalog.relations[&q("t")];
    assert_eq!(
        t.columns[1].ty,
        TypeRef::Array(Box::new(TypeRef::named(q("mood"))))
    );
}

#[test]
fn unknown_type_is_reported_not_guessed() {
    let (r, d) = replay("CREATE TABLE t (a citext);");
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code, "type-not-found");
    assert!(matches!(
        r.catalog.relations[&q("t")].columns[0].ty,
        TypeRef::Unresolved(_)
    ));
}

#[test]
fn extension_brings_its_types_into_its_schema() {
    let (r, d) = replay(
        "CREATE SCHEMA ext;\n\
         CREATE EXTENSION citext SCHEMA ext;\n\
         SET search_path TO public, ext;\n\
         CREATE TABLE t (a citext);\n\
         CREATE EXTENSION hstore;",
    );
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code, "extension-unavailable");
    assert_eq!(
        r.catalog.relations[&q("t")].columns[0].ty,
        TypeRef::named(QName::new("ext", "citext"))
    );
}

#[test]
fn drop_and_dependencies() {
    let (r, d) = replay(
        "CREATE TABLE a (id INT PRIMARY KEY);\n\
         CREATE TABLE b (a_id INT REFERENCES a);\n\
         CREATE INDEX ON b (a_id);\n\
         DROP TABLE a;",
    );
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code, "dependent-objects");
    assert!(r.catalog.indexes.contains_key(&q("b_a_id_idx")));

    let (r, d) = replay(
        "CREATE TABLE a (id INT PRIMARY KEY);\n\
         CREATE TABLE b (a_id INT REFERENCES a);\n\
         DROP TABLE a CASCADE;",
    );
    assert!(d.is_empty(), "{d:?}");
    assert!(r.catalog.relations[&q("b")].constraints.is_empty());
}

#[test]
fn overloads_and_drop_function() {
    let (r, d) = replay(
        "CREATE FUNCTION f(a INT) RETURNS INT LANGUAGE sql AS 'SELECT a';\n\
         CREATE FUNCTION f(a TEXT) RETURNS TEXT LANGUAGE sql AS 'SELECT a';\n\
         DROP FUNCTION f(int);",
    );
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(r.catalog.routines[&q("f")].len(), 1);

    let (_, d) = replay(
        "CREATE FUNCTION f(a INT) RETURNS INT LANGUAGE sql AS 'SELECT a';\n\
         CREATE FUNCTION f(a TEXT) RETURNS TEXT LANGUAGE sql AS 'SELECT a';\n\
         DROP FUNCTION f;",
    );
    assert_eq!(d[0].code, "ambiguous-routine");
}

#[test]
fn temporary_tables_in_pg_temp() {
    let (r, _) = replay("CREATE TEMP TABLE tmp (a INT);");
    assert!(
        r.catalog
            .relations
            .contains_key(&QName::new("pg_temp", "tmp"))
    );
}

#[test]
fn do_blocks_guarded_by_the_catalog() {
    let (r, d) = replay(
        "CREATE TABLE a (id INT PRIMARY KEY);\n\
         CREATE TABLE b (a_id INT);\n\
         DO $$ BEGIN\n\
           IF NOT EXISTS (SELECT 1 FROM information_schema.table_constraints\n\
                          WHERE constraint_name = 'b_a_fk' AND table_name = 'b') THEN\n\
             ALTER TABLE b ADD CONSTRAINT b_a_fk FOREIGN KEY (a_id) REFERENCES a (id);\n\
           END IF;\n\
         END $$;\n\
         DO $$ BEGIN\n\
           IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'citext') THEN RETURN; END IF;\n\
           BEGIN CREATE EXTENSION citext; EXCEPTION WHEN OTHERS THEN RAISE EXCEPTION 'no'; END;\n\
         END $$;\n\
         CREATE TYPE e AS ENUM ('X', 'OLD');\n\
         DO $$ BEGIN\n\
           IF EXISTS (SELECT 1 FROM pg_type t JOIN pg_enum v ON v.enumtypid = t.oid\n\
                      WHERE t.typname = 'e' AND v.enumlabel = 'OLD') THEN\n\
             ALTER TYPE e RENAME VALUE 'OLD' TO 'NEW';\n\
           END IF;\n\
           IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'e') THEN\n\
             CREATE TYPE e AS ENUM ('never');\n\
           END IF;\n\
         END $$;",
    );
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(constraint_names(&r, "b"), ["b_a_fk"]);
    assert!(r.catalog.extensions.contains_key("citext"));
    let TypeKind::Enum { labels } = &r.catalog.types[&q("e")].kind else {
        panic!()
    };
    assert_eq!(labels, &["X", "NEW"]);
}

#[test]
fn non_evaluable_condition_is_reported() {
    let (r, d) = replay(
        "DO $$ BEGIN\n\
           IF current_setting('x') = 'y' THEN CREATE TABLE t (a INT); END IF;\n\
         END $$;\n\
         DO $$ BEGIN EXECUTE 'CREATE TABLE u (a INT)'; END $$;",
    );
    assert_eq!(d.len(), 2, "{d:?}");
    assert!(d.iter().all(|x| x.code == "ddl-not-replayed"));
    assert!(!r.catalog.relations.contains_key(&q("t")));
}
