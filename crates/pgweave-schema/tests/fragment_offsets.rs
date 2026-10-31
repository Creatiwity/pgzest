//! Le source-mapping de bout en bout : un fragment SQL extrait d'un document
//! hôte doit produire des spans exprimés dans le repère de ce document.

use pgweave_schema::{FileRole, FragmentSource, SchemaResolver, SchemaSource, SqlChunk};

/// Document TypeScript hôte, avec du SQL dans un littéral de template.
const TS_DOC: &str = r#"import { sql } from "pgweave";

export const q = sql`
CREATE TABLE users (id UUID, email TEXT);
`;
"#;

fn fragment_of(doc: &str, needle: &str) -> (String, usize) {
    let start = doc.find(needle).expect("fragment introuvable");
    (needle.to_string(), start)
}

#[test]
fn le_chunk_porte_loffset_reel() {
    let (sql, offset) = fragment_of(TS_DOC, "CREATE TABLE users (id UUID, email TEXT);");
    let src = FragmentSource::new("app/q.ts", &sql, offset, FileRole::Schema);
    let chunks: Vec<SqlChunk> = src.chunks();

    assert_eq!(chunks.len(), 1);
    let origin = &chunks[0].origin;
    assert_eq!(
        origin.byte_range.start, offset,
        "byte_range doit refléter la position dans le .ts"
    );
    assert_eq!(origin.byte_range.end, offset + sql.len());
    assert_ne!(
        origin.byte_range.start, 0,
        "un fragment n'est pas à l'offset 0 — c'est tout l'intérêt"
    );
}

#[test]
fn le_schema_est_indexe_depuis_un_fragment_ts() {
    let (sql, offset) = fragment_of(TS_DOC, "CREATE TABLE users (id UUID, email TEXT);");
    let src = FragmentSource::new("app/q.ts", &sql, offset, FileRole::Schema);
    let resolver = SchemaResolver::from_source(&src);

    use pgweave_schema::SchemaProvider;
    let table = resolver
        .get_table("users")
        .expect("la table doit être indexée depuis le fragment TS");
    assert!(table.get_column("email").is_some());
}

#[test]
fn les_spans_dun_fragment_indexent_le_document_hote() {
    let (sql, offset) = fragment_of(TS_DOC, "CREATE TABLE users (id UUID, email TEXT);");

    // On passe par le parser avec l'offset du fragment, comme le fait le resolver.
    let stmts = pgweave_parser::parse_at(&sql, offset).expect("parse");
    assert_eq!(stmts.len(), 1);

    let pgweave_ast::Statement::Ddl(pgweave_ast::DdlStatement::CreateTable(ct)) = &stmts[0] else {
        panic!("CREATE TABLE attendu");
    };

    let slice = &TS_DOC[ct.span.start as usize..ct.span.end as usize];
    assert!(
        slice.starts_with("CREATE TABLE users"),
        "le span doit découper le .ts au bon endroit, obtenu: {slice:?}"
    );
}

#[test]
fn un_fragment_invalide_est_signale_a_la_bonne_position() {
    let doc = "const q = sql`\nSELCT nope;\n`;\n";
    let (sql, offset) = fragment_of(doc, "SELCT nope;");

    let out = pgweave_parser::parse_at_with_outcome(&sql, offset).expect("parse");
    assert_eq!(out.skipped.len(), 1);
    let sk = &out.skipped[0];
    assert_eq!(
        &doc[sk.start..sk.end],
        "SELCT nope",
        "l'écart doit pointer dans le document hôte"
    );
}

#[test]
fn un_fichier_sql_reste_a_loffset_zero() {
    // Régression : FileSource/InlineSource ne doivent pas être affectés.
    let src = pgweave_schema::InlineSource::new(
        "schema.sql",
        "CREATE TABLE t (a INT);",
        FileRole::Schema,
    );
    let chunks = src.chunks();
    assert_eq!(chunks[0].origin.byte_range.start, 0);
}

#[test]
fn les_ecarts_dindexation_sont_exposes() {
    // Une table définie dans un statement illisible n'entre pas dans l'index :
    // le resolver doit le dire au lieu de laisser croire à un schéma complet.
    let src = pgweave_schema::InlineSource::new(
        "schema.sql",
        "CREATE TABLE ok (a INT);\nCREATE TABL oops (b INT);",
        FileRole::Schema,
    );
    let resolver = SchemaResolver::from_source(&src);

    use pgweave_schema::SchemaProvider;
    assert!(resolver.get_table("ok").is_some());
    assert!(
        !resolver.skipped.is_empty(),
        "l'écart d'indexation doit être exposé, pas avalé"
    );
}
