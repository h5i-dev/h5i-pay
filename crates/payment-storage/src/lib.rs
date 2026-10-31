//! PostgreSQL adapters for the h5i-pay kernels.
//!
//! Each kernel gets an h5i-app [`Kernel`](h5i_app_core::Kernel) marker, a
//! [`Store`](h5i_app_pg::Store) generated from its `schema!`, the
//! [`ReplyCodec`](h5i_app_pg::ReplyCodec) for idempotency keys and the JSON
//! API. Every request runs in one SERIALIZABLE PostgreSQL transaction through
//! [`h5i_app_pg::Engine`]: load the tenant, run the pure `transition`, write,
//! commit, with retries on serialization failures and dropped connections.
//!
//! Each kernel lives in its own PostgreSQL schema (`ledger`, `payments`), so
//! their tables never mix.

pub mod ledger;
pub mod payments;

use h5i_app_pg::{Engine, EngineConfig, Pool, DbError};
use std::sync::Arc;

pub use h5i_app_pg::tls::Roots;

pub type LedgerEngine = Engine<ledger::LedgerApp, ledger::LedgerStore>;
pub type PaymentsEngine = Engine<payments::PaymentsApp, payments::PaymentsStore>;

pub const LEDGER_SCHEMA: &str = "ledger";
pub const PAYMENTS_SCHEMA: &str = "payments";

/// How connections to PostgreSQL are secured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DbTls {
    /// Plain TCP. Only for a local database (tests, `scripts/dev-db.sh`).
    Off,
    /// TLS on every connection, the server's certificate chain checked
    /// against these roots and its name against the URL's host (libpq's
    /// `verify-full`). A server without TLS is refused, never used in the
    /// clear (h5i-app-pg `pool_tls`).
    Verified(Roots),
}

impl DbTls {
    /// `DB_CA_FILE` set: [`DbTls::Verified`] with only the CA certificates
    /// in that PEM file (in the images, the Amazon RDS global bundle at
    /// `/etc/h5i-pay/rds-global-bundle.pem`). Unset: [`DbTls::Off`].
    pub fn from_env() -> Result<Self, DbError> {
        match std::env::var_os("DB_CA_FILE") {
            Some(p) if !p.is_empty() => Self::from_ca_file(std::path::Path::new(&p)),
            _ => Ok(DbTls::Off),
        }
    }

    /// [`DbTls::Verified`] with the CA certificates in `path`, checked now
    /// (a missing or empty bundle fails at startup, not on the first request).
    pub fn from_ca_file(path: &std::path::Path) -> Result<Self, DbError> {
        let roots = Roots::PemFile(path.to_path_buf());
        h5i_app_pg::tls::RustlsConnect::new(&roots)?;
        Ok(DbTls::Verified(roots))
    }
}

/// A plain-TCP pool whose connections see only `schema`'s tables.
pub fn schema_pool(url: &str, schema: &str, size: usize) -> Result<Pool, DbError> {
    schema_pool_with(url, schema, size, &DbTls::Off)
}

/// A pool whose connections see only `schema`'s tables, secured as `tls` says.
pub fn schema_pool_with(url: &str, schema: &str, size: usize, tls: &DbTls) -> Result<Pool, DbError> {
    let url = h5i_app_pg::with_schema(url, schema)?;
    match tls {
        DbTls::Off => h5i_app_pg::pool(&url, size),
        DbTls::Verified(roots) => h5i_app_pg::pool_tls(&url, size, roots),
    }
}

/// Both engines over plain TCP, with their schemas installed.
pub async fn engines(url: &str, pool_size: usize, config: EngineConfig) -> Result<(Arc<LedgerEngine>, Arc<PaymentsEngine>), DbError> {
    engines_with(url, pool_size, config, &DbTls::Off).await
}

/// Both engines, secured as `tls` says, with their schemas installed.
pub async fn engines_with(
    url: &str,
    pool_size: usize,
    config: EngineConfig,
    tls: &DbTls,
) -> Result<(Arc<LedgerEngine>, Arc<PaymentsEngine>), DbError> {
    let ledger = Arc::new(LedgerEngine::new(schema_pool_with(url, LEDGER_SCHEMA, pool_size, tls)?, config.clone()));
    ledger.install_schema().await?;
    let payments = Arc::new(PaymentsEngine::new(schema_pool_with(url, PAYMENTS_SCHEMA, pool_size, tls)?, config));
    payments.install_schema().await?;
    Ok((ledger, payments))
}

/// Tenants with a payment that `POST /v1/recover` would act on: in
/// `PROCESSING`, `AUTHORIZED` with automatic capture, `CAPTURED` (ledger
/// postings to re-sync), or with a refund pending. The same condition the
/// orchestrator's per-tenant recovery applies to each payment.
///
/// This is the one query that crosses tenants, and it is outside the
/// kernels: it reads only `tenant_id`, writes nothing, and runs in a
/// read-only transaction. The scheduled `h5i-pay recover` job uses it to
/// know which tenants to recover; every change is still made per tenant
/// through the engines.
pub async fn tenants_to_recover(url: &str, tls: &DbTls) -> Result<Vec<u64>, DbError> {
    let sql = format!(
        "SELECT DISTINCT tenant_id FROM {PAYMENTS_SCHEMA}.payments \
         WHERE status IN ({processing}, {captured}) \
            OR (status = {authorized} AND NOT manual_capture) \
            OR refund_pending > 0 \
         ORDER BY tenant_id",
        processing = payment_domain::PROCESSING,
        captured = payment_domain::CAPTURED,
        authorized = payment_domain::AUTHORIZED,
    );
    let mut cfg: tokio_postgres::Config = url.parse()?;
    let (mut client, conn) = match tls {
        DbTls::Off => {
            let (c, conn) = cfg.connect(tokio_postgres::NoTls).await?;
            (c, tokio::spawn(conn))
        }
        DbTls::Verified(roots) => {
            if cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
                return Err(DbError::Pool("TLS requested but the URL says sslmode=disable".into()));
            }
            cfg.ssl_mode(tokio_postgres::config::SslMode::Require);
            let (c, conn) = cfg.connect(h5i_app_pg::tls::RustlsConnect::new(roots)?).await?;
            (c, tokio::spawn(conn))
        }
    };
    let tx = client.build_transaction().read_only(true).start().await?;
    let rows = tx.query(sql.as_str(), &[]).await;
    let rows = match rows {
        Ok(r) => r,
        // No payments table yet: nothing to recover.
        Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    drop(tx);
    drop(client);
    let _ = conn.await;
    rows.iter()
        .map(|r| {
            let t: i64 = r.get(0);
            u64::try_from(t).map_err(|_| DbError::Decode(format!("tenant id {t} out of range")))
        })
        .collect()
}
