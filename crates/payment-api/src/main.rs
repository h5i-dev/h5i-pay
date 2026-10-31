//! `h5i-pay`: the payment API server.
//!
//! Environment:
//! * `DATABASE_URL` (required) — PostgreSQL;
//! * `H5I_APP_SECRET` (required) — the HMAC key for bearer tokens;
//! * `LISTEN` — default `127.0.0.1:8080`;
//! * `H5I_APP_ISSUE=<tenant>:<user>` — print a one-day token and exit.

use h5i_app_http::HmacAuth;
use h5i_app_pg::EngineConfig;
use payment_api::{router, AppState};
use payment_storage::ledger::{self, LedgerApp};

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
    let (ledger_engine, _payments) = payment_storage::engines(&url, 16, EngineConfig::default()).await?;
    let state = AppState::new(ledger_engine, &secret);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "h5i-pay listening");
    axum::serve(listener, router(state)).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
