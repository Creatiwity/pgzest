//! `pgweave migrate` : migrations générées depuis le schéma déclaratif
//! (PLAN.md, D10 et P7).
//!
//! Le schéma (`[schema] declarative = true`) décrit l'état cible ; les
//! migrations, l'historique. `create` compare l'état atteint par les
//! migrations à la cible, écrit la migration qui les sépare, et la vérifie
//! par rejeu avant d'écrire : la migration rejouée après les autres doit
//! atteindre exactement la cible. `check` signale l'écart (CI).

use std::path::{Path, PathBuf};

use pgweave_catalog::ProjectCatalog;
use pgweave_catalog::diff::{Migration, diff};
use pgweave_catalog::project::{ProjectBuilder, parse_source};
use pgweave_diagnostics::{Severity, codes};
use pgweave_schema::{FileRole, ProjectConfig, SchemaSource, SourceOrigin, SqlChunk};

/// Projet déclaratif chargé : configuration, fragments, état des migrations
/// et état cible.
struct Loaded {
    root: PathBuf,
    config: ProjectConfig,
    chunks: Vec<SqlChunk>,
    macros: pgweave_macro::MacroSet,
    project: ProjectCatalog,
}

fn build(chunks: &[SqlChunk], macros: &pgweave_macro::MacroSet) -> ProjectCatalog {
    let mut b = ProjectBuilder::new()
        .with_typer(Some(pgweave_sema::ViewTyper::shared()))
        .with_checker(Some(pgweave_sema::check::SemaChecker::shared()))
        .declarative(true)
        .keep_sources(true);
    for chunk in chunks {
        let parsed = parse_source(&chunk.sql, chunk.origin.byte_range.start, macros);
        b.add_file(&chunk.origin.path, &chunk.sql, &chunk.role, parsed.as_ref());
    }
    b.finish()
}

/// Erreurs qui rendent un état reconstruit inexploitable : statement non lu,
/// erreur de rejeu ou d'analyse.
fn blocking(project: &ProjectCatalog) -> Vec<String> {
    let mut out: Vec<String> = project
        .skipped
        .iter()
        .map(|(p, s)| format!("{}: statement non lu ({:?})", p.display(), s.reason))
        .collect();
    out.extend(
        project
            .diagnostics
            .iter()
            .filter(|(_, d)| d.severity == Severity::Error && d.code != codes::SCHEMA_DRIFT)
            .map(|(p, d)| format!("{}: error[{}]: {}", p.display(), d.code, d.message)),
    );
    out
}

fn load() -> Result<Loaded, (i32, String)> {
    let (root, config) = crate::discover_project()
        .map_err(|e| (2, e))?
        .ok_or_else(|| (2, format!("pas de {} trouvé", ProjectConfig::FILENAME)))?;
    if !config.schema.declarative {
        return Err((
            2,
            "le projet n'est pas déclaratif : `pgweave migrate` génère les migrations depuis le \
             schéma quand `[schema] declarative = true` (le schéma décrit alors l'état complet)"
                .into(),
        ));
    }
    let (macros, macro_errors) = crate::load_macros(Some(&(root.clone(), config.clone())));
    if !macro_errors.is_empty() {
        return Err((2, macro_errors.join("\n")));
    }
    let chunks: Vec<SqlChunk> = config
        .to_source(&root)
        .chunks()
        .into_iter()
        .filter(|c| c.role != FileRole::Query)
        .collect();
    let project = build(&chunks, &macros);
    let errors = blocking(&project);
    if !errors.is_empty() {
        return Err((
            1,
            format!(
                "état reconstruit incomplet ou en erreur, aucune migration ne peut en être déduite :\n{}",
                errors.join("\n")
            ),
        ));
    }
    Ok(Loaded {
        root,
        config,
        chunks,
        macros,
        project,
    })
}

fn history_and_target(l: &Loaded) -> Migration {
    let history = l
        .project
        .history
        .as_ref()
        .expect("projet déclaratif : historique rejoué");
    diff(history, &l.project.catalog)
}

// ── Rendu ─────────────────────────────────────────────────────────────────────

fn print_unsupported(m: &Migration) {
    for u in &m.unsupported {
        eprintln!("error: {} : {}", u.object, u.change);
    }
}

