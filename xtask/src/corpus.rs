//! `cargo xtask corpus` : taux d'analyse de pgweave sur un corpus externe.
//!
//! Le rapport est destiné à être commité (`docs/baselines/`) alors que le
//! corpus, lui, est privé. Règle stricte : **aucun identifiant du corpus**
//! dans le rapport. On n'y écrit que des compteurs, des codes de diagnostic et
//! des « formes » de statements où tout nom est remplacé par `…`. Les messages
//! d'erreur (de sqlparser, de pgweave ou de PostgreSQL) citent des noms : ils
//! ne vont que sur la sortie d'erreur avec `--details`, ou sont masqués.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pgweave_ast::Statement;
use pgweave_ast::ddl::{DdlStatement, FunctionBody};
use pgweave_diagnostics::Severity;
use pgweave_parser::{SkipReason, SkippedStatement};
use pgweave_schema::{FileRole, SchemaResolver, SourceOrigin, SqlChunk};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, Tokenizer};

/// Organisation d'un répertoire de migrations. Explicite : la deviner
/// ferait prendre un script de retour arrière pour une migration.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Layout {
    /// Tous les `.sql`, récursivement, triés par chemin.
    Flat,
    /// Prisma : `<horodatage>_<nom>/migration.sql`, seul fichier appliqué
    /// (les autres, comme un `down.sql`, sont ignorés).
    Prisma,
}

impl Layout {
    fn describe(self) -> &'static str {
        match self {
            Layout::Flat => "tous les `.sql`, triés par chemin",
            Layout::Prisma => "Prisma : `*/migration.sql`, triés par répertoire horodaté",
        }
    }

    fn accepts(self, path: &Path) -> bool {
        match self {
            Layout::Flat => path.extension().is_some_and(|e| e == "sql"),
            Layout::Prisma => path.file_name().is_some_and(|n| n == "migration.sql"),
        }
    }
}

pub fn run(
    name: &str,
    path: Option<PathBuf>,
    layout: Layout,
    pg_url: Option<String>,
    out: Option<PathBuf>,
    details: bool,
) -> Result<(), String> {
    let env_var = format!("PGWEAVE_CORPUS_{}", name.to_ascii_uppercase());
    let dir = path
        .or_else(|| std::env::var_os(&env_var).map(PathBuf::from))
        .ok_or_else(|| format!("corpus introuvable : passer --path ou définir {env_var}"))?;

    let files = collect_sql(&dir, layout)?;
    if files.is_empty() {
        return Err(format!("aucun fichier .sql sous {}", dir.display()));
    }
    let sources: Vec<(PathBuf, String)> = files
        .into_iter()
        .map(|p| {
            std::fs::read_to_string(&p)
                .map(|s| (p.clone(), s))
                .map_err(|e| format!("{}: {e}", p.display()))
        })
        .collect::<Result<_, _>>()?;

    let mut stats = Stats::default();
    let parse_start = Instant::now();
    let mut parsed_files = Vec::new();
    for (path, sql) in &sources {
        match pgweave_parser::parse_with_outcome(sql) {
            Ok(outcome) => {
                stats.record_parse(&outcome.statements, &outcome.skipped);
                if details {
                    for sk in &outcome.skipped {
                        eprintln!("{}: {:?}", path.display(), sk.reason);
                    }
                }
                parsed_files.push((path, sql, outcome.statements));
            }
            Err(e) => {
                stats.files_unreadable += 1;
                if details {
                    eprintln!("{}: fichier illisible : {e}", path.display());
                }
            }
        }
    }
    stats.parse_time = parse_start.elapsed();

    // Index du projet, comme `pgweave check` le construit pour un répertoire
    // de migrations : chaque fichier est une migration, dans l'ordre.
    let chunks: Vec<SqlChunk> = sources
        .iter()
        .enumerate()
        .map(|(i, (path, sql))| SqlChunk {
            origin: SourceOrigin {
                path: path.clone(),
                byte_range: 0..sql.len(),
            },
            sql: sql.clone(),
            role: FileRole::Migration { sequence: i as u64 },
        })
        .collect();
    let index_start = Instant::now();
    let resolver = SchemaResolver::from_chunks(&chunks);
    stats.index_time = index_start.elapsed();
    stats.index_skipped = resolver.skipped.len();

    // Diagnostics de `check` sur chaque migration, contre l'index complet —
    // c'est ce que voit aujourd'hui un utilisateur qui lance `check` sur ce
    // répertoire. Les migrations ayant été appliquées en production, une
    // erreur est a priori un faux positif (ou un effet du schéma final,
    // l'analyse n'étant pas encore rejouée migration par migration : P2).
    let all_stmts: Vec<Statement> = parsed_files
        .iter()
        .flat_map(|(_, _, s)| s.iter().cloned())
        .collect();
    let checker = pgweave_checker::Checker::with_builtin_rules();
    let check_start = Instant::now();
    for (path, _, stmts) in &parsed_files {
        // L'index contient déjà le DDL de ce fichier : pas de fournisseur
        // supplémentaire, contrairement au mode fichier isolé de la CLI.
        let diags = checker.run_with_context(stmts, &all_stmts, &resolver);
        let mut file_has_error = false;
        for d in &diags {
            let sev = match d.severity {
                Severity::Error => {
                    file_has_error = true;
                    "error"
                }
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            *stats.diagnostics.entry((sev, d.code)).or_default() += 1;
            if details {
                eprintln!("{}: {sev}[{}] {}", path.display(), d.code, d.message);
            }
        }
        if file_has_error {
            stats.files_with_errors += 1;
        }
    }
    stats.check_time = check_start.elapsed();

    let pg = match pg_url.or_else(|| std::env::var("PGWEAVE_ORACLE_URL").ok()) {
        Some(url) => Some(replay_on_postgres(&url, &sources, details)?),
        None => None,
    };

    let report = render(name, layout, sources.len(), &stats, pg.as_ref());
    match out {
        Some(path) => std::fs::write(&path, report).map_err(|e| format!("{}: {e}", path.display())),
        None => {
            print!("{report}");
            Ok(())
        }
    }
}

// ── Collecte ──────────────────────────────────────────────────────────────────

fn collect_sql(dir: &Path, layout: Layout) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                stack.push(path);
            } else if layout.accepts(&path) {
                out.push(path);
            }
        }
    }
    // Tri par chemin : l'ordre d'application des migrations horodatées.
    out.sort();
    Ok(out)
}

