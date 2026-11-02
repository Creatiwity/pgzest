//! Sondes générées depuis le catalogue reconstruit d'un corpus : pour chaque
//! relation du projet, `SELECT *` (types et noms des colonnes), puis par
//! colonne un filtre `= $1` et, pour une table, une insertion `VALUES ($1)`
//! (types des paramètres, colonnes générées). Préparées par PostgreSQL sur
//! la base des migrations rejouées, et décrites par `pgweave-sema`.
//!
//! Le rapport n'en publie que des agrégats ; le détail (noms de relations)
//! ne va que sur la sortie d'erreur, avec `--details`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pgweave_catalog::model::{RelationKind, quote_sql_ident};
use pgweave_catalog::{Catalog, QName, SearchPath};
use pgweave_sema::{Analyzer, Ty, TypeSystem};

/// Une sonde : sa famille et son SQL.
pub struct Probe {
    pub kind: &'static str,
    pub sql: String,
    /// Requête d'un corps de routine : préparée avec ces types de
    /// paramètres (les variables), et décrite d'avance par sema dans la
    /// portée de la routine.
    pub typed: Option<TypedProbe>,
}

pub struct TypedProbe {
    /// Types des paramètres, qualifiés et quotés pour `PREPARE`.
    pub params: Vec<String>,
    /// SQL exécuté avant, dans une transaction annulée ensuite : tables
    /// temporaires du corps, tables de transition.
    pub setup: Vec<String>,
    /// Description par pgweave : types des colonnes, qualifiés.
    pub expected: Result<Described, String>,
}

/// Description d'une requête : types des paramètres, nom et type des
/// colonnes.
pub type Described = (Vec<String>, Vec<(String, String)>);

fn qualified(q: &QName) -> String {
    format!(
        "{}.{}",
        quote_sql_ident(&q.schema),
        quote_sql_ident(&q.name)
    )
}

/// Relations du projet : hors schémas système et hors membres d'extensions.
fn project_relations(cat: &Catalog) -> Vec<QName> {
    let mut from_extensions: Vec<QName> = Vec::new();
    for (ext, pack) in &cat.extension_packs {
        let Some(schema) = cat.extensions.get(ext).map(|e| e.schema.clone()) else {
            continue;
        };
        from_extensions.extend(
            pack.relations
                .iter()
                .map(|r| QName::new(&schema, &r.name.name)),
        );
    }
    cat.relations
        .keys()
        .filter(|q| {
            !matches!(
                q.schema.as_str(),
                "pg_catalog" | "information_schema" | "pg_temp"
            )
        })
        .filter(|q| !from_extensions.contains(q))
        .cloned()
        .collect()
}

pub fn generate(cat: &Catalog) -> Vec<Probe> {
    let mut out = Vec::new();
    for q in project_relations(cat) {
        let rel = &cat.relations[&q];
        let name = qualified(&q);
        out.push(Probe {
            kind: "SELECT *",
            sql: format!("SELECT * FROM {name}"),
            typed: None,
        });
        for c in &rel.columns {
            let col = quote_sql_ident(&c.name);
            out.push(Probe {
                kind: "WHERE colonne = $1",
                sql: format!("SELECT 1 FROM {name} WHERE {col} = $1"),
                typed: None,
            });
            if matches!(
                rel.kind,
                RelationKind::Table | RelationKind::PartitionedTable
            ) {
                out.push(Probe {
                    kind: "INSERT … VALUES ($1)",
                    sql: format!("INSERT INTO {name} ({col}) VALUES ($1)"),
                    typed: None,
                });
            }
        }
    }
    out
}

/// Sondes des requêtes de corps de routines : chaque référence à une
/// variable devient un paramètre `$n` du type établi par sema, comme
/// PL/pgSQL la passe à la requête qu'il prépare.
/// Renvoie aussi le nombre de requêtes non sondées : non analysées par
/// pgweave (un accord de refus n'y prouverait rien), ou dont une référence à
/// une variable n'a pas de position dans l'AST.
pub fn body_probes(
    cat: &Catalog,
    queries: Vec<pgweave_sema::check::BodyQuery>,
) -> (Vec<Probe>, usize) {
    let ts = TypeSystem::new(cat);
    let mut out = Vec::new();
    let mut skipped = 0;
    for q in queries {
        if q.result
            .as_ref()
            .is_err_and(|e| e.starts_with("[not-analyzed]"))
        {
            skipped += 1;
            continue;
        }
        let Some((sql, params)) = substitute(&q) else {
            skipped += 1;
            continue;
        };
        let setup = q.setup.clone();
        let expected = q.result.map(|cols| {
            (
                params.iter().map(qualified_type).collect(),
                cols.iter()
                    .map(|c| (String::new(), qualified_type(&ts.base_type(&c.ty))))
                    .collect(),
            )
        });
        out.push(Probe {
            kind: "requête de corps de routine",
            sql,
            typed: Some(TypedProbe {
                params: params
                    .iter()
                    .map(|t| {
                        format!(
                            "{}.{}",
                            quote_sql_ident(&t.schema),
                            quote_sql_ident(&t.name)
                        )
                    })
                    .collect(),
                setup,
                expected,
            }),
        });
    }
    (out, skipped)
}

