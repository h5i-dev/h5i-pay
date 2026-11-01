//! Payments end to end: the HTTP API, the orchestrator, the Mock PSP, the
//! payment and ledger kernels on PostgreSQL, webhooks, recovery and
//! reconciliation.

use h5i_app_pg::EngineConfig;
use h5i_pay_tests::{database_url, fresh_tenant};
use payment_connectors::mock::MockPsp;
use payment_connectors::Outcome;
use serde_json::{json, Value};
use std::sync::Arc;


struct Env {
    base: String,
    http: reqwest::Client,
    mock: Arc<MockPsp>,
    org: u64,
    op: String,
    merchant: String,
}

async fn start() -> Option<Env> {
    let url = database_url()?;
    let (ledger, payments) = payment_storage::engines(&url, 8, EngineConfig::default()).await.unwrap();
    let mock = Arc::new(MockPsp::new("whsec_test"));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    mock.set_now(now);
    let state = payment_api::AppState::with_payments(ledger, payments, mock.clone(), &h5i_pay_tests::keys());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });
    let org = fresh_tenant();
    Some(Env {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        mock,
        org,
        op: h5i_pay_tests::operator_token(org),
        merchant: h5i_pay_tests::user_token(org, 7),
    })
}

impl Env {
    async fn send(&self, method: reqwest::Method, path: &str, tok: &str, body: Option<Value>, key: Option<&str>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(tok);
        if let Some(b) = body {
            req = req.json(&b);
        }
        if let Some(k) = key {
            req = req.header("Idempotency-Key", k);
        }
        let resp = req.send().await.unwrap();
        let st = resp.status().as_u16();
        (st, resp.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, tok: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, tok, Some(body), None).await
    }

    async fn get(&self, path: &str, tok: &str) -> (u16, Value) {
        self.send(reqwest::Method::GET, path, tok, None, None).await
    }

    async fn pay(&self, amount: u64, capture: &str, pm: &str) -> (u16, Value) {
        self.post("/v1/payments", &self.merchant, json!({"amount": amount, "currency": "jpy", "capture": capture, "payment_method": pm})).await
    }

