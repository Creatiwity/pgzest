//! Connection to a database, TLS read as libpq reads it.
//!
//! tokio-postgres knows `sslmode` `disable`, `prefer` and `require`, and
//! nothing of the certificates. A managed PostgreSQL asks for more: TLS
//! required, the server checked against an authority (`verify-ca`,
//! `verify-full`, `sslrootcert`). The TLS parameters are taken out of the
//! connection string and applied as libpq does
//! (<https://www.postgresql.org/docs/current/libpq-ssl.html>); the rest goes
//! to tokio-postgres, which rejects what it does not know.
//!
//! - `sslmode`: `disable`, `prefer` (default), `require`, `verify-ca`,
//!   `verify-full`; `PGSSLMODE` when absent. `allow` (TLS only if the
//!   server insists) has no tokio-postgres equivalent: refused.
//! - `sslrootcert`: the authorities' PEM file, or `system` for the
//!   platform's store (which then requires `verify-full`, as in libpq);
//!   `PGSSLROOTCERT` when absent, then `~/.postgresql/root.crt`
//!   (`%APPDATA%\postgresql\root.crt` on Windows) if it exists.
//! - `require` and `prefer` check the certificate chain only when an
//!   authority file is there, the hostname never; `verify-ca` always checks
//!   the chain; `verify-full` the chain and the hostname.

use std::path::PathBuf;
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_postgres::Client;
use tokio_postgres::config::SslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

/// A database to connect to: tokio-postgres's configuration, and TLS.
#[derive(Clone)]
pub struct Database {
    pub config: tokio_postgres::Config,
    tls: MakeRustlsConnect,
}

/// Mode written in `sslmode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

/// Where the certificate authorities come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Roots {
    /// No authority file: the chain is not checked.
    None,
    File(PathBuf),
    /// `sslrootcert=system`.
    System,
}

impl Database {
    /// Reads a connection string: URL (`postgres://…?sslmode=verify-full`)
    /// or `key=value` pairs.
    pub fn parse(url: &str) -> Result<Self, String> {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let (rest, ssl) = split_ssl_params(url)?;
        let mut config: tokio_postgres::Config =
            rest.parse().map_err(|e| format!("database URL: {e}"))?;
        let mode = match ssl.mode.or_else(|| env("PGSSLMODE")) {
            Some(m) => Some(parse_mode(&m)?),
            None => None,
        };
        let rootcert = ssl.rootcert.or_else(|| env("PGSSLROOTCERT"));
        let roots = match rootcert.as_deref() {
            Some("system") => Roots::System,
            Some(path) => Roots::File(PathBuf::from(path)),
            None => default_root_file()
                .filter(|p| p.exists())
                .map_or(Roots::None, Roots::File),
        };
        let mode = match (mode, &roots) {
            // libpq: `system` roots imply `verify-full` and refuse a weaker
            // mode: any public authority vouches for some certificate, only
            // the hostname makes it this server's.
            (None, Roots::System) => Mode::VerifyFull,
            (Some(m), Roots::System) if m != Mode::VerifyFull => {
                return Err(format!(
                    "weak sslmode \"{}\" may not be used with sslrootcert=system (use \
                     \"verify-full\")",
                    mode_name(m)
                ));
            }
            (m, _) => m.unwrap_or(Mode::Prefer),
        };
        config.ssl_mode(match mode {
            Mode::Disable => SslMode::Disable,
            Mode::Prefer => SslMode::Prefer,
            Mode::Require | Mode::VerifyCa | Mode::VerifyFull => SslMode::Require,
        });
        let tls = client_config(mode, &roots, rootcert.is_some())?;
        Ok(Self {
            config,
            tls: MakeRustlsConnect::new(tls),
        })
    }

    /// The same server, database `name`.
    pub fn with_dbname(&self, name: &str) -> Self {
        let mut other = self.clone();
        other.config.dbname(name);
        other
    }

    /// Connects; the connection runs in the background. An error ending it
    /// also shows up on the client's queries, which report it
    /// ([`Client::is_closed`] afterwards).
    ///
    /// The error comes with its causes: tokio-postgres says "error
    /// performing TLS handshake" or "db error" and keeps the reason (unknown
    /// authority, `pg_hba.conf`…) in its source.
    pub async fn connect(&self) -> Result<Client, String> {
        let (client, connection) = self
            .config
            .connect(self.tls.clone())
            .await
            .map_err(|e| error_chain(&e))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(client)
    }
}

/// `e` and its causes, `: `-separated.
pub fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = s.source();
    }
    out
}

