//! Statement checking by sema: queries, routine bodies.

use pgweave_catalog::ProjectCatalog;

/// Diagnostics of a script: catalog replayed from the script, then each
/// statement checked against the final state.
fn check(sql: &str) -> Vec<String> {
    let stmts = pgweave_parser::parse(sql).expect("parse");
    let p = ProjectCatalog::from_statements(&stmts);
    pgweave_sema::check::check_statements(&p.catalog, &p.search_path, &stmts)
        .into_iter()
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect()
}

const SCHEMA: &str = "
CREATE TABLE users (id int PRIMARY KEY, name text NOT NULL, email text);
CREATE TABLE posts (id int PRIMARY KEY, user_id int REFERENCES users, title text);
";

#[test]
fn valid_queries_without_diagnostic() {
    let sql = format!(
        "{SCHEMA}
        SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id WHERE u.id = $1;
        INSERT INTO posts (id, user_id, title) VALUES (1, 2, 'x');
        UPDATE users SET email = NULL WHERE id = 3;
        DELETE FROM posts WHERE user_id IN (SELECT id FROM users);
        SELECT public.users.name FROM public.users;"
    );
    assert_eq!(check(&sql), Vec::<String>::new());
}

#[test]
fn type_compatibility() {
    let sql = format!("{SCHEMA} SELECT * FROM users WHERE name = 42;");
    assert_eq!(
        check(&sql),
        vec!["function-not-found: operator does not exist: text = integer"]
    );
}

#[test]
fn unknown_column_and_table() {
    let sql = format!("{SCHEMA} SELECT nam FROM users; SELECT 1 FROM userz;");
    assert_eq!(
        check(&sql),
        vec![
            "column-not-found: column \"nam\" does not exist",
            "table-not-found: relation \"userz\" does not exist",
        ]
    );
}

#[test]
fn plpgsql_body_variables_and_columns() {
    let sql = format!(
        "{SCHEMA}
        CREATE FUNCTION f(p_id int) RETURNS text LANGUAGE plpgsql AS $$
        DECLARE
            v_name text;
            r record;
        BEGIN
            SELECT name INTO v_name FROM users WHERE id = p_id;
            FOR r IN SELECT id, title FROM posts WHERE user_id = p_id LOOP
                RAISE NOTICE '% %', r.id, r.title;
            END LOOP;
            IF NOT FOUND THEN
                RETURN v_nam;
            END IF;
            RETURN v_name || r.titl;
        END $$;"
    );
    assert_eq!(
        check(&sql),
        vec![
            "undeclared-variable: variable 'v_nam' is not declared",
            "column-not-found: record \"r\" has no field \"titl\"",
        ]
    );
}

#[test]
fn variable_column_conflict() {
    let sql = format!(
        "{SCHEMA}
        CREATE FUNCTION f(id int) RETURNS text LANGUAGE plpgsql AS $$
        BEGIN
            RETURN (SELECT name FROM users WHERE users.id = id);
        END $$;"
    );
    assert_eq!(
        check(&sql),
        vec![
            "ambiguous-column: column reference \"id\" is ambiguous (PL/pgSQL variable or table column)"
        ]
    );
}

#[test]
fn trigger_new_old() {
    let sql = format!(
        "{SCHEMA}
        CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            NEW.name := lower(NEW.name);
            NEW.nam := 'x';
            IF TG_OP = 'UPDATE' AND OLD.email IS DISTINCT FROM NEW.email THEN
                RETURN NEW;
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER t BEFORE INSERT OR UPDATE ON users FOR EACH ROW EXECUTE FUNCTION touch();"
    );
    assert_eq!(
        check(&sql),
        vec!["column-not-found: record \"new\" has no field \"nam\" (type users)"]
    );
}

#[test]
fn sql_function_parameters() {
    let sql = format!(
        "{SCHEMA}
        CREATE FUNCTION g(uid int) RETURNS SETOF posts LANGUAGE sql AS $$
            SELECT * FROM posts WHERE user_id = uid AND title = $2
        $$;"
    );
    assert_eq!(
        check(&sql),
        vec!["parameter-not-found: there is no parameter $2"]
    );
}