// ── Statistiques ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    files_unreadable: usize,
    statements: usize,
    analyzed: usize,
    unparsable: BTreeMap<String, usize>,
    unsupported: BTreeMap<String, usize>,
    functions: usize,
    bodies_parsed: usize,
    bodies_sql: usize,
    bodies_unparsed: usize,
    index_skipped: usize,
    diagnostics: BTreeMap<(&'static str, &'static str), usize>,
    files_with_errors: usize,
    parse_time: Duration,
    index_time: Duration,
    check_time: Duration,
}

impl Stats {
    fn record_parse(&mut self, stmts: &[Statement], skipped: &[SkippedStatement]) {
        self.analyzed += stmts.len();
        self.statements += stmts.len();
        for stmt in stmts {
            let body = match stmt {
                Statement::Ddl(DdlStatement::CreateFunction(f)) => Some(&f.body),
                Statement::Ddl(DdlStatement::CreateProcedure(p)) => Some(&p.body),
                _ => None,
            };
            if let Some(body) = body {
                self.functions += 1;
                match body {
                    FunctionBody::Parsed(_) => self.bodies_parsed += 1,
                    FunctionBody::SqlQuery(_)
                    | FunctionBody::Sql(_)
                    | FunctionBody::SqlReturn(_) => self.bodies_sql += 1,
                    FunctionBody::Raw(_) => {}
                }
            }
        }
        for sk in skipped {
            match &sk.reason {
                SkipReason::Unparsable(_) => {
                    self.statements += 1;
                    *self.unparsable.entry(shape(&sk.sql)).or_default() += 1;
                }
                SkipReason::Unsupported(_) => {
                    self.statements += 1;
                    *self.unsupported.entry(shape(&sk.sql)).or_default() += 1;
                }
                // Le statement est compté parmi les analysés ; seul son corps
                // ne l'est pas.
                SkipReason::UnparsedBody { .. } => self.bodies_unparsed += 1,
            }
        }
    }
}