/// Texte de la migration : en-tête de contexte, remarques, statements
/// précédés de leurs risques.
fn render(m: &Migration, header: &str, name: &str) -> String {
    let mut out =
        format!("{header}\n-- Migration « {name} », générée par `pgweave migrate create`.\n");
    for n in &m.notes {
        out.push_str(&format!("-- note : {n}\n"));
    }
    for step in &m.steps {
        out.push('\n');
        for r in &step.risks {
            out.push_str(&format!("-- !! {} : {}\n", r.code, r.message));
        }
        out.push_str(&step.sql);
        out.push('\n');
    }
    out
}

// ── Emplacement ───────────────────────────────────────────────────────────────

/// Organisation du répertoire, lue sur les migrations existantes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Flat,
    Prisma,
    Diesel,
}

fn layout(chunks: &[SqlChunk]) -> Layout {
    let named = |n: &str| {
        chunks.iter().any(|c| {
            matches!(c.role, FileRole::Migration { .. })
                && c.origin.path.file_name().is_some_and(|f| f == n)
        })
    };
    if named("migration.sql") {
        Layout::Prisma
    } else if named("up.sql") {
        Layout::Diesel
    } else {
        Layout::Flat
    }
}

/// Préfixe numérique d'un nom (`0003_x` → `0003`).
fn digits(stem: &str) -> &str {
    let end = stem
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(stem.len());
    &stem[..end]
}

fn migration_path(dir: &Path, chunks: &[SqlChunk], name: &str) -> PathBuf {
    let now = chrono::Utc::now();
    match layout(chunks) {
        Layout::Prisma => dir
            .join(format!("{}_{name}", now.format("%Y%m%d%H%M%S")))
            .join("migration.sql"),
        Layout::Diesel => dir
            .join(format!("{}_{name}", now.format("%Y-%m-%d-%H%M%S")))
            .join("up.sql"),
        Layout::Flat => {
            // Numérotation des migrations existantes : compteur court
            // (`0007`) prolongé, horodatage sinon.
            let prefixes: Vec<&str> = chunks
                .iter()
                .filter(|c| matches!(c.role, FileRole::Migration { .. }))
                .filter_map(|c| c.origin.path.file_stem().and_then(|s| s.to_str()))
                .map(digits)
                .filter(|d| !d.is_empty())
                .collect();
            let width = prefixes.iter().map(|d| d.len()).max().unwrap_or(4);
            let prefix = if width <= 6 {
                let next = prefixes
                    .iter()
                    .filter_map(|d| d.parse::<u64>().ok())
                    .max()
                    .map_or(1, |n| n + 1);
                format!("{next:0width$}")
            } else {
                now.format("%Y%m%d%H%M%S").to_string()
            };
            dir.join(format!("{prefix}_{name}.sql"))
        }
    }
}

// ── Commandes ─────────────────────────────────────────────────────────────────

