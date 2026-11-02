//! Code Rust des requêtes nommées : traduction des types, de la nullabilité
//! et de la cardinalité ; refus explicites.

use pgweave_catalog::ProjectCatalog;
use pgweave_codegen::rust::{RustOptions, Temporal, generate_rust};
use pgweave_query::{describe_named, parse_query_file};

const SCHEMA: &str = "
CREATE TYPE mood AS ENUM ('sad', 'so-so', 'happy');
CREATE TABLE notes (
    id      BIGINT PRIMARY KEY,
    body    TEXT NOT NULL,
    mood    mood,
    at      TIMESTAMPTZ NOT NULL,
    \"type\"  INT,
    \"createdBy\" TEXT,
    spot    POINT
);
";

fn generate(queries: &str, opts: &RustOptions) -> Result<String, Vec<String>> {
    let cat = ProjectCatalog::from_sql(SCHEMA).expect("schéma");
    let file = parse_query_file(queries);
    assert!(file.diagnostics.is_empty(), "{:?}", file.diagnostics);
    let descriptors: Vec<_> = file
        .queries
        .iter()
        .map(|q| describe_named(&cat, q).expect("descripteur"))
        .collect();
    generate_rust(&descriptors, &cat, opts)
}

#[test]
fn types_nullabilite_cardinalite() {
    let code = generate(
        "-- name: note_by_id\nSELECT id, body, mood, at, \"type\", \"createdBy\" FROM notes WHERE id = $id;\n\n\
         -- name: insert_note\nINSERT INTO notes (id, body, mood, at) VALUES ($id, $body, $mood, now());\n",
        &RustOptions::default(),
    )
    .expect("code");
    for expected in [
        "pub enum Mood {",
        "    SoSo,",
        "\"so-so\" => Some(Mood::SoSo),",
        "pub struct NoteByIdParams {\n    pub id: i64,\n}",
        "    pub body: String,",
        "    pub mood: Option<Mood>,",
        "    pub at: time::OffsetDateTime,",
        "    pub r#type: Option<i32>,",
        "    pub created_by: Option<String>,",
        "-> Result<Option<NoteByIdRow>, pgweave_runtime::Error>",
        "pub struct InsertNoteParams<'a> {\n    pub id: i64,\n    pub body: &'a str,\n    pub mood: Option<Mood>,\n}",
        // Un enum n'a pas d'OID fixe : le serveur type la requête.
        "param_oids: None",
        "-> Result<u64, pgweave_runtime::Error>",
    ] {
        assert!(code.contains(expected), "{expected:?} attendu :\n{code}");
    }
}

#[test]
fn chrono_au_choix_du_projet() {
    let opts = RustOptions {
        temporal: Temporal::Chrono,
        ..RustOptions::default()
    };
    let code = generate("-- name: q\nSELECT at FROM notes;\n", &opts).expect("code");
    assert!(
        code.contains("pub at: chrono::DateTime<chrono::Utc>,"),
        "{code}"
    );
}

#[test]
fn refus_explicites() {
    let err = generate(
        "-- name: q\nSELECT spot FROM notes;\n",
        &RustOptions::default(),
    )
    .expect_err("point sans correspondance");
    assert_eq!(
        err,
        [
            "q : pas de type Rust pour pg_catalog.point : le déclarer dans la table de correspondance du projet"
        ]
    );
    // La table de correspondance du projet lève le refus.
    let mut opts = RustOptions::default();
    opts.overrides
        .insert("pg_catalog.point".into(), "geo_types::Point<f64>".into());
    let code = generate("-- name: q\nSELECT spot FROM notes;\n", &opts).expect("code");
    assert!(
        code.contains("pub spot: Option<geo_types::Point<f64>>,"),
        "{code}"
    );

    let err = generate("-- name: q\nSELECT 1, 2;\n", &RustOptions::default()).expect_err("noms");
    assert_eq!(
        err,
        ["q : `?column?` n'est pas un identifiant Rust (colonne de sortie : lui donner un alias)"]
    );
    let err = generate(
        "-- name: q\nSELECT id, id FROM notes;\n",
        &RustOptions::default(),
    )
    .expect_err("doublon");
    assert_eq!(
        err,
        ["q : deux colonnes de sortie s'appellent `id` : leur donner des alias distincts"]
    );
}

#[test]
fn types_d_extension() {
    let cat = ProjectCatalog::from_sql(
        "CREATE SCHEMA ext; CREATE EXTENSION citext SCHEMA ext; CREATE EXTENSION pg_trgm;\n\
         CREATE TABLE u (email ext.citext NOT NULL);",
    )
    .expect("schéma");
    let file = parse_query_file("-- name: q\nSELECT email FROM u WHERE email = $email;\n");
    let d = describe_named(&cat, &file.queries[0]).expect("descripteur");
    let code = generate_rust(&[d], &cat, &RustOptions::default()).expect("code");
    assert!(code.contains("pub email: String,"), "{code}");
    assert!(code.contains("pub email: &'a str,"), "{code}");

    let file = parse_query_file("-- name: q\nSELECT show_trgm('x') AS t, 'x'::gtrgm AS g;\n");
    if let Ok(d) = describe_named(&cat, &file.queries[0]) {
        let err = generate_rust(&[d], &cat, &RustOptions::default()).expect_err("gtrgm");
        assert!(err[0].contains("(extension pg_trgm)"), "{err:?}");
    }
}