fn qualified_type(t: &Ty) -> String {
    format!("{}.{}", t.schema, t.name)
}

/// SQL de la requête, les variables remplacées par `$1`, `$2`… (une par
/// position), et les types de ces paramètres. `None` si une référence n'a
/// pas de position dans l'AST.
fn substitute(q: &pgweave_sema::check::BodyQuery) -> Option<(String, Vec<Ty>)> {
    use serde_json::Value;
    let mut refs: Vec<(u32, u32, Ty)> = Vec::new();
    for (span, ty) in &q.var_refs {
        if *span == pgweave_ast::Span::DUMMY {
            return None;
        }
        if !refs
            .iter()
            .any(|(s, e, _)| (*s, *e) == (span.start, span.end))
        {
            refs.push((span.start, span.end, ty.clone()));
        }
    }
    let mut json = serde_json::to_value(&q.statement).ok()?;
    let mut replaced = vec![false; refs.len()];
    fn walk(v: &mut Value, refs: &[(u32, u32, Ty)], replaced: &mut [bool]) {
        match v {
            Value::Object(map) => {
                let hit = match (map.get("span"), map.get("kind")) {
                    (Some(span), Some(Value::Object(kind)))
                        if kind.contains_key("Column") || kind.contains_key("Parameter") =>
                    {
                        let start = span.get("start").and_then(Value::as_u64);
                        let end = span.get("end").and_then(Value::as_u64);
                        refs.iter().position(|(s, e, _)| {
                            Some(u64::from(*s)) == start && Some(u64::from(*e)) == end
                        })
                    }
                    _ => None,
                };
                if let Some(i) = hit {
                    replaced[i] = true;
                    map.insert(
                        "kind".into(),
                        serde_json::json!({ "Parameter": format!("${}", i + 1) }),
                    );
                    return;
                }
                for x in map.values_mut() {
                    walk(x, refs, replaced);
                }
            }
            Value::Array(items) => {
                for x in items {
                    walk(x, refs, replaced);
                }
            }
            _ => {}
        }
    }
    walk(&mut json, &refs, &mut replaced);
    if replaced.iter().any(|r| !r) {
        return None;
    }
    let stmt: pgweave_ast::Statement = serde_json::from_value(json).ok()?;
    let sql = pgweave_ast::emit::emit_statement(&stmt);
    Some((sql, refs.into_iter().map(|(_, _, t)| t).collect()))
}

/// Description par PostgreSQL d'une requête à paramètres typés : `PREPARE`,
/// puis les types de résultat de `pg_prepared_statements` (qualifiés).
pub async fn theirs_typed(
    client: &tokio_postgres::Client,
    sql: &str,
    params: &[String],
    setup: &[String],
) -> Result<Described, String> {
    client
        .batch_execute("BEGIN")
        .await
        .map_err(|e| e.to_string())?;
    let mut result = Ok(());
    for s in setup {
        if let Err(e) = client.batch_execute(s).await {
            result = Err(e
                .as_db_error()
                .map(|d| format!("contexte : {}", d.message()))
                .unwrap_or_else(|| e.to_string()));
            break;
        }
    }
    let described = match result {
        Ok(()) => prepare_typed(client, sql, params).await,
        Err(e) => Err(e),
    };
    client
        .batch_execute("ROLLBACK")
        .await
        .map_err(|e| e.to_string())?;
    described
}

async fn prepare_typed(
    client: &tokio_postgres::Client,
    sql: &str,
    params: &[String],
) -> Result<Described, String> {
    let message = |e: tokio_postgres::Error| {
        e.as_db_error()
            .map(|d| d.message().to_string())
            .unwrap_or_else(|| e.to_string())
    };
    // Un nom par sondage : une instruction préparée survit à l'annulation
    // de la transaction.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let name = format!(
        "pgweave_body_probe_{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let sql = sql.trim_end().trim_end_matches(';');
    let types = if params.is_empty() {
        String::new()
    } else {
        format!(" ({})", params.join(", "))
    };
    client
        .batch_execute(&format!("PREPARE {name}{types} AS {sql}"))
        .await
        .map_err(message)?;
    let row = client
        .query_one(
            "SELECT coalesce((
                 SELECT array_agg(format('%s.%s', n.nspname, t.typname) ORDER BY u.i)
                 FROM unnest(p.result_types::oid[]) WITH ORDINALITY AS u(oid, i)
                 JOIN pg_type t ON t.oid = u.oid
                 JOIN pg_namespace n ON n.oid = t.typnamespace
             ), '{}')
             FROM pg_prepared_statements p WHERE p.name = $1",
            &[&name],
        )
        .await
        .map_err(message);
    client
        .batch_execute(&format!("DEALLOCATE {name}"))
        .await
        .map_err(message)?;
    let types: Vec<String> = row?.get(0);
    // Paramètres : ceux qui ont été déclarés, qualifiés comme les colonnes.
    let params = params.iter().map(|p| p.replace('"', "")).collect();
    Ok((
        params,
        types.into_iter().map(|t| (String::new(), t)).collect(),
    ))
}