/// Forme d'un statement sans aucun nom : les mots-clés SQL non quotés sont
/// gardés, tout le reste (identifiants, littéraux, nombres) devient `…`.
/// Limitée aux 8 premiers éléments — assez pour reconnaître la construction.
fn shape(sql: &str) -> String {
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize() else {
        return "(non tokenisable)".into();
    };
    let mut parts: Vec<String> = Vec::new();
    for tok in tokens {
        let part = match tok {
            Token::Whitespace(_) | Token::Comma | Token::Period | Token::SemiColon => continue,
            Token::LParen | Token::RParen => continue,
            Token::Word(w) if w.quote_style.is_none() && w.keyword != Keyword::NoKeyword => {
                w.value.to_ascii_uppercase()
            }
            _ => "…".to_string(),
        };
        if part == "…" && parts.last().is_some_and(|p| p == "…") {
            continue;
        }
        parts.push(part);
        if parts.len() == 8 {
            parts.push("…".into());
            break;
        }
    }
    parts.dedup();
    parts.join(" ")
}

// ── Rejeu sur PostgreSQL ──────────────────────────────────────────────────────

struct PgReplay {
    major: u32,
    applied: usize,
    /// Rang (1-basé) de la première migration refusée et message masqué.
    first_failure: Option<(usize, String)>,
}

fn replay_on_postgres(
    url: &str,
    sources: &[(PathBuf, String)],
    details: bool,
) -> Result<PgReplay, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let admin = connect(url).await?;
        let major: i32 = admin
            .query_one(
                "SELECT current_setting('server_version_num')::int / 10000",
                &[],
            )
            .await
            .map_err(|e| db_message(&e))?
            .get(0);
        let db = format!("pgweave_corpus_{}", std::process::id());
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
            .await
            .map_err(|e| db_message(&e))?;
        admin
            .batch_execute(&format!("CREATE DATABASE \"{db}\""))
            .await
            .map_err(|e| db_message(&e))?;

        let mut config: tokio_postgres::Config = url.parse().map_err(|e| format!("URL : {e}"))?;
        config.dbname(&db);
        let result = async {
            let client = connect_config(&config).await?;
            let mut applied = 0;
            for (i, (path, sql)) in sources.iter().enumerate() {
                if let Err(e) = client.batch_execute(sql).await {
                    let msg = db_message(&e);
                    if details {
                        eprintln!("{}: refusée par PostgreSQL : {msg}", path.display());
                    }
                    return Ok::<_, String>((applied, Some((i + 1, redact(&msg)))));
                }
                applied += 1;
            }
            Ok((applied, None))
        }
        .await;

        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
            .await
            .map_err(|e| db_message(&e))?;
        let (applied, first_failure) = result?;
        Ok(PgReplay {
            major: major as u32,
            applied,
            first_failure,
        })
    })
}

/// Masque les identifiants cités entre guillemets par PostgreSQL.
fn redact(msg: &str) -> String {
    let mut out = String::new();
    let mut in_quote = false;
    for c in msg.chars() {
        if c == '"' {
            if in_quote {
                out.push_str("…\"");
            } else {
                out.push('"');
            }
            in_quote = !in_quote;
        } else if !in_quote {
            out.push(c);
        }
    }
    out
}

async fn connect(url: &str) -> Result<tokio_postgres::Client, String> {
    let config: tokio_postgres::Config = url.parse().map_err(|e| format!("URL : {e}"))?;
    connect_config(&config).await
}

