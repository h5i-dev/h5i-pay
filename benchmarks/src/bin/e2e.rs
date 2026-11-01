//! End-to-end measurements through PostgreSQL (B4): the API's payment flow,
//! the scheduled recovery pass and the API's reconciliation, at a given
//! tenant size, against the real engines and an in-process Mock PSP.
//!
//! ```text
//! e2e seed    <base-db-url> <template-db> <payments>
//! e2e measure <base-db-url> <template-db> <label> [out.json]
//! ```
//!
//! `seed` builds a database with one tenant holding `<payments>` captured
//! payments (one in ten partly refunded), through the orchestrator, so the
//! rows are what the kernels write. `measure` copies it (`CREATE DATABASE
//! ... TEMPLATE`), starts the API on the copy with a fresh Mock PSP, and
//! times, on that tenant:
//!
//! * `POST /v1/payments` (automatic capture): sequential latency, and the
//!   throughput of 8 concurrent clients;
//! * `GET /v1/payments/{id}`; `POST /v1/payments/{id}/refunds`;
//! * kernel transactions per request (`EngineStats::attempts` of both
//!   engines, so retries count);
//! * `jobs::recover_all` (the scheduled job's pass) with 5 payments left
//!   `PROCESSING` by a lost PSP answer, and a second pass with nothing left;
//!   PSP calls each pass makes;
//! * `POST /v1/reconcile` (loads both kernels' snapshots and the PSP report,
//!   classifies, renders JSON).
//!
//! The same source builds against older revisions (it uses only APIs that
//! exist since SEC-02), which is how the "before" numbers were taken. The
//! Mock PSP of `measure` does not know the seeded payments: their PSP
//! report rows are missing, which reconciliation then reports; the PSP
//! report itself is an in-memory copy here, not Stripe's paginated API.

use h5i_app_pg::EngineConfig;
use payment_connectors::mock::MockPsp;
use serde_json::{json, Value};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

use payment_connectors::{self as psp, BoxFut, Psp, PspError};

/// The Mock PSP, counting every call (reads included).
struct Counted(Arc<MockPsp>, std::sync::atomic::AtomicU64);

impl Counted {
    fn tick(&self) {
        self.1.fetch_add(1, Relaxed);
    }
}

impl Psp for Counted {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn authorize<'a>(&'a self, req: &'a psp::AuthorizeRequest, k: &'a str) -> BoxFut<'a, Result<psp::PspPayment, PspError>> {
        self.tick();
        self.0.authorize(req, k)
    }
    fn capture<'a>(&'a self, r: &'a str, amount: u64, k: &'a str) -> BoxFut<'a, Result<psp::Outcome, PspError>> {
        self.tick();
        self.0.capture(r, amount, k)
    }
    fn cancel<'a>(&'a self, r: &'a str, k: &'a str) -> BoxFut<'a, Result<psp::Outcome, PspError>> {
        self.tick();
        self.0.cancel(r, k)
    }
    fn refund<'a>(&'a self, key: psp::PaymentKey, r: &'a str, refund: u64, amount: u64, k: &'a str) -> BoxFut<'a, Result<psp::RefundOutcome, PspError>> {
        self.tick();
        self.0.refund(key, r, refund, amount, k)
    }
    fn retrieve<'a>(&'a self, r: &'a str) -> BoxFut<'a, Result<psp::PspPayment, PspError>> {
        self.tick();
        self.0.retrieve(r)
    }
    fn find<'a>(&'a self, key: psp::PaymentKey) -> BoxFut<'a, Result<Option<psp::PspPayment>, PspError>> {
        self.tick();
        self.0.find(key)
    }
    fn retrieve_refund<'a>(&'a self, r: &'a str, refund: u64) -> BoxFut<'a, Result<Option<psp::RefundOutcome>, PspError>> {
        self.tick();
        self.0.retrieve_refund(r, refund)
    }
    fn checkout<'a>(&'a self, req: &'a psp::CheckoutRequest) -> BoxFut<'a, Result<psp::CheckoutSession, PspError>> {
        self.tick();
        self.0.checkout(req)
    }
    fn verify_webhook(&self, sig: &str, body: &[u8], now: u64) -> Result<psp::WebhookEvent, psp::WebhookError> {
        self.0.verify_webhook(sig, body, now)
    }
    fn settlement_report<'a>(&'a self, tenant: u64) -> BoxFut<'a, Result<Vec<psp::SettlementRecord>, PspError>> {
        self.tick();
        self.0.settlement_report(tenant)
    }
}

const TENANT: u64 = 1;
const USER_KEY: &str = "e2e-user-secret-0123456789abcdef";
const OPERATOR_KEY: &str = "e2e-operator-secret-0123456789ab";

fn keys() -> payment_api::Keys {
    payment_api::Keys::new(USER_KEY, OPERATOR_KEY).unwrap()
}

