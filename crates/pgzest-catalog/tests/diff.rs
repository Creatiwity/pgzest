//! Migration diff (PLAN.md P7): each generated migration is replayed on the
//! starting state, and the resulting state must be exactly the target.

use pgzest_catalog::ProjectCatalog;
use pgzest_catalog::diff::{Migration, diff};
use pgzest_diagnostics::Severity;

fn replay(sql: &str) -> ProjectCatalog {
    let p = ProjectCatalog::from_sql(sql).unwrap_or_else(|e| panic!("parse: {e}\n{sql}"));
    let errors: Vec<_> = p
        .diagnostics
        .iter()
        .filter(|(_, d)| d.severity == Severity::Error)
        .map(|(_, d)| d.message.clone())
        .collect();
    assert!(errors.is_empty(), "replay failed: {errors:?}\n{sql}");
    p
}

/// Migration from `old` to `new`, checked by replay.
fn migrate(old: &str, new: &str) -> Migration {
    let before = replay(old);
    let target = replay(new);
    let m = diff(&before.catalog, &target.catalog);
    assert!(m.unsupported.is_empty(), "unsupported: {:?}", m.unsupported);
    let applied = replay(&format!("{old}\n{}", m.to_sql()));
    let rest = diff(&applied.catalog, &target.catalog);
    assert!(
        rest.is_empty(),
        "the migration does not reach the target.\n--- migration\n{}\n--- rest\n{}\n{:?}",
        m.to_sql(),
        rest.to_sql(),
        rest.unsupported
    );
    m
}

fn sql(m: &Migration) -> String {
    m.to_sql()
}

#[test]
fn identical_schema_produces_nothing() {
    let s = "CREATE TYPE k AS ENUM ('a');\nCREATE TABLE t (id BIGINT PRIMARY KEY, k k);\n\
             CREATE VIEW v AS SELECT id FROM t;\nCREATE INDEX t_k ON t (k);";
    assert!(migrate(s, s).is_empty());
}

#[test]
fn new_table_with_foreign_key_and_index() {
    let m = migrate(
        "CREATE TABLE a (id BIGINT PRIMARY KEY);",
        "CREATE TABLE a (id BIGINT PRIMARY KEY);\n\
         CREATE TABLE b (id BIGINT PRIMARY KEY, a_id BIGINT NOT NULL REFERENCES a (id));\n\
         CREATE INDEX b_a_id ON b (a_id);",
    );
    let s = sql(&m);
    let create = s.find("CREATE TABLE b").unwrap();
    let index = s.find("CREATE INDEX b_a_id").unwrap();
    let fk = s.find("ADD CONSTRAINT b_a_id_fkey").unwrap();
    assert!(create < index && index < fk, "{s}");
}

#[test]
fn columns_added_changed_removed() {
    let m = migrate(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b TEXT DEFAULT 'x', gone TEXT);",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a BIGINT NOT NULL, b TEXT, c TEXT DEFAULT 'y');",
    );
    let s = sql(&m);
    assert!(s.contains("ALTER COLUMN a SET DATA TYPE bigint"), "{s}");
    assert!(s.contains("ALTER COLUMN a SET NOT NULL"), "{s}");
    assert!(s.contains("ALTER COLUMN b DROP DEFAULT"), "{s}");
    assert!(s.contains("ADD COLUMN c text DEFAULT 'y'"), "{s}");
    let drop = m
        .steps
        .iter()
        .find(|st| st.sql.contains("DROP COLUMN gone"))
        .unwrap();
    assert!(drop.is_destructive());
    let codes: Vec<&str> = m
        .steps
        .iter()
        .flat_map(|st| st.risks.iter().map(|r| r.code))
        .collect();
    assert!(
        codes.contains(&"M004") && codes.contains(&"M005"),
        "{codes:?}"
    );
    // The drop comes last.
    assert!(m.steps.last().unwrap().sql.contains("DROP COLUMN gone"));
}

#[test]
fn enum_values_added_in_place() {
    let m = migrate(
        "CREATE TYPE s AS ENUM ('b', 'd');",
        "CREATE TYPE s AS ENUM ('a', 'b', 'c', 'd', 'e');",
    );
    let s = sql(&m);
    assert!(s.contains("ADD VALUE 'a' BEFORE 'b'"), "{s}");
    assert!(s.contains("ADD VALUE 'c' AFTER 'b'"), "{s}");
    assert!(s.contains("ADD VALUE 'e' AFTER 'd'"), "{s}");
    assert!(!m.notes.is_empty());
}

#[test]
fn removed_enum_value_unsupported() {
    let before = replay("CREATE TYPE s AS ENUM ('a', 'b');");
    let after = replay("CREATE TYPE s AS ENUM ('a');");
    let m = diff(&before.catalog, &after.catalog);
    assert_eq!(m.unsupported.len(), 1, "{m:?}");
    assert!(m.steps.is_empty());
}

