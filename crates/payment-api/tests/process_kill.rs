//! The API process killed (SIGKILL) in the middle of a request.
//!
//! These tests run the real `h5i-pay` binary, built as production builds it
//! (no Mock PSP), against PostgreSQL and a fake Stripe API in this process
//! (`STRIPE_API_BASE`). The fake can hold a request open, either before it
//! acts or after (the charge or refund exists at the PSP, the answer never
//! arrives). While the API waits for that answer, the test kills it. What
//! must hold afterwards: nothing is half applied in the kernels, money moves
//! at the PSP at most once, a client retry with the same `Idempotency-Key`
//! or the scheduled `h5i-pay recover` job finishes the work, the ledger
//! audit passes and reconciliation finds no exceptions.
//!
//! Each test uses its own database (the recovery job sees every tenant).
//! Needs `H5I_APP_TEST_DATABASE_URL`; skipped without it.

use payment_api::Keys;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const USER_SECRET: &str = "process-kill-user-secret-0123456789";
const OPERATOR_SECRET: &str = "process-kill-operator-secret-012345";

fn keys() -> Keys {
    Keys::new(USER_SECRET, OPERATOR_SECRET).unwrap()
}

// ---------------------------------------------------------------- fake Stripe

/// Which request the fake holds open, and whether it acts first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hold {
    /// `POST /v1/payment_intents`, without creating it.
    BeforeCharge,
    /// `POST /v1/payment_intents`, after creating (and charging) it.
    AfterCharge,
    /// `POST /v1/refunds`, after creating it.
    AfterRefund,
}

#[derive(Default)]
struct StripeState {
    intents: Vec<Value>,
    refunds: Vec<Value>,
    /// Idempotency-Key → the response body first sent for it.
    idem: HashMap<String, Value>,
    hold: Option<Hold>,
    /// Requests that changed state at the PSP (creates, captures, refunds),
    /// replays excluded.
    effects: u32,
}

#[derive(Clone)]
struct FakeStripe {
    st: Arc<Mutex<StripeState>>,
    /// Signalled when a held request has arrived (and acted, if it does).
    held: Arc<Notify>,
}

fn form_map(body: &str) -> HashMap<String, String> {
    serde_urlencoded::from_str::<Vec<(String, String)>>(body).unwrap().into_iter().collect()
}

fn metadata_of(f: &HashMap<String, String>) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in f {
        if let Some(name) = k.strip_prefix("metadata[").and_then(|s| s.strip_suffix(']')) {
            m.insert(name.to_string(), json!(v));
        }
    }
    Value::Object(m)
}

/// `metadata['name']:'value'` terms and `status:'x'` of a Stripe search query.
fn query_terms(q: &str) -> Vec<(String, String)> {
    q.split(" AND ")
        .filter_map(|t| {
            let (k, v) = t.split_once(':')?;
            let k = k.trim().trim_start_matches("metadata['").trim_end_matches("']").to_string();
            Some((k, v.trim().trim_matches('\'').to_string()))
        })
        .collect()
}

