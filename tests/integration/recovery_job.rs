//! The scheduled recovery job (`h5i-pay recover`, `jobs::recover_all`):
//! one pass finds every tenant with half-done work and recovers it.
//!
//! `tenants_to_recover` sees every tenant in the database, so this test
//! runs in a database of its own: recovering another test's tenants while
//! that test runs would race with its assertions.

use h5i_app_pg::EngineConfig;
use h5i_pay_tests::{database_url, fresh_tenant};
use payment_connectors::mock::MockPsp;
use payment_storage::DbTls;
use serde_json::{json, Value};
use std::sync::Arc;


/// A new empty database next to the test database; dropped by `Drop`.
struct ScratchDb {
    admin: String,
    name: String,
    url: String,
}

impl ScratchDb {
    async fn create(base: &str) -> ScratchDb {
        let (prefix, _) = base.rsplit_once('/').expect("database URL with a path");
        let name = format!("h5i_pay_recover_{}_{}", std::process::id(), fresh_tenant());
        let (c, conn) = tokio_postgres::connect(base, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(conn);
        c.batch_execute(&format!("CREATE DATABASE {name}")).await.unwrap();
        ScratchDb { admin: base.to_string(), url: format!("{prefix}/{name}"), name }
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        let (admin, name) = (self.admin.clone(), self.name.clone());
        // Best effort, on a thread of its own (Drop cannot await).
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                if let Ok((c, conn)) = tokio_postgres::connect(&admin, tokio_postgres::NoTls).await {
                    tokio::spawn(conn);
                    let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).await;
                }
            })
        })
        .join();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduled_recovery_covers_every_tenant() {
    let Some(base) = database_url() else { return };
    let db = ScratchDb::create(&base).await;

    // An empty database (no tables yet) has nothing to recover.
    assert_eq!(payment_storage::tenants_to_recover(&db.url, &DbTls::Off).await.unwrap(), Vec::<u64>::new());

    let (ledger, payments) = payment_storage::engines(&db.url, 8, EngineConfig::default()).await.unwrap();
    let mock = Arc::new(MockPsp::new("whsec_test"));
    mock.set_now(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    let state = payment_api::AppState::with_payments(ledger.clone(), payments.clone(), mock.clone(), &h5i_pay_tests::keys());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });
    let http = reqwest::Client::new();
    let pay = |tok: String, amount: u64| {
        let req = http.post(format!("{base_url}/v1/payments")).bearer_auth(tok).json(&json!({
            "amount": amount, "currency": "jpy", "capture": "automatic", "payment_method": "tok_visa"
        }));
        async move {
            let r = req.send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };

    // Three tenants: two lose the PSP's answer to a charge, one is healthy.
    let (a, b, c) = (fresh_tenant(), fresh_tenant(), fresh_tenant());
    for t in [a, b] {
        mock.timeout_next(1);
        let (st, p) = pay(h5i_pay_tests::user_token(t, 7), 700).await;
        assert_eq!((st, p["status"].as_str()), (202, Some("processing")), "{p}");
    }
    let (st, p) = pay(h5i_pay_tests::user_token(c, 7), 300).await;
    assert_eq!((st, p["status"].as_str()), (200, Some("captured")), "{p}");

    let mut tenants = vec![a, b, c];
    tenants.sort_unstable();
    assert_eq!(payment_storage::tenants_to_recover(&db.url, &DbTls::Off).await.unwrap(), tenants);

    // One pass, as the scheduled task runs it, with its own orchestrator.
    let o = payment_api::orchestrator::Orchestrator::new(ledger.clone(), payments.clone(), mock.clone());
    let r = payment_api::jobs::recover_all(&o, &db.url, &DbTls::Off).await.unwrap();
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!((r.tenants, r.total.payments_resolved, r.total.payments_unresolved), (3, 2, 0), "{r:?}");
    let s = r.summary_json();
    assert_eq!((s["event"].as_str(), s["payments_resolved"].as_u64()), (Some("recovery"), Some(2)), "{s}");

    for t in [a, b] {
        let tok = h5i_pay_tests::user_token(t, 7);
        let v: Value = http.get(format!("{base_url}/v1/payments/0")).bearer_auth(tok).send().await.unwrap().json().await.unwrap();
        assert_eq!(v["status"], "captured", "tenant {t}: {v}");
        let op = h5i_pay_tests::operator_token(t);
        let audit: Value = http.get(format!("{base_url}/v1/ledger/audit")).bearer_auth(op).send().await.unwrap().json().await.unwrap();
        assert_eq!(audit["ok"], json!(true), "tenant {t}: {audit}");
    }

    // A second pass changes nothing.
    let r = payment_api::jobs::recover_all(&o, &db.url, &DbTls::Off).await.unwrap();
    assert_eq!((r.total.payments_resolved, r.total.refunds_resolved, r.failed.len()), (0, 0, 0), "{r:?}");
}
