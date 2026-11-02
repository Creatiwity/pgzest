use tokio_postgres::types::{Kind, Type};
use tokio_postgres::{Client, Config, NoTls};

use crate::case::Case;

/// Variable d'environnement donnant l'URL d'un serveur PostgreSQL de test.
///
/// L'oracle crée une base par cas (et la supprime) : l'utilisateur doit avoir
/// le droit `CREATEDB`. Ne jamais pointer vers un serveur de production.
pub const URL_ENV: &str = "PGWEAVE_ORACLE_URL";

/// Ce que PostgreSQL dit d'une requête préparée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgDescription {
    /// Type de chaque paramètre, dans l'ordre `$1 … $n`.
    pub params: Vec<String>,
    /// Nom et type de chaque colonne de sortie.
    pub columns: Vec<(String, String)>,
}

/// Version majeure du serveur (`server_version_num / 10000`).
pub async fn server_major_version(url: &str) -> Result<u32, String> {
    let client = connect(url).await?;
    let row = client
        .query_one("SELECT current_setting('server_version_num')::int", &[])
        .await
        .map_err(|e| db_error_message(&e))?;
    let num: i32 = row.get(0);
    Ok((num / 10000) as u32)
}

/// Applique le schéma du cas sur une base vierge, puis prépare chaque requête.
///
/// Renvoie, pour chaque requête et dans l'ordre, la description de
/// PostgreSQL ou son message de refus. La base est supprimée dans tous les
/// cas, y compris en erreur.
pub async fn describe_case(
    admin_url: &str,
    case: &Case,
) -> Result<Vec<Result<PgDescription, String>>, String> {
    let admin = connect(admin_url).await?;
    let db = format!(
        "pgweave_oracle_{}_{}",
        case.name.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
        std::process::id()
    );
    // Deux appels distincts : plusieurs statements dans un même envoi forment
    // un bloc transactionnel implicite, que CREATE DATABASE refuse.
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .await
        .map_err(|e| format!("nettoyage de la base {db} : {}", db_error_message(&e)))?;
    admin
        .batch_execute(&format!("CREATE DATABASE \"{db}\""))
        .await
        .map_err(|e| format!("création de la base {db} : {}", db_error_message(&e)))?;

    let result = describe_in(admin_url, &db, case).await;

    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .await
        .map_err(|e| format!("suppression de la base {db} : {}", db_error_message(&e)))?;
    result
}

async fn describe_in(
    admin_url: &str,
    db: &str,
    case: &Case,
) -> Result<Vec<Result<PgDescription, String>>, String> {
    let mut config: Config = admin_url.parse().map_err(|e| format!("URL : {e}"))?;
    config.dbname(db);
    let client = connect_config(&config).await?;

    client.batch_execute(&case.schema).await.map_err(|e| {
        format!(
            "cas `{}` : schema.sql refusé par PostgreSQL : {}",
            case.name,
            db_error_message(&e)
        )
    })?;

    let mut out = Vec::new();
    for q in &case.queries {
        let described = match client.prepare(&q.positional_sql()).await {
            Ok(stmt) => Ok(PgDescription {
                params: stmt.params().iter().map(type_name).collect(),
                columns: stmt
                    .columns()
                    .iter()
                    .map(|c| (c.name().to_string(), type_name(c.type_())))
                    .collect(),
            }),
            Err(e) => Err(db_error_message(&e)),
        };
        out.push(described);
    }
    Ok(out)
}

/// Nom lisible et stable d'un type : nom interne de PostgreSQL (`int8`,
/// `timestamptz`), suffixe `[]` pour un tableau.
fn type_name(ty: &Type) -> String {
    match ty.kind() {
        Kind::Array(inner) => format!("{}[]", inner.name()),
        _ => ty.name().to_string(),
    }
}

pub(crate) fn db_error_message(e: &tokio_postgres::Error) -> String {
    e.as_db_error()
        .map(|d| d.message().to_string())
        .unwrap_or_else(|| e.to_string())
}

async fn connect(url: &str) -> Result<Client, String> {
    connect_url(url).await
}

pub(crate) async fn connect_url(url: &str) -> Result<Client, String> {
    let config: Config = url.parse().map_err(|e| format!("URL : {e}"))?;
    connect_config(&config).await
}

pub(crate) async fn connect_config(config: &Config) -> Result<Client, String> {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .map_err(|e| format!("connexion à PostgreSQL : {e}"))?;
    tokio::spawn(async move {
        // Une erreur de connexion se manifeste aussi sur les requêtes du
        // client, qui la remontent : rien à faire de plus ici.
        let _ = connection.await;
    });
    Ok(client)
}