/// TLS parameters taken out of a connection string.
#[derive(Debug, Default, PartialEq, Eq)]
struct SslParams {
    mode: Option<String>,
    rootcert: Option<String>,
}

/// The connection string without `sslmode` and `sslrootcert`, and their
/// values.
fn split_ssl_params(url: &str) -> Result<(String, SslParams), String> {
    let mut ssl = SslParams::default();
    let mut unsupported: Option<String> = None;
    let mut take = |key: &str, value: String| match key {
        "sslmode" => {
            ssl.mode = Some(value);
            true
        }
        "sslrootcert" => {
            ssl.rootcert = Some(value);
            true
        }
        // libpq's other TLS parameters: not applied, so not accepted.
        "sslcert"
        | "sslkey"
        | "sslpassword"
        | "sslcertmode"
        | "sslcrl"
        | "sslcrldir"
        | "sslsni"
        | "sslcompression"
        | "requiressl"
        | "ssl_min_protocol_version"
        | "ssl_max_protocol_version"
        | "requirepeer"
        | "gssencmode" => {
            unsupported.get_or_insert_with(|| key.to_string());
            true
        }
        _ => false,
    };
    let rest = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let Some((base, query)) = url.split_once('?') else {
            return Ok((url.to_string(), ssl));
        };
        let kept: Vec<&str> = query
            .split('&')
            .filter(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                !take(key, percent_decode(value))
            })
            .collect();
        if kept.is_empty() {
            base.to_string()
        } else {
            format!("{base}?{}", kept.join("&"))
        }
    } else {
        let mut kept = Vec::new();
        for (key, value, raw) in key_value_pairs(url)? {
            if !take(&key, value) {
                kept.push(raw);
            }
        }
        kept.join(" ")
    };
    if let Some(key) = unsupported {
        return Err(format!("database URL: {key} is not supported"));
    }
    Ok((rest, ssl))
}

/// `%XX` sequences of a URL parameter decoded.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Pairs of a `key=value` connection string: key, unquoted value, and the
/// pair as written.
fn key_value_pairs(s: &str) -> Result<Vec<(String, String, String)>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut pairs = Vec::new();
    let mut i = 0;
    loop {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i == chars.len() {
            return Ok(pairs);
        }
        let start = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        let key: String = chars[start..i].iter().collect();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i == chars.len() || chars[i] != '=' {
            return Err(format!("database URL: missing \"=\" after \"{key}\""));
        }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < chars.len() && chars[i] == '\'' {
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("database URL: unterminated quoted value".into()),
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some('\\') if i + 1 < chars.len() => {
                        value.push(chars[i + 1]);
                        i += 2;
                    }
                    Some(c) => {
                        value.push(*c);
                        i += 1;
                    }
                }
            }
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                value.push(chars[i]);
                i += 1;
            }
        }
        pairs.push((key, value, chars[start..i].iter().collect()));
    }
}

fn parse_mode(s: &str) -> Result<Mode, String> {
    Ok(match s {
        "disable" => Mode::Disable,
        "prefer" => Mode::Prefer,
        "require" => Mode::Require,
        "verify-ca" => Mode::VerifyCa,
        "verify-full" => Mode::VerifyFull,
        "allow" => {
            return Err("sslmode \"allow\" is not supported: use \"disable\" or \"prefer\"".into());
        }
        other => return Err(format!("invalid sslmode value: \"{other}\"")),
    })
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Disable => "disable",
        Mode::Prefer => "prefer",
        Mode::Require => "require",
        Mode::VerifyCa => "verify-ca",
        Mode::VerifyFull => "verify-full",
    }
}

