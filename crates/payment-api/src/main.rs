//! `h5i-pay`: the payment API server.
//!
//! Environment:
//! * `DATABASE_URL` — PostgreSQL; or `DB_HOST`, `DB_NAME` (and optional
//!   `DB_PORT`) with `DB_CREDENTIALS`, the JSON `{"username","password"}` of
//!   an RDS-managed secret (as the ECS task definition injects it);
//! * `DB_CA_FILE` — a PEM CA bundle: every database connection then uses
//!   TLS with the server certificate and host name verified, and a server
//!   without TLS is refused. Required with `DB_CREDENTIALS`; the image ships
//!   the Amazon RDS global bundle at `/etc/h5i-pay/rds-global-bundle.pem`.
//!   Unset with `DATABASE_URL`: plain TCP (local development only);
//! * `H5I_APP_SECRET` — the key user tokens are signed with;
//! * `H5I_PAY_OPERATOR_SECRET` — the key operator tokens are signed with
//!   (must differ from `H5I_APP_SECRET`); both are required to serve, and
//!   with `DB_CREDENTIALS` (deployed) both must be at least 32 bytes.
//!   `H5I_APP_SECRET_PREVIOUS` / `H5I_PAY_OPERATOR_SECRET_PREVIOUS`: an
//!   old value still accepted during a rotation (see `payment_api::auth`);
//! * `LISTEN` — default `127.0.0.1:8080`;
//! * `H5I_PAY_PSP` — `stripe` (the default; test mode only: reads
//!   `STRIPE_SECRET_KEY`, which must be `sk_test_…`, and `STRIPE_WEBHOOK_SECRET`)
//!   or `mock`, only in builds with the `mock-psp` feature (then the default);
//! * `H5I_PAY_MOCK_WEBHOOK_SECRET` — the Mock PSP's signing secret (`mock-psp` builds);
//! * `H5I_APP_ISSUE=<tenant>:<user>` — print a one-day user token (user ≥ 1)
//!   and exit; needs only `H5I_APP_SECRET`;
//! * `H5I_PAY_ISSUE_OPERATOR=<tenant>:<operator-id>[:<ttl-secs>]` — print an
//!   operator token (default one hour, at most twelve) and exit; needs only
//!   `H5I_PAY_OPERATOR_SECRET`.
//!
//! `h5i-pay recover` runs one recovery pass over every tenant with work
//! (the scheduled ECS task), prints a JSON summary line and exits: 0 when
//! every tenant was processed, 1 when any tenant failed. Payments the PSP
//! has no record of stay `PROCESSING` (`payments_unresolved`); failing them
//! is left to an operator.