/// Nom d'un type comme le rapporte le `Prepare` : `[]` pour le tableau
/// attitré d'un élément.
fn pg_name(ts: &TypeSystem, ty: &Ty) -> String {
    match ts.element(ty) {
        Some(e) if ts.array_of(&e).as_ref() == Some(ty) => format!("{}[]", e.name),
        _ => ty.name.clone(),
    }
}

/// Description par pgweave.
pub fn ours(cat: &Catalog, path: &SearchPath, sql: &str) -> Result<Described, String> {
    let stmts = pgweave_parser::parse(sql).map_err(|e| e.to_string())?;
    let [stmt] = stmts.as_slice() else {
        return Err("un statement attendu".into());
    };
    let ts = TypeSystem::new(cat);
    let mut a = Analyzer::new(TypeSystem::new(cat), path.clone());
    let d = a
        .describe(stmt)
        .map_err(|e| format!("[{}] {}", e.code, e.message))?;
    let params = d
        .params
        .iter()
        .map(|p| {
            p.ty.as_ref()
                .map(|t| pg_name(&ts, t))
                .ok_or("paramètre non typé")
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Le protocole décrit une colonne de domaine par son type de base.
    let columns = d
        .columns
        .iter()
        .map(|c| (c.name.clone(), pg_name(&ts, &ts.base_type(&c.ty))))
        .collect();
    Ok((params, columns))
}

/// Description par PostgreSQL (`Prepare`, sans exécution).
pub async fn theirs(client: &tokio_postgres::Client, sql: &str) -> Result<Described, String> {
    use tokio_postgres::types::Kind;
    let name = |t: &tokio_postgres::types::Type| match t.kind() {
        Kind::Array(inner) => format!("{}[]", inner.name()),
        _ => t.name().to_string(),
    };
    match client.prepare(sql).await {
        Ok(stmt) => Ok((
            stmt.params().iter().map(name).collect(),
            stmt.columns()
                .iter()
                .map(|c| (c.name().to_string(), name(c.type_())))
                .collect(),
        )),
        Err(e) => Err(e
            .as_db_error()
            .map(|d| d.message().to_string())
            .unwrap_or_else(|| e.to_string())),
    }
}

/// Accord par famille de sondes : (total, identiques).
#[derive(Default)]
pub struct ProbeStats {
    pub by_kind: BTreeMap<&'static str, (usize, usize)>,
    /// Requêtes de corps de routines non sondées.
    pub bodies_skipped: usize,
}

pub fn compare(
    probes: &[Probe],
    pg: &[Result<Described, String>],
    cat: &Catalog,
    path: &SearchPath,
    details: bool,
) -> ProbeStats {
    let mut stats = ProbeStats::default();
    for (probe, theirs) in probes.iter().zip(pg) {
        let mine = match &probe.typed {
            Some(t) => t.expected.clone(),
            None => ours(cat, path, &probe.sql),
        };
        let same = match (theirs, &mine) {
            (Ok(a), Ok(b)) => a == b,
            // Refusée des deux côtés : le message n'est pas comparé.
            (Err(_), Err(_)) => true,
            _ => false,
        };
        let entry = stats.by_kind.entry(probe.kind).or_default();
        entry.0 += 1;
        if same {
            entry.1 += 1;
        } else if details {
            eprintln!(
                "sonde : {}\n    PostgreSQL : {theirs:?}\n    pgweave    : {mine:?}",
                probe.sql
            );
        }
    }
    stats
}

pub fn render(o: &mut String, stats: Option<&ProbeStats>) {
    let _ = writeln!(o, "\n## Requêtes générées (oracle)\n");
    let Some(s) = stats else {
        let _ = writeln!(o, "Non vérifié (rejeu PostgreSQL absent ou incomplet).");
        return;
    };
    let _ = writeln!(
        o,
        "Sondes tirées du catalogue reconstruit, préparées par PostgreSQL sur la base des \
         migrations et décrites par `pgweave-sema` (types des paramètres, noms et types des \
         colonnes ; un refus des deux côtés compte comme accord). Les requêtes des corps de \
         routines en vigueur sont préparées avec leurs variables en paramètres typés \
         (`PREPARE`), comme PL/pgSQL les exécute ; seuls les types des colonnes sont \
         comparés :\n"
    );
    let _ = writeln!(o, "| Sonde | Total | Identiques |\n|---|--:|--:|");
    for (kind, (total, same)) in &s.by_kind {
        let _ = writeln!(o, "| {kind} | {total} | {same} |");
    }
    let _ = writeln!(
        o,
        "\nRequêtes de corps non sondées (non analysées par pgweave, ou variable sans \
         position dans l'AST) : {}.",
        s.bodies_skipped
    );
}
