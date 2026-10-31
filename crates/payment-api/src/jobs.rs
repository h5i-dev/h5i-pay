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

/// Run [`Orchestrator::recover`] for every tenant that has something to
/// recover ([`payment_storage::tenants_to_recover`]). A failing tenant does
/// not stop the others. `fail_unsubmitted` is always false here: deciding
/// that a payment never reached the PSP stays an operator's call
/// (`POST /v1/recover?fail_unsubmitted=true`).
pub async fn recover_all(o: &Orchestrator, url: &str, tls: &DbTls) -> Result<RecoverAll, h5i_app_pg::DbError> {
    let tenants = payment_storage::tenants_to_recover(url, tls).await?;
    let mut out = RecoverAll { tenants: tenants.len() as u64, ..Default::default() };
    for t in tenants {
        match o.recover(t, false).await {
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
