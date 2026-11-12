use tokio_postgres::Client;
use tokio_postgres::types::{Kind, Type};

use crate::connect::Database;

use crate::case::Case;

/// Environment variable giving the URL of a test PostgreSQL server.
///
/// The oracle creates one database per case (and drops it): the user needs
/// the `CREATEDB` privilege. Never point it at a production server.
pub const URL_ENV: &str = "PGZEST_ORACLE_URL";

/// What PostgreSQL says about a prepared query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgDescription {
    /// Type of each parameter, in `$1 … $n` order.
    pub params: Vec<String>,
    /// Name and type of each output column.
    pub columns: Vec<(String, String)>,
}

/// Server major version (`server_version_num / 10000`).
pub async fn server_major_version(url: &str) -> Result<u32, String> {
    let client = connect(url).await?;
    let row = client
        .query_one("SELECT current_setting('server_version_num')::int", &[])
        .await
        .map_err(|e| db_error_message(&e))?;
    let num: i32 = row.get(0);
    Ok((num / 10000) as u32)
}

/// Applies the case schema to an empty database, then prepares each query.
///
/// Returns, for each query and in order, PostgreSQL's description or its
/// refusal message. The database is dropped in every case, including on
/// error.
pub async fn describe_case(
    admin_url: &str,
    case: &Case,
) -> Result<Vec<Result<PgDescription, String>>, String> {
    let admin = connect(admin_url).await?;
    let db = format!(
        "pgzest_oracle_{}_{}",
        case.name.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
        std::process::id()
    );
    // Two separate calls: several statements in one send form an implicit
    // transaction block, which CREATE DATABASE rejects.
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .await
        .map_err(|e| format!("cleaning up database {db}: {}", db_error_message(&e)))?;
    admin
        .batch_execute(&format!("CREATE DATABASE \"{db}\""))
        .await
        .map_err(|e| format!("creating database {db}: {}", db_error_message(&e)))?;

    let result = describe_in(admin_url, &db, case).await;

    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .await
        .map_err(|e| format!("dropping database {db}: {}", db_error_message(&e)))?;
    result
}

async fn describe_in(
    admin_url: &str,
    db: &str,
    case: &Case,
) -> Result<Vec<Result<PgDescription, String>>, String> {
    let client = connect_database(&Database::parse(admin_url)?.with_dbname(db)).await?;

    client.batch_execute(&case.schema).await.map_err(|e| {
        format!(
            "case `{}`: schema.sql rejected by PostgreSQL: {}",
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

/// Readable, stable type name: PostgreSQL internal name (`int8`,
/// `timestamptz`), `[]` suffix for an array.
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
    connect_database(&Database::parse(url)?).await
}

pub(crate) async fn connect_database(database: &Database) -> Result<Client, String> {
    database
        .connect()
        .await
        .map_err(|e| format!("connecting to PostgreSQL: {e}"))
}