fn db_url(base: &str, name: &str) -> String {
    let (prefix, _) = base.rsplit_once('/').expect("database URL with a path");
    format!("{prefix}/{name}")
}

async fn admin(base: &str, sql: &str) {
    let (c, conn) = tokio_postgres::connect(base, tokio_postgres::NoTls).await.unwrap();
    let h = tokio::spawn(conn);
    // One statement at a time: CREATE/DROP DATABASE refuse multi-statement batches.
    for stmt in sql.split(';').map(str::trim).filter(|x| !x.is_empty()) {
        c.batch_execute(stmt).await.unwrap();
    }
    drop(c);
    let _ = h.await;
}

struct Api {
    base: String,
    http: reqwest::Client,
    mock: Arc<MockPsp>,
    psp: Arc<Counted>,
    ledger: Arc<payment_storage::LedgerEngine>,
    payments: Arc<payment_storage::PaymentsEngine>,
    merchant: String,
    op: String,
    url: String,
}

async fn start(url: &str) -> Api {
    let (ledger, payments) = payment_storage::engines(url, 16, EngineConfig::default()).await.unwrap();
    let mock = Arc::new(MockPsp::new("whsec_e2e"));
    mock.set_now(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    let counted = Arc::new(Counted(mock.clone(), Default::default()));
    let state = payment_api::AppState::with_payments(ledger.clone(), payments.clone(), counted.clone(), &keys());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });
    Api {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        mock,
        psp: counted,
        ledger,
        payments,
        merchant: keys().issue_user(TENANT, 7, 86_400).unwrap(),
        op: keys().issue_operator(TENANT, 1, 3600).unwrap(),
        url: url.to_string(),
    }
}