/// libpq's default authority file.
fn default_root_file() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("postgresql").join("root.crt"))
    } else {
        std::env::var_os("HOME").map(|d| PathBuf::from(d).join(".postgresql").join("root.crt"))
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// rustls configuration for `mode`; `explicit`: the authority file was
/// named (`sslrootcert`, `PGSSLROOTCERT`), so it must exist.
fn client_config(mode: Mode, roots: &Roots, explicit: bool) -> Result<ClientConfig, String> {
    let provider = provider();
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?;
    let store = match roots {
        Roots::None => None,
        Roots::System => {
            let loaded = rustls_native_certs::load_native_certs();
            if loaded.certs.is_empty() {
                return Err(format!(
                    "sslrootcert=system: no certificate in the system's store{}",
                    loaded
                        .errors
                        .first()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default()
                ));
            }
            let mut store = RootCertStore::empty();
            store.add_parsable_certificates(loaded.certs);
            Some(store)
        }
        Roots::File(path) => {
            if !path.exists() {
                if explicit || matches!(mode, Mode::VerifyCa | Mode::VerifyFull) {
                    return Err(missing_root(path));
                }
                None
            } else {
                let mut store = RootCertStore::empty();
                for cert in CertificateDer::pem_file_iter(path)
                    .map_err(|e| format!("sslrootcert {}: {e}", path.display()))?
                {
                    let cert = cert.map_err(|e| format!("sslrootcert {}: {e}", path.display()))?;
                    store
                        .add(cert)
                        .map_err(|e| format!("sslrootcert {}: {e}", path.display()))?;
                }
                if store.is_empty() {
                    return Err(format!(
                        "sslrootcert {}: no certificate in the file",
                        path.display()
                    ));
                }
                Some(store)
            }
        }
    };
    let verifier: Arc<dyn ServerCertVerifier> = match (mode, store) {
        (Mode::Disable, _) | (Mode::Prefer | Mode::Require, None) => {
            Arc::new(NoChainCheck(provider.signature_verification_algorithms))
        }
        (Mode::VerifyCa | Mode::VerifyFull, None) => {
            return Err(missing_root(
                &default_root_file().unwrap_or_else(|| PathBuf::from("root.crt")),
            ));
        }
        (mode, Some(store)) => {
            let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(store), provider)
                .build()
                .map_err(|e| format!("TLS: {e}"))?;
            if mode == Mode::VerifyFull {
                webpki
            } else {
                Arc::new(ChainOnly(webpki))
            }
        }
    };
    Ok(builder
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

fn missing_root(path: &std::path::Path) -> String {
    format!(
        "root certificate file \"{}\" does not exist: provide it (sslrootcert), use the \
         system's authorities (sslrootcert=system), or lower sslmode to check no certificate",
        path.display()
    )
}

/// `prefer`, `require` without authority: encrypted, the server not
/// authenticated (libpq's behavior). The handshake signatures are still
/// checked.
#[derive(Debug)]
struct NoChainCheck(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for NoChainCheck {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

/// `verify-ca`: the chain up to an authority, not the hostname.
#[derive(Debug)]
struct ChainOnly(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(url: &str) -> (String, SslParams) {
        split_ssl_params(url).expect("split")
    }

    #[test]
    fn ssl_params_are_taken_out_of_a_url() {
        assert_eq!(
            split(
                "postgres://u:p@h:5432/db?sslmode=verify-full&sslrootcert=%2Fetc%2Fca.pem&application_name=x"
            ),
            (
                "postgres://u:p@h:5432/db?application_name=x".into(),
                SslParams {
                    mode: Some("verify-full".into()),
                    rootcert: Some("/etc/ca.pem".into()),
                }
            )
        );
        assert_eq!(
            split("postgresql://h/db?sslmode=require"),
            (
                "postgresql://h/db".into(),
                SslParams {
                    mode: Some("require".into()),
                    rootcert: None,
                }
            )
        );
        assert_eq!(split("postgres://h/db").0, "postgres://h/db");
    }

    #[test]
    fn ssl_params_are_taken_out_of_key_value_pairs() {
        assert_eq!(
            split("host=h sslmode = verify-ca sslrootcert='/a b/ca.pem' password='it\\'s'"),
            (
                "host=h password='it\\'s'".into(),
                SslParams {
                    mode: Some("verify-ca".into()),
                    rootcert: Some("/a b/ca.pem".into()),
                }
            )
        );
        assert!(split_ssl_params("host").is_err());
        assert_eq!(
            split_ssl_params("postgres://h/db?sslcert=c.pem").unwrap_err(),
            "database URL: sslcert is not supported"
        );
        assert!(split_ssl_params("host='h").is_err());
    }

    #[test]
    fn modes_as_libpq_reads_them() {
        assert!(parse_mode("allow").unwrap_err().contains("not supported"));
        assert!(
            parse_mode("verify_full")
                .unwrap_err()
                .contains("invalid sslmode")
        );
        let err = Database::parse("postgres://h/db?sslmode=require&sslrootcert=system")
            .err()
            .expect("refused");
        assert!(err.contains("weak sslmode \"require\""), "{err}");
        let err = Database::parse("postgres://h/db?sslmode=verify-full&sslrootcert=/nowhere.pem")
            .err()
            .expect("refused");
        assert!(err.contains("\"/nowhere.pem\" does not exist"), "{err}");
        let db = Database::parse("postgres://h/db?sslmode=disable").expect("parsed");
        assert_eq!(db.config.get_ssl_mode(), SslMode::Disable);
        let db = Database::parse("postgres://h/db?sslmode=verify-ca&sslrootcert=system");
        assert!(db.is_err(), "verify-ca with system roots is refused");
    }
}
