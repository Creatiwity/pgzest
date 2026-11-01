//! Colonnes de vues typées pendant le rejeu du catalogue.

use std::path::Path;

use pgweave_catalog::model::{RelationKind, TypeRef};
use pgweave_catalog::project::ProjectBuilder;
use pgweave_catalog::{ProjectCatalog, QName};
use pgweave_schema::FileRole;
use pgweave_sema::ViewTyper;

fn replay(sql: &str, typed: bool) -> ProjectCatalog {
    let typer = typed.then(ViewTyper::shared);
    let mut b = ProjectBuilder::new().with_typer(typer);
    let parsed = pgweave_parser::parse_with_outcome(sql);
    b.add_file(Path::new("s.sql"), sql, &FileRole::Schema, parsed.as_ref());
    b.finish()
}

const SCHEMA: &str = "CREATE TABLE t (a varchar(10) NOT NULL, b numeric(5, 2), tags text[]);
CREATE VIEW v AS SELECT a, b * 2 AS c, tags, count(*) OVER () AS n FROM t;
CREATE VIEW w AS SELECT c, n FROM v;";

fn columns(p: &ProjectCatalog, view: &str) -> Vec<(String, String)> {
    let r = &p.catalog.relations[&QName::new("public", view)];
    assert_eq!(r.kind, RelationKind::View);
    r.columns
        .iter()
        .map(|c| (c.name.clone(), c.ty.to_string()))
        .collect()
}

#[test]
fn types_des_colonnes_de_vues() {
    let p = replay(SCHEMA, true);
    assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
    // Modificateur conservé pour une colonne, perdu pour un calcul.
    assert_eq!(
        columns(&p, "v"),
        [
            ("a".into(), "pg_catalog.varchar(10)".into()),
            ("c".into(), "pg_catalog.numeric".into()),
            ("tags".into(), "pg_catalog.text[]".into()),
            ("n".into(), "pg_catalog.int8".into()),
        ]
    );
    // Une vue sur une vue voit les types établis.
    assert_eq!(
        columns(&p, "w"),
        [
            ("c".into(), "pg_catalog.numeric".into()),
            ("n".into(), "pg_catalog.int8".into()),
        ]
    );
}

#[test]
fn sans_typeur_les_types_restent_inconnus() {
    let p = replay(SCHEMA, false);
    let r = &p.catalog.relations[&QName::new("public", "v")];
    assert!(r.columns.iter().all(|c| c.ty == TypeRef::Unknown));
}

#[test]
fn une_vue_non_analysable_est_signalee() {
    let p = replay(
        "CREATE TABLE t (a int); CREATE VIEW v AS SELECT nope FROM t;",
        true,
    );
    assert!(
        p.diagnostics
            .iter()
            .any(|(_, d)| d.code == pgweave_diagnostics::codes::VIEW_NOT_TYPED),
        "{:?}",
        p.diagnostics
    );
}
