//! The API and both engines against a PostgreSQL that accepts only TLS
//! (SEC-01).
//!
//! Start the servers with the h5i repo's `scripts/app/pg-tls.sh DIR`, which
//! prints the three variables these tests read:
//! `H5I_APP_TEST_TLS_DATABASE_URL` (default protocol versions),
//! `H5I_APP_TEST_TLS12_DATABASE_URL` (TLS 1.2 only) and
//! `H5I_APP_TEST_TLS_CA` (the throwaway CA). The servers' `pg_hba.conf`
//! rejects every non-TLS TCP connection. Tests skip when a variable is unset.

use h5i_app_http::HmacAuth;
use h5i_app_pg::EngineConfig;
use h5i_pay_tests::fresh_tenant;
use payment_connectors::mock::MockPsp;
use payment_storage::{DbTls, LedgerEngine, Roots, LEDGER_SCHEMA};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SECRET: &str = "tls-secret";

fn env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            eprintln!("{name} not set; skipping (start the servers with h5i's scripts/app/pg-tls.sh)");
            None
        }
    }
}

fn tls_env(url_var: &str) -> Option<(String, PathBuf)> {
    Some((env(url_var)?, PathBuf::from(env("H5I_APP_TEST_TLS_CA")?)))
}

/// The Amazon RDS bundle the images ship: a real CA set that did not sign
/// the test server's certificate.
fn rds_bundle() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../infra/docker/rds-global-bundle.pem")
}

