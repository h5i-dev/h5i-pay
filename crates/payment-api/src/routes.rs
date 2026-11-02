//! Routes and shared state.

// Handlers return early with a finished axum `Response` as the error, as
// h5i-app-http's `H5iApp::run` does; boxing it would only add unboxing.
#![allow(clippy::result_large_err)]

use crate::auth::Keys;
use crate::orchestrator::{CheckoutParams, OrchError, Orchestrator, View};
use axum::extract::{FromRef, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use h5i_app_core::TenantId;
use h5i_app_http::{error_body, reply, Actor, Auth, H5iApp};
use h5i_app_json::Value as Out;
use payment_connectors::{Psp, WebhookError};
use payment_domain as d;
use payment_reconcile::{rules, Engine, Report};
use payment_storage::ledger::{self, LedgerApp, LedgerStore};
use payment_storage::payments::{self, PaymentsApp, PaymentsStore};
use payment_storage::{LedgerEngine, PaymentsEngine};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub ledger: H5iApp<LedgerApp, LedgerStore>,
    pub payments: H5iApp<PaymentsApp, PaymentsStore>,
    pub ledger_engine: Arc<LedgerEngine>,
    pub orchestrator: Option<Arc<Orchestrator>>,
    /// Where ingested batch reports live (`payment_storage::ops`); `None`:
    /// `GET /v1/reconcile/reports` answers 503.
    pub ops: Option<Arc<(String, payment_storage::DbTls)>>,
}

impl FromRef<AppState> for Auth<LedgerApp> {
    fn from_ref(s: &AppState) -> Self {
        Auth::from_ref(&s.ledger)
    }
}

impl FromRef<AppState> for Auth<PaymentsApp> {
    fn from_ref(s: &AppState) -> Self {
        Auth::from_ref(&s.payments)
    }
}

impl AppState {
    /// The ledger only (no PSP configured).
    pub fn new(ledger_engine: Arc<LedgerEngine>, keys: &Keys) -> Self {
        Self::build(ledger_engine, None, None, keys)
    }

    /// Ledger, payments and the orchestrator with a PSP.
    pub fn with_payments(ledger_engine: Arc<LedgerEngine>, payments_engine: Arc<PaymentsEngine>, psp: Arc<dyn Psp>, keys: &Keys) -> Self {
        let orch = Arc::new(Orchestrator::new(ledger_engine.clone(), payments_engine.clone(), psp));
        Self::build(ledger_engine, Some(payments_engine), Some(orch), keys)
    }

    fn build(ledger_engine: Arc<LedgerEngine>, payments_engine: Option<Arc<PaymentsEngine>>, orchestrator: Option<Arc<Orchestrator>>, keys: &Keys) -> Self {
        let ledger = H5iApp::new(ledger_engine.clone(), crate::auth::ledger_auth(keys));
        let payments_engine = payments_engine.unwrap_or_else(|| {
            // A payments engine on the same pool configuration is still needed
            // for the type; without a PSP its routes answer 503.
            Arc::new(PaymentsEngine::new(
                h5i_app_pg::pool("postgres://invalid.invalid/none", 1).expect("pool config"),
                h5i_app_pg::EngineConfig::default(),
            ))
        });
        let payments = H5iApp::new(payments_engine, crate::auth::payments_auth(keys));
        AppState { ledger, payments, ledger_engine, orchestrator, ops: None }
    }

