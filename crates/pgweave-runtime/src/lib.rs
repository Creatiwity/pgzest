//! Exécution des requêtes générées par pgweave (PLAN.md D8, P5).
//!
//! Le code généré décrit chaque requête par une [`Query`] (SQL positionnel,
//! types des paramètres) et appelle la fonction alignée sur sa cardinalité :
//! [`fetch_one`], [`fetch_optional`], [`fetch_all`] ou [`execute`].
//!
//! - Types des paramètres connus à la compilation (types intégrés, OID
//!   fixe) : `query_typed`, sans aller-retour de préparation.
//! - Type défini par le projet (enum, domaine…) : son OID n'existe qu'en
//!   base ; la requête est préparée, le serveur fixe les types.
//! - Lignes décodées par index de colonne, dans des structs générées.
//!
//! Tout client convient : [`GenericClient`] couvre client, transaction et
//! connexion de pool.

use std::fmt;

pub use tokio_postgres::types::{self, FromSql, ToSql, Type};
pub use tokio_postgres::{self, GenericClient, Row};

/// Une requête générée.
#[derive(Debug, Clone, Copy)]
pub struct Query {
    pub name: &'static str,
    /// SQL envoyé au serveur (paramètres positionnels).
    pub sql: &'static str,
    /// OID de chaque paramètre ; `None` si l'un d'eux est un type du projet,
    /// dont l'OID n'est connu qu'en base.
    pub param_oids: Option<&'static [u32]>,
}

/// Paramètre lié à une requête.
pub type Param<'a> = &'a (dyn ToSql + Sync);

#[derive(Debug)]
pub enum Error {
    /// Erreur renvoyée par PostgreSQL ou par la connexion.
    Db {
        query: &'static str,
        source: tokio_postgres::Error,
    },
    /// Nombre de lignes contraire à la cardinalité de la requête.
    RowCount {
        query: &'static str,
        expected: &'static str,
        got: usize,
    },
    /// Valeur de colonne non décodable vers le type généré.
    Decode {
        query: &'static str,
        source: tokio_postgres::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Db { query, source } => write!(f, "requête `{query}` : {source}"),
            Error::RowCount {
                query,
                expected,
                got,
            } => write!(
                f,
                "requête `{query}` : {expected} attendue, {got} ligne(s) renvoyée(s)"
            ),
            Error::Decode { query, source } => {
                write!(f, "requête `{query}` : décodage d'une ligne : {source}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Db { source, .. } | Error::Decode { source, .. } => Some(source),
            Error::RowCount { .. } => None,
        }
    }
}

/// Décodage d'une ligne vers la struct générée.
pub type Decode<T> = fn(&Row) -> Result<T, tokio_postgres::Error>;

/// Types des paramètres, s'ils sont tous connus de `tokio-postgres`.
fn static_types(q: &Query) -> Option<Vec<Type>> {
    q.param_oids?
        .iter()
        .map(|oid| Type::from_oid(*oid))
        .collect()
}

async fn rows<C: GenericClient + ?Sized>(
    client: &C,
    q: &Query,
    params: &[Param<'_>],
) -> Result<Vec<Row>, Error> {
    let db = |source| Error::Db {
        query: q.name,
        source,
    };
    match static_types(q) {
        Some(types) => {
            let typed: Vec<(Param<'_>, Type)> = params.iter().copied().zip(types).collect();
            client.query_typed(q.sql, &typed).await.map_err(db)
        }
        None => client.query(q.sql, params).await.map_err(db),
    }
}

fn decode<T>(q: &Query, row: &Row, f: Decode<T>) -> Result<T, Error> {
    f(row).map_err(|source| Error::Decode {
        query: q.name,
        source,
    })
}

/// Toutes les lignes.
pub async fn fetch_all<C: GenericClient + ?Sized, T>(
    client: &C,
    q: &Query,
    params: &[Param<'_>],
    f: Decode<T>,
) -> Result<Vec<T>, Error> {
    rows(client, q, params)
        .await?
        .iter()
        .map(|r| decode(q, r, f))
        .collect()
}

/// Zéro ou une ligne ; plus d'une est une erreur.
pub async fn fetch_optional<C: GenericClient + ?Sized, T>(
    client: &C,
    q: &Query,
    params: &[Param<'_>],
    f: Decode<T>,
) -> Result<Option<T>, Error> {
    match rows(client, q, params).await?.as_slice() {
        [] => Ok(None),
        [row] => decode(q, row, f).map(Some),
        more => Err(Error::RowCount {
            query: q.name,
            expected: "au plus une ligne",
            got: more.len(),
        }),
    }
}

/// Exactement une ligne.
pub async fn fetch_one<C: GenericClient + ?Sized, T>(
    client: &C,
    q: &Query,
    params: &[Param<'_>],
    f: Decode<T>,
) -> Result<T, Error> {
    match rows(client, q, params).await?.as_slice() {
        [row] => decode(q, row, f),
        other => Err(Error::RowCount {
            query: q.name,
            expected: "exactement une ligne",
            got: other.len(),
        }),
    }
}

/// Statement sans ligne renvoyée : nombre de lignes touchées.
pub async fn execute<C: GenericClient + ?Sized>(
    client: &C,
    q: &Query,
    params: &[Param<'_>],
) -> Result<u64, Error> {
    let db = |source| Error::Db {
        query: q.name,
        source,
    };
    match static_types(q) {
        Some(types) => {
            let typed: Vec<(Param<'_>, Type)> = params.iter().copied().zip(types).collect();
            client.execute_typed(q.sql, &typed).await.map_err(db)
        }
        None => client.execute(q.sql, params).await.map_err(db),
    }
}

/// Enum PostgreSQL vue comme ses libellés : encodage et décodage texte
/// (binaire d'un enum = son libellé). Implémentée par le code généré.
pub trait PgEnum: Sized {
    /// Nom du type en base (`pg_type.typname`).
    const TYPE_NAME: &'static str;
    fn label(&self) -> &'static str;
    fn from_label(label: &str) -> Option<Self>;
}

/// Décodage d'un enum généré.
pub fn enum_from_sql<E: PgEnum>(raw: &[u8]) -> Result<E, Box<dyn std::error::Error + Sync + Send>> {
    let label = std::str::from_utf8(raw)?;
    E::from_label(label)
        .ok_or_else(|| format!("libellé inconnu pour {} : {label:?}", E::TYPE_NAME).into())
}

/// Encodage d'un enum généré.
pub fn enum_to_sql<E: PgEnum>(
    e: &E,
    out: &mut types::private::BytesMut,
) -> Result<types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
    out.extend_from_slice(e.label().as_bytes());
    Ok(types::IsNull::No)
}
