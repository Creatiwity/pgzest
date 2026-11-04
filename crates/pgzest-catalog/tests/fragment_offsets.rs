//! End-to-end source mapping: a SQL fragment extracted from a host document
//! must produce spans expressed in that document's coordinates.

use pgzest_catalog::ProjectCatalog;
use pgzest_schema::{FileRole, FragmentSource, SchemaSource, SqlChunk};

/// Host TypeScript document, with SQL in a template literal.
const TS_DOC: &str = r#"import { sql } from "pgzest";

export const q = sql`
CREATE TABLE users (id UUID, email TEXT);
`;
"#;

fn fragment_of(doc: &str, needle: &str) -> (String, usize) {
    let start = doc.find(needle).expect("fragment not found");
    (needle.to_string(), start)
}

#[test]
fn chunk_carries_the_real_offset() {
    let (sql, offset) = fragment_of(TS_DOC, "CREATE TABLE users (id UUID, email TEXT);");
    let src = FragmentSource::new("app/q.ts", &sql, offset, FileRole::Schema);
    let chunks: Vec<SqlChunk> = src.chunks();

    assert_eq!(chunks.len(), 1);
    let origin = &chunks[0].origin;
    assert_eq!(
        origin.byte_range.start, offset,
        "byte_range must reflect the position in the .ts"
    );
    assert_eq!(origin.byte_range.end, offset + sql.len());
    assert_ne!(
        origin.byte_range.start, 0,
        "a fragment is not at offset 0 — that is the whole point"
    );
}

#[test]
fn schema_is_indexed_from_a_ts_fragment() {
    let (sql, offset) = fragment_of(TS_DOC, "CREATE TABLE users (id UUID, email TEXT);");
    let src = FragmentSource::new("app/q.ts", &sql, offset, FileRole::Schema);
    let resolver = ProjectCatalog::from_chunks(&src.chunks());

    use pgzest_schema::SchemaProvider;
    let table = resolver
        .get_table("users")
        .expect("the table must be indexed from the TS fragment");
    assert!(table.get_column("email").is_some());
}

#[test]
fn fragment_spans_index_the_host_document() {
    let (sql, offset) = fragment_of(TS_DOC, "CREATE TABLE users (id UUID, email TEXT);");

    // Go through the parser with the fragment offset, as the resolver does.
    let stmts = pgzest_parser::parse_at(&sql, offset).expect("parse");
    assert_eq!(stmts.len(), 1);

    let pgzest_ast::Statement::Ddl(pgzest_ast::DdlStatement::CreateTable(ct)) = &stmts[0] else {
        panic!("expected CREATE TABLE");
    };

    let slice = &TS_DOC[ct.span.start as usize..ct.span.end as usize];
    assert!(
        slice.starts_with("CREATE TABLE users"),
        "the span must slice the .ts at the right place, got: {slice:?}"
    );
}

#[test]
fn invalid_fragment_is_reported_at_the_right_position() {
    let doc = "const q = sql`\nSELCT nope;\n`;\n";
    let (sql, offset) = fragment_of(doc, "SELCT nope;");

    let out = pgzest_parser::parse_at_with_outcome(&sql, offset).expect("parse");
    assert_eq!(out.skipped.len(), 1);
    let sk = &out.skipped[0];
    assert_eq!(
        &doc[sk.start..sk.end],
        "SELCT nope",
        "the skipped range must point into the host document"
    );
}

#[test]
fn sql_file_stays_at_offset_zero() {
    // Regression: FileSource/InlineSource must not be affected.
    let src =
        pgzest_schema::InlineSource::new("schema.sql", "CREATE TABLE t (a INT);", FileRole::Schema);
    let chunks = src.chunks();
    assert_eq!(chunks[0].origin.byte_range.start, 0);
}

#[test]
fn indexing_gaps_are_exposed() {
    // A table defined in an unreadable statement does not enter the index:
    // the resolver must say so instead of implying a complete schema.
    let src = pgzest_schema::InlineSource::new(
        "schema.sql",
        "CREATE TABLE ok (a INT);\nCREATE TABL oops (b INT);",
        FileRole::Schema,
    );
    let resolver = ProjectCatalog::from_chunks(&src.chunks());

    use pgzest_schema::SchemaProvider;
    assert!(resolver.get_table("ok").is_some());
    assert!(
        !resolver.skipped.is_empty(),
        "the indexing gap must be exposed, not swallowed"
    );
}
