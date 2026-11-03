use pgweave_codegen::generate_ts_types_from_sql;

#[test]
fn generates_interface_for_returns_table() {
    let sql = "
        CREATE OR REPLACE FUNCTION project_summary(org_id UUID)
        RETURNS TABLE(
            project_id UUID,
            project_name TEXT,
            total_tasks INTEGER,
            completion_rate NUMERIC
        )
        LANGUAGE sql STABLE AS $$ SELECT NULL::UUID, ''::TEXT, 0, 0.0 $$;
    ";
    let ts = generate_ts_types_from_sql(sql);
    println!("{}", ts);
    assert!(
        ts.contains("export interface ProjectSummaryRow {"),
        "expected ProjectSummaryRow interface, got:\n{}",
        ts
    );
    // A function result column may be null; `numeric` is rendered as a
    // string by node-postgres.
    for field in [
        "project_id: string | null;",
        "project_name: string | null;",
        "total_tasks: number | null;",
        "completion_rate: string | null;",
    ] {
        assert!(ts.contains(field), "{field} expected:\n{ts}");
    }
}

#[test]
fn generates_interface_for_named_query() {
    let sql = "
        CREATE TABLE users (id UUID PRIMARY KEY, email TEXT NOT NULL, active BOOLEAN);

        -- @name: UserRecord
        SELECT id, email, active FROM users WHERE id = id;
    ";
    let ts = generate_ts_types_from_sql(sql);
    println!("{}", ts);
    assert!(
        ts.contains("export interface UserRecord {"),
        "expected UserRecord interface, got:\n{}",
        ts
    );
    // Column nullability: NOT NULL (and primary key) or not.
    for field in ["id: string;", "email: string;", "active: boolean | null;"] {
        assert!(ts.contains(field), "{field} expected:\n{ts}");
    }
}

#[test]
fn an_invalid_query_is_not_guessed() {
    let sql = "
        CREATE TABLE users (id UUID PRIMARY KEY);

        -- @name: Broken
        SELECT nope FROM users;
    ";
    let ts = generate_ts_types_from_sql(sql);
    assert!(
        ts.contains("// Broken: untyped (column \"nope\" does not exist)"),
        "{ts}"
    );
    assert!(!ts.contains("export interface Broken"), "{ts}");
}

#[test]
fn enums_and_arrays() {
    let sql = "
        CREATE TYPE mood AS ENUM ('sad', 'ok');
        CREATE TABLE t (m mood NOT NULL, tags text[] NOT NULL, n bigint NOT NULL);

        -- @name: Row
        SELECT m, tags, n FROM t;
    ";
    let ts = generate_ts_types_from_sql(sql);
    for field in ["m: 'sad' | 'ok';", "tags: (string | null)[];", "n: string;"] {
        assert!(ts.contains(field), "{field} expected:\n{ts}");
    }
}

#[test]
fn no_interfaces_emits_comment() {
    let sql = "CREATE TABLE events (id UUID PRIMARY KEY, payload JSONB);";
    let ts = generate_ts_types_from_sql(sql);
    assert!(
        ts.contains("No named queries"),
        "expected no-interfaces comment"
    );
}
