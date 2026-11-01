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

/// One tenant's unfinished work, by payment id (`recovery_work`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveryWork {
    pub tenant: u64,
    /// `PROCESSING`, or `AUTHORIZED` with automatic capture: ask the PSP.
    pub psp: Vec<u64>,
    /// A refund pending: ask the PSP about it.
    pub refunds: Vec<u64>,
    /// `CAPTURED` with a ledger posting missing (`needs_ledger_sync`).
    pub ledger: Vec<u64>,
}

impl RecoveryWork {
    pub fn is_empty(&self) -> bool {
        self.psp.is_empty() && self.refunds.is_empty() && self.ledger.is_empty()
    }
}

/// Whether a `CAPTURED` payment's ledger postings are incomplete, given its
/// refunds and which transfer ids the ledger has. The rule
/// `Orchestrator::sync_ledger` follows: the capture posting; for each
/// refund, its reservation, plus the settlement once it succeeded, or the
/// release once it failed (unless the reservation never happened: then
/// there is nothing to release).
pub fn needs_ledger_sync(p: &payment_domain::Payment, refunds: &[payment_domain::Refund], has: impl Fn(u64) -> bool) -> bool {
    use ledger::{system_transfer_id as id, POST_CAPTURE, POST_REFUND_RELEASE, POST_REFUND_RESERVE, POST_REFUND_SETTLE};
    if p.status != payment_domain::CAPTURED {
        return false;
    }
    if !has(id(POST_CAPTURE, p.id)) {
        return true;
    }
    refunds.iter().filter(|r| r.payment == p.id).any(|r| {
        let reserved = has(id(POST_REFUND_RESERVE, r.id));
        match r.status {
            payment_domain::REFUND_SUCCEEDED => !reserved || !has(id(POST_REFUND_SETTLE, r.id)),
            payment_domain::REFUND_FAILED => reserved && !has(id(POST_REFUND_RELEASE, r.id)),
            _ => !reserved,
        }
    })
}

/// A read-only connection outside the engines (cross-tenant queries).
async fn connect_ro(url: &str, tls: &DbTls) -> Result<(tokio_postgres::Client, tokio::task::JoinHandle<()>), DbError> {
    let mut cfg: tokio_postgres::Config = url.parse()?;
    match tls {
        DbTls::Off => {
            let (c, conn) = cfg.connect(tokio_postgres::NoTls).await?;
            Ok((c, tokio::spawn(async move { let _ = conn.await; })))
        }
        DbTls::Verified(roots) => {
            if cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
                return Err(DbError::Pool("TLS requested but the URL says sslmode=disable".into()));
            }
            cfg.ssl_mode(tokio_postgres::config::SslMode::Require);
            let (c, conn) = cfg.connect(h5i_app_pg::tls::RustlsConnect::new(roots)?).await?;
            Ok((c, tokio::spawn(async move { let _ = conn.await; })))
        }
    }
}

/// Every tenant's unfinished work, in one read-only query: payments to ask
/// the PSP about, payments with a pending refund, and captured payments
/// whose ledger postings are incomplete ([`needs_ledger_sync`], the same
/// rule in SQL, an anti-join on the ledger's transfer ids). Settled
/// payments cost an index probe here, not a kernel command and a PSP call
/// per pass. The unfinished statuses have a partial index
/// (`payments_unfinished`); the captured ones are scanned once.
///
/// This query crosses tenants and is outside the kernels: it reads ids and
/// statuses, writes nothing, and runs in a read-only transaction. It only
/// says where to look; every change is made per tenant through the engines,
/// and a payment it missed would show in reconciliation.
pub async fn recovery_work(url: &str, tls: &DbTls) -> Result<Vec<RecoveryWork>, DbError> {
    use ledger::{POST_CAPTURE, POST_REFUND_RELEASE, POST_REFUND_RESERVE, POST_REFUND_SETTLE};
    use payment_domain::{AUTHORIZED, CAPTURED, PROCESSING, REFUND_FAILED, REFUND_PENDING, REFUND_SUCCEEDED};
    // `ledger::system_transfer_id(kind, id)` in SQL: the top bit, the kind
    // in bits 56..63, the id below. BIGINT holds the u64 bit pattern.
    let tid = |kind: u32, col: &str| {
        format!("((-9223372036854775808)::bigint | ({kind}::bigint << 56) | ({col} & 72057594037927935))")
    };
    let has = |kind: u32, col: &str, tenant: &str| {
        format!("EXISTS (SELECT 1 FROM {LEDGER_SCHEMA}.transfers t WHERE t.tenant_id = {tenant} AND t.id = {})", tid(kind, col))
    };
    let sql = format!(
        "SELECT tenant_id, id, 0 FROM {PAYMENTS_SCHEMA}.payments \
           WHERE status = {PROCESSING} OR (status = {AUTHORIZED} AND NOT manual_capture) \
         UNION ALL \
         SELECT tenant_id, id, 1 FROM {PAYMENTS_SCHEMA}.payments WHERE refund_pending > 0 \
         UNION ALL \
         SELECT p.tenant_id, p.id, 2 FROM {PAYMENTS_SCHEMA}.payments p \
           WHERE p.status = {CAPTURED} AND (NOT {cap} OR EXISTS ( \
             SELECT 1 FROM {PAYMENTS_SCHEMA}.refunds r WHERE r.tenant_id = p.tenant_id AND r.payment = p.id AND ( \
               (r.status = {REFUND_SUCCEEDED} AND (NOT {res} OR NOT {settle})) \
               OR (r.status = {REFUND_FAILED} AND {res} AND NOT {release}) \
               OR (r.status NOT IN ({REFUND_SUCCEEDED}, {REFUND_FAILED}) AND NOT {res})))) \
         ORDER BY 1, 3, 2",
        cap = has(POST_CAPTURE, "p.id", "p.tenant_id"),
        res = has(POST_REFUND_RESERVE, "r.id", "r.tenant_id"),
        settle = has(POST_REFUND_SETTLE, "r.id", "r.tenant_id"),
        release = has(POST_REFUND_RELEASE, "r.id", "r.tenant_id"),
    );
    let _ = REFUND_PENDING;
    let (mut client, conn) = connect_ro(url, tls).await?;
    let tx = client.build_transaction().read_only(true).start().await?;
    let rows = match tx.query(sql.as_str(), &[]).await {
        Ok(r) => r,
        // No tables yet: nothing to recover.
        Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    drop(tx);
    drop(client);
    let _ = conn.await;
    let mut out: Vec<RecoveryWork> = Vec::new();
    for r in &rows {
        let (t, id, kind): (i64, i64, i32) = (r.get(0), r.get(1), r.get(2));
        let t = u64::try_from(t).map_err(|_| DbError::Decode(format!("tenant id {t} out of range")))?;
        let id = id as u64;
        if out.last().is_none_or(|w| w.tenant != t) {
            out.push(RecoveryWork { tenant: t, ..Default::default() });
        }
        let w = out.last_mut().expect("pushed");
        match kind {
            0 => w.psp.push(id),
            1 => w.refunds.push(id),
            _ => w.ledger.push(id),
        }
    }
    Ok(out)
}

/// Tenants with unfinished work ([`recovery_work`]).
pub async fn tenants_to_recover(url: &str, tls: &DbTls) -> Result<Vec<u64>, DbError> {
    Ok(recovery_work(url, tls).await?.into_iter().map(|w| w.tenant).collect())
}
