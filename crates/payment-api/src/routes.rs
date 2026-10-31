//! Routes and shared state.

use axum::extract::{FromRef, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use h5i_app_core::TenantId;
use h5i_app_http::{error_body, reply, Actor, Auth, H5iApp, HmacAuth};
use h5i_app_json::Value as Out;
use payment_storage::ledger::{self, LedgerApp, LedgerStore};
use payment_storage::LedgerEngine;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub ledger: H5iApp<LedgerApp, LedgerStore>,
    pub ledger_engine: Arc<LedgerEngine>,
}

impl FromRef<AppState> for Auth<LedgerApp> {
    fn from_ref(s: &AppState) -> Self {
        Auth::from_ref(&s.ledger)
    }
}

impl AppState {
    pub fn new(ledger_engine: Arc<LedgerEngine>, secret: &str) -> Self {
        let ledger = H5iApp::new(ledger_engine.clone(), HmacAuth::<LedgerApp>::new(secret.to_string(), ledger::principal));
        AppState { ledger, ledger_engine }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/ledger", post(ledger_rpc))
        .route("/v1/ledger/audit", get(ledger_audit))
        .with_state(state)
}

/// One ledger command as JSON, e.g.
/// `{"cmd":"transfer","id":7,"debit":0,"credit":1,"amount":500}`.
async fn ledger_rpc(State(st): State<AppState>, actor: Actor<LedgerApp>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
    match ledger::decode_for(actor.principal(), body) {
        Ok(cmd) => st.ledger.respond(&actor, cmd, &headers).await,
        Err(m) => reply(StatusCode::BAD_REQUEST, error_body(&m)),
    }
}

/// Re-check conservation, the balanced journal and the account limits on
/// the committed rows of the caller's tenant. Operators only.
async fn ledger_audit(State(st): State<AppState>, actor: Actor<LedgerApp>) -> Response {
    let p = actor.principal();
    if !p.operator {
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
