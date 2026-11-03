//! `PgweaveDialect` must behave like `PostgreSqlDialect`, except for the
//! documented additions (`src/dialect.rs`).

use pgweave_parser::dialect::PgweaveDialect;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// sqlparser version for which the `DELEGATED` list was established.
const AUDITED_SQLPARSER_VERSION: &str = "0.62.0";

#[test]
fn the_sqlparser_version_is_the_audited_one() {
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
        .expect("Cargo.lock");
    let version = lock
        .split("[[package]]")
        .find(|p| p.contains("\nname = \"sqlparser\"\n"))
        .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = \"")))
        .map(|v| v.trim_end_matches('"').to_string())
        .expect("sqlparser in Cargo.lock");
    assert_eq!(
        version, AUDITED_SQLPARSER_VERSION,
        "sqlparser changed version: review the list of methods that \
         PostgreSqlDialect overrides (src/dialect.rs, DELEGATED), then update \
         AUDITED_SQLPARSER_VERSION"
    );
}

#[test]
fn meme_ast_que_postgresql_hors_complements() {
    let corpus = [
        "SELECT a, b::int, $1, $name, x ->> 'k', y @> '{}', z ~* 'a' FROM t WHERE a IS DISTINCT FROM b",
        "SELECT 1 UNION ALL SELECT 2 ORDER BY 1",
        "WITH x AS MATERIALIZED (SELECT 1) SELECT * FROM x",
        "SELECT count(*) FILTER (WHERE a > 0), array_agg(b ORDER BY c) FROM t GROUP BY ROLLUP (a)",
        "SELECT trim(' x '), trim(both 'x' from y), substring(a from 1 for 2), extract(epoch from now())",
        "SELECT a FROM t WHERE b = ANY($1) AND c ILIKE 'x%' AND d NOTNULL",
        "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = excluded.a RETURNING *",
        "UPDATE t SET a = a + 1 FROM u WHERE u.id = t.id RETURNING old.a, new.a",
        "DELETE FROM t USING u WHERE u.id = t.id",
        "CREATE TABLE t (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, a text NOT NULL DEFAULT '', b numeric CHECK (b > 0) REFERENCES u (id) ON DELETE CASCADE)",
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS i ON t USING gin (a) WHERE b",
        "CREATE TRIGGER tr AFTER INSERT ON t REFERENCING NEW TABLE AS inserted FOR EACH STATEMENT EXECUTE FUNCTION f()",
        "CREATE POLICY p ON t AS RESTRICTIVE FOR SELECT TO r USING (a = current_user)",
        "ALTER TABLE t ADD COLUMN x int, ALTER COLUMN a TYPE text USING a::text, DROP COLUMN b",
        "CREATE TYPE s AS ENUM ('a', 'b')",
        "COPY t (a, b) FROM STDIN WITH (FORMAT csv, HEADER true)",
        "SELECT E'a\\nb', U&'x', B'101', 1_000, 1e3, interval '1 day'",
    ];
    for sql in corpus {
        let ours = Parser::parse_sql(&PgweaveDialect, sql).map_err(|e| e.to_string());
        let theirs = Parser::parse_sql(&PostgreSqlDialect {}, sql).map_err(|e| e.to_string());
        assert_eq!(ours, theirs, "divergence on: {sql}");
        assert!(ours.is_ok(), "{sql} : {ours:?}");
    }
}

#[test]
fn additions_trim_and_generated_columns() {
    for sql in [
        "SELECT trim(both from ' x ')",
        "SELECT trim(leading from a), trim(from b)",
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL)",
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a * 2))",
        "CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a * 2) STORED NOT NULL)",
    ] {
        assert!(
            Parser::parse_sql(&PostgreSqlDialect {}, sql).is_err() || sql.contains("STORED"),
            "if sqlparser now reads it, the addition should be removed: {sql}"
        );
        let parsed = Parser::parse_sql(&PgweaveDialect, sql);
        assert!(parsed.is_ok(), "{sql} : {parsed:?}");
    }
}
