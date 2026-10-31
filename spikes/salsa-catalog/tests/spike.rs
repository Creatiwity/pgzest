//! Propriétés attendues du moteur incrémental (PLAN.md, D3).

use std::path::PathBuf;
use std::time::Instant;

use salsa::Setter;
use salsa_catalog_spike::{
    Database, Project, Resolved, SourceFile, TableName, qualify, resolve_in_routine, resolve_table,
    table_index,
};

fn file(db: &Database, name: &str, text: &str) -> SourceFile {
    SourceFile::new(db, PathBuf::from(name), text.to_string())
}

fn table(db: &Database, project: Project, name: &str) -> Option<Vec<String>> {
    resolve_table(db, project, TableName::new(db, qualify(name)))
        .as_ref()
        .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
}

/// Trois migrations : `a` crée t1, `b` crée t2, `c` modifie t1.
fn setup() -> (Database, Project, [SourceFile; 3]) {
    let db = Database::default();
    let a = file(&db, "001_a.sql", "CREATE TABLE t1 (id INT);");
    let b = file(&db, "002_b.sql", "CREATE TABLE t2 (x TEXT);\nSELECT 1;");
    let c = file(&db, "003_c.sql", "ALTER TABLE t1 ADD COLUMN label TEXT;");
    let project = Project::new(&db, vec![a, b, c]);
    (db, project, [a, b, c])
}

#[test]
fn l_index_ne_parse_aucun_fichier() {
    let (db, project, _) = setup();
    let index = table_index(&db, project);
    assert_eq!(
        index.keys().cloned().collect::<Vec<_>>(),
        ["public.t1", "public.t2"]
    );
    let executed = db.take_executions();
    assert!(
        executed.iter().all(|e| !e.starts_with("parse_file")),
        "l'index se construit sur un balayage de tokens : {executed:?}"
    );
}

#[test]
fn une_table_ne_fait_parser_que_ses_fichiers_dans_l_ordre() {
    let (db, project, _) = setup();
    assert_eq!(
        table(&db, project, "t1").unwrap(),
        ["id", "label"],
        "CREATE puis ALTER, dans l'ordre des migrations"
    );
    let parsed: Vec<_> = db
        .take_executions()
        .into_iter()
        .filter(|e| e.starts_with("parse_file"))
        .collect();
    assert_eq!(parsed, ["parse_file(001_a.sql)", "parse_file(003_c.sql)"]);
}

#[test]
fn modifier_un_fichier_sans_toucher_aux_declarations_ne_recalcule_rien_en_aval() {
    let (mut db, project, [_, b, _]) = setup();
    table(&db, project, "t1");
    table(&db, project, "t2");
    db.take_executions();

    b.set_text(&mut db)
        .to("CREATE TABLE t2 (x TEXT);\nSELECT 2; -- requête modifiée".into());
    assert_eq!(table(&db, project, "t1").unwrap(), ["id", "label"]);
    let executed = db.take_executions();
    assert_eq!(
        executed,
        ["file_declarations(002_b.sql)"],
        "seules les déclarations de b sont relues ; elles n'ont pas changé, \
         donc ni l'index ni t1 ne sont recalculés"
    );
}

#[test]
fn ajouter_une_colonne_ne_recalcule_que_cette_table() {
    let (mut db, project, [_, _, c]) = setup();
    table(&db, project, "t1");
    table(&db, project, "t2");
    db.take_executions();

    c.set_text(&mut db).to(
        "ALTER TABLE t1 ADD COLUMN label TEXT;\nALTER TABLE t1 ADD COLUMN done BOOLEAN;".into(),
    );
    assert_eq!(table(&db, project, "t1").unwrap(), ["id", "label", "done"]);
    assert_eq!(table(&db, project, "t2").unwrap(), ["x"]);
    let executed = db.take_executions();
    assert!(
        executed.contains(&"resolve_table(public.t1)".to_string()),
        "{executed:?}"
    );
    assert!(
        !executed.contains(&"resolve_table(public.t2)".to_string()),
        "t2 ne dépend pas de c : {executed:?}"
    );
}