    /// Deliver every webhook the Mock PSP has queued; returns the statuses.
    async fn deliver(&self) -> Vec<String> {
        let mut out = Vec::new();
        for d in self.mock.take_deliveries() {
            let resp = self
                .http
                .post(format!("{}/v1/webhooks/mock", self.base))
                .header("Stripe-Signature", &d.signature)
                .body(d.body.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), 200);
            let v: Value = resp.json().await.unwrap();
            out.push(v["status"].as_str().unwrap().to_string());
        }
        out
    }

    async fn audit_ledger(&self) {
        let tok = h5i_pay_tests::operator_token(self.org);
        let (st, v) = self.get("/v1/ledger/audit", &tok).await;
        assert_eq!((st, &v["ok"]), (200, &json!(true)), "{v}");
    }

    /// The ledger's merchant payable balance (credits − debits) for merchant 7.
    async fn merchant_balance(&self) -> i64 {
        let tok = h5i_pay_tests::operator_token(self.org);
        let resp = self.http.post(format!("{}/v1/ledger", self.base)).bearer_auth(tok).json(&json!({"cmd": "accounts"})).send().await.unwrap();
        let accts: Value = resp.json().await.unwrap();
        accts
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["owner"] == json!(7) && a["code"] == json!(1002))
            .map(|a| a["credits"].as_i64().unwrap() - a["debits"].as_i64().unwrap())
            .sum()
    }

    async fn reconcile(&self) -> Value {
        let (st, v) = self.post("/v1/reconcile", &self.op, json!({})).await;
        assert_eq!(st, 200, "{v}");
        v
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automatic_capture_posts_the_ledger() {
    let Some(e) = start().await else { return };
    let (st, p) = e.pay(1200, "automatic", "tok_visa").await;
    assert_eq!(st, 200, "{p}");
    assert_eq!(p["status"], "captured");
    assert_eq!(p["captured"], 1200);
    assert!(p["psp_reference"].as_str().unwrap().starts_with("pi_mock_"));
    assert_eq!(e.merchant_balance().await, 1200);
    e.audit_ledger().await;
    // Every webhook the PSP sent for it is now stale or a replay.
    for s in e.deliver().await {
        assert!(s == "processed" || s == "stale" || s == "duplicate", "{s}");
    }
    assert_eq!(e.merchant_balance().await, 1200, "a webhook replay must not post twice");
    let r = e.reconcile().await;
    assert_eq!(r["psp"]["exceptions"], 0, "{r}");
    assert_eq!(r["ledger"]["exceptions"], 0, "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_capture_partial_refunds_and_limits() {
    let Some(e) = start().await else { return };
    let (st, p) = e.pay(1000, "manual", "tok_visa").await;
    assert_eq!((st, p["status"].as_str()), (200, Some("authorized")), "{p}");
    let id = p["id"].as_u64().unwrap();
    let (st, p) = e.post(&format!("/v1/payments/{id}/capture"), &e.merchant, json!({"amount": 600})).await;
    assert_eq!((st, p["status"].as_str(), p["captured"].as_u64()), (200, Some("captured"), Some(600)), "{p}");
    // Capturing again changes nothing.
    let (st, p) = e.post(&format!("/v1/payments/{id}/capture"), &e.merchant, json!({})).await;
    assert_eq!((st, p["captured"].as_u64()), (200, Some(600)));

    let (st, r) = e.post(&format!("/v1/payments/{id}/refunds"), &e.merchant, json!({"amount": 200})).await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["refunded"], 200);
    let (st, r) = e.post(&format!("/v1/payments/{id}/refunds"), &e.merchant, json!({"amount": 401})).await;
    assert_eq!((st, r["error"].as_str()), (409, Some("refund_exceeds_captured")));
    let (st, r) = e.post(&format!("/v1/payments/{id}/refunds"), &e.merchant, json!({"amount": 400})).await;
    assert_eq!((st, r["refunded"].as_u64()), (200, Some(600)), "{r}");
    assert_eq!(e.merchant_balance().await, 0);
    e.audit_ledger().await;

    // Another merchant cannot see or refund it.
    let other = h5i_pay_tests::user_token(e.org, 8);
    assert_eq!(e.get(&format!("/v1/payments/{id}"), &other).await.0, 404);
    assert_eq!(e.post(&format!("/v1/payments/{id}/refunds"), &other, json!({"amount": 1})).await.0, 404);

    e.deliver().await;
    let r = e.reconcile().await;
    assert_eq!(r["psp"]["exceptions"], 0, "{r}");
    assert_eq!(r["ledger"]["exceptions"], 0, "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declines_cancels_and_card_numbers() {
    let Some(e) = start().await else { return };
    let (st, p) = e.pay(500, "automatic", "tok_decline").await;
    assert_eq!((st, p["status"].as_str()), (200, Some("failed")));
    let (st, p) = e.pay(500, "manual", "tok_visa").await;
    assert_eq!(st, 200);
    let id = p["id"].as_u64().unwrap();
    let (st, p) = e.post(&format!("/v1/payments/{id}/cancel"), &e.merchant, json!({})).await;
    assert_eq!((st, p["status"].as_str()), (200, Some("canceled")), "{p}");
    // A canceled payment cannot be captured.
    assert_eq!(e.post(&format!("/v1/payments/{id}/capture"), &e.merchant, json!({})).await.0, 409);
    // A raw card number never reaches the PSP, and no payment is created.
    let calls = e.mock.calls();
    let (st, _) = e.pay(500, "automatic", "4242 4242 4242 4242").await;
    assert_eq!(st, 400);
    assert_eq!(e.mock.calls(), calls);
    assert_eq!(e.merchant_balance().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_payment_completes_by_webhook_once() {
    let Some(e) = start().await else { return };
    let (st, p) = e.pay(800, "automatic", "tok_pending").await;
    assert_eq!((st, p["status"].as_str()), (202, Some("processing")), "{p}");
    let id = p["id"].as_u64().unwrap();
    let reference = p["psp_reference"].as_str().unwrap().to_string();
    e.mock.complete(&reference, Outcome::Captured { amount: 800 });
    let ds = e.mock.take_deliveries();
    assert_eq!(ds.len(), 1);
    let d = ds[0].clone();
    // The same delivery arrives ten times at once.
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let (http, base, d) = (e.http.clone(), e.base.clone(), d.clone());
        tasks.push(tokio::spawn(async move {
            let r = http.post(format!("{base}/v1/webhooks/mock")).header("Stripe-Signature", &d.signature).body(d.body).send().await.unwrap();
            let v: Value = r.json().await.unwrap();
            v["status"].as_str().unwrap().to_string()
        }));
    }
    let mut statuses = Vec::new();
    for t in tasks {
        statuses.push(t.await.unwrap());
    }
    assert_eq!(statuses.iter().filter(|s| *s == "processed").count(), 1, "{statuses:?}");
    assert_eq!(statuses.iter().filter(|s| *s == "duplicate").count(), 9, "{statuses:?}");
    let (_, p) = e.get(&format!("/v1/payments/{id}"), &e.merchant).await;
    assert_eq!(p["status"], "captured");
    assert_eq!(e.merchant_balance().await, 800);
    e.audit_ledger().await;

    // A forged or tampered delivery is refused.
    let r = e.http.post(format!("{}/v1/webhooks/mock", e.base)).header("Stripe-Signature", "t=1,v1=00").body(d.body.clone()).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_out_of_order() {
    let Some(e) = start().await else { return };
    let (_, p) = e.pay(300, "manual", "tok_pending").await;
    let reference = p["psp_reference"].as_str().unwrap().to_string();
    let id = p["id"].as_u64().unwrap();
    e.mock.complete(&reference, Outcome::Authorized { amount: 300 });
    e.mock.complete(&reference, Outcome::Captured { amount: 300 });
    let mut ds = e.mock.take_deliveries();
    assert_eq!(ds.len(), 2);
    ds.reverse(); // `captured` first, then `authorized`
    e.mock.tamper_settlement(|_| {});
    let mut statuses = Vec::new();
    for d in ds {
        let r = e.http.post(format!("{}/v1/webhooks/mock", e.base)).header("Stripe-Signature", &d.signature).body(d.body).send().await.unwrap();
        let v: Value = r.json().await.unwrap();
        statuses.push(v["status"].as_str().unwrap().to_string());
    }
    assert_eq!(statuses, vec!["processed", "stale"]);
    let (_, p) = e.get(&format!("/v1/payments/{id}"), &e.merchant).await;
    assert_eq!((p["status"].as_str(), p["captured"].as_u64()), (Some("captured"), Some(300)));
    assert_eq!(e.merchant_balance().await, 300);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeouts_are_recovered() {
    let Some(e) = start().await else { return };
    // The PSP charges, but its answer is lost.
    e.mock.timeout_next(1);
    let (st, p) = e.pay(900, "automatic", "tok_visa").await;
    assert_eq!((st, p["status"].as_str()), (202, Some("processing")), "{p}");
    let id = p["id"].as_u64().unwrap();
    assert_eq!(p["psp_reference"], Value::Null);
    assert_eq!(e.merchant_balance().await, 0);
    // A refund whose answer is lost stays pending, reserved in both kernels.
    let (st, p2) = e.pay(500, "automatic", "tok_visa").await;
    assert_eq!(st, 200);
    let id2 = p2["id"].as_u64().unwrap();
    e.mock.timeout_next(1);
    let (st, r) = e.post(&format!("/v1/payments/{id2}/refunds"), &e.merchant, json!({"amount": 100})).await;
    assert_eq!((st, r["refund_pending"].as_u64()), (202, Some(100)), "{r}");
    assert_eq!(e.merchant_balance().await, 400, "the refund is reserved in the ledger");

    let (st, rep) = e.post("/v1/recover", &e.op, json!({})).await;
    assert_eq!(st, 200, "{rep}");
    assert_eq!(rep["payments_resolved"], 1);
    assert_eq!(rep["refunds_resolved"], 1);
    let (_, p) = e.get(&format!("/v1/payments/{id}"), &e.merchant).await;
    assert_eq!((p["status"].as_str(), p["psp_reference"].is_string()), (Some("captured"), true));
    let (_, p2) = e.get(&format!("/v1/payments/{id2}"), &e.merchant).await;
    assert_eq!((p2["refunded"].as_u64(), p2["refund_pending"].as_u64()), (Some(100), Some(0)));
    assert_eq!(e.merchant_balance().await, 900 + 400);
    // Recovery is idempotent.
    let (_, rep) = e.post("/v1/recover", &e.op, json!({})).await;
    assert_eq!((rep["payments_resolved"].as_u64(), rep["refunds_resolved"].as_u64()), (Some(0), Some(0)));
    assert_eq!(e.merchant_balance().await, 1300);
    e.audit_ledger().await;
    // Only an operator may run it.
    assert_eq!(e.post("/v1/recover", &e.merchant, json!({})).await.0, 403);

    // A PSP outage before the request reaches it: nothing at the PSP; the
    // operator can fail it explicitly.
    e.mock.fail_next(1);
    let (st, p3) = e.pay(50, "automatic", "tok_visa").await;
    assert_eq!((st, p3["status"].as_str()), (202, Some("processing")));
    let (_, rep) = e.post("/v1/recover?fail_unsubmitted=true", &e.op, json!({})).await;
    assert_eq!(rep["payments_failed"], 1, "{rep}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refund_needs_merchant_funds_in_the_ledger() {
    let Some(e) = start().await else { return };
    let (_, p) = e.pay(700, "automatic", "tok_visa").await;
    let id = p["id"].as_u64().unwrap();
    // The operator pays the merchant out of the ledger (to an external account).
    let lt = h5i_pay_tests::operator_token(e.org);
    let accts: Value = e.http.post(format!("{}/v1/ledger", e.base)).bearer_auth(&lt).json(&json!({"cmd": "accounts"})).send().await.unwrap().json().await.unwrap();
    let payable = accts.as_array().unwrap().iter().find(|a| a["code"] == json!(1002)).unwrap()["id"].as_u64().unwrap();
    let r = e.http.post(format!("{}/v1/ledger", e.base)).bearer_auth(&lt).json(&json!({"cmd": "open", "currency": 392, "code": 5})).send().await.unwrap();
    let bank = r.json::<Value>().await.unwrap()["id"].as_u64().unwrap();
    let r = e.http.post(format!("{}/v1/ledger", e.base)).bearer_auth(&lt).json(&json!({"cmd": "transfer", "id": 1, "debit": payable, "credit": bank, "amount": 650})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let calls = e.mock.calls();
    let (st, r) = e.post(&format!("/v1/payments/{id}/refunds"), &e.merchant, json!({"amount": 100})).await;
    assert_eq!((st, r["error"].as_str()), (409, Some("merchant_balance_insufficient")), "{r}");
    assert_eq!(e.mock.calls(), calls, "the PSP was not asked to refund");
    let (_, p) = e.get(&format!("/v1/payments/{id}"), &e.merchant).await;
    assert_eq!((p["refunded"].as_u64(), p["refund_pending"].as_u64()), (Some(0), Some(0)));
    assert_eq!(p["refunds"][0]["status"], "failed");
    // 50 is still there.
    let (st, _) = e.post(&format!("/v1/payments/{id}/refunds"), &e.merchant, json!({"amount": 50})).await;
    assert_eq!(st, 200);
    assert_eq!(e.merchant_balance().await, 0);
    e.audit_ledger().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconciliation_finds_discrepancies() {
    let Some(e) = start().await else { return };
    let mut ids = Vec::new();
    for amount in [100, 200, 300, 400] {
        let (_, p) = e.pay(amount, "automatic", "tok_visa").await;
        ids.push(p["id"].as_u64().unwrap());
    }
    let r = e.reconcile().await;
    assert_eq!(r["psp"]["exceptions"], 0, "{r}");
    // The PSP's report: one amount differs, one capture is missing, one is unknown to us.
    let (a, b) = (ids[0], ids[1]);
    let org = e.org;
    e.mock.tamper_settlement(|rows| {
        rows.iter_mut().find(|x| x.tenant == org && x.id == a).unwrap().amount = 99;
        rows.retain(|x| !(x.tenant == org && x.id == b));
        let mut extra = rows.iter().find(|x| x.tenant == org).unwrap().clone();
        extra.id = 9_999;
        rows.push(extra);
    });
    for engine in ["single", "parallel", "simd", "reference"] {
        let (st, r) = e.post(&format!("/v1/reconcile?engine={engine}"), &e.op, json!({})).await;
        assert_eq!(st, 200);
        assert_eq!(r["psp"]["internal_codes"], json!({"matched": 2, "amount_mismatch": 1, "missing_external": 1}), "{engine}: {r}");
        assert_eq!(r["psp"]["external_codes"], json!({"matched": 3, "extra_external": 1}), "{engine}: {r}");
        assert_eq!(r["ledger"]["exceptions"], 0, "{engine}: {r}");
    }
    assert_eq!(e.post("/v1/reconcile", &e.merchant, json!({})).await.0, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idempotency_key_creates_one_payment() {
    let Some(e) = start().await else { return };
    let body = json!({"amount": 1500, "currency": 392, "payment_method": "tok_visa"});
    let (s1, p1) = e.send(reqwest::Method::POST, "/v1/payments", &e.merchant, Some(body.clone()), Some("order-17")).await;
    let (s2, p2) = e.send(reqwest::Method::POST, "/v1/payments", &e.merchant, Some(body), Some("order-17")).await;
    assert_eq!((s1, s2), (200, 200));
    assert_eq!(p1["id"], p2["id"]);
    assert_eq!(e.merchant_balance().await, 1500, "charged once");
    let (_, list) = e.post("/v1/payments/rpc", &e.merchant, json!({"cmd": "list"})).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
}
