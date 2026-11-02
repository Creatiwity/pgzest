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

pub use futures_util;
pub use tokio;
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
    /// Lecture des données d'un `COPY`.
    Io {
        query: &'static str,
        source: std::io::Error,
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
            Error::Io { query, source } => {
                write!(f, "requête `{query}` : lecture des données : {source}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Db { source, .. } | Error::Decode { source, .. } => Some(source),
            Error::Io { source, .. } => Some(source),
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

// ── Requêtes inline (`query!`) ────────────────────────────────────────────────

/// Valeur liée à un paramètre d'une requête inline, possédée par elle.
pub type Bound<'a> = Box<dyn ToSql + Sync + 'a>;

/// Lie une valeur à un paramètre (code généré par `query!`).
pub fn bind<'a, T: ToSql + Sync + 'a>(v: T) -> Bound<'a> {
    Box::new(v)
}

/// Marqueurs de cardinalité d'une requête inline.
pub mod cardinality {
    /// Exactement une ligne.
    pub struct One;
    /// Zéro ou une ligne.
    pub struct Optional;
    /// Un nombre quelconque de lignes.
    pub struct Many;
    /// Aucune ligne renvoyée.
    pub struct NoRows;
}

/// Requête inline prête à s'exécuter : ses paramètres liés, sa ligne, sa
/// cardinalité `K`. `fetch` renvoie ce que la cardinalité promet ; les
/// variantes plus étroites sont des contrats vérifiés à l'exécution.
pub struct Inline<'a, T, K> {
    query: &'static Query,
    params: Vec<Bound<'a>>,
    decode: Decode<T>,
    _cardinality: std::marker::PhantomData<K>,
}

impl<'a, T, K> Inline<'a, T, K> {
    pub fn new(query: &'static Query, params: Vec<Bound<'a>>, decode: Decode<T>) -> Self {
        Inline {
            query,
            params,
            decode,
            _cardinality: std::marker::PhantomData,
        }
    }

    fn params(&self) -> Vec<Param<'_>> {
        self.params
            .iter()
            .map(|b| b.as_ref() as Param<'_>)
            .collect()
    }
}

impl<T> Inline<'_, T, cardinality::One> {
    pub async fn fetch<C: GenericClient + ?Sized>(self, client: &C) -> Result<T, Error> {
        fetch_one(client, self.query, &self.params(), self.decode).await
    }
}

impl<T> Inline<'_, T, cardinality::Optional> {
    pub async fn fetch<C: GenericClient + ?Sized>(self, client: &C) -> Result<Option<T>, Error> {
        fetch_optional(client, self.query, &self.params(), self.decode).await
    }

    /// Exactement une ligne attendue : une absence est une erreur.
    pub async fn fetch_one<C: GenericClient + ?Sized>(self, client: &C) -> Result<T, Error> {
        fetch_one(client, self.query, &self.params(), self.decode).await
    }
}

impl<T> Inline<'_, T, cardinality::Many> {
    pub async fn fetch<C: GenericClient + ?Sized>(self, client: &C) -> Result<Vec<T>, Error> {
        fetch_all(client, self.query, &self.params(), self.decode).await
    }

    /// Au plus une ligne attendue.
    pub async fn fetch_optional<C: GenericClient + ?Sized>(
        self,
        client: &C,
    ) -> Result<Option<T>, Error> {
        fetch_optional(client, self.query, &self.params(), self.decode).await
    }

    /// Exactement une ligne attendue.
    pub async fn fetch_one<C: GenericClient + ?Sized>(self, client: &C) -> Result<T, Error> {
        fetch_one(client, self.query, &self.params(), self.decode).await
    }
}

impl Inline<'_, (), cardinality::NoRows> {
    /// Nombre de lignes touchées.
    pub async fn execute<C: GenericClient + ?Sized>(self, client: &C) -> Result<u64, Error> {
        execute(client, self.query, &self.params()).await
    }
}

// ── COPY … FROM STDIN ─────────────────────────────────────────────────────────

/// Client capable de `COPY … FROM STDIN` : client nu ou transaction
/// (`GenericClient` ne l'expose pas).
pub trait CopyClient: GenericClient {
    fn copy_in_sink(
        &self,
        sql: &str,
    ) -> impl std::future::Future<
        Output = Result<tokio_postgres::CopyInSink<bytes::Bytes>, tokio_postgres::Error>,
    > + Send;
}

impl CopyClient for tokio_postgres::Client {
    async fn copy_in_sink(
        &self,
        sql: &str,
    ) -> Result<tokio_postgres::CopyInSink<bytes::Bytes>, tokio_postgres::Error> {
        self.copy_in(sql).await
    }
}

