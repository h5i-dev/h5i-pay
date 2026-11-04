//! Concurrent startups on one database: every process opens the engines,
//! which installs the schemas and runs the startup migrations. While other
//! processes serve requests, that DDL (`ALTER TABLE … IF NOT EXISTS`,
//! `CREATE INDEX IF NOT EXISTS`, triggers) takes table locks in another
//! order than a payment's writes (payments, history, open_payments) and
//! deadlocked with them in CI (40P01, two integration tests).
//! `payment_storage::startup` serializes the installs under one advisory
//! lock and skips them once the current version is recorded, so a process
//! that starts next to live traffic takes no table locks at all.

use h5i_app_pg::EngineConfig;
use h5i_pay_tests::ledger::{op, open, transfer};
use h5i_pay_tests::{database_url, fresh_tenant, ScratchDb};
use payment_connectors::mock::MockPsp;
use payment_storage::DbTls;
use serde_json::json;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn startups_next_to_live_traffic_do_not_deadlock() {
    let Some(base) = database_url() else { return };
    for round in 0..3 {
        let db = ScratchDb::create(&base).await;
        let (ledger, payments) = payment_storage::engines(&db.url, 8, EngineConfig::default()).await.unwrap();
        let mock = Arc::new(MockPsp::new("whsec_test"));
        let state = payment_api::AppState::with_payments(ledger, payments, mock, &h5i_pay_tests::keys()).with_ops(&db.url, &DbTls::Off);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });

        // Live traffic: ledger transfers (an account update, then the
        // transfer insert: the CI deadlock's lock order) ...
        let org = fresh_tenant();
        ledger_traffic_setup(&*ledger_for_traffic(&db.url).await, org).await;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut clients = Vec::new();
        for c in 0..6u64 {
            let (url, stop) = (db.url.clone(), stop.clone());
            clients.push(tokio::spawn(async move {
                let e = ledger_for_traffic(&url).await;
                let mut n = 0u32;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let id = 10_000_000 * (c + 1) + n as u64;
                    e.execute(&op(org), &transfer(id, 0, 1 + (n as u64 % 4), 1)).await.unwrap().unwrap();
                    n += 1;
                }
                n
            }));
        }
        // ... and captured payments (payments, history, open_payments,
        // ledger postings) from several clients ...
        for _ in 0..4 {
            let (api, stop) = (api.clone(), stop.clone());
            clients.push(tokio::spawn(async move {
                let http = reqwest::Client::new();
                let tok = h5i_pay_tests::user_token(fresh_tenant(), 7);
                let mut n = 0u32;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let r = http
                        .post(format!("{api}/v1/payments"))
                        .bearer_auth(&tok)
                        .json(&json!({"amount": 500, "currency": "jpy", "capture": "automatic", "payment_method": "tok_visa"}))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(r.status().as_u16(), 200, "{}", r.text().await.unwrap_or_default());
                    n += 1;
                }
                n
            }));
        }
        // ... while other processes start (rolling deploy, jobs), several at once.
        let mut starts = Vec::new();
        for _ in 0..24 {
            let url = db.url.clone();
            starts.push(tokio::spawn(async move {
                payment_storage::engines(&url, 2, EngineConfig::default()).await?;
                payment_storage::ops::install(&url, &DbTls::Off).await
            }));
        }
        for s in starts {
            s.await.unwrap().unwrap_or_else(|e| panic!("round {round}: startup next to live traffic: {e}"));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut created = 0;
        for c in clients {
            created += c.await.unwrap();
        }
        assert!(created > 0);
        // Installed once per version.
        let (c, conn) = tokio_postgres::connect(&db.url, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(conn);
        let n: i64 = c.query_one("SELECT count(*) FROM h5i_pay_meta.installed", &[]).await.unwrap().get(0);
        assert_eq!(n, 2, "engines and ops");
    }
}

async fn ledger_for_traffic(url: &str) -> Arc<payment_storage::LedgerEngine> {
    let pool = payment_storage::schema_pool(url, payment_storage::LEDGER_SCHEMA, 2).unwrap();
    Arc::new(payment_storage::LedgerEngine::new(pool, EngineConfig { max_attempts: 500, ..EngineConfig::default() }))
}

/// A funding account (0, may go negative) and four wallets.
async fn ledger_traffic_setup(e: &payment_storage::LedgerEngine, org: u64) {
    for a in 0..5u64 {
        e.execute(&op(org), &open(a, 392, false)).await.unwrap().unwrap();
    }
}
