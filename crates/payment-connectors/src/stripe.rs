//! Stripe, in test mode only.
//!
//! The secret key comes from the environment (`STRIPE_SECRET_KEY`) and must
//! be a test key (`sk_test_…` or `rk_test_…`); a live key is refused at
//! construction, so this build can never move real money. Card data never
//! passes through: payments are confirmed with a PaymentMethod id or test
//! token (`pm_card_visa`), or collected on Stripe's hosted Checkout page.
//!
//! Every POST carries an `Idempotency-Key` derived from our ids. Stripe keeps
//! keys for 24 hours; recovery after that finds objects by their `h5i_*`
//! metadata instead.

use crate::webhook;
use crate::*;
use serde_json::Value;
use std::time::Duration;

pub struct StripePsp {
    key: String,
    webhook_secret: String,
    base: String,
    http: reqwest::Client,
}

pub const DEFAULT_BASE: &str = "https://api.stripe.com";

impl StripePsp {
    /// From `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET` and optionally
    /// `STRIPE_API_BASE` (for a local fake in tests).
    pub fn from_env() -> Result<Self, PspError> {
        let key = std::env::var("STRIPE_SECRET_KEY").map_err(|_| PspError::Config("STRIPE_SECRET_KEY is not set".into()))?;
        let secret = std::env::var("STRIPE_WEBHOOK_SECRET").unwrap_or_default();
        let base = std::env::var("STRIPE_API_BASE").unwrap_or_else(|_| DEFAULT_BASE.into());
        Self::new(key, secret, base)
    }

    pub fn new(key: String, webhook_secret: String, base: String) -> Result<Self, PspError> {
        if !(key.starts_with("sk_test_") || key.starts_with("rk_test_")) {
            return Err(PspError::Config("only Stripe test-mode keys (sk_test_/rk_test_) are accepted".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| PspError::Config(e.to_string()))?;
        Ok(StripePsp { key, webhook_secret, base: base.trim_end_matches('/').to_string(), http })
    }

    async fn send(&self, req: reqwest::RequestBuilder, idempotency_key: Option<&str>) -> Result<(u16, Value), PspError> {
        let mut req = req.bearer_auth(&self.key).header("Stripe-Version", "2024-06-20");
        if let Some(k) = idempotency_key {
            req = req.header("Idempotency-Key", k);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_connect() {
                PspError::Unavailable(e.to_string())
            } else {
                // Sent, or maybe sent: the outcome is unknown.
                PspError::Timeout
            }
        })?;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.map_err(|_| PspError::Timeout)?;
        if status >= 500 {
            return Err(PspError::Timeout);
        }
        Ok((status, body))
    }

    async fn post(&self, path: &str, form: &[(String, String)], key: &str) -> Result<(u16, Value), PspError> {
        self.send(self.http.post(format!("{}{path}", self.base)).form(form), Some(key)).await
    }

    async fn get(&self, path: &str, query: &[(String, String)]) -> Result<(u16, Value), PspError> {
        self.send(self.http.get(format!("{}{path}", self.base)).query(query), None).await
    }
}

fn kv(k: &str, v: impl ToString) -> (String, String) {
    (k.to_string(), v.to_string())
}

fn meta(key: PaymentKey) -> Vec<(String, String)> {
    vec![kv("metadata[h5i_tenant]", key.tenant), kv("metadata[h5i_payment]", key.payment)]
}

fn rejected(body: &Value) -> PspError {
    let e = &body["error"];
    PspError::Rejected {
        code: e["code"].as_str().or(e["type"].as_str()).unwrap_or("unknown").to_string(),
        message: e["message"].as_str().unwrap_or("").to_string(),
    }
}

/// A PaymentIntent object as an [`Outcome`].
pub fn intent_outcome(pi: &Value) -> Outcome {
    let n = |k: &str| pi[k].as_u64().unwrap_or(0);
    match pi["status"].as_str().unwrap_or("") {
        "requires_capture" => Outcome::Authorized { amount: n("amount_capturable") },
        "succeeded" => Outcome::Captured { amount: n("amount_received") },
        "canceled" => Outcome::Canceled,
        "requires_payment_method" => Outcome::Declined {
            reason: pi["last_payment_error"]["code"].as_str().unwrap_or("requires_payment_method").to_string(),
        },
        _ => Outcome::Pending,
    }
}

fn refund_outcome(r: &Value) -> RefundOutcome {
    match r["status"].as_str().unwrap_or("") {
        "succeeded" => RefundOutcome::Succeeded,
        "failed" | "canceled" => RefundOutcome::Failed,
        _ => RefundOutcome::Pending,
    }
}

fn reference(pi: &Value) -> Result<String, PspError> {
    pi["id"].as_str().map(String::from).ok_or_else(|| PspError::Rejected { code: "no_id".into(), message: pi.to_string() })
}

impl Psp for StripePsp {
    fn name(&self) -> &'static str {
        "stripe"
    }