#[test]
fn extension_and_schema_before_what_depends_on_them() {
    let m = migrate(
        "CREATE TABLE t (id BIGINT PRIMARY KEY);",
        "CREATE SCHEMA app;\nCREATE EXTENSION citext;\n\
         CREATE TABLE t (id BIGINT PRIMARY KEY, email citext);\n\
         CREATE TABLE app.u (id BIGINT PRIMARY KEY);",
    );
    let s = sql(&m);
    let schema = s.find("CREATE SCHEMA app").unwrap();
    let ext = s.find("CREATE EXTENSION IF NOT EXISTS citext").unwrap();
    let col = s.find("ADD COLUMN email").unwrap();
    assert!(schema < ext && ext < col, "{s}");
}

#[test]
fn extension_removed_last() {
    let m = migrate(
        "CREATE EXTENSION citext;\nCREATE TABLE t (id BIGINT PRIMARY KEY, email citext);",
        "CREATE TABLE t (id BIGINT PRIMARY KEY);",
    );
    assert!(
        m.steps.last().unwrap().sql.starts_with("DROP EXTENSION"),
        "{}",
        sql(&m)
    );
}

#[test]
fn view_with_untyped_columns_rebuilt() {
    // Without view analysis, their columns are not typed: nothing proves that
    // `CREATE OR REPLACE` would be accepted (the typed case is checked with
    // `pgzest-sema`, in the CLI).
    let m = migrate(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a TEXT);\nCREATE VIEW v AS SELECT id FROM t;",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a TEXT);\nCREATE VIEW v AS SELECT id, a FROM t;",
    );
    let s = sql(&m);
    assert!(
        s.contains("DROP VIEW v") && s.contains("CREATE VIEW v"),
        "{s}"
    );
}

#[test]
fn view_rebuilt_when_its_table_changes_type() {
    let m = migrate(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a INT);\n\
         CREATE VIEW v AS SELECT a FROM t;\nCREATE VIEW w AS SELECT a FROM v;",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a BIGINT);\n\
         CREATE VIEW v AS SELECT a FROM t;\nCREATE VIEW w AS SELECT a FROM v;",
    );
    let s = sql(&m);
    // w depends on v: dropped first, recreated after.
    let drop_w = s.find("DROP VIEW w").unwrap();
    let drop_v = s.find("DROP VIEW v").unwrap();
    let alter = s.find("SET DATA TYPE bigint").unwrap();
    let create_v = s.find("CREATE VIEW v").unwrap();
    let create_w = s.find("CREATE VIEW w").unwrap();
    assert!(
        drop_w < drop_v && drop_v < alter && alter < create_v && create_v < create_w,
        "{s}"
    );
}

#[test]
fn function_replaced_or_rebuilt() {
    let f = |ret: &str, body: &str| {
        format!(
            "CREATE TABLE t (id BIGINT PRIMARY KEY);\n\
             CREATE FUNCTION f(x INT) RETURNS {ret} LANGUAGE sql AS $$ SELECT {body} $$;"
        )
    };
    let m = migrate(&f("INT", "x"), &f("INT", "x + 1"));
    assert!(
        sql(&m).contains("CREATE OR REPLACE FUNCTION"),
        "{}",
        sql(&m)
    );

    let m = migrate(&f("INT", "x"), &f("BIGINT", "x::bigint"));
    let s = sql(&m);
    let drop = s.find("DROP FUNCTION f(integer)").unwrap();
    let create = s.find("CREATE FUNCTION f").unwrap();
    assert!(drop < create, "{s}");
}

#[test]
fn function_removed_after_the_trigger_using_it() {
    let m = migrate(
        "CREATE TABLE t (id BIGINT PRIMARY KEY);\n\
         CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;\n\
         CREATE TRIGGER t_trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();",
        "CREATE TABLE t (id BIGINT PRIMARY KEY);",
    );
    let s = sql(&m);
    let trigger = s.find("DROP TRIGGER t_trg ON t").unwrap();
    let function = s.find("DROP FUNCTION trg()").unwrap();
    assert!(trigger < function, "{s}");
}

#[test]
fn table_removed_after_the_foreign_key_targeting_it() {
    let m = migrate(
        "CREATE TABLE a (id BIGINT PRIMARY KEY);\n\
         CREATE TABLE b (id BIGINT PRIMARY KEY, a_id BIGINT REFERENCES a (id));",
        "CREATE TABLE b (id BIGINT PRIMARY KEY, a_id BIGINT);",
    );
    let s = sql(&m);
    let fk = s.find("DROP CONSTRAINT b_a_id_fkey").unwrap();
    let table = s.find("DROP TABLE a").unwrap();
    assert!(fk < table, "{s}");
    assert!(
        m.steps
            .iter()
            .any(|st| st.sql.contains("DROP TABLE a") && st.is_destructive())
    );
}

#[test]
fn constraints_policies_and_row_level_security() {
    migrate(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n INT, owner TEXT);\n\
         CREATE POLICY p ON t USING (owner = current_user);",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n INT CHECK (n > 0), owner TEXT);\n\
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;\n\
         CREATE POLICY p ON t FOR SELECT USING (owner = current_user);",
    );
}