/// Table expressions typed in their context; each case is refused
/// (or accepted) by PostgreSQL 18 with this message.
#[test]
fn table_expressions() {
    assert_eq!(
        check(
            "CREATE TABLE ok1 (a int DEFAULT 1 + 1, b text DEFAULT 'x', c timestamptz DEFAULT now(), \
             d int GENERATED ALWAYS AS (a * 2) STORED, e text GENERATED ALWAYS AS (a > 1) STORED, \
             CHECK (a > 0 AND b <> ''));
             CREATE DOMAIN pos AS int CHECK (VALUE > 0);
             ALTER TABLE ok1 ALTER COLUMN b SET DEFAULT 'y', ADD COLUMN f int DEFAULT 0 CHECK (f >= 0);"
        ),
        Vec::<String>::new()
    );
    for (sql, expected) in [
        (
            "CREATE TABLE e1 (a int DEFAULT 'x'::text)",
            "type-mismatch: column \"a\" is of type integer but default expression is of type text",
        ),
        (
            "CREATE TABLE e2 (a int, b int DEFAULT a)",
            "column-not-found: cannot use column reference in DEFAULT expression",
        ),
        (
            "CREATE TABLE e4 (a int, b int GENERATED ALWAYS AS (a + 1) STORED, c int GENERATED ALWAYS AS (b + 1) STORED)",
            "generated-column: cannot use generated column \"b\" in column generation expression",
        ),
        (
            "CREATE TABLE e5 (a int CHECK (a))",
            "type-mismatch: argument must be type boolean, not type integer",
        ),
        (
            "CREATE TABLE e6 (a text, CHECK (a > 5))",
            "function-not-found: operator does not exist: text > integer",
        ),
        (
            "CREATE DOMAIN bad AS text CHECK (VALUE > 0)",
            "function-not-found: operator does not exist: text > integer",
        ),
        (
            "CREATE TABLE e7 (a int); ALTER TABLE e7 ALTER COLUMN a SET DEFAULT now()",
            "type-mismatch: column \"a\" is of type integer but default expression is of type timestamp with time zone",
        ),
    ] {
        assert_eq!(check(sql), vec![expected.to_string()], "{sql}");
    }
}

#[test]
fn default_outside_assignment() {
    let sql = format!(
        "{SCHEMA}
        SELECT default FROM users;
        INSERT INTO posts (id, user_id, title) VALUES (1, DEFAULT, DEFAULT);
        UPDATE posts SET title = DEFAULT WHERE id = 1;"
    );
    assert_eq!(
        check(&sql),
        vec!["default-not-allowed: DEFAULT is not allowed in this context"]
    );
}

#[test]
fn index_checked_in_the_context_of_its_table() {
    let d = check(
        "CREATE TABLE t (a int, c text);
         CREATE INDEX t_ok ON t (a DESC NULLS LAST, (lower(c))) INCLUDE (c) WHERE a > 0;
         CREATE INDEX t_b ON t (b);
         CREATE INDEX t_i ON t (a) INCLUDE (zz);
         CREATE INDEX t_x ON t ((lower(zz)));
         CREATE INDEX t_w ON t (a) WHERE c;
         CREATE INDEX t_g ON t USING gin (c gin_trgm_ops);",
    );
    assert_eq!(
        d,
        vec![
            "column-not-found: column \"b\" does not exist",
            "column-not-found: column \"zz\" does not exist",
            "column-not-found: column \"zz\" does not exist",
            "type-mismatch: argument must be type boolean, not type text",
            "object-not-found: operator class \"gin_trgm_ops\" does not exist for access method \"gin\"",
        ],
        "{d:?}"
    );
    // Provided by pg_trgm: known once the extension is created.
    assert!(
        check(
            "CREATE EXTENSION pg_trgm;
             CREATE TABLE t (c text);
             CREATE INDEX t_g ON t USING gin (c gin_trgm_ops);"
        )
        .is_empty()
    );
}