impl CopyClient for tokio_postgres::Transaction<'_> {
    async fn copy_in_sink(
        &self,
        sql: &str,
    ) -> Result<tokio_postgres::CopyInSink<bytes::Bytes>, tokio_postgres::Error> {
        self.copy_in(sql).await
    }
}

/// Un `COPY … FROM STDIN` généré.
#[derive(Debug, Clone, Copy)]
pub struct CopyIn {
    pub name: &'static str,
    pub sql: &'static str,
    /// Table et colonnes alimentées, quotées, pour retrouver leurs types.
    pub table: &'static str,
    pub columns: &'static str,
    /// OID des colonnes, `None` si l'une est d'un type du projet.
    pub column_oids: Option<&'static [u32]>,
}

/// Écriture binaire typée d'un `COPY` : une ligne par appel à [`write`].
///
/// [`write`]: BinaryCopy::write
pub struct BinaryCopy {
    copy: &'static CopyIn,
    writer: std::pin::Pin<Box<tokio_postgres::binary_copy::BinaryCopyInWriter>>,
}

impl BinaryCopy {
    /// Ouvre le `COPY` ; les types des colonnes sont ceux du catalogue
    /// (OID fixes) ou, pour un type du projet, ceux de la base.
    pub async fn open<C: CopyClient + ?Sized>(
        client: &C,
        copy: &'static CopyIn,
    ) -> Result<Self, Error> {
        let db = |source| Error::Db {
            query: copy.name,
            source,
        };
        let types: Vec<Type> = match copy.column_oids.and_then(|oids| {
            oids.iter()
                .map(|o| Type::from_oid(*o))
                .collect::<Option<Vec<_>>>()
        }) {
            Some(t) => t,
            None => client
                .prepare(&format!(
                    "SELECT {} FROM {} LIMIT 0",
                    copy.columns, copy.table
                ))
                .await
                .map_err(db)?
                .columns()
                .iter()
                .map(|c| c.type_().clone())
                .collect(),
        };
        let sink = client.copy_in_sink(copy.sql).await.map_err(db)?;
        Ok(BinaryCopy {
            copy,
            writer: Box::pin(tokio_postgres::binary_copy::BinaryCopyInWriter::new(
                sink, &types,
            )),
        })
    }

    pub async fn write(&mut self, values: &[Param<'_>]) -> Result<(), Error> {
        self.writer
            .as_mut()
            .write(values)
            .await
            .map_err(|source| Error::Db {
                query: self.copy.name,
                source,
            })
    }

    /// Termine le `COPY` : nombre de lignes copiées.
    pub async fn finish(mut self) -> Result<u64, Error> {
        self.writer
            .as_mut()
            .finish()
            .await
            .map_err(|source| Error::Db {
                query: self.copy.name,
                source,
            })
    }
}

/// `COPY` au format texte ou CSV : les octets de `reader`, tels quels, par
/// blocs. Renvoie le nombre de lignes copiées.
pub async fn copy_in_stream<C, R>(
    client: &C,
    copy: &'static CopyIn,
    mut reader: R,
) -> Result<u64, Error>
where
    C: CopyClient + ?Sized,
    R: tokio::io::AsyncRead + Unpin,
{
    use futures_util::SinkExt;
    use tokio::io::AsyncReadExt;
    let db = |source| Error::Db {
        query: copy.name,
        source,
    };
    let sink = client.copy_in_sink(copy.sql).await.map_err(db)?;
    let mut sink = Box::pin(sink);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).await.map_err(|e| Error::Io {
            query: copy.name,
            source: e,
        })?;
        if n == 0 {
            break;
        }
        sink.send(bytes::Bytes::copy_from_slice(&buf[..n]))
            .await
            .map_err(db)?;
    }
    sink.as_mut().finish().await.map_err(db)
}

// ── Flux de lignes ────────────────────────────────────────────────────────────

/// Lignes décodées au fil de leur arrivée, sans tout charger en mémoire.
pub async fn stream<C: GenericClient + ?Sized, T>(
    client: &C,
    q: &'static Query,
    params: &[Param<'_>],
    f: Decode<T>,
) -> Result<impl futures_util::Stream<Item = Result<T, Error>> + Send + use<T, C>, Error>
where
    T: Send + 'static,
{
    use futures_util::StreamExt;
    let db = |source| Error::Db {
        query: q.name,
        source,
    };
    let rows = match static_types(q) {
        Some(types) => client
            .query_typed_raw(q.sql, params.iter().copied().zip(types))
            .await
            .map_err(db)?,
        None => client
            .query_raw(q.sql, params.iter().copied())
            .await
            .map_err(db)?,
    };
    let name = q.name;
    Ok(rows.map(move |r| match r {
        Ok(row) => f(&row).map_err(|source| Error::Decode {
            query: name,
            source,
        }),
        Err(source) => Err(Error::Db {
            query: name,
            source,
        }),
    }))
}