#[test]
fn portee_de_routine_tables_temporaires_puis_globales() {
    let db = Database::default();
    let schema = file(
        &db,
        "001_schema.sql",
        "CREATE TABLE pages (id BIGINT, title TEXT);",
    );
    let routine = file(
        &db,
        "002_routine.sql",
        "CREATE FUNCTION refresh(p_limit INTEGER) RETURNS VOID LANGUAGE plpgsql AS $$
         DECLARE
             n INTEGER;
         BEGIN
             CREATE TEMP TABLE staging (id BIGINT, score NUMERIC);
             INSERT INTO staging SELECT id, 1 FROM pages LIMIT p_limit;
         END;
         $$;",
    );
    let project = Project::new(&db, vec![schema, routine]);

    match resolve_in_routine(&db, project, "refresh", "staging") {
        Some(Resolved::TempTable(t)) => {
            let cols: Vec<_> = t.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(cols, ["id", "score"]);
        }
        other => panic!("table temporaire attendue : {other:?}"),
    }
    assert!(
        matches!(
            resolve_in_routine(&db, project, "refresh", "pages"),
            Some(Resolved::Global(_))
        ),
        "repli sur le schéma global"
    );
    assert!(
        table(&db, project, "staging").is_none(),
        "une table temporaire du corps n'existe pas hors de la routine"
    );

    let scope = salsa_catalog_spike::routine_scope(
        &db,
        project,
        salsa_catalog_spike::RoutineName::new(&db, qualify("refresh")),
    )
    .clone()
    .expect("routine indexée");
    assert_eq!(scope.params[0].0, "p_limit");
    assert_eq!(scope.variables[0].0, "n");
}

/// Mesure sur Mesalia et équivalence avec `SchemaResolver`. Corpus privé :
/// `PGWEAVE_CORPUS_MESALIA=…/prisma/migrations cargo test -p
/// salsa-catalog-spike --release -- --ignored --nocapture`.
#[test]
#[ignore = "corpus privé : PGWEAVE_CORPUS_MESALIA"]
fn mesalia_equivalence_et_mesures() {
    use pgweave_schema::{FileRole, SchemaProvider, SchemaResolver, SourceOrigin, SqlChunk};

    let dir =
        PathBuf::from(std::env::var("PGWEAVE_CORPUS_MESALIA").expect("PGWEAVE_CORPUS_MESALIA"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| Some(e.ok()?.path().join("migration.sql")))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    let sources: Vec<(PathBuf, String)> = paths
        .iter()
        .map(|p| (p.clone(), std::fs::read_to_string(p).unwrap()))
        .collect();

    // ── Référence : SchemaResolver (eager) ──
    let chunks: Vec<SqlChunk> = sources
        .iter()
        .enumerate()
        .map(|(i, (p, sql))| SqlChunk {
            origin: SourceOrigin {
                path: p.clone(),
                byte_range: 0..sql.len(),
            },
            sql: sql.clone(),
            role: FileRole::Migration { sequence: i as u64 },
        })
        .collect();
    let t = Instant::now();
    let reference = SchemaResolver::from_chunks(&chunks);
    let eager = t.elapsed();

    // ── salsa : index seul ──
    let mut db = Database::default();
    let files: Vec<SourceFile> = sources
        .iter()
        .map(|(p, sql)| SourceFile::new(&db, p.clone(), sql.clone()))
        .collect();
    let project = Project::new(&db, files.clone());
    let t = Instant::now();
    let names: Vec<String> = table_index(&db, project).keys().cloned().collect();
    let index_time = t.elapsed();

    // ── salsa : toutes les tables ──
    let t = Instant::now();
    for n in &names {
        resolve_table(&db, project, TableName::new(&db, n.clone()));
    }
    let all_time = t.elapsed();
    db.take_executions();

    // ── Équivalence ──
    let mut differences = Vec::new();
    let mut compared = 0;
    for name in reference.all_table_names() {
        compared += 1;
        let expected: Vec<(String, String, bool)> = reference
            .get_table(name)
            .unwrap()
            .columns
            .iter()
            .map(|c| (c.name.clone(), format!("{:?}", c.data_type), c.nullable))
            .collect();
        let got: Option<Vec<(String, String, bool)>> =
            resolve_table(&db, project, TableName::new(&db, name.to_string()))
                .as_ref()
                .map(|t| {
                    t.columns
                        .iter()
                        .map(|c| (c.name.clone(), c.data_type.clone(), c.nullable))
                        .collect()
                });
        if got.as_ref() != Some(&expected) {
            differences.push(name.to_string());
        }
    }

    // ── Incrémental : commentaire ajouté au dernier fichier ──
    let last = *files.last().unwrap();
    let text = format!("{}\n-- édition\n", last.text(&db));
    last.set_text(&mut db).to(text);
    let t = Instant::now();
    for n in &names {
        resolve_table(&db, project, TableName::new(&db, n.clone()));
    }
    let edit_time = t.elapsed();
    let edit_execs = db.take_executions();

    println!("fichiers                       : {}", files.len());
    println!(
        "tables (index salsa / référence): {} / {compared}",
        names.len()
    );
    println!("SchemaResolver::from_chunks    : {eager:?}");
    println!("salsa, index seul              : {index_time:?}");
    println!(
        "salsa, index + toutes les tables: {:?}",
        index_time + all_time
    );
    println!("salsa, après édition (commentaire), toutes les tables : {edit_time:?}");
    println!("  requêtes ré-exécutées        : {edit_execs:?}");
    println!("différences avec la référence  : {differences:?}");

    assert!(
        differences.is_empty(),
        "tables divergentes : {differences:?}"
    );
}