impl FakeStripe {
    async fn start() -> (FakeStripe, String) {
        use axum::extract::{Path, Query, State};
        use axum::http::HeaderMap;
        use axum::routing::{get, post};

        let fake = FakeStripe { st: Arc::default(), held: Arc::new(Notify::new()) };

        async fn create_intent(State(f): State<FakeStripe>, h: HeaderMap, body: String) -> axum::Json<Value> {
            let key = h.get("idempotency-key").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            let hold = {
                let mut st = f.st.lock().unwrap();
                if let Some(v) = st.idem.get(&key) {
                    return axum::Json(v.clone());
                }
                st.hold.take_if(|h| matches!(h, Hold::BeforeCharge | Hold::AfterCharge))
            };
            if hold == Some(Hold::BeforeCharge) {
                f.held.notify_one();
                std::future::pending::<()>().await;
            }
            let fm = form_map(&body);
            let amount: u64 = fm["amount"].parse().unwrap();
            let manual = fm.get("capture_method").map(String::as_str) == Some("manual");
            let pi = {
                let mut st = f.st.lock().unwrap();
                let pi = json!({
                    "id": format!("pi_fake_{}", st.intents.len()),
                    "object": "payment_intent",
                    "status": if manual { "requires_capture" } else { "succeeded" },
                    "amount": amount,
                    "amount_capturable": if manual { amount } else { 0 },
                    "amount_received": if manual { 0 } else { amount },
                    "currency": fm["currency"],
                    "metadata": metadata_of(&fm),
                });
                st.intents.push(pi.clone());
                st.idem.insert(key, pi.clone());
                st.effects += 1;
                pi
            };
            if hold == Some(Hold::AfterCharge) {
                f.held.notify_one();
                std::future::pending::<()>().await;
            }
            axum::Json(pi)
        }

        async fn get_intent(State(f): State<FakeStripe>, Path(id): Path<String>) -> axum::response::Response {
            use axum::response::IntoResponse;
            let st = f.st.lock().unwrap();
            match st.intents.iter().find(|p| p["id"] == json!(id)) {
                Some(p) => axum::Json(p.clone()).into_response(),
                None => (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"error": {"code": "resource_missing"}}))).into_response(),
            }
        }

        async fn search(State(f): State<FakeStripe>, Query(q): Query<HashMap<String, String>>) -> axum::Json<Value> {
            let terms = query_terms(&q["query"]);
            let st = f.st.lock().unwrap();
            let data: Vec<Value> = st
                .intents
                .iter()
                .filter(|p| terms.iter().all(|(k, v)| if k == "status" { p["status"] == json!(v) } else { p["metadata"][k] == json!(v) }))
                .cloned()
                .collect();
            axum::Json(json!({"object": "search_result", "data": data, "has_more": false}))
        }

        async fn create_refund(State(f): State<FakeStripe>, h: HeaderMap, body: String) -> axum::Json<Value> {
            let key = h.get("idempotency-key").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            let fm = form_map(&body);
            let (r, hold) = {
                let mut st = f.st.lock().unwrap();
                if let Some(v) = st.idem.get(&key) {
                    return axum::Json(v.clone());
                }
                let currency = st.intents.iter().find(|p| p["id"] == json!(fm["payment_intent"])).map(|p| p["currency"].clone()).unwrap();
                let r = json!({
                    "id": format!("re_fake_{}", st.refunds.len()),
                    "object": "refund",
                    "status": "succeeded",
                    "amount": fm["amount"].parse::<u64>().unwrap(),
                    "currency": currency,
                    "payment_intent": fm["payment_intent"],
                    "metadata": metadata_of(&fm),
                });
                st.refunds.push(r.clone());
                st.idem.insert(key, r.clone());
                st.effects += 1;
                (r, st.hold.take_if(|h| *h == Hold::AfterRefund))
            };
            if hold.is_some() {
                f.held.notify_one();
                std::future::pending::<()>().await;
            }
            axum::Json(r)
        }

        async fn list_refunds(State(f): State<FakeStripe>, Query(q): Query<HashMap<String, String>>) -> axum::Json<Value> {
            let st = f.st.lock().unwrap();
            let data: Vec<Value> = st.refunds.iter().filter(|r| r["payment_intent"] == json!(q["payment_intent"])).cloned().collect();
            axum::Json(json!({"object": "list", "data": data, "has_more": false}))
        }

        let app = axum::Router::new()
            .route("/v1/payment_intents", post(create_intent))
            .route("/v1/payment_intents/search", get(search))
            .route("/v1/payment_intents/{id}", get(get_intent))
            .route("/v1/refunds", post(create_refund).get(list_refunds))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, base)
    }

    fn hold(&self, h: Hold) {
        self.st.lock().unwrap().hold = Some(h);
    }

    fn effects(&self) -> u32 {
        self.st.lock().unwrap().effects
    }

    fn intents_for(&self, tenant: u64) -> Vec<Value> {
        let t = tenant.to_string();
        self.st.lock().unwrap().intents.iter().filter(|p| p["metadata"]["h5i_tenant"] == json!(t)).cloned().collect()
    }
}

// ------------------------------------------------------------ the database

