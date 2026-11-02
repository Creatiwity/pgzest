//! Vérification des statements par sema : requêtes, corps de routines.

use pgweave_catalog::ProjectCatalog;

/// Diagnostics d'un script : catalogue rejoué depuis le script, puis
/// chaque statement vérifié contre l'état final.
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
fn requetes_valides_sans_diagnostic() {
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
fn compatibilite_de_types() {
    let sql = format!("{SCHEMA} SELECT * FROM users WHERE name = 42;");
    assert_eq!(
        check(&sql),
        vec!["function-not-found: operator text = int4 does not exist"]
    );
}

#[test]
fn colonne_et_table_inconnues() {
    let sql = format!("{SCHEMA} SELECT nom FROM users; SELECT 1 FROM usagers;");
    assert_eq!(
        check(&sql),
        vec![
            "column-not-found: column \"nom\" does not exist",
            "table-not-found: relation \"usagers\" does not exist",
        ]
    );
}

#[test]
fn corps_plpgsql_variables_et_colonnes() {
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
                RETURN v_nom;
            END IF;
            RETURN v_name || r.titre;
        END $$;"
    );
    assert_eq!(
        check(&sql),
        vec![
            "undeclared-variable: variable 'v_nom' is not declared",
            "column-not-found: record \"r\" has no field \"titre\"",
        ]
    );
}

#[test]
fn conflit_variable_colonne() {
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
            NEW.nom := 'x';
            IF TG_OP = 'UPDATE' AND OLD.email IS DISTINCT FROM NEW.email THEN
                RETURN NEW;
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER t BEFORE INSERT OR UPDATE ON users FOR EACH ROW EXECUTE FUNCTION touch();"
    );
    assert_eq!(
        check(&sql),
        vec!["column-not-found: record \"new\" has no field \"nom\" (type users)"]
    );
}

#[test]
fn fonction_sql_parametres() {
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
