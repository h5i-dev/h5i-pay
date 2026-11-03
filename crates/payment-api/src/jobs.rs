//! Scheduled jobs the API binary runs as one-shot tasks (`h5i-pay recover`).

use crate::orchestrator::{Orchestrator, RecoveryReport};
use payment_storage::DbTls;

/// The outcome of one scheduled recovery pass over every tenant.
#[derive(Debug, Default)]
pub struct RecoverAll {
    pub tenants: u64,
    /// Sum of the per-tenant reports that succeeded.
    pub total: RecoveryReport,
    /// Tenants whose recovery failed (PSP or database error), with the error.
    pub failed: Vec<(u64, String)>,
}

/// Tenants recovered at once by one pass.
pub const TENANT_CONCURRENCY: usize = 4;

/// One pass: prune `open_payments` of payments marked since the last pass
/// ([`payment_storage::prune_open_payments`]), then, on the same
/// connection, find every tenant's unfinished work with one read-only query
/// ([`payment_storage::recovery_work`]: only payments still waiting on the
/// PSP, with a pending refund, or with a ledger posting missing) and run
/// [`Orchestrator::recover_work`] for up to [`TENANT_CONCURRENCY`] tenants
/// at once. A failing tenant does not stop the others. `fail_unsubmitted`
/// is always false here: deciding that a payment never reached the PSP
/// stays an operator's call (`POST /v1/recover?fail_unsubmitted=true`).
pub async fn recover_all(o: &Orchestrator, url: &str, tls: &DbTls) -> Result<RecoverAll, h5i_app_pg::DbError> {
    use futures_util::stream::{self, StreamExt};
    let work = payment_storage::recovery_work_with(url, tls, true).await?;
    let mut out = RecoverAll { tenants: work.len() as u64, ..Default::default() };
    let results: Vec<_> = stream::iter(work.iter())
        .map(|w| async move { (w.tenant, o.recover_work(w, false).await) })
        .buffer_unordered(TENANT_CONCURRENCY)
        .collect()
        .await;
    for (t, r) in results {
        match r {
            Ok(r) => {
                tracing::info!(tenant = t, ?r, "recovered");
                out.total.payments_resolved += r.payments_resolved;
                out.total.payments_failed += r.payments_failed;
                out.total.payments_unresolved += r.payments_unresolved;
                out.total.refunds_resolved += r.refunds_resolved;
                out.total.ledger_synced += r.ledger_synced;
            }
            Err(e) => {
                tracing::error!(tenant = t, error = %e, "recovery failed");
                out.failed.push((t, e.to_string()));
            }
        }
    }
    out.failed.sort();
    Ok(out)
}

impl RecoverAll {
    /// One JSON line for the `recovery` CloudWatch metric filters.
    pub fn summary_json(&self) -> serde_json::Value {
        serde_json::json!({
            "event": "recovery",
            "tenants": self.tenants,
            "tenants_failed": self.failed.len(),
            "payments_resolved": self.total.payments_resolved,
            "payments_unresolved": self.total.payments_unresolved,
            "refunds_resolved": self.total.refunds_resolved,
            "ledger_synced": self.total.ledger_synced,
        })
    }
}
