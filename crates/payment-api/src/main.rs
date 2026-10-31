//! `h5i-pay`: the payment API server.
//!
//! Environment:
//! * `DATABASE_URL` (required) — PostgreSQL;
//! * `H5I_APP_SECRET` (required) — the HMAC key for bearer tokens;
//! * `LISTEN` — default `127.0.0.1:8080`;
//! * `H5I_PAY_PSP` — `mock` (default) or `stripe` (test mode only: reads
//!   `STRIPE_SECRET_KEY`, which must be `sk_test_…`, and `STRIPE_WEBHOOK_SECRET`);
//! * `H5I_PAY_MOCK_WEBHOOK_SECRET` — the Mock PSP's signing secret;
//! * `H5I_APP_ISSUE=<tenant>:<user>` — print a one-day token and exit.

use h5i_app_http::HmacAuth;
use h5i_app_pg::EngineConfig;
use payment_api::{router, AppState};
use payment_connectors::{mock::MockPsp, stripe::StripePsp, Psp};
use payment_storage::ledger::{self, LedgerApp};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let secret = std::env::var("H5I_APP_SECRET").map_err(|_| "H5I_APP_SECRET is required")?;
    if let Ok(spec) = std::env::var("H5I_APP_ISSUE") {
        let (o, u) = spec.split_once(':').ok_or("H5I_APP_ISSUE=<tenant>:<user>")?;
        println!("{}", HmacAuth::<LedgerApp>::new(secret, ledger::principal).issue(o.parse()?, u.parse()?, 86_400));
        return Ok(());
    }
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is required")?;
    let addr = std::env::var("LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let psp: Arc<dyn Psp> = match std::env::var("H5I_PAY_PSP").as_deref().unwrap_or("mock") {
        "mock" => Arc::new(MockPsp::new(std::env::var("H5I_PAY_MOCK_WEBHOOK_SECRET").unwrap_or_else(|_| "whsec_mock_dev".into()))),
        "stripe" => Arc::new(StripePsp::from_env()?),
        other => return Err(format!("H5I_PAY_PSP={other}: expected mock or stripe").into()),
    };
    let (ledger_engine, payments_engine) = payment_storage::engines(&url, 16, EngineConfig::default()).await?;
    tracing::info!(psp = psp.name(), "payment provider");
    let state = AppState::with_payments(ledger_engine, payments_engine, psp, &secret);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "h5i-pay listening");
    axum::serve(listener, router(state)).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
