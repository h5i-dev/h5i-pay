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

pub type LedgerEngine = Engine<ledger::LedgerApp, ledger::LedgerStore>;
pub type PaymentsEngine = Engine<payments::PaymentsApp, payments::PaymentsStore>;

pub const LEDGER_SCHEMA: &str = "ledger";
pub const PAYMENTS_SCHEMA: &str = "payments";

/// A pool whose connections see only `schema`'s tables.
pub fn schema_pool(url: &str, schema: &str, size: usize) -> Result<Pool, DbError> {
    h5i_app_pg::pool(&h5i_app_pg::with_schema(url, schema)?, size)
}

/// Both engines, with their schemas installed.
pub async fn engines(url: &str, pool_size: usize, config: EngineConfig) -> Result<(Arc<LedgerEngine>, Arc<PaymentsEngine>), DbError> {
    let ledger = Arc::new(LedgerEngine::new(schema_pool(url, LEDGER_SCHEMA, pool_size)?, config.clone()));
    ledger.install_schema().await?;
    let payments = Arc::new(PaymentsEngine::new(schema_pool(url, PAYMENTS_SCHEMA, pool_size)?, config));
    payments.install_schema().await?;
    Ok((ledger, payments))
}