/// Every connection of `user` the server currently has, and how many of them
/// are not TLS, asked over a separate verified TLS connection.
async fn server_side_ssl(url: &str, ca: &Path) -> (i64, i64) {
    let connector = h5i_app_pg::tls::RustlsConnect::new(&Roots::PemFile(ca.to_path_buf())).unwrap();
    let mut cfg: tokio_postgres::Config = url.parse().unwrap();
    cfg.ssl_mode(tokio_postgres::config::SslMode::Require);
    let (client, conn) = cfg.connect(connector).await.unwrap();
    tokio::spawn(conn);
    let row = client
        .query_one(
            "SELECT count(*), count(*) FILTER (WHERE NOT s.ssl) \
             FROM pg_stat_ssl s JOIN pg_stat_activity a USING (pid) \
             WHERE a.usename = current_user AND a.backend_type = 'client backend'",
            &[],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

async fn payments_end_to_end(url_var: &str) {
    let Some((url, ca)) = tls_env(url_var) else { return };
    let tls = DbTls::from_ca_file(&ca).unwrap();
    let (ledger, payments) = payment_storage::engines_with(&url, 4, EngineConfig::default(), &tls).await.unwrap();
    let mock = Arc::new(MockPsp::new("whsec_test"));
    mock.set_now(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    let state = payment_api::AppState::with_payments(ledger, payments, mock, SECRET);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });

    let org = fresh_tenant();
    let merchant = HmacAuth::<payment_storage::payments::PaymentsApp>::new(SECRET, payment_storage::payments::principal).issue(org, 7, 600);
    let op = HmacAuth::<payment_storage::ledger::LedgerApp>::new(SECRET, payment_storage::ledger::principal).issue(org, 0, 600);
    let http = reqwest::Client::new();
    let post = |path: &str, tok: &str, body: Value| {
        let req = http.post(format!("{base}{path}")).bearer_auth(tok).json(&body);
        async move {
            let r = req.send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };

    let (st, p) = post("/v1/payments", &merchant, json!({"amount": 1200, "currency": "jpy", "capture": "manual", "payment_method": "tok_visa"})).await;
    assert_eq!((st, p["status"].as_str()), (200, Some("authorized")), "{p}");
    let id = p["id"].as_u64().unwrap();
    let (st, p) = post(&format!("/v1/payments/{id}/capture"), &merchant, json!({"amount": 1000})).await;
    assert_eq!((st, p["captured"].as_u64()), (200, Some(1000)), "{p}");
    let (st, r) = post(&format!("/v1/payments/{id}/refunds"), &merchant, json!({"amount": 300})).await;
    assert_eq!(st, 200, "{r}");
    let (st, r) = post("/v1/reconcile", &op, json!({})).await;
    assert_eq!((st, &r["ledger"]["exceptions"]), (200, &json!(0)), "{r}");
    let audit = http.get(format!("{base}/v1/ledger/audit")).bearer_auth(&op).send().await.unwrap();
    let v: Value = audit.json().await.unwrap();
    assert_eq!(v["ok"], json!(true), "{v}");

    // The server agrees: the engines' pooled connections (and this check's
    // own) are TLS, none in the clear.
    let (total, plain) = server_side_ssl(&url, &ca).await;
    assert!(total >= 2, "expected the engines' pooled connections, saw {total}");
    assert_eq!(plain, 0, "{plain} of {total} connections are not TLS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payments_over_verified_tls() {
    payments_end_to_end("H5I_APP_TEST_TLS_DATABASE_URL").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payments_over_tls_1_2() {
    payments_end_to_end("H5I_APP_TEST_TLS12_DATABASE_URL").await;
}

async fn ledger_install(url: &str, tls: &DbTls) -> Result<(), String> {
    let pool = payment_storage::schema_pool_with(url, LEDGER_SCHEMA, 1, tls).map_err(|e| e.to_string())?;
    LedgerEngine::new(pool, EngineConfig::default()).install_schema().await.map_err(|e| e.to_string())
}

#[tokio::test]
async fn plain_tcp_is_refused_by_the_server() {
    let Some((url, _)) = tls_env("H5I_APP_TEST_TLS_DATABASE_URL") else { return };
    let err = ledger_install(&url, &DbTls::Off).await.expect_err("a plain connection must not reach a TLS-only server");
    assert!(err.contains("no encryption") || err.contains("pg_hba") || err.contains("db error"), "{err}");
}

/// The full error chain of one direct connection through h5i-app-pg's
/// verifying connector (the pool's error text stops at "TLS handshake").
async fn direct_tls_error(url: &str, ca: &Path) -> String {
    let connector = h5i_app_pg::tls::RustlsConnect::new(&Roots::PemFile(ca.to_path_buf())).unwrap();
    let mut cfg: tokio_postgres::Config = url.parse().unwrap();
    cfg.ssl_mode(tokio_postgres::config::SslMode::Require);
    let err = cfg.connect(connector).await.err().expect("connection must fail");
    let mut chain = vec![err.to_string()];
    let mut src = std::error::Error::source(&err);
    while let Some(e) = src {
        chain.push(format!("{e:?}"));
        src = e.source();
    }
    chain.join(" <- ")
}

#[tokio::test]
async fn certificate_from_another_ca_is_refused() {
    let Some((url, _)) = tls_env("H5I_APP_TEST_TLS_DATABASE_URL") else { return };
    let tls = DbTls::from_ca_file(&rds_bundle()).unwrap();
    let err = ledger_install(&url, &tls).await.expect_err("the test server's certificate is not signed by an RDS CA");
    assert!(err.contains("TLS handshake"), "{err}");
    let why = direct_tls_error(&url, &rds_bundle()).await;
    assert!(why.contains("UnknownIssuer"), "{why}");
}

#[tokio::test]
async fn host_name_must_match_the_certificate() {
    let Some((url, ca)) = tls_env("H5I_APP_TEST_TLS_DATABASE_URL") else { return };
    // The certificate names `localhost` only.
    let by_ip = url.replacen("@localhost:", "@127.0.0.1:", 1);
    assert_ne!(by_ip, url, "test URL should use localhost");
    let err = ledger_install(&by_ip, &DbTls::from_ca_file(&ca).unwrap()).await.expect_err("127.0.0.1 is not in the certificate");
    assert!(err.contains("TLS handshake"), "{err}");
    let why = direct_tls_error(&by_ip, &ca).await;
    assert!(why.contains("NotValidForName"), "{why}");
}

#[tokio::test]
async fn sslmode_disable_is_an_error_not_a_downgrade() {
    let Some((url, ca)) = tls_env("H5I_APP_TEST_TLS_DATABASE_URL") else { return };
    let err = ledger_install(&format!("{url}?sslmode=disable"), &DbTls::from_ca_file(&ca).unwrap()).await.unwrap_err();
    assert!(err.contains("sslmode=disable"), "{err}");
}

#[test]
fn ca_bundle_is_checked_at_startup() {
    assert!(DbTls::from_ca_file(Path::new("/nonexistent/ca.pem")).is_err());
    let empty = std::env::temp_dir().join(format!("h5i-pay-empty-ca-{}.pem", std::process::id()));
    std::fs::write(&empty, "not a certificate\n").unwrap();
    let r = DbTls::from_ca_file(&empty);
    std::fs::remove_file(&empty).unwrap();
    assert!(r.is_err(), "a bundle with no certificates must be refused");
    assert!(matches!(DbTls::from_ca_file(&rds_bundle()), Ok(DbTls::Verified(_))), "the shipped RDS bundle must load");
}