async fn connect_config(config: &tokio_postgres::Config) -> Result<tokio_postgres::Client, String> {
    let (client, connection) = config
        .connect(tokio_postgres::NoTls)
        .await
        .map_err(|e| format!("connexion à PostgreSQL : {e}"))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

fn db_message(e: &tokio_postgres::Error) -> String {
    e.as_db_error()
        .map(|d| d.message().to_string())
        .unwrap_or_else(|| e.to_string())
}

// ── Rendu ─────────────────────────────────────────────────────────────────────

fn render(name: &str, layout: Layout, files: usize, s: &Stats, pg: Option<&PgReplay>) -> String {
    let mut o = String::new();
    let skipped_unparsable: usize = s.unparsable.values().sum();
    let skipped_unsupported: usize = s.unsupported.values().sum();
    let date = chrono::Local::now().format("%Y-%m-%d");
    let commit = git_head().unwrap_or_else(|| "inconnu".into());

    let _ = writeln!(o, "# Corpus `{name}` — taux d'analyse\n");
    let _ = writeln!(
        o,
        "> Généré le {date} par `cargo xtask corpus {name}` (pgweave `{commit}`).\n\
         > Fichiers retenus : {}.\n\
         > Agrégats uniquement : aucun identifiant du corpus n'apparaît ici.",
        layout.describe()
    );

    let _ = writeln!(o, "\n## Lecture\n");
    let _ = writeln!(o, "| Mesure | Valeur |\n|---|--:|");
    let _ = writeln!(o, "| Fichiers | {files} |");
    let _ = writeln!(o, "| Fichiers illisibles | {} |", s.files_unreadable);
    let _ = writeln!(o, "| Statements | {} |", s.statements);
    let _ = writeln!(
        o,
        "| Analysés (dans l'AST) | {} ({} %) |",
        s.analyzed,
        pct(s.analyzed, s.statements)
    );
    let _ = writeln!(
        o,
        "| Écartés : illisibles pour sqlparser | {skipped_unparsable} |"
    );
    let _ = writeln!(o, "| Écartés : non modélisés | {skipped_unsupported} |");
    let _ = writeln!(o, "| Fonctions et procédures | {} |", s.functions);
    let _ = writeln!(o, "| Corps PL/pgSQL analysés | {} |", s.bodies_parsed);
    let _ = writeln!(o, "| Corps `LANGUAGE sql` analysés | {} |", s.bodies_sql);
    let _ = writeln!(o, "| Corps non analysés | {} |", s.bodies_unparsed);

    for (title, map) in [
        ("Statements illisibles, par forme", &s.unparsable),
        ("Statements non modélisés, par forme", &s.unsupported),
    ] {
        if map.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n## {title}\n");
        let _ = writeln!(o, "| Forme | Nombre |\n|---|--:|");
        let mut rows: Vec<_> = map.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (shape, n) in rows {
            let _ = writeln!(o, "| `{shape}` | {n} |");
        }
    }

    let _ = writeln!(o, "\n## Diagnostics de `check`\n");
    let _ = writeln!(
        o,
        "Chaque migration est vérifiée contre l'index de **tout** le corpus (état \
         final), comme le fait `pgweave check` aujourd'hui. Les migrations ayant été \
         appliquées en production, une erreur est a priori un faux positif.\n"
    );
    let _ = writeln!(
        o,
        "Fichiers avec au moins une erreur : {}/{files}. Statements absents de \
         l'index : {}.\n",
        s.files_with_errors, s.index_skipped
    );
    if s.diagnostics.is_empty() {
        let _ = writeln!(o, "Aucun diagnostic.");
    } else {
        let _ = writeln!(o, "| Sévérité | Code | Nombre |\n|---|---|--:|");
        for ((sev, code), n) in &s.diagnostics {
            let _ = writeln!(o, "| {sev} | `{code}` | {n} |");
        }
    }

    let _ = writeln!(o, "\n## Rejeu sur PostgreSQL\n");
    match pg {
        None => {
            let _ = writeln!(o, "Non effectué (ni `--pg-url` ni `PGWEAVE_ORACLE_URL`).");
        }
        Some(r) => {
            let _ = writeln!(
                o,
                "PostgreSQL {} : {}/{files} migrations appliquées sur une base vierge.",
                r.major, r.applied
            );
            if let Some((rank, msg)) = &r.first_failure {
                let _ = writeln!(
                    o,
                    "\nPremière migration refusée : n° {rank} — « {msg} » (noms masqués)."
                );
            }
        }
    }

    let _ = writeln!(o, "\n## Temps (indicatif, dépend de la machine)\n");
    let _ = writeln!(o, "| Étape | Durée |\n|---|--:|");
    let _ = writeln!(
        o,
        "| Parse de tous les fichiers | {} ms |",
        s.parse_time.as_millis()
    );
    let _ = writeln!(
        o,
        "| Construction de l'index (`SchemaResolver`) | {} ms |",
        s.index_time.as_millis()
    );
    let _ = writeln!(
        o,
        "| Vérification de tous les fichiers | {} ms |",
        s.check_time.as_millis()
    );
    o
}

fn pct(n: usize, total: usize) -> usize {
    (n * 100).checked_div(total).unwrap_or(100)
}

fn git_head() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_forme_ne_contient_aucun_nom() {
        let s = shape(
            r#"ALTER TABLE "MenuComponent" ADD CONSTRAINT "fk_x" FOREIGN KEY ("a") REFERENCES "Other"("id") ON DELETE CASCADE"#,
        );
        assert!(
            !s.contains("Menu") && !s.contains("Other") && !s.contains("fk_x"),
            "{s}"
        );
        assert!(s.starts_with("ALTER TABLE … ADD CONSTRAINT …"), "{s}");
    }

    #[test]
    fn redact_masque_les_identifiants() {
        assert_eq!(
            redact(r#"relation "MenuComponent" does not exist"#),
            r#"relation "…" does not exist"#
        );
    }
}
