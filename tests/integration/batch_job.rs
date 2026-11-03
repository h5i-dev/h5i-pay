//! The nightly batch reconciliation end to end, without AWS: `export`
//! writes every tenant's records, the worker's code reconciles each pair
//! (`payment_batch::files` + the engine, as `h5i-pay-batch reconcile` does),
//! `ingest` stores the reports, and `GET /v1/reconcile/reports` shows a
//! tenant's operators their own report only. Own database: the export sees
//! every tenant.

use h5i_app_pg::EngineConfig;
use h5i_pay_tests::{database_url, fresh_tenant, ScratchDb};
use payment_batch::{binfile, files};
use payment_connectors::mock::MockPsp;
use payment_reconcile::{Engine, Recon};
use payment_storage::DbTls;
use serde_json::{json, Value};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_reconcile_ingest() {
    let Some(base) = database_url() else { return };
    let db = ScratchDb::create(&base).await;
    let (ledger, payments) = payment_storage::engines(&db.url, 8, EngineConfig::default()).await.unwrap();
    let mock = Arc::new(MockPsp::new("whsec_test"));
    mock.set_now(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    let state = payment_api::AppState::with_payments(ledger.clone(), payments.clone(), mock.clone(), &h5i_pay_tests::keys())
        .with_ops(&db.url, &DbTls::Off);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });
    let http = reqwest::Client::new();

    // Two tenants: three captured payments each, one refunded in full.
    let (a, b) = (fresh_tenant(), fresh_tenant());
    for t in [a, b] {
        for amount in [500u64, 700, 900] {
            let r: Value = http
                .post(format!("{url}/v1/payments"))
                .bearer_auth(h5i_pay_tests::user_token(t, 7))
                .json(&json!({"amount": amount, "currency": "jpy", "capture": "automatic", "payment_method": "tok_visa"}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(r["status"], "captured", "{r}");
        }
        let r = http
            .post(format!("{url}/v1/payments/0/refunds"))
            .bearer_auth(h5i_pay_tests::user_token(t, 7))
            .json(&json!({"amount": 500}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
    }
    // Tenant a's PSP report settles one capture short by 1.
    mock.tamper_settlement(|rows| {
        let r = rows.iter_mut().find(|r| r.tenant == a && r.amount == 700).unwrap();
        r.amount = 699;
    });

    let dir = tempdir("export");
    let o = payment_api::orchestrator::Orchestrator::new(ledger.clone(), payments.clone(), mock.clone());
    let m = payment_api::batch::export(&o, &db.url, &DbTls::Off, &dir, payment_api::batch::Format::Binary).await.unwrap();
    assert_eq!(m["format"], "h5ib", "{m}");
    // The same export as CSV (still supported) holds the same rows.
    let csv_dir = tempdir("export-csv");
    payment_api::batch::export(&o, &db.url, &DbTls::Off, &csv_dir, payment_api::batch::Format::Csv).await.unwrap();
    let mut tenants: Vec<u64> = m["tenants"].as_array().unwrap().iter().map(|t| t["tenant"].as_u64().unwrap()).collect();
    tenants.sort_unstable();
    let mut want = vec![a, b];
    want.sort_unstable();
    assert_eq!(tenants, want, "{m}");
    // Three captures and one settled refund per tenant on both sides.
    for t in &m["tenants"].as_array().unwrap()[..] {
        assert_eq!((t["internal_rows"].as_u64(), t["external_rows"].as_u64()), (Some(4), Some(4)), "{t}");
    }

    // The worker, per tenant: what `h5i-pay-batch reconcile` runs.
    let reports = dir.join("reports");
    std::fs::create_dir_all(&reports).unwrap();
    for t in [a, b] {
        let path = |d: &std::path::Path, side: &str, ext: &str| d.join(format!("{side}/{t}.{ext}")).to_str().unwrap().to_string();
        let (pi, pe) = (path(&dir, "internal", "h5ib"), path(&dir, "external", "h5ib"));
        let (fi, fe) = (binfile::Mapped::open(&pi).unwrap().verify(&pi).unwrap(), binfile::Mapped::open(&pe).unwrap().verify(&pe).unwrap());
        let rep = payment_reconcile::cpu_parallel_cols(fi.cols(), fe.cols(), false).unwrap();
        let out = reports.join(format!("{t}.json"));
        files::write_report_rows(out.to_str().unwrap(), &fi.cols(), &fe.cols(), &rep.internal, &rep.external, "parallel", 1.0).unwrap();
        // The CSV path gives the same report, byte for byte.
        let internal = files::read_records(&path(&csv_dir, "internal", "csv")).unwrap();
        let external = files::read_records(&path(&csv_dir, "external", "csv")).unwrap();
        let r = Recon { internal, external };
        let rep = Engine::Parallel.run(&r).unwrap();
        let csv_out = csv_dir.join(format!("{t}.json"));
        files::write_report(csv_out.to_str().unwrap(), &r, &rep, "parallel", 1.0).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&csv_out).unwrap());
    }
    let _ = std::fs::remove_dir_all(&csv_dir);

    let s = payment_api::batch::ingest(&db.url, &DbTls::Off, "2026-11-01", &reports).await.unwrap();
    assert_eq!((s["reports"].as_u64(), s["exceptions"].as_u64()), (Some(2), Some(1)), "{s}");
    // Ingesting the same run again stores nothing twice.
    let s = payment_api::batch::ingest(&db.url, &DbTls::Off, "2026-11-01", &reports).await.unwrap();
    assert_eq!((s["reports"].as_u64(), s["already_ingested"].as_u64()), (Some(0), Some(2)), "{s}");

    let get = |tok: String| {
        let req = http.get(format!("{url}/v1/reconcile/reports")).bearer_auth(tok);
        async move {
            let r = req.send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let (st, ra) = get(h5i_pay_tests::operator_token(a)).await;
    assert_eq!(st, 200, "{ra}");
    assert_eq!((ra["run"].as_str(), ra["exceptions_total"].as_u64()), (Some("2026-11-01"), Some(1)), "{ra}");
    assert_eq!(ra["exceptions"][0]["code"], "amount_mismatch", "{ra}");
    assert_eq!(ra["counts"]["internal"]["matched"], 3, "{ra}");
    let (st, rb) = get(h5i_pay_tests::operator_token(b)).await;
    assert_eq!((st, rb["exceptions_total"].as_u64()), (200, Some(0)), "{rb}");
    // A merchant may not read reports; another tenant sees none.
    assert_eq!(get(h5i_pay_tests::user_token(a, 7)).await.0, 403);
    assert_eq!(get(h5i_pay_tests::operator_token(fresh_tenant())).await.0, 404);
    let _ = std::fs::remove_dir_all(&dir);
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("h5i-pay-{tag}-{}-{}", std::process::id(), fresh_tenant()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
