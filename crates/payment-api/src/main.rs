//! `h5i-pay`: the payment API server.
//!
//! Environment:
//! * `DATABASE_URL` — PostgreSQL; or `DB_HOST`, `DB_NAME` (and optional
//!   `DB_PORT`) with `DB_CREDENTIALS`, the JSON `{"username","password"}` of
//!   an RDS-managed secret (as the ECS task definition injects it);
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
    let url = database_url()?;
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

/// `DATABASE_URL`, or one built from the RDS-managed credentials secret.
fn database_url() -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(u) = std::env::var("DATABASE_URL") {
        return Ok(u);
    }
    let creds = std::env::var("DB_CREDENTIALS").map_err(|_| "DATABASE_URL or DB_HOST/DB_NAME/DB_CREDENTIALS is required")?;
    let v: serde_json::Value = serde_json::from_str(&creds)?;
    let user = v["username"].as_str().ok_or("DB_CREDENTIALS has no username")?;
    let pass = v["password"].as_str().ok_or("DB_CREDENTIALS has no password")?;
    let host = std::env::var("DB_HOST")?;
    let db = std::env::var("DB_NAME")?;
    let port = std::env::var("DB_PORT").unwrap_or_else(|_| "5432".into());
    Ok(format!("postgres://{}:{}@{host}:{port}/{db}", encode(user), encode(pass)))
}

/// Percent-encode a URL userinfo component.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