use h5i_app_pg::EngineConfig;
use payment_api::{auth, router, AppState, Keys};
use payment_connectors::{stripe::StripePsp, Psp};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let args: Vec<String> = std::env::args().collect();
    // `recover`, `export <dir>` and `ingest <run> <dir>` are one-shot jobs
    // (scheduled ECS tasks); without a command the binary serves the API.
    let job = match args.get(1).map(String::as_str) {
        None => None,
        Some("recover") => Some(Job::Recover),
        Some("export") if args.len() == 3 => Some(Job::Export(args[2].clone())),
        Some("ingest") if args.len() == 4 => Some(Job::Ingest(args[2].clone(), args[3].clone())),
        Some(other) => {
            return Err(format!("unknown command {other:?}; expected none, `recover`, `export <dir>` or `ingest <run> <dir>`").into());
        }
    };
    let recover = job.is_some();
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let required = |k: &'static str| env(k).ok_or(format!("{k} is required"));
    if let Some(spec) = env("H5I_APP_ISSUE") {
        let (t, u) = spec.split_once(':').ok_or("H5I_APP_ISSUE=<tenant>:<user>")?;
        println!("{}", auth::issue_user(required("H5I_APP_SECRET")?.as_bytes(), t.parse()?, u.parse()?, 86_400)?);
        return Ok(());
    }
    if let Some(spec) = env("H5I_PAY_ISSUE_OPERATOR") {
        let mut it = spec.split(':');
        let usage = "H5I_PAY_ISSUE_OPERATOR=<tenant>:<operator-id>[:<ttl-secs>]";
        let (t, o) = (it.next().ok_or(usage)?, it.next().ok_or(usage)?);
        let ttl = it.next().map(str::parse).transpose()?.unwrap_or(auth::DEFAULT_OPERATOR_TTL);
        println!("{}", auth::issue_operator(required("H5I_PAY_OPERATOR_SECRET")?.as_bytes(), t.parse()?, o.parse()?, ttl)?);
        return Ok(());
    }
    let (url, from_rds_secret) = database_url()?;
    // The jobs act in-process and verify no tokens.
    let keys = if recover {
        None
    } else {
        let k = Keys::new(&required("H5I_APP_SECRET")?, &required("H5I_PAY_OPERATOR_SECRET")?)?
            .with_previous(env("H5I_APP_SECRET_PREVIOUS").as_deref(), env("H5I_PAY_OPERATOR_SECRET_PREVIOUS").as_deref())?;
        if from_rds_secret {
            k.require_len(auth::MIN_DEPLOYED_SECRET_LEN)?;
        }
        Some(k)
    };
    let tls = payment_storage::DbTls::from_env()?;
    if from_rds_secret && tls == payment_storage::DbTls::Off {
        // The deployed path (ECS with an RDS-managed secret) never runs in the clear.
        return Err("DB_CREDENTIALS is set but DB_CA_FILE is not: refusing a database connection without TLS".into());
    }
    let addr = std::env::var("LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let psp = select_psp(std::env::var("H5I_PAY_PSP").ok().as_deref())?;
    let (ledger_engine, payments_engine) = payment_storage::engines_with(&url, 16, EngineConfig::default(), &tls).await?;
    tracing::info!(psp = psp.name(), db_tls = matches!(tls, payment_storage::DbTls::Verified(_)), "payment provider and database");
    if let Some(job) = job {
        // Each job acts as its own system operator (SEC-02): what it writes
        // records which one.
        let o = payment_api::orchestrator::Orchestrator::new(ledger_engine, payments_engine, psp).acting_as(match job {
            Job::Recover => payment_storage::RECOVERY_OPERATOR,
            Job::Export(_) | Job::Ingest(..) => payment_storage::EXPORT_OPERATOR,
        });
        match job {
            Job::Recover => {
                let r = payment_api::jobs::recover_all(&o, &url, &tls).await?;
                println!("{}", r.summary_json());
                std::process::exit(if r.failed.is_empty() { 0 } else { 1 });
            }
            Job::Export(dir) => {
                let m = payment_api::batch::export(&o, &url, &tls, std::path::Path::new(&dir)).await?;
                let failed = m["failed"].as_array().map_or(0, Vec::len);
                println!("{}", serde_json::json!({"event": "reconcile_export", "tenants": m["tenants"].as_array().map_or(0, Vec::len), "tenants_failed": failed}));
                std::process::exit(if failed == 0 { 0 } else { 1 });
            }
            Job::Ingest(run, dir) => {
                println!("{}", payment_api::batch::ingest(&url, &tls, &run, std::path::Path::new(&dir)).await?);
                return Ok(());
            }
        }
    }
    let keys = keys.expect("keys are loaded unless running a job");
    payment_storage::ops::install(&url, &tls).await?;
    let state = AppState::with_payments(ledger_engine, payments_engine, psp, &keys).with_ops(&url, &tls);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "h5i-pay listening");
    axum::serve(listener, router(state)).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

enum Job {
    Recover,
    Export(String),
    Ingest(String, String),
}

/// The PSP named by `H5I_PAY_PSP`. The Mock PSP exists only in builds with
/// the `mock-psp` feature (SEC-05); there it is also the default. Without
/// it the default is Stripe (test mode), and `mock` is refused.
fn select_psp(name: Option<&str>) -> Result<Arc<dyn Psp>, Box<dyn std::error::Error>> {
    const DEFAULT: &str = if cfg!(feature = "mock-psp") { "mock" } else { "stripe" };
    match name.unwrap_or(DEFAULT) {
        #[cfg(feature = "mock-psp")]
        "mock" => {
            tracing::warn!("Mock PSP: accepts any token and settles instantly; synthetic funds only");
            let secret = std::env::var("H5I_PAY_MOCK_WEBHOOK_SECRET").unwrap_or_else(|_| "whsec_mock_dev".into());
            Ok(Arc::new(payment_connectors::mock::MockPsp::new(secret)))
        }
        #[cfg(not(feature = "mock-psp"))]
        "mock" => Err("H5I_PAY_PSP=mock: this binary is built without the Mock PSP (feature mock-psp)".into()),
        "stripe" => Ok(Arc::new(StripePsp::from_env()?)),
        other => Err(format!("H5I_PAY_PSP={other}: expected mock or stripe").into()),
    }
}

/// `DATABASE_URL`, or one built from the RDS-managed credentials secret
/// (then `true`).
fn database_url() -> Result<(String, bool), Box<dyn std::error::Error>> {
    if let Ok(u) = std::env::var("DATABASE_URL") {
        return Ok((u, false));
    }
    let creds = std::env::var("DB_CREDENTIALS").map_err(|_| "DATABASE_URL or DB_HOST/DB_NAME/DB_CREDENTIALS is required")?;
    let v: serde_json::Value = serde_json::from_str(&creds)?;
    let user = v["username"].as_str().ok_or("DB_CREDENTIALS has no username")?;
    let pass = v["password"].as_str().ok_or("DB_CREDENTIALS has no password")?;
    let host = std::env::var("DB_HOST")?;
    let db = std::env::var("DB_NAME")?;
    let port = std::env::var("DB_PORT").unwrap_or_else(|_| "5432".into());
    Ok((format!("postgres://{}:{}@{host}:{port}/{db}", encode(user), encode(pass)), true))
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