impl Api {
    async fn post(&self, path: &str, tok: &str, body: Value) -> (u16, Value) {
        let r = self.http.post(format!("{}{path}", self.base)).bearer_auth(tok).json(&body).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str, tok: &str) -> (u16, Value) {
        let r = self.http.get(format!("{}{path}", self.base)).bearer_auth(tok).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn pay(&self, amount: u64) -> (u16, Value) {
        self.post("/v1/payments", &self.merchant, json!({"amount": amount, "currency": "jpy", "capture": "automatic", "payment_method": "tok_visa"}))
            .await
    }

    fn attempts(&self) -> u64 {
        self.ledger.stats.attempts.load(Relaxed) + self.payments.stats.attempts.load(Relaxed)
    }
}

fn stats(mut xs: Vec<f64>) -> Value {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| xs[((xs.len() as f64 - 1.0) * p).round() as usize];
    json!({"n": xs.len(), "p50_ms": q(0.5), "p95_ms": q(0.95), "max_ms": xs[xs.len() - 1]})
}

async fn seed(base: &str, name: &str, n: u64) {
    admin(base, &format!("DROP DATABASE IF EXISTS {name} WITH (FORCE); CREATE DATABASE {name}")).await;
    let api = Arc::new(start(&db_url(base, name)).await);
    let t = Instant::now();
    let next = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let (api, next) = (api.clone(), next.clone());
        tasks.push(tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Relaxed);
                if i >= n {
                    break;
                }
                let (st, p) = api.pay(1000 + i % 500).await;
                assert_eq!(st, 200, "{p}");
                if i % 10 == 0 {
                    let id = p["id"].as_u64().unwrap();
                    let (st, r) = api.post(&format!("/v1/payments/{id}/refunds"), &api.merchant, json!({"amount": 100})).await;
                    assert!(st == 200 || st == 202, "{r}");
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    println!("{}", json!({"seeded": n, "seconds": t.elapsed().as_secs_f64()}));
}

async fn measure(base: &str, template: &str, label: &str, out: Option<&str>) {
    let name = format!("{template}_m");
    admin(base, &format!("DROP DATABASE IF EXISTS {name} WITH (FORCE); CREATE DATABASE {name} TEMPLATE {template}")).await;
    let api = start(&db_url(base, &name)).await;
    let snap = api.payments.snapshot(h5i_app_core::TenantId(TENANT)).await.unwrap();
    let journal = api.ledger.snapshot(h5i_app_core::TenantId(TENANT)).await.unwrap();
    let tenant_rows = json!({
        "payments": snap.payments.len(), "refunds": snap.refunds.len(), "psp_refs": snap.refs.len(),
        "accounts": journal.accounts.len(), "transfers": journal.transfers.len(),
    });
    eprintln!("{label}: tenant {tenant_rows}");

    // Warm up the pools and the orchestrator's account cache.
    for _ in 0..3 {
        api.pay(500).await;
    }

    // Sequential creates.
    let mut create = Vec::new();
    let mut ids = Vec::new();
    let a0 = api.attempts();
    for i in 0..30 {
        let t = Instant::now();
        let (st, p) = api.pay(700 + i).await;
        create.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(st, 200, "{p}");
        ids.push(p["id"].as_u64().unwrap());
    }
    let create_tx = (api.attempts() - a0) as f64 / 30.0;

    let mut view = Vec::new();
    let a0 = api.attempts();
    for id in &ids {
        let t = Instant::now();
        let (st, _) = api.get(&format!("/v1/payments/{id}"), &api.merchant).await;
        view.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(st, 200);
    }
    let view_tx = (api.attempts() - a0) as f64 / ids.len() as f64;

    let mut refund = Vec::new();
    let a0 = api.attempts();
    for id in ids.iter().take(20) {
        let t = Instant::now();
        let (st, r) = api.post(&format!("/v1/payments/{id}/refunds"), &api.merchant, json!({"amount": 50})).await;
        refund.push(t.elapsed().as_secs_f64() * 1e3);
        assert!(st == 200 || st == 202, "{r}");
    }
    let refund_tx = (api.attempts() - a0) as f64 / 20.0;

    // Throughput: 8 clients, 25 payments each.
    let api = Arc::new(api);
    let t = Instant::now();
    let a0 = api.attempts();
    let mut tasks = Vec::new();
    for c in 0..8u64 {
        let api = api.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..25 {
                let (st, p) = api.pay(900 + c * 25 + i).await;
                assert_eq!(st, 200, "{p}");
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let conc_s = t.elapsed().as_secs_f64();
    let retries = api.ledger.stats.retries.load(Relaxed) + api.payments.stats.retries.load(Relaxed);
    let conc_tx = api.attempts() - a0;

    // Five payments whose PSP answer is lost: left PROCESSING (202).
    for _ in 0..5 {
        api.mock.timeout_next(1);
        let (st, p) = api.pay(1234).await;
        assert_eq!(st, 202, "{p}");
    }
    let o = payment_api::orchestrator::Orchestrator::new(api.ledger.clone(), api.payments.clone(), api.psp.clone());
    let mut passes = Vec::new();
    // E2E_RECOVERY_PASSES (default 2): a whole-tenant-load build needs about
    // an hour per pass at 10k payments, so the "before" runs use fewer.
    let n_passes: usize = std::env::var("E2E_RECOVERY_PASSES").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    for _ in 0..n_passes {
        let (c0, a0) = (api.psp.1.load(Relaxed), api.attempts());
        let t = Instant::now();
        let r = payment_api::jobs::recover_all(&o, &api.url, &payment_storage::DbTls::Off).await.unwrap();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        // The orchestrator's engines are the API's, so attempts count its commands.
        passes.push(json!({
            "ms": ms, "psp_calls": api.psp.1.load(Relaxed) - c0, "kernel_tx": api.attempts() - a0,
            "tenants": r.tenants, "payments_resolved": r.total.payments_resolved, "ledger_synced": r.total.ledger_synced,
            "failed": r.failed.len(),
        }));
    }

    let mut recon = Vec::new();
    let mut exceptions = Value::Null;
    for _ in 0..3 {
        let t = Instant::now();
        let (st, r) = api.post("/v1/reconcile", &api.op, json!({})).await;
        recon.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(st, 200, "{r}");
        exceptions = json!({"psp": r["psp"]["exceptions"], "ledger": r["ledger"]["exceptions"]});
    }
    let (st, audit) = api.get("/v1/ledger/audit", &api.op).await;
    assert_eq!((st, audit["ok"].as_bool()), (200, Some(true)), "{audit}");

    let result = json!({
        "label": label,
        "tenant": tenant_rows,
        "create": stats(create), "create_kernel_tx": create_tx,
        "view": stats(view), "view_kernel_tx": view_tx,
        "refund": stats(refund), "refund_kernel_tx": refund_tx,
        "concurrent_create": {"clients": 8, "payments": 200, "seconds": conc_s, "per_s": 200.0 / conc_s,
            "kernel_tx": conc_tx, "retries_total": retries},
        "recovery_passes": passes,
        "reconcile": stats(recon), "reconcile_exceptions": exceptions,
    });
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
    if let Some(path) = out {
        let mut all: Vec<Value> = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        all.push(result);
        std::fs::write(path, serde_json::to_vec_pretty(&all).unwrap()).unwrap();
    }
    drop(o);
    drop(api);
    admin(base, &format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).await;
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("seed") if args.len() == 5 => seed(&args[2], &args[3], args[4].parse().unwrap()).await,
        Some("measure") if args.len() >= 5 => measure(&args[2], &args[3], &args[4], args.get(5).map(String::as_str)).await,
        _ => eprintln!("usage: e2e seed <base-url> <template> <payments> | e2e measure <base-url> <template> <label> [out.json]"),
    }
}
