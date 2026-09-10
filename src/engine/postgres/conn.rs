//! One Postgres connection over the wire protocol (tokio-postgres + rustls):
//! TLS per the source's `tls` mode, the server-version gate (KB §11.4), the
//! pinned session settings the canonical text depends on (KB §11.3), and the
//! `REPEATABLE READ` snapshot every read happens inside (KB §11.1).

use std::sync::Arc;
use std::time::Duration;

use futures::TryStreamExt;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::task::JoinHandle;
use tokio_postgres::config::SslMode;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Config as PgConfig, CopyOutStream, NoTls, Row};
use tokio_postgres_rustls::MakeRustlsConnect;
use tracing::{debug, warn};

use super::sql::escape;
use crate::config::{Source, TlsMode};
use crate::error::{ArkError, Result};
use crate::redact::redact;

/// Oldest server this backend dumps (PRD §5.1.4).
pub const MIN_SERVER_VERSION_NUM: i32 = 130_000;

/// Settings pinned before any read so the emitted text is canonical; the
/// manifest records exactly these (KB §11.3).
pub const SESSION_SETTINGS: &[(&str, &str)] = &[
    ("DateStyle", "ISO, YMD"),
    ("IntervalStyle", "postgres"),
    ("extra_float_digits", "3"),
    ("bytea_output", "hex"),
    ("TimeZone", "UTC"),
    ("client_encoding", "UTF8"),
    ("standard_conforming_strings", "on"),
];

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A live connection plus the version it reported.
pub struct Conn {
    client: Client,
    driver: JoinHandle<()>,
    pub server_version: String,
    pub server_version_num: i32,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl Conn {
    /// Connect, gate the version, and pin the session settings.
    pub async fn open(source: &Source) -> Result<Self> {
        let config = pg_config(source);
        let (client, driver) = match source.tls {
            TlsMode::Disable => connect_plain(&config).await?,
            mode => connect_tls(&config, mode, source.tls_ca_file.as_deref()).await?,
        };
        let mut conn = Self {
            client,
            driver,
            server_version: String::new(),
            server_version_num: 0,
        };
        conn.read_version().await?;
        conn.pin_session().await?;
        Ok(conn)
    }

    /// Run a statement with bound parameters and collect its rows.
    pub async fn rows(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
        let stream = self
            .client
            .query_raw(sql, params.iter().copied())
            .await
            .map_err(|e| engine_err("query failed", e))?;
        stream
            .try_collect()
            .await
            .map_err(|e| engine_err("query failed", e))
    }

    /// Run one or more parameterless statements (simple protocol).
    pub async fn exec(&self, sql: &str) -> Result<()> {
        self.client
            .batch_execute(sql)
            .await
            .map_err(|e| engine_err("statement failed", e))
    }

    /// Start `COPY … TO STDOUT` and hand back the byte stream.
    pub async fn copy_out(&self, sql: &str) -> Result<CopyOutStream> {
        self.client
            .copy_out(sql)
            .await
            .map_err(|e| engine_err("COPY failed", e))
    }

    /// Open the source-wide read-only `REPEATABLE READ` transaction and
    /// export its snapshot id (KB §11.1). Every later read sees this point.
    pub async fn begin_snapshot(&self) -> Result<String> {
        self.exec("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await?;
        let rows = self
            .rows("SELECT pg_catalog.pg_export_snapshot()", &[])
            .await?;
        let id: String = rows
            .first()
            .ok_or_else(|| ArkError::Engine {
                engine: "PostgreSQL",
                message: "pg_export_snapshot returned no row".into(),
            })?
            .try_get(0)
            .map_err(|e| engine_err("cannot read snapshot id", e))?;
        Ok(id)
    }

    /// Close the snapshot transaction.
    pub async fn end_snapshot(&self) -> Result<()> {
        self.exec("ROLLBACK").await
    }

    async fn read_version(&mut self) -> Result<()> {
        let rows = self
            .rows(
                "SELECT current_setting('server_version_num')::int, current_setting('server_version')",
                &[],
            )
            .await?;
        let row = rows.first().ok_or_else(|| ArkError::Engine {
            engine: "PostgreSQL",
            message: "server reported no version".into(),
        })?;
        self.server_version_num = row
            .try_get(0)
            .map_err(|e| engine_err("cannot read server version", e))?;
        self.server_version = row
            .try_get(1)
            .map_err(|e| engine_err("cannot read server version", e))?;
        if self.server_version_num < MIN_SERVER_VERSION_NUM {
            return Err(ArkError::Engine {
                engine: "PostgreSQL",
                message: format!(
                    "server version {} is not supported (this build dumps PostgreSQL 13 and newer)",
                    self.server_version
                ),
            });
        }
        debug!(version = %self.server_version, "connected");
        Ok(())
    }

    /// Pin the canonical-text settings, disable timeouts, and empty the
    /// search path so every name the server renders is schema-qualified.
    async fn pin_session(&self) -> Result<()> {
        let mut sql = String::new();
        for (name, value) in SESSION_SETTINGS {
            sql.push_str(&format!("SET {name} = {};\n", escape(value)));
        }
        sql.push_str("SET statement_timeout = 0;\n");
        sql.push_str("SET lock_timeout = 0;\n");
        sql.push_str("SET idle_in_transaction_session_timeout = 0;\n");
        sql.push_str("SELECT pg_catalog.set_config('search_path', '', false);\n");
        self.exec(&sql).await
    }
}

fn pg_config(source: &Source) -> PgConfig {
    let mut config = PgConfig::new();
    config
        .host(source.host.as_deref().unwrap_or("localhost"))
        .port(source.port())
        .user(source.user.as_deref().unwrap_or("postgres"))
        .dbname(source.database())
        .application_name("arkstore")
        .connect_timeout(CONNECT_TIMEOUT)
        .ssl_mode(match source.tls {
            TlsMode::Disable => SslMode::Disable,
            TlsMode::Prefer => SslMode::Prefer,
            TlsMode::Require | TlsMode::VerifyFull => SslMode::Require,
        });
    if let Some(password) = &source.password {
        config.password(password.expose());
    }
    config
}

async fn connect_plain(config: &PgConfig) -> Result<(Client, JoinHandle<()>)> {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .map_err(|e| engine_err("connection failed", e))?;
    let driver = tokio::spawn(async move {
        if let Err(e) = connection.await {
            warn!(error = %redact(&e.to_string()), "postgres connection closed with error");
        }
    });
    Ok((client, driver))
}

async fn connect_tls(
    config: &PgConfig,
    mode: TlsMode,
    ca_file: Option<&str>,
) -> Result<(Client, JoinHandle<()>)> {
    let tls = MakeRustlsConnect::new(tls_config(mode, ca_file)?);
    let (client, connection) = config
        .connect(tls)
        .await
        .map_err(|e| engine_err("connection failed", e))?;
    let driver = tokio::spawn(async move {
        if let Err(e) = connection.await {
            warn!(error = %redact(&e.to_string()), "postgres connection closed with error");
        }
    });
    Ok((client, driver))
}

/// `verify-full` checks the chain and host name against the Mozilla roots
/// (or the source's `tls_ca_file`); `prefer` / `require` encrypt without
/// verifying the peer, exactly like libpq's modes of the same name.
fn tls_config(mode: TlsMode, ca_file: Option<&str>) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| ArkError::Engine {
            engine: "PostgreSQL",
            message: format!("cannot configure TLS: {e}"),
        })?;
    if mode == TlsMode::VerifyFull {
        return Ok(builder
            .with_root_certificates(root_store(ca_file)?)
            .with_no_client_auth());
    }
    Ok(builder
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(EncryptOnly { provider }))
        .with_no_client_auth())
}

fn root_store(ca_file: Option<&str>) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let Some(path) = ca_file else {
        roots.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
        return Ok(roots);
    };
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| ArkError::Validation(format!("tls_ca_file `{path}`: {e}")))?;
    for cert in certs {
        let cert = cert.map_err(|e| ArkError::Validation(format!("tls_ca_file `{path}`: {e}")))?;
        roots
            .add(cert)
            .map_err(|e| ArkError::Validation(format!("tls_ca_file `{path}`: {e}")))?;
    }
    Ok(roots)
}

/// Accepts any certificate but still checks handshake signatures with the
/// provider's algorithms — encryption without peer identity (`require`).
#[derive(Debug)]
struct EncryptOnly {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for EncryptOnly {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Wrap a driver error with context; the text is redacted since driver
/// messages can echo connection details.
pub fn engine_err(context: &str, err: tokio_postgres::Error) -> ArkError {
    let detail = match err.as_db_error() {
        Some(db) => format!("{} (SQLSTATE {})", db.message(), db.code().code()),
        None => err.to_string(),
    };
    ArkError::Engine {
        engine: "PostgreSQL",
        message: redact(&format!("{context}: {detail}")),
    }
}