    fn authorize<'a>(&'a self, req: &'a AuthorizeRequest, key: &'a str) -> BoxFut<'a, Result<PspPayment, PspError>> {
        Box::pin(async move {
            check_payment_method(&req.payment_method)?;
            let currency = currency_alpha(req.currency).ok_or_else(|| PspError::Config(format!("currency {}", req.currency)))?;
            let mut form = vec![
                kv("amount", req.amount),
                kv("currency", currency),
                kv("payment_method", &req.payment_method),
                kv("confirm", "true"),
                kv("capture_method", if req.manual_capture { "manual" } else { "automatic" }),
                kv("automatic_payment_methods[enabled]", "true"),
                kv("automatic_payment_methods[allow_redirects]", "never"),
            ];
            form.extend(meta(req.key));
            let (status, body) = self.post("/v1/payment_intents", &form, key).await?;
            match status {
                200 => Ok(PspPayment { reference: reference(&body)?, outcome: intent_outcome(&body) }),
                // A card decline still created the PaymentIntent.
                402 if body["error"]["payment_intent"].is_object() => {
                    let pi = &body["error"]["payment_intent"];
                    let reason = body["error"]["decline_code"].as_str().or(body["error"]["code"].as_str()).unwrap_or("declined");
                    Ok(PspPayment { reference: reference(pi)?, outcome: Outcome::Declined { reason: reason.to_string() } })
                }
                _ => Err(rejected(&body)),
            }
        })
    }

    fn capture<'a>(&'a self, reference: &'a str, amount: u64, key: &'a str) -> BoxFut<'a, Result<Outcome, PspError>> {
        Box::pin(async move {
            let (status, body) = self.post(&format!("/v1/payment_intents/{reference}/capture"), &[kv("amount_to_capture", amount)], key).await?;
            if status == 200 { Ok(intent_outcome(&body)) } else { Err(rejected(&body)) }
        })
    }

    fn cancel<'a>(&'a self, reference: &'a str, key: &'a str) -> BoxFut<'a, Result<Outcome, PspError>> {
        Box::pin(async move {
            let (status, body) = self.post(&format!("/v1/payment_intents/{reference}/cancel"), &[], key).await?;
            if status == 200 { Ok(intent_outcome(&body)) } else { Err(rejected(&body)) }
        })
    }

    fn refund<'a>(&'a self, pk: PaymentKey, reference: &'a str, refund: u64, amount: u64, key: &'a str) -> BoxFut<'a, Result<RefundOutcome, PspError>> {
        Box::pin(async move {
            let mut form = vec![kv("payment_intent", reference), kv("amount", amount), kv("metadata[h5i_refund]", refund)];
            form.extend(meta(pk));
            let (status, body) = self.post("/v1/refunds", &form, key).await?;
            if status == 200 { Ok(refund_outcome(&body)) } else { Err(rejected(&body)) }
        })
    }

    fn retrieve<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Result<PspPayment, PspError>> {
        Box::pin(async move {
            let (status, body) = self.get(&format!("/v1/payment_intents/{reference}"), &[]).await?;
            match status {
                200 => Ok(PspPayment { reference: reference.to_string(), outcome: intent_outcome(&body) }),
                404 => Err(PspError::NotFound),
                _ => Err(rejected(&body)),
            }
        })
    }

    fn find<'a>(&'a self, key: PaymentKey) -> BoxFut<'a, Result<Option<PspPayment>, PspError>> {
        Box::pin(async move {
            // The search index lags writes by up to a minute; recovery runs later than that.
            let q = format!("metadata['h5i_tenant']:'{}' AND metadata['h5i_payment']:'{}'", key.tenant, key.payment);
            let (status, body) = self.get("/v1/payment_intents/search", &[kv("query", q)]).await?;
            if status != 200 {
                return Err(rejected(&body));
            }
            match body["data"].as_array().and_then(|d| d.first()) {
                Some(pi) => Ok(Some(PspPayment { reference: reference(pi)?, outcome: intent_outcome(pi) })),
                None => Ok(None),
            }
        })
    }

    fn retrieve_refund<'a>(&'a self, reference: &'a str, refund: u64) -> BoxFut<'a, Result<Option<RefundOutcome>, PspError>> {
        Box::pin(async move {
            let (status, body) = self.get("/v1/refunds", &[kv("payment_intent", reference), kv("limit", 100)]).await?;
            if status != 200 {
                return Err(rejected(&body));
            }
            let want = refund.to_string();
            Ok(body["data"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["metadata"]["h5i_refund"].as_str() == Some(want.as_str()))
                .map(refund_outcome))
        })
    }

    fn checkout<'a>(&'a self, req: &'a CheckoutRequest) -> BoxFut<'a, Result<CheckoutSession, PspError>> {
        Box::pin(async move {
            let currency = currency_alpha(req.currency).ok_or_else(|| PspError::Config(format!("currency {}", req.currency)))?;
            let form = vec![
                kv("mode", "payment"),
                kv("success_url", &req.success_url),
                kv("cancel_url", &req.cancel_url),
                kv("line_items[0][quantity]", 1),
                kv("line_items[0][price_data][currency]", currency),
                kv("line_items[0][price_data][unit_amount]", req.amount),
                kv("line_items[0][price_data][product_data][name]", &req.description),
                kv("payment_intent_data[capture_method]", if req.manual_capture { "manual" } else { "automatic" }),
                kv("payment_intent_data[metadata][h5i_tenant]", req.key.tenant),
                kv("payment_intent_data[metadata][h5i_payment]", req.key.payment),
                kv("metadata[h5i_tenant]", req.key.tenant),
                kv("metadata[h5i_payment]", req.key.payment),
            ];
            let idem = idempotency_key(req.key.tenant, "payment", req.key.payment, "checkout");
            let (status, body) = self.post("/v1/checkout/sessions", &form, &idem).await?;
            if status != 200 {
                return Err(rejected(&body));
            }
            Ok(CheckoutSession {
                id: body["id"].as_str().unwrap_or_default().to_string(),
                url: body["url"].as_str().unwrap_or_default().to_string(),
            })
        })
    }

    fn verify_webhook(&self, header: &str, body: &[u8], now: u64) -> Result<WebhookEvent, WebhookError> {
        if self.webhook_secret.is_empty() {
            // Without a secret nothing can be verified, so nothing is accepted.
            return Err(WebhookError::BadSignature);
        }
        webhook::verify(&self.webhook_secret, header, body, now)?;
        webhook::parse_event(body)
    }

    fn settlement_report<'a>(&'a self, tenant: u64) -> BoxFut<'a, Result<Vec<SettlementRecord>, PspError>> {
        Box::pin(async move {
            let mut out = Vec::new();
            let q = format!("metadata['h5i_tenant']:'{tenant}' AND status:'succeeded'");
            let mut page: Option<String> = None;
            loop {
                let mut query = vec![kv("query", &q), kv("limit", 100)];
                if let Some(p) = &page {
                    query.push(kv("page", p));
                }
                let (status, body) = self.get("/v1/payment_intents/search", &query).await?;
                if status != 200 {
                    return Err(rejected(&body));
                }
                for pi in body["data"].as_array().into_iter().flatten() {
                    let Some(id) = pi["metadata"]["h5i_payment"].as_str().and_then(|s| s.parse().ok()) else { continue };
                    let pref = reference(pi)?;
                    out.push(SettlementRecord {
                        tenant,
                        kind: SettlementKind::Capture,
                        id,
                        amount: pi["amount_received"].as_u64().unwrap_or(0),
                        currency: pi["currency"].as_str().and_then(currency_numeric).unwrap_or(0),
                        reference: pref.clone(),
                    });
                    let (st, refunds) = self.get("/v1/refunds", &[kv("payment_intent", &pref), kv("limit", 100)]).await?;
                    if st != 200 {
                        return Err(rejected(&refunds));
                    }
                    for r in refunds["data"].as_array().into_iter().flatten() {
                        if refund_outcome(r) != RefundOutcome::Succeeded {
                            continue;
                        }
                        let Some(rid) = r["metadata"]["h5i_refund"].as_str().and_then(|s| s.parse().ok()) else { continue };
                        out.push(SettlementRecord {
                            tenant,
                            kind: SettlementKind::Refund,
                            id: rid,
                            amount: r["amount"].as_u64().unwrap_or(0),
                            currency: r["currency"].as_str().and_then(currency_numeric).unwrap_or(0),
                            reference: pref.clone(),
                        });
                    }
                }
                match (body["has_more"].as_bool(), body["next_page"].as_str()) {
                    (Some(true), Some(p)) => page = Some(p.to_string()),
                    _ => break,
                }
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Form, Path};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[test]
    fn live_keys_are_refused() {
        assert!(StripePsp::new("sk_live_x".into(), String::new(), DEFAULT_BASE.into()).is_err());
        assert!(StripePsp::new("pk_test_x".into(), String::new(), DEFAULT_BASE.into()).is_err());
        assert!(StripePsp::new("sk_test_x".into(), String::new(), DEFAULT_BASE.into()).is_ok());
    }

    #[test]
    fn intent_statuses() {
        let o = |v: Value| intent_outcome(&v);
        assert_eq!(o(serde_json::json!({"status": "requires_capture", "amount_capturable": 5})), Outcome::Authorized { amount: 5 });
        assert_eq!(o(serde_json::json!({"status": "succeeded", "amount_received": 5})), Outcome::Captured { amount: 5 });
        assert_eq!(o(serde_json::json!({"status": "processing"})), Outcome::Pending);
        assert!(matches!(o(serde_json::json!({"status": "requires_payment_method"})), Outcome::Declined { .. }));
    }

    /// A local stand-in for the Stripe API: it records what it was sent and
    /// answers like Stripe does, so the connector's encoding and parsing are
    /// checked without network access or a key.
    #[tokio::test]
    async fn speaks_the_stripe_api() {
        type Seen = Arc<Mutex<Vec<(String, HashMap<String, String>, Option<String>)>>>;
        let seen: Seen = Arc::default();
        let s1 = seen.clone();
        let s2 = seen.clone();
        let s3 = seen.clone();
        let app = Router::new()
            .route(
                "/v1/payment_intents",
                post(move |h: HeaderMap, Form(f): Form<HashMap<String, String>>| {
                    let seen = s1.clone();
                    async move {
                        let idem = h.get("idempotency-key").map(|v| v.to_str().unwrap().to_string());
                        assert_eq!(h.get("authorization").unwrap(), "Bearer sk_test_fake");
                        seen.lock().unwrap().push(("create".into(), f.clone(), idem));
                        if f["payment_method"] == "pm_card_chargeDeclined" {
                            return (StatusCode::PAYMENT_REQUIRED, Json(serde_json::json!({"error": {
                                "type": "card_error", "code": "card_declined", "decline_code": "generic_decline",
                                "payment_intent": {"id": "pi_declined", "status": "requires_payment_method"}}})));
                        }
                        let status = if f["capture_method"] == "manual" { "requires_capture" } else { "succeeded" };
                        let amount: u64 = f["amount"].parse().unwrap();
                        (StatusCode::OK, Json(serde_json::json!({"id": "pi_123", "status": status,
                            "amount_capturable": if status == "requires_capture" { amount } else { 0 },
                            "amount_received": if status == "succeeded" { amount } else { 0 }})))
                    }
                }),
            )
            .route(
                "/v1/payment_intents/{id}/capture",
                post(move |Path(id): Path<String>, Form(f): Form<HashMap<String, String>>| {
                    let seen = s2.clone();
                    async move {
                        seen.lock().unwrap().push((format!("capture {id}"), f.clone(), None));
                        Json(serde_json::json!({"id": id, "status": "succeeded", "amount_received": f["amount_to_capture"].parse::<u64>().unwrap()}))
                    }
                }),
            )
            .route(
                "/v1/refunds",
                post(move |Form(f): Form<HashMap<String, String>>| {
                    let seen = s3.clone();
                    async move {
                        seen.lock().unwrap().push(("refund".into(), f.clone(), None));
                        Json(serde_json::json!({"id": "re_1", "status": "succeeded"}))
                    }
                })
                .get(|| async {
                    Json(serde_json::json!({"data": [{"id": "re_1", "status": "succeeded", "metadata": {"h5i_refund": "9"}}]}))
                }),
            )
            .route("/v1/payment_intents/{id}", get(|Path(id): Path<String>| async move {
                if id == "pi_missing" {
                    (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": {"type": "invalid_request_error", "code": "resource_missing"}})))
                } else {
                    (StatusCode::OK, Json(serde_json::json!({"id": id, "status": "requires_capture", "amount_capturable": 700})))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let psp = StripePsp::new("sk_test_fake".into(), "whsec_x".into(), format!("http://{addr}")).unwrap();
        let key = PaymentKey { tenant: 3, payment: 41 };
        let req = AuthorizeRequest { key, amount: 700, currency: 392, manual_capture: true, payment_method: "pm_card_visa".into() };
        let p = psp.authorize(&req, "h5i-pay-3-payment-41-authorize").await.unwrap();
        assert_eq!(p, PspPayment { reference: "pi_123".into(), outcome: Outcome::Authorized { amount: 700 } });
        {
            let seen = seen.lock().unwrap();
            let (_, f, idem) = &seen[0];
            assert_eq!(f["currency"], "jpy");
            assert_eq!(f["metadata[h5i_payment]"], "41");
            assert_eq!(f["metadata[h5i_tenant]"], "3");
            assert_eq!(idem.as_deref(), Some("h5i-pay-3-payment-41-authorize"));
        }
        let declined = AuthorizeRequest { payment_method: "pm_card_chargeDeclined".into(), ..req.clone() };
        let d = psp.authorize(&declined, "k2").await.unwrap();
        assert_eq!(d.outcome, Outcome::Declined { reason: "generic_decline".into() });
        assert_eq!(psp.capture("pi_123", 500, "k3").await.unwrap(), Outcome::Captured { amount: 500 });
        assert_eq!(psp.refund(key, "pi_123", 9, 200, "k4").await.unwrap(), RefundOutcome::Succeeded);
        assert_eq!(psp.retrieve_refund("pi_123", 9).await.unwrap(), Some(RefundOutcome::Succeeded));
        assert_eq!(psp.retrieve_refund("pi_123", 10).await.unwrap(), None);
        assert_eq!(psp.retrieve("pi_123").await.unwrap().outcome, Outcome::Authorized { amount: 700 });
        assert_eq!(psp.retrieve("pi_missing").await, Err(PspError::NotFound));
        // A raw card number never leaves this process.
        let pan = AuthorizeRequest { payment_method: "4242424242424242".into(), ..req.clone() };
        assert!(matches!(psp.authorize(&pan, "k5").await, Err(PspError::Config(_))));
        assert_eq!(seen.lock().unwrap().iter().filter(|s| s.0 == "create").count(), 2);
        // Signed webhooks verify with the configured secret only.
        let body = br#"{"id":"evt_1","type":"payment_intent.canceled","data":{"object":{"id":"pi_123","metadata":{"h5i_tenant":"3","h5i_payment":"41"}}}}"#;
        let sig = webhook::sign("whsec_x", 100, body);
        assert_eq!(psp.verify_webhook(&sig, body, 100).unwrap().result, EventResult::Canceled);
        assert!(psp.verify_webhook(&webhook::sign("whsec_y", 100, body), body, 100).is_err());
    }

    /// Against the real Stripe test API, only when a test key is configured.
    #[tokio::test]
    async fn live_test_mode_roundtrip() {
        let Ok(psp) = StripePsp::from_env() else {
            eprintln!("STRIPE_SECRET_KEY (sk_test_) not set; skipping the Stripe test-mode round trip");
            return;
        };
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
        let key = PaymentKey { tenant: 1, payment: n };
        let req = AuthorizeRequest { key, amount: 1234, currency: 392, manual_capture: true, payment_method: "pm_card_visa".into() };
        let p = psp.authorize(&req, &idempotency_key(1, "payment", n, "authorize")).await.unwrap();
        assert_eq!(p.outcome, Outcome::Authorized { amount: 1234 });
        assert_eq!(psp.capture(&p.reference, 1000, &idempotency_key(1, "payment", n, "capture")).await.unwrap(), Outcome::Captured { amount: 1000 });
        let r = psp.refund(key, &p.reference, n, 400, &idempotency_key(1, "refund", n, "refund")).await.unwrap();
        assert_ne!(r, RefundOutcome::Failed);
    }
}