pub fn create(name: &str, allow_destructive: bool, dry_run: bool) -> i32 {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        eprintln!("error: nom de migration attendu en minuscules, chiffres et `_` : « {name} »");
        return 2;
    }
    let l = match load() {
        Ok(l) => l,
        Err((code, e)) => {
            eprintln!("error: {e}");
            return code;
        }
    };
    let m = history_and_target(&l);
    if m.is_empty() {
        println!("aucun changement : les migrations atteignent déjà le schéma déclaratif");
        return 0;
    }
    if !m.unsupported.is_empty() {
        print_unsupported(&m);
        eprintln!(
            "error: {} changement(s) non exprimable(s) automatiquement : écrire cette migration à la main",
            m.unsupported.len()
        );
        return 1;
    }
    let destructive: Vec<_> = m
        .steps
        .iter()
        .flat_map(|s| s.risks.iter().filter(|r| r.destructive))
        .collect();
    if !destructive.is_empty() && !allow_destructive {
        for r in &destructive {
            eprintln!("error[{}]: {}", r.code, r.message);
        }
        eprintln!(
            "error: la migration détruit des données ; un renommage s'écrit à la main, sinon relancer avec --allow-destructive"
        );
        return 1;
    }

    let header = pgweave_catalog::dump::header(&l.project.catalog, l.config.postgres.version);
    let sql = render(&m, &header, name);
    let dir = l.root.join(&l.config.migrations.directory);
    let path = migration_path(&dir, &l.chunks, name);

    // Vérification : la migration rejouée après les autres atteint la cible,
    // sans erreur.
    let sequence = l
        .chunks
        .iter()
        .filter_map(|c| match c.role {
            FileRole::Migration { sequence } => Some(sequence),
            _ => None,
        })
        .max()
        .map_or(1, |s| s + 1);
    let mut chunks = l.chunks.clone();
    let insert_at = chunks
        .iter()
        .position(|c| !matches!(c.role, FileRole::Migration { .. }))
        .unwrap_or(chunks.len());
    chunks.insert(
        insert_at,
        SqlChunk {
            origin: SourceOrigin {
                path: path.clone(),
                byte_range: 0..sql.len(),
            },
            sql: sql.clone(),
            role: FileRole::Migration { sequence },
        },
    );
    let verified = build(&chunks, &l.macros);
    let errors = blocking(&verified);
    let rest = diff(
        verified.history.as_ref().expect("projet déclaratif"),
        &verified.catalog,
    );
    if !errors.is_empty() || !rest.is_empty() {
        eprintln!(
            "error: la migration générée ne passe pas sa vérification par rejeu (défaut de pgweave) :"
        );
        for e in &errors {
            eprintln!("  {e}");
        }
        if !rest.is_empty() {
            eprintln!("  écart restant :\n{}", rest.to_sql());
        }
        eprintln!("--- migration\n{sql}");
        return 1;
    }

    for step in &m.steps {
        for r in step.risks.iter().filter(|r| !r.destructive) {
            eprintln!("warning[{}]: {}", r.code, r.message);
        }
    }
    for n in &m.notes {
        eprintln!("note: {n}");
    }
    if dry_run {
        print!("{sql}");
        return 0;
    }
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&path, &sql));
    if let Err(e) = written {
        eprintln!("error: {}: {e}", path.display());
        return 2;
    }
    println!(
        "écrit : {} ({} statement(s), vérifiée par rejeu)",
        path.strip_prefix(&l.root).unwrap_or(&path).display(),
        m.steps.len()
    );
    0
}

pub fn check() -> i32 {
    let l = match load() {
        Ok(l) => l,
        Err((code, e)) => {
            eprintln!("error: {e}");
            return code;
        }
    };
    let m = history_and_target(&l);
    if m.is_empty() {
        println!("à jour : les migrations atteignent le schéma déclaratif");
        return 0;
    }
    eprintln!(
        "error: le schéma déclaratif diffère des migrations ({} statement(s) à générer) :",
        m.steps.len()
    );
    for step in &m.steps {
        eprintln!("  {}", step.sql.lines().next().unwrap_or_default());
    }
    print_unsupported(&m);
    eprintln!("hint: `pgweave migrate create <nom>` génère la migration");
    1
}

/// Statements d'un fichier SQL, ou des `.sql` d'un répertoire (ordre des
/// chemins), rejoués.
fn replay_path(path: &str) -> Result<ProjectCatalog, String> {
    let p = Path::new(path);
    let mut files = Vec::new();
    if p.is_dir() {
        let mut stack = vec![p.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))? {
                let e = e.map_err(|e| e.to_string())?.path();
                if e.is_dir() {
                    stack.push(e);
                } else if e.extension().is_some_and(|x| x == "sql") {
                    files.push(e);
                }
            }
        }
        files.sort();
    } else {
        files.push(p.to_path_buf());
    }
    let mut b = ProjectBuilder::new()
        .with_typer(Some(pgweave_sema::ViewTyper::shared()))
        .keep_sources(true);
    let macros = pgweave_macro::MacroSet::builtin();
    for f in &files {
        let sql = std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
        let parsed = parse_source(&sql, 0, &macros);
        b.add_file(f, &sql, &FileRole::Schema, parsed.as_ref());
    }
    let project = b.finish();
    let errors = blocking(&project);
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(project)
}

/// `migrate diff ancien nouveau` : migration entre deux schémas (fichiers ou
/// répertoires), sans projet.
pub fn diff_files(old: &str, new: &str) -> i32 {
    let (before, after) = match (replay_path(old), replay_path(new)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let m = diff(&before.catalog, &after.catalog);
    if m.is_empty() {
        println!("-- aucun changement");
        return 0;
    }
    let header =
        pgweave_catalog::dump::header(&after.catalog, pgweave_catalog::project::DEFAULT_POSTGRES);
    print!("{}", render(&m, &header, "diff"));
    print_unsupported(&m);
    if m.unsupported.is_empty() { 0 } else { 1 }
}
