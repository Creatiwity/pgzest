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
        });
        for c in &rel.columns {
            let col = quote_sql_ident(&c.name);
            out.push(Probe {
                kind: "WHERE colonne = $1",
                sql: format!("SELECT 1 FROM {name} WHERE {col} = $1"),
            });
            if matches!(
                rel.kind,
                RelationKind::Table | RelationKind::PartitionedTable
            ) {
                out.push(Probe {
                    kind: "INSERT … VALUES ($1)",
                    sql: format!("INSERT INTO {name} ({col}) VALUES ($1)"),
                });
            }
        }
    }
    out
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
        let mine = ours(cat, path, &probe.sql);
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
         colonnes ; un refus des deux côtés compte comme accord) :\n"
    );
    let _ = writeln!(o, "| Sonde | Total | Identiques |\n|---|--:|--:|");
    for (kind, (total, same)) in &s.by_kind {
        let _ = writeln!(o, "| {kind} | {total} | {same} |");
    }
}