/// A new empty database next to the test database, dropped at the end.
struct ScratchDb {
    admin: String,
    name: String,
    url: String,
}

impl ScratchDb {
    async fn create(base: &str) -> ScratchDb {
        let (prefix, _) = base.rsplit_once('/').expect("database URL with a path");
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("h5i_pay_kill_{}_{n}", std::process::id());
        let (c, conn) = tokio_postgres::connect(base, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(conn);
        c.batch_execute(&format!("CREATE DATABASE {name}")).await.unwrap();
        ScratchDb { admin: base.to_string(), url: format!("{prefix}/{name}"), name }
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        let (admin, name) = (self.admin.clone(), self.name.clone());
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

// ------------------------------------------------------------- the process

struct Env {
    db: ScratchDb,
    stripe: FakeStripe,
    stripe_base: String,
    http: reqwest::Client,
    tenant: u64,
}

struct Api {
    child: Child,
    base: String,
}

impl Env {
    async fn new() -> Option<Env> {
        let url = std::env::var("H5I_APP_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty());
        let Some(url) = url else {
            eprintln!("H5I_APP_TEST_DATABASE_URL not set; skipping");
            return None;
        };
        let db = ScratchDb::create(&url).await;
        let (stripe, stripe_base) = FakeStripe::start().await;
        let tenant = 1000 + u64::from(std::process::id() % 1000);
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
        Some(Env { db, stripe, stripe_base, http, tenant })
    }

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_h5i-pay"));
        c.env_clear()
            .env("DATABASE_URL", &self.db.url)
            .env("H5I_APP_SECRET", USER_SECRET)
            .env("H5I_PAY_OPERATOR_SECRET", OPERATOR_SECRET)
            .env("H5I_PAY_PSP", "stripe")
            .env("STRIPE_SECRET_KEY", "sk_test_fake")
            .env("STRIPE_WEBHOOK_SECRET", "whsec_fake")
            .env("STRIPE_API_BASE", &self.stripe_base)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        c
    }

    /// Start the API process and wait until it answers `/healthz`.
    async fn start_api(&self) -> Api {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // Owned by `Api` at once, so its `Drop` kills and reaps it on every path.
        let api = Api {
            child: self.command().env("LISTEN", format!("127.0.0.1:{port}")).spawn().expect("start h5i-pay"),
            base: format!("http://127.0.0.1:{port}"),
        };
        for _ in 0..200 {
            if let Ok(r) = self.http.get(format!("{}/healthz", api.base)).send().await
                && r.status().is_success()
            {
                return api;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("h5i-pay did not start");
    }

    /// `h5i-pay recover`, the scheduled job, to completion; its JSON line.
    fn recover_job(&self) -> Value {
        let out = self.command().arg("recover").output().expect("run h5i-pay recover");
        assert!(out.status.success(), "recover exited {:?}", out.status);
        let line = String::from_utf8(out.stdout).unwrap();
        serde_json::from_str(line.lines().last().unwrap()).unwrap()
    }

    fn merchant(&self) -> String {
        keys().issue_user(self.tenant, 7, 600).unwrap()
    }

    fn operator_payments(&self) -> String {
        keys().issue_operator(self.tenant, 1, 600).unwrap()
    }

    fn operator(&self) -> String {
        keys().issue_operator(self.tenant, 1, 600).unwrap()
    }

    async fn call(&self, api: &Api, method: reqwest::Method, path: &str, tok: &str, body: Option<Value>, key: Option<&str>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", api.base)).bearer_auth(tok);
        if let Some(b) = body {
            req = req.json(&b);
        }
        if let Some(k) = key {
            req = req.header("Idempotency-Key", k);
        }
        let r = req.send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    /// Credits − debits of the merchant's payable account (code 1002).
    async fn merchant_balance(&self, api: &Api) -> i64 {
        let (_, accts) = self.call(api, reqwest::Method::POST, "/v1/ledger", &self.operator(), Some(json!({"cmd": "accounts", "owner": 7})), None).await;
        accts["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["owner"] == json!(7) && a["code"] == json!(1002))
            .map(|a| a["credits"].as_i64().unwrap() - a["debits"].as_i64().unwrap())
            .sum()
    }

    /// The ledger audit passes and both reconciliations are clean.
    async fn books_are_consistent(&self, api: &Api) {
        let (st, a) = self.call(api, reqwest::Method::GET, "/v1/ledger/audit", &self.operator(), None, None).await;
        assert_eq!((st, &a["ok"]), (200, &json!(true)), "{a}");
        let (st, r) = self.call(api, reqwest::Method::POST, "/v1/reconcile", &self.operator(), Some(json!({})), None).await;
        assert_eq!(st, 200, "{r}");
        assert_eq!((&r["psp"]["exceptions"], &r["ledger"]["exceptions"]), (&json!(0), &json!(0)), "{r}");
    }
}

impl Api {
    /// SIGKILL: no shutdown handler runs, open transactions are abandoned.
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Send a request that the API will never finish (the test kills it).
fn send_and_abandon(e: &Env, api: &Api, path: &str, tok: String, body: Value, key: Option<&str>) -> tokio::task::JoinHandle<()> {
    let mut req = e.http.post(format!("{}{path}", api.base)).bearer_auth(tok).json(&body);
    if let Some(k) = key {
        req = req.header("Idempotency-Key", k);
    }
    tokio::spawn(async move {
        // The connection dies with the process.
        assert!(req.send().await.is_err(), "the API answered a request it should have died in");
    })
}

/// Until the fake PSP holds the request (after acting, if it acts).
async fn wait_held(e: &Env) {
    tokio::time::timeout(Duration::from_secs(20), e.stripe.held.notified()).await.expect("the request never reached the PSP");
}

// ----------------------------------------------------------------- tests

/// Killed after the PSP charged, before its answer arrived. The payment is
/// left `PROCESSING` with no PSP reference and nothing in the ledger. The
/// client retries with the same `Idempotency-Key` against a new process:
/// the same payment, the same PSP charge (Stripe idempotency key), posted
/// to the ledger once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_after_the_psp_charged_then_client_retry() {
    let Some(e) = Env::new().await else { return };
    let api = e.start_api().await;
    let body = json!({"amount": 1500, "currency": "jpy", "capture": "automatic", "payment_method": "pm_card_visa"});

    e.stripe.hold(Hold::AfterCharge);
    let req = send_and_abandon(&e, &api, "/v1/payments", e.merchant(), body.clone(), Some("order-42"));
    wait_held(&e).await;
    api.kill();
    req.await.unwrap();
    assert_eq!(e.stripe.intents_for(e.tenant).len(), 1, "the PSP charged once");

    let api = e.start_api().await;
    let (_, p) = e.call(&api, reqwest::Method::GET, "/v1/payments/0", &e.merchant(), None, None).await;
    assert_eq!((p["status"].as_str(), &p["psp_reference"]), (Some("processing"), &Value::Null), "half done, nothing invented: {p}");
    assert_eq!(e.merchant_balance(&api).await, 0, "nothing posted before the PSP's answer");

    let (st, p) = e.call(&api, reqwest::Method::POST, "/v1/payments", &e.merchant(), Some(body), Some("order-42")).await;
    assert_eq!((st, p["id"].as_u64(), p["status"].as_str()), (200, Some(0), Some("captured")), "{p}");
    assert_eq!(p["psp_reference"], json!("pi_fake_0"));
    assert_eq!(e.stripe.intents_for(e.tenant).len(), 1, "the retry reused the PSP's charge");
    assert_eq!(e.stripe.effects(), 1);
    assert_eq!(e.merchant_balance(&api).await, 1500);

    // The scheduled job afterwards has nothing left to do.
    let r = e.recover_job();
    assert_eq!((r["payments_resolved"].as_u64(), r["tenants_failed"].as_u64()), (Some(0), Some(0)), "{r}");
    assert_eq!(e.merchant_balance(&api).await, 1500, "recovery posts nothing twice");
    e.books_are_consistent(&api).await;
}

/// Killed after the PSP made a refund, before its answer arrived. The
/// refund is reserved (payment kernel and ledger) and pending. The
/// scheduled `h5i-pay recover` job, with no API running, finds the refund
/// at the PSP and settles it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_during_a_refund_then_scheduled_recovery() {
    let Some(e) = Env::new().await else { return };
    let api = e.start_api().await;
    let (st, p) = e
        .call(&api, reqwest::Method::POST, "/v1/payments", &e.merchant(), Some(json!({"amount": 2000, "currency": "jpy", "capture": "automatic", "payment_method": "pm_card_visa"})), None)
        .await;
    assert_eq!((st, p["status"].as_str()), (200, Some("captured")), "{p}");

    e.stripe.hold(Hold::AfterRefund);
    let req = send_and_abandon(&e, &api, "/v1/payments/0/refunds", e.merchant(), json!({"amount": 700}), None);
    wait_held(&e).await;
    api.kill();
    req.await.unwrap();

    let api = e.start_api().await;
    let (_, p) = e.call(&api, reqwest::Method::GET, "/v1/payments/0", &e.merchant(), None, None).await;
    assert_eq!((p["refunded"].as_u64(), p["refund_pending"].as_u64()), (Some(0), Some(700)), "reserved, not settled: {p}");
    assert_eq!(e.merchant_balance(&api).await, 1300, "the reservation is in the ledger");
    // Another refund cannot use the reserved amount.
    let (st, _) = e.call(&api, reqwest::Method::POST, "/v1/payments/0/refunds", &e.merchant(), Some(json!({"amount": 1400})), None).await;
    assert_eq!(st, 409, "2000 captured, 700 reserved: 1400 more must be refused");
    drop(api);

    let r = e.recover_job();
    assert_eq!((r["refunds_resolved"].as_u64(), r["tenants_failed"].as_u64()), (Some(1), Some(0)), "{r}");

    let api = e.start_api().await;
    let (_, p) = e.call(&api, reqwest::Method::GET, "/v1/payments/0", &e.merchant(), None, None).await;
    assert_eq!((p["refunded"].as_u64(), p["refund_pending"].as_u64()), (Some(700), Some(0)), "{p}");
    assert_eq!(e.merchant_balance(&api).await, 1300);
    assert_eq!(e.stripe.effects(), 2, "one charge, one refund");
    e.books_are_consistent(&api).await;
}

/// Killed while the request was on its way to the PSP, before the PSP did
/// anything. The job cannot find it at the PSP and leaves it alone
/// (`payments_unresolved`); an operator fails it, and no money ever moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_before_the_psp_acted_then_operator_fails_it() {
    let Some(e) = Env::new().await else { return };
    let api = e.start_api().await;
    e.stripe.hold(Hold::BeforeCharge);
    let req = send_and_abandon(
        &e,
        &api,
        "/v1/payments",
        e.merchant(),
        json!({"amount": 900, "currency": "jpy", "capture": "automatic", "payment_method": "pm_card_visa"}),
        None,
    );
    wait_held(&e).await;
    api.kill();
    req.await.unwrap();

    let r = e.recover_job();
    assert_eq!((r["payments_resolved"].as_u64(), r["payments_unresolved"].as_u64()), (Some(0), Some(1)), "{r}");

    let api = e.start_api().await;
    let (_, p) = e.call(&api, reqwest::Method::GET, "/v1/payments/0", &e.merchant(), None, None).await;
    assert_eq!(p["status"], "processing", "the job never fails a payment by itself: {p}");
    let (st, r) = e.call(&api, reqwest::Method::POST, "/v1/recover?fail_unsubmitted=true", &e.operator_payments(), Some(json!({})), None).await;
    assert_eq!((st, r["payments_failed"].as_u64()), (200, Some(1)), "{r}");
    let (_, p) = e.call(&api, reqwest::Method::GET, "/v1/payments/0", &e.merchant(), None, None).await;
    assert_eq!(p["status"], "failed");
    assert_eq!(e.stripe.effects(), 0, "no money moved");
    assert_eq!(e.merchant_balance(&api).await, 0);
    e.books_are_consistent(&api).await;
}