#[test]
fn comments_set_and_removed() {
    let m = migrate(
        "CREATE TABLE t (id BIGINT PRIMARY KEY);\nCOMMENT ON TABLE t IS 'before';",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a TEXT);\nCOMMENT ON COLUMN t.a IS 'new';",
    );
    let s = sql(&m);
    assert!(s.contains("COMMENT ON COLUMN t.a IS 'new'"), "{s}");
    assert!(s.contains("COMMENT ON TABLE t IS NULL"), "{s}");
}

#[test]
fn changed_identity_unsupported() {
    let before = replay("CREATE TABLE t (id BIGINT PRIMARY KEY);");
    let after = replay("CREATE TABLE t (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY);");
    let m = diff(&before.catalog, &after.catalog);
    assert_eq!(m.unsupported.len(), 1, "{m:?}");
}

#[test]
fn serial_column_added() {
    migrate(
        "CREATE TABLE t (a TEXT);",
        "CREATE TABLE t (a TEXT, id SERIAL);",
    );
}

fn declarative(migration: &str, schema: &str) -> ProjectCatalog {
    use pgzest_catalog::project::{ProjectBuilder, parse_source};
    use pgzest_schema::FileRole;
    let macros = pgzest_expand::MacroSet::builtin();
    let mut b = ProjectBuilder::new().declarative(true);
    let m = parse_source(migration, 0, &macros);
    b.add_file(
        std::path::Path::new("migrations/0001_init.sql"),
        migration,
        &FileRole::Migration { sequence: 1 },
        m.as_ref(),
    );
    let s = parse_source(schema, 0, &macros);
    b.add_file(
        std::path::Path::new("schema/app.sql"),
        schema,
        &FileRole::Schema,
        s.as_ref(),
    );
    b.finish()
}

#[test]
fn declarative_mode_replays_migrations_separately() {
    let schema = "CREATE TABLE t (id BIGINT PRIMARY KEY, a TEXT);";
    let p = declarative("CREATE TABLE t (id BIGINT PRIMARY KEY, a TEXT);", schema);
    let codes: Vec<&str> = p.diagnostics.iter().map(|(_, d)| d.code).collect();
    assert!(
        !codes.contains(&"object-already-exists") && !codes.contains(&"schema-drift"),
        "{codes:?}"
    );
    assert!(p.history.is_some());

    let p = declarative("CREATE TABLE t (id BIGINT PRIMARY KEY);", schema);
    let drift: Vec<_> = p
        .diagnostics
        .iter()
        .filter(|(_, d)| d.code == "schema-drift")
        .collect();
    assert_eq!(drift.len(), 1, "{:?}", p.diagnostics);
    assert_eq!(
        drift[0].0,
        std::path::PathBuf::from("migrations/0001_init.sql")
    );
    // Analysis sees the target.
    assert!(
        p.catalog
            .relations
            .values()
            .any(|r| r.column("a").is_some())
    );
}

#[test]
fn function_body_kept_as_written() {
    use pgzest_catalog::project::{ProjectBuilder, parse_source};
    use pgzest_schema::FileRole;
    let written = "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql\n\
                   AS $$ SELECT x -- kept comment\n $$";
    let macros = pgzest_expand::MacroSet::builtin();
    let mut b = ProjectBuilder::new().keep_sources(true);
    let parsed = parse_source(written, 0, &macros);
    b.add_file(
        std::path::Path::new("schema.sql"),
        written,
        &FileRole::Schema,
        parsed.as_ref(),
    );
    let after = b.finish();
    let m = diff(&replay("").catalog, &after.catalog);
    assert!(sql(&m).contains("kept comment"), "{}", sql(&m));
}

#[test]
fn index_options_kept() {
    // Order, NULLS, operator class and INCLUDE are part of the index.
    let m = migrate(
        "CREATE TABLE t (a INT, b TEXT, c TEXT);",
        "CREATE TABLE t (a INT, b TEXT, c TEXT);\n\
         CREATE INDEX t_ab ON t (a DESC NULLS LAST, b text_pattern_ops) INCLUDE (c);",
    );
    assert!(
        sql(&m).contains("(a DESC NULLS LAST, b text_pattern_ops) INCLUDE (c)"),
        "{}",
        sql(&m)
    );
    let before = replay("CREATE TABLE t (a INT);\nCREATE INDEX t_a ON t (a);");
    let after = replay("CREATE TABLE t (a INT);\nCREATE INDEX t_a ON t (a DESC);");
    let m = diff(&before.catalog, &after.catalog);
    assert!(sql(&m).contains("DROP INDEX t_a"), "{}", sql(&m));
}

#[test]
fn literal_default_keeps_its_type_on_type_change() {
    // PostgreSQL converts the original constant on each insert.
    let p = replay(
        "CREATE TABLE t (d TIMESTAMPTZ DEFAULT '2023-01-01 00:00:00 +00:00');\n\
         ALTER TABLE t ALTER COLUMN d SET DATA TYPE DATE;",
    );
    let t = p
        .catalog
        .relations
        .values()
        .find(|r| r.name.name == "t")
        .unwrap();
    assert_eq!(
        t.column("d").unwrap().default.as_deref(),
        Some("CAST('2023-01-01 00:00:00 +00:00' AS TIMESTAMP WITH TIME ZONE)")
    );
}
