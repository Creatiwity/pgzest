use pgsql_codegen::generate_ts_types_from_sql;

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
    assert!(ts.contains("export interface ProjectSummaryRow {"),
        "expected ProjectSummaryRow interface, got:\n{}", ts);
    assert!(ts.contains("project_id: string;"), "expected project_id: string");
    assert!(ts.contains("project_name: string;"), "expected project_name: string");
    assert!(ts.contains("total_tasks: number;"), "expected total_tasks: number");
    assert!(ts.contains("completion_rate: number;"), "expected completion_rate: number");
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
    assert!(ts.contains("export interface UserRecord {"),
        "expected UserRecord interface, got:\n{}", ts);
}

#[test]
fn no_interfaces_emits_comment() {
    let sql = "CREATE TABLE events (id UUID PRIMARY KEY, payload JSONB);";
    let ts = generate_ts_types_from_sql(sql);
    assert!(ts.contains("No named queries"), "expected no-interfaces comment");
}