    /// Serve ingested batch reconciliation reports from this database.
    pub fn with_ops(mut self, url: &str, tls: &payment_storage::DbTls) -> Self {
        self.ops = Some(Arc::new((url.to_string(), tls.clone())));
        self
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/ledger", post(ledger_rpc))
        .route("/v1/ledger/audit", get(ledger_audit))
        .route("/v1/payments", post(create_payment))
        .route("/v1/payments/rpc", post(payments_rpc))
        .route("/v1/payments/{id}", get(get_payment))
        .route("/v1/payments/{id}/capture", post(capture_payment))
        .route("/v1/payments/{id}/cancel", post(cancel_payment))
        .route("/v1/payments/{id}/refunds", post(refund_payment))
        .route("/v1/checkout", post(checkout))
        .route("/v1/webhooks/{psp}", post(webhook))
        .route("/v1/recover", post(recover))
        .route("/v1/reconcile", post(reconcile))
        .route("/v1/reconcile/reports", get(reconcile_reports))
        .with_state(state)
}

/// One ledger command as JSON, e.g.
/// `{"cmd":"transfer","id":7,"debit":0,"credit":1,"amount":500}`.
async fn ledger_rpc(State(st): State<AppState>, actor: Actor<LedgerApp>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
    let p = *actor.principal();
    match ledger::decode_for(&p, body) {
        Ok(cmd) => {
            if let Some(r) = legacy_key::<LedgerApp, LedgerStore>(&st, payment_storage::LEDGER_SCHEMA, p.org, payment_ledger::is_operator(&p), &headers, &cmd).await {
                return r;
            }
            st.ledger.respond(&actor, cmd, &headers).await
        }
        Err(m) => reply(StatusCode::BAD_REQUEST, error_body(&m)),
    }
}

/// G2: an operator's keyed request whose key some operator used before
/// idempotency keys became per principal (stored under `u0/<key>`) replays
/// that reply if the command is the same, and is refused if it differs, as
/// it was then (`payment_storage::idempotency`). `None`: no such key (or no
/// database configured for the lookup), run the request.
async fn legacy_key<K, S>(st: &AppState, schema: &str, tenant: u64, operator: bool, headers: &HeaderMap, cmd: &K::Command) -> Option<Response>
where
    K: h5i_app_core::Kernel,
    S: h5i_app_pg::ReplyCodec<K> + h5i_app_http::Api<K>,
{
    if !operator {
        return None;
    }
    let k = key(headers)?;
    let ops = st.ops.as_ref()?;
    match payment_storage::idempotency::legacy_reply(&ops.0, &ops.1, schema, tenant, k).await {
        Ok(None) => None,
        Ok(Some((fp, bytes))) => {
            if fp != S::fingerprint(cmd) {
                return Some(reply(StatusCode::UNPROCESSABLE_ENTITY, error_body("idempotency key reused")));
            }
            Some(match S::decode(&bytes) {
                Ok(r) => reply(StatusCode::OK, S::encode_reply(&r)),
                Err(e) => {
                    tracing::error!(error = %e, "stored reply does not decode");
                    reply(StatusCode::SERVICE_UNAVAILABLE, error_body("unavailable"))
                }
            })
        }
        Err(e) => {
            tracing::error!(error = %e, "legacy idempotency lookup failed");
            Some(reply(StatusCode::SERVICE_UNAVAILABLE, error_body("unavailable")))
        }
    }
}

/// Re-check conservation, the balanced journal and the account limits on
/// the committed rows of the caller's tenant. Operators only.
async fn ledger_audit(State(st): State<AppState>, actor: Actor<LedgerApp>) -> Response {
    let p = actor.principal();
    if !payment_ledger::is_operator(p) {
        return reply(StatusCode::FORBIDDEN, error_body("forbidden"));
    }
    match st.ledger_engine.snapshot(TenantId(p.org)).await {
        Ok(snap) => match ledger::audit(&snap) {
            Ok(()) => reply(
                StatusCode::OK,
                Out::obj([
                    ("ok", true.into()),
                    ("accounts", (snap.accounts.len() as u64).into()),
                    ("transfers", (snap.transfers.len() as u64).into()),
                ]),
            ),
            Err(e) => {
                tracing::error!(tenant = p.org, error = %e, "ledger invariant violated");
                reply(StatusCode::INTERNAL_SERVER_ERROR, Out::obj([("ok", false.into()), ("violation", Out::str(e))]))
            }
        },
        Err(e) => {
            tracing::error!(error = %e, "audit failed");
            reply(StatusCode::SERVICE_UNAVAILABLE, error_body("unavailable"))
        }
    }
}

/// Read-only payment commands (`get`, `list`, `get_refund`, `refunds`).
async fn payments_rpc(State(st): State<AppState>, actor: Actor<PaymentsApp>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
    if st.orchestrator.is_none() {
        return unavailable();
    }
    let p = *actor.principal();
    match payments::decode_for(&p, body) {
        Ok(cmd) => {
            if let Some(r) = legacy_key::<PaymentsApp, PaymentsStore>(&st, payment_storage::PAYMENTS_SCHEMA, p.org, payment_domain::is_operator(&p), &headers, &cmd).await {
                return r;
            }
            st.payments.respond(&actor, cmd, &headers).await
        }
        Err(m) => reply(StatusCode::BAD_REQUEST, error_body(&m)),
    }
}

fn unavailable() -> Response {
    reply(StatusCode::SERVICE_UNAVAILABLE, error_body("payments are not configured"))
}

fn orch(st: &AppState) -> Result<&Arc<Orchestrator>, Response> {
    st.orchestrator.as_ref().ok_or_else(unavailable)
}

fn view_json(v: &View) -> Out {
    let mut fields = match payments::payment_json(&v.payment) {
        Out::Obj(f) => f,
        _ => unreachable!(),
    };
    fields.push((b"refunds".to_vec(), Out::Arr(v.refunds.iter().map(payments::refund_json).collect())));
    fields.push((b"psp_reference".to_vec(), v.reference.as_deref().map_or(Out::Null, Out::str)));
    fields.push((b"psp_pending".to_vec(), v.pending_psp.into()));
    Out::Obj(fields)
}

fn view_reply(v: &View) -> Response {
    let status = if v.pending_psp { StatusCode::ACCEPTED } else { StatusCode::OK };
    reply(status, view_json(v))
}

fn orch_error(e: OrchError) -> Response {
    match e {
        OrchError::Payment(e) => {
            let (status, code) = payments::error_code(&e);
            reply(status, error_body(code))
        }
        OrchError::Ledger(e) => {
            let (status, code) = ledger::error_code(&e);
            reply(status, error_body(code))
        }
        OrchError::MerchantBalance => reply(StatusCode::CONFLICT, error_body("merchant_balance_insufficient")),
        OrchError::BadRequest(m) => reply(StatusCode::BAD_REQUEST, error_body(&m)),
        OrchError::Psp(payment_connectors::PspError::Config(m)) => reply(StatusCode::BAD_REQUEST, error_body(&m)),
        OrchError::Psp(e) => {
            tracing::warn!(error = %e, "PSP error");
            reply(StatusCode::BAD_GATEWAY, error_body("psp_error"))
        }
        OrchError::Db(e) => {
            tracing::error!(error = %e, "database error");
            reply(StatusCode::SERVICE_UNAVAILABLE, error_body("unavailable"))
        }
    }
}

/// A currency as ISO 4217 numeric (392) or alphabetic ("jpy").
#[derive(Deserialize)]
#[serde(untagged)]
enum Currency {
    Numeric(u64),
    Alpha(String),
}

impl Currency {
    fn numeric(&self) -> Result<u32, Response> {
        let unsupported = || reply(StatusCode::BAD_REQUEST, error_body("unsupported currency"));
        match self {
            // Out of u32 range is refused here, never truncated.
            Currency::Numeric(n) => u32::try_from(*n).map_err(|_| unsupported()),
            Currency::Alpha(a) => payment_connectors::currency_numeric(a).ok_or_else(unsupported),
        }
    }
}

#[derive(Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CaptureMethod {
    #[default]
    Automatic,
    Manual,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePayment {
    amount: u64,
    currency: Currency,
    #[serde(default)]
    capture: CaptureMethod,
    /// A PSP token (`pm_card_visa`, `tok_visa`), never a card number.
    payment_method: String,
}

fn key(headers: &HeaderMap) -> Option<&str> {
    headers.get("idempotency-key").and_then(|v| v.to_str().ok())
}

async fn create_payment(State(st): State<AppState>, actor: Actor<PaymentsApp>, headers: HeaderMap, Json(body): Json<CreatePayment>) -> Response {
    let o = match orch(&st) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let currency = match body.currency.numeric() {
        Ok(c) => c,
        Err(r) => return r,
    };
    match o.create(actor.principal(), key(&headers), body.amount, currency, body.capture == CaptureMethod::Manual, &body.payment_method).await {
        Ok(v) => view_reply(&v),
        Err(e) => orch_error(e),
    }
}

async fn get_payment(State(st): State<AppState>, actor: Actor<PaymentsApp>, Path(id): Path<u64>) -> Response {
    let o = match orch(&st) {
        Ok(o) => o,
        Err(r) => return r,
    };
    match o.view(actor.principal(), id).await {
        Ok(v) => view_reply(&v),
        Err(e) => orch_error(e),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CaptureBody {
    amount: Option<u64>,
}

async fn capture_payment(State(st): State<AppState>, actor: Actor<PaymentsApp>, Path(id): Path<u64>, body: Option<Json<CaptureBody>>) -> Response {
    let o = match orch(&st) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let amount = body.and_then(|b| b.0.amount);
    match o.capture(actor.principal(), id, amount).await {
        Ok(v) => view_reply(&v),
        Err(e) => orch_error(e),
    }
}

async fn cancel_payment(State(st): State<AppState>, actor: Actor<PaymentsApp>, Path(id): Path<u64>) -> Response {
    let o = match orch(&st) {
        Ok(o) => o,
        Err(r) => return r,
    };
    match o.cancel(actor.principal(), id).await {
        Ok(v) => view_reply(&v),
        Err(e) => orch_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefundBody {
    amount: u64,
}

async fn refund_payment(State(st): State<AppState>, actor: Actor<PaymentsApp>, headers: HeaderMap, Path(id): Path<u64>, Json(body): Json<RefundBody>) -> Response {
    let o = match orch(&st) {
        Ok(o) => o,
        Err(r) => return r,
    };
    match o.refund(actor.principal(), key(&headers), id, body.amount).await {
        Ok((rid, v)) => {
            let mut j = view_json(&v);
            if let Out::Obj(f) = &mut j {
                f.insert(0, (b"refund_id".to_vec(), rid.into()));
            }
            let pending = v.refunds.iter().any(|r| r.id == rid && r.status == d::REFUND_PENDING);
            reply(if pending { StatusCode::ACCEPTED } else { StatusCode::OK }, j)
        }
        Err(e) => orch_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutBody {
    amount: u64,
    currency: Currency,
    #[serde(default)]
    capture: CaptureMethod,
    description: String,
    success_url: String,
    cancel_url: String,
}

async fn checkout(State(st): State<AppState>, actor: Actor<PaymentsApp>, headers: HeaderMap, Json(body): Json<CheckoutBody>) -> Response {
    let o = match orch(&st) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let currency = match body.currency.numeric() {
        Ok(c) => c,
        Err(r) => return r,
    };
    let params = CheckoutParams {
        amount: body.amount,
        currency,
        manual_capture: body.capture == CaptureMethod::Manual,
        description: body.description,
        success_url: body.success_url,
        cancel_url: body.cancel_url,
    };
    match o.checkout(actor.principal(), key(&headers), params).await {
        Ok((v, session)) => {
            let mut j = view_json(&v);
            if let Out::Obj(f) = &mut j {
                f.push((b"checkout_url".to_vec(), Out::str(&session.url)));
                f.push((b"checkout_session".to_vec(), Out::str(&session.id)));
            }
            reply(StatusCode::OK, j)
        }
        Err(e) => orch_error(e),
    }
}

/// A PSP webhook. No bearer token: the PSP's signature authenticates it.
async fn webhook(State(st): State<AppState>, Path(name): Path<String>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let o = match orch(&st) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if name != o.psp.name() {
        return reply(StatusCode::NOT_FOUND, error_body("unknown psp"));
    }
    let sig = headers.get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let ev = match o.psp.verify_webhook(sig, &body, now) {
        Ok(ev) => ev,
        Err(WebhookError::Ignored(what)) => {
            return reply(StatusCode::OK, Out::obj([("status", "ignored".into()), ("event", Out::str(what))]));
        }
        Err(e) => {
            tracing::warn!(error = %e, "webhook rejected");
            return reply(StatusCode::BAD_REQUEST, error_body("invalid webhook"));
        }
    };
    match o.webhook(&ev).await {
        Ok(r) => {
            let status = match r {
                d::Reply::Duplicate => "duplicate",
                d::Reply::Stale => "stale",
                _ => "processed",
            };
            reply(StatusCode::OK, Out::obj([("status", status.into())]))
        }
        // An event for a payment we do not know: acknowledge, so the PSP
        // stops retrying, and leave it to reconciliation.
        Err(OrchError::Payment(d::Error::NotFound)) => reply(StatusCode::OK, Out::obj([("status", "unknown_payment".into())])),
        Err(e) => orch_error(e),
    }
}

#[derive(Deserialize, Default)]
struct RecoverQuery {
    #[serde(default)]
    fail_unsubmitted: bool,
}

/// Finish payments and refunds left half done (operators only).
async fn recover(State(st): State<AppState>, actor: Actor<PaymentsApp>, Query(q): Query<RecoverQuery>) -> Response {
    let o = match orch(&st) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let p = actor.principal();
    if !payment_domain::is_operator(p) {
        return reply(StatusCode::FORBIDDEN, error_body("forbidden"));
    }
    // Recovery's writes record the operator who asked for it (SEC-02).
    let o = o.acting_as(payment_domain::operator_of(p));
    match o.recover(p.org, q.fail_unsubmitted).await {
        Ok(r) => reply(
            StatusCode::OK,
            Out::obj([
                ("payments_resolved", r.payments_resolved.into()),
                ("payments_failed", r.payments_failed.into()),
                ("payments_unresolved", r.payments_unresolved.into()),
                ("refunds_resolved", r.refunds_resolved.into()),
                ("ledger_synced", r.ledger_synced.into()),
            ]),
        ),
        Err(e) => orch_error(e),
    }
}

#[derive(Deserialize, Default)]
struct ReconcileQuery {
    /// `single` (default), `parallel`, `simd` or `reference`. CPU only, by
    /// decision: this is one tenant on demand (milliseconds at 10k
    /// payments, `docs/benchmarks.md`), the API image has no CUDA, and a
    /// per-request GPU context would cost more than the join. Large runs
    /// go to the nightly batch worker, which picks the GPU itself.
    engine: Option<String>,
}

const CODE_NAMES: [&str; rules::CODES] =
    ["matched", "amount_mismatch", "currency_mismatch", "status_mismatch", "missing_external", "duplicate_external", "duplicate_internal", "extra_external"];

fn describe(key: u64) -> Out {
    let kind = match key >> 56 {
        rules::KIND_CAPTURE => "capture",
        rules::KIND_REFUND => "refund",
        _ => "other",
    };
    Out::obj([("kind", kind.into()), ("id", (key & ((1 << 56) - 1)).into())])
}

fn report_json(r: &payment_reconcile::Recon, rep: &Report) -> Out {
    let (ci, ce) = rep.counts();
    let counts = |c: [u64; rules::CODES]| Out::obj(CODE_NAMES.iter().zip(c).filter(|(_, n)| *n > 0).map(|(k, n)| (*k, n.into())));
    let mut exceptions = Vec::new();
    for (i, c) in rep.internal.iter().enumerate() {
        if *c != rules::MATCHED && exceptions.len() < 100 {
            exceptions.push(Out::obj([
                ("side", "internal".into()),
                ("code", CODE_NAMES[*c as usize].into()),
                ("record", describe(r.internal[i].key)),
                ("amount", r.internal[i].amount.into()),
            ]));
        }
    }
    for (j, c) in rep.external.iter().enumerate() {
        if *c != rules::MATCHED && *c != rules::DUPLICATE_INTERNAL && exceptions.len() < 100 {
            exceptions.push(Out::obj([
                ("side", "external".into()),
                ("code", CODE_NAMES[*c as usize].into()),
                ("record", describe(r.external[j].key)),
                ("amount", r.external[j].amount.into()),
            ]));
        }
    }
    Out::obj([
        ("internal", (r.internal.len() as u64).into()),
        ("external", (r.external.len() as u64).into()),
        ("internal_codes", counts(ci)),
        ("external_codes", counts(ce)),
        ("exceptions", (rep.exceptions() as u64).into()),
        ("first_exceptions", Out::Arr(exceptions)),
    ])
}

/// Compare the payment kernel with the PSP's settlement report and with the
/// ledger journal. A report only: corrections are separate, audited ledger
/// transfers. Operators only.
async fn reconcile(State(st): State<AppState>, actor: Actor<PaymentsApp>, Query(q): Query<ReconcileQuery>) -> Response {
    let o = match orch(&st) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let p = actor.principal();
    if !payment_domain::is_operator(p) {
        return reply(StatusCode::FORBIDDEN, error_body("forbidden"));
    }
    let engine = match q.engine.as_deref().unwrap_or("single") {
        "single" => Engine::Single,
        "parallel" => Engine::Parallel,
        "simd" => Engine::Simd,
        "reference" => Engine::Reference,
        other => return reply(StatusCode::BAD_REQUEST, error_body(&format!("unknown engine {other}"))),
    };
    let inputs = match o.reconcile_inputs(p.org).await {
        Ok(i) => i,
        Err(e) => return orch_error(e),
    };
    let run = |r: &payment_reconcile::Recon| engine.run(r).map(|rep| report_json(r, &rep));
    match (run(&inputs.psp), run(&inputs.ledger)) {
        (Ok(a), Ok(b)) => reply(StatusCode::OK, Out::obj([("psp", a), ("ledger", b)])),
        (Err(e), _) | (_, Err(e)) => reply(StatusCode::UNPROCESSABLE_ENTITY, error_body(&e.to_string())),
    }
}

#[derive(Deserialize, Default)]
struct ReportsQuery {
    /// A run id (the nightly job uses the date); default: the latest.
    run: Option<String>,
    /// Exceptions to return, at most 10 000 (default 1 000).
    limit: Option<i64>,
}

/// The tenant's latest (or `run`'s) ingested batch reconciliation report,
/// with its exceptions (operators only). Read-only; it moves no money.
async fn reconcile_reports(State(st): State<AppState>, actor: Actor<PaymentsApp>, Query(q): Query<ReportsQuery>) -> Response {
    let p = actor.principal();
    if !payment_domain::is_operator(p) {
        return reply(StatusCode::FORBIDDEN, error_body("forbidden"));
    }
    let Some(ops) = st.ops.clone() else {
        return reply(StatusCode::SERVICE_UNAVAILABLE, error_body("reports not configured"));
    };
    let limit = q.limit.unwrap_or(1000).clamp(0, 10_000);
    match payment_storage::ops::report(&ops.0, &ops.1, p.org, q.run.as_deref(), limit).await {
        Ok(None) => reply(StatusCode::NOT_FOUND, error_body("no report")),
        Ok(Some(r)) => {
            let counts = serde_json::from_str::<serde_json::Value>(&r.counts).map_or(Out::Null, |v| json_out(&v));
            let ex = r
                .exceptions
                .iter()
                .map(|e| {
                    Out::obj([
                        ("side", Out::str(e.side.as_bytes())),
                        ("code", Out::str(e.code.as_bytes())),
                        ("object", describe(e.key)),
                        ("amount", e.amount.into()),
                    ])
                })
                .collect();
            reply(
                StatusCode::OK,
                Out::obj([
                    ("run", Out::str(r.run.as_bytes())),
                    ("engine", Out::str(r.engine.as_bytes())),
                    ("internal_rows", r.internal_rows.into()),
                    ("external_rows", r.external_rows.into()),
                    ("counts", counts),
                    ("exceptions_total", r.exceptions_total.into()),
                    ("exceptions", Out::Arr(ex)),
                ]),
            )
        }
        Err(e) => reply(StatusCode::SERVICE_UNAVAILABLE, error_body(&e.to_string())),
    }
}

/// A stored JSON document (counts only: objects of numbers) as a reply value.
fn json_out(v: &serde_json::Value) -> Out {
    match v {
        serde_json::Value::Object(m) => Out::Obj(m.iter().map(|(k, v)| (k.as_bytes().to_vec(), json_out(v))).collect()),
        serde_json::Value::Array(a) => Out::Arr(a.iter().map(json_out).collect()),
        serde_json::Value::Number(n) => n.as_u64().map_or(Out::Null, Out::from),
        serde_json::Value::String(s) => Out::str(s.as_bytes()),
        serde_json::Value::Bool(b) => Out::Bool(*b),
        serde_json::Value::Null => Out::Null,
    }
}
