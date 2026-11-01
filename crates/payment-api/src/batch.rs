//! The nightly batch reconciliation, around the GPU worker
//! (`h5i-pay-batch`), which has no database access:
//!
//! 1. `h5i-pay export <dir>` ([`export`]): for every tenant with payments,
//!    `internal/<tenant>.csv` from the payment kernel's state (the same
//!    records `POST /v1/reconcile` compares), `external/<tenant>.csv` from
//!    the PSP's settlement report, and `manifest.json`.
//! 2. The worker reconciles each pair (`batch-entrypoint.sh reconcile-run`)
//!    and writes `reports/<tenant>.json`.
//! 3. `h5i-pay ingest <run> <dir>` ([`ingest`]): stores each report and its
//!    exceptions in the `ops` schema, where `GET /v1/reconcile/reports`
//!    shows them to the tenant's operators.
//!
//! On AWS the three steps are one Step Functions execution per night
//! (`infra/terraform/reconcile.tf`), the files go through S3. Nothing here
//! moves money: corrections stay audited ledger transfers by an operator
//! (`docs/runbooks/reconciliation-exceptions.md`).

use crate::orchestrator::{OrchError, Orchestrator};
use h5i_app_core::TenantId;
use payment_connectors as psp;
use payment_reconcile::rules::{key, KIND_CAPTURE, KIND_REFUND};
use payment_reconcile::{inputs, Record};
use payment_storage::{ops, DbTls};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug)]
pub enum BatchError {
    Orch(OrchError),
    Db(h5i_app_pg::DbError),
    Io(std::io::Error),
    Report(String),
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchError::Orch(e) => write!(f, "{e}"),
            BatchError::Db(e) => write!(f, "{e}"),
            BatchError::Io(e) => write!(f, "{e}"),
            BatchError::Report(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BatchError {}

impl From<OrchError> for BatchError {
    fn from(e: OrchError) -> Self {
        BatchError::Orch(e)
    }
}
impl From<h5i_app_pg::DbError> for BatchError {
    fn from(e: h5i_app_pg::DbError) -> Self {
        BatchError::Db(e)
    }
}
impl From<std::io::Error> for BatchError {
    fn from(e: std::io::Error) -> Self {
        BatchError::Io(e)
    }
}

/// The PSP's settlement report as reconciliation records.
pub fn external_records(report: &[psp::SettlementRecord]) -> Vec<Record> {
    report
        .iter()
        .map(|r| Record {
            key: key(
                match r.kind {
                    psp::SettlementKind::Capture => KIND_CAPTURE,
                    psp::SettlementKind::Refund => KIND_REFUND,
                },
                r.id,
            ),
            amount: r.amount,
            currency: r.currency,
            status: inputs::SETTLED,
        })
        .collect()
}

/// Write every tenant's internal and external records under `dir`, and a
/// manifest; returns the manifest. A tenant whose PSP report fails is
/// listed with its error and left out; the others are still exported.
pub async fn export(o: &Orchestrator, url: &str, tls: &DbTls, dir: &Path) -> Result<Value, BatchError> {
    std::fs::create_dir_all(dir.join("internal"))?;
    std::fs::create_dir_all(dir.join("external"))?;
    let mut tenants = Vec::new();
    let mut failed = Vec::new();
    for t in payment_storage::tenants_with_payments(url, tls).await? {
        let snap = o.payments.snapshot(TenantId(t)).await?;
        let internal = inputs::internal_records(&snap);
        let report = match o.psp.settlement_report(t).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(tenant = t, error = %e, "settlement report failed");
                failed.push(json!({"tenant": t, "error": e.to_string()}));
                continue;
            }
        };
        let external = external_records(&report);
        payment_batch::files::write_records(&dir.join(format!("internal/{t}.csv")), &internal)?;
        payment_batch::files::write_records(&dir.join(format!("external/{t}.csv")), &external)?;
        tenants.push(json!({"tenant": t, "internal_rows": internal.len(), "external_rows": external.len()}));
    }
    let manifest = json!({"event": "reconcile_export", "tenants": tenants, "failed": failed});
    std::fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&manifest).expect("json"))?;
    Ok(manifest)
}

/// Parse one worker report (the JSON `h5i-pay-batch reconcile` writes).
pub fn parse_report(tenant: u64, run: &str, bytes: &[u8]) -> Result<ops::Report, BatchError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| BatchError::Report(e.to_string()))?;
    let bad = |what: &str| BatchError::Report(format!("report for tenant {tenant}: bad or missing {what}"));
    let sum = |side: &str| -> Result<u64, BatchError> {
        let o = v[side].as_object().ok_or_else(|| bad(side))?;
        o.values().map(|x| x.as_u64().ok_or_else(|| bad(side))).sum()
    };
    let exceptions = v["exceptions"]
        .as_array()
        .ok_or_else(|| bad("exceptions"))?
        .iter()
        .map(|e| {
            Ok(ops::Exception {
                side: e["side"].as_str().ok_or_else(|| bad("side"))?.to_string(),
                code: e["code"].as_str().ok_or_else(|| bad("code"))?.to_string(),
                key: e["key"].as_u64().ok_or_else(|| bad("key"))?,
                amount: e["amount"].as_u64().ok_or_else(|| bad("amount"))?,
            })
        })
        .collect::<Result<Vec<_>, BatchError>>()?;
    Ok(ops::Report {
        tenant,
        run: run.to_string(),
        engine: v["engine"].as_str().ok_or_else(|| bad("engine"))?.to_string(),
        internal_rows: sum("internal")?,
        external_rows: sum("external")?,
        counts: json!({"internal": v["internal"], "external": v["external"]}).to_string(),
        exceptions_total: exceptions.len() as u64,
        exceptions,
    })
}

/// Ingest every `<tenant>.json` in `dir` as run `run`. Re-ingesting a run is
/// a no-op per tenant. Returns one summary line (logged for CloudWatch).
pub async fn ingest(url: &str, tls: &DbTls, run: &str, dir: &Path) -> Result<Value, BatchError> {
    ops::install(url, tls).await?;
    let mut reports = 0u64;
    let mut exceptions = 0u64;
    let mut repeated = 0u64;
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(t) = name.strip_suffix(".json").and_then(|t| t.parse::<u64>().ok()) else { continue };
        let r = parse_report(t, run, &std::fs::read(e.path())?)?;
        exceptions += r.exceptions_total;
        if ops::ingest(url, tls, &r).await? {
            reports += 1;
        } else {
            repeated += 1;
        }
    }
    Ok(json!({"event": "reconcile_ingest", "run": run, "reports": reports, "already_ingested": repeated, "exceptions": exceptions}))
}
