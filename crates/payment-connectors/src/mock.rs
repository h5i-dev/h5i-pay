//! An in-process PSP for tests and local development.
//!
//! It behaves like a small Stripe: idempotency keys, manual or automatic
//! capture, partial capture, refunds, Stripe-shaped signed webhooks and a
//! settlement report. Faults can be injected: fail a call before it takes
//! effect, or take effect and lose the response (a timeout), deliver
//! webhooks twice or out of order, and tamper with settlement lines.
//!
//! Test tokens: `tok_visa` succeeds, `tok_decline` is declined,
//! `tok_pending` stays pending until [`MockPsp::complete`], and
//! `tok_refund_fail` makes every refund of the payment fail.

use crate::webhook;
use crate::*;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug)]
struct MockPayment {
    key: PaymentKey,
    amount: u64,
    currency: u32,
    status: Outcome,
    refunds_fail: bool,
    refunds: Vec<(u64, u64, RefundOutcome)>,
}

#[derive(Clone, Debug)]
enum Cached {
    Payment(PspPayment),
    Outcome(Outcome),
    Refund(RefundOutcome),
}

#[derive(Default)]
struct State {
    next: u64,
    payments: HashMap<String, MockPayment>,
    idem: HashMap<String, Cached>,
    deliveries: Vec<(String, Vec<u8>)>,
    settlement: Vec<SettlementRecord>,
    fail_next: u32,
    timeout_next: u32,
    calls: u64,
    now: u64,
}

pub struct MockPsp {
    secret: String,
    state: Mutex<State>,
}

/// One signed webhook delivery: the `Stripe-Signature` header and body.
#[derive(Clone, Debug)]
pub struct Delivery {
    pub signature: String,
    pub body: Vec<u8>,
}

impl MockPsp {
    pub fn new(webhook_secret: impl Into<String>) -> Self {
        MockPsp { secret: webhook_secret.into(), state: Mutex::new(State { now: 1_700_000_000, ..State::default() }) }
    }

    pub fn webhook_secret(&self) -> &str {
        &self.secret
    }

    /// The next `n` mutating calls fail before they take effect.
    pub fn fail_next(&self, n: u32) {
        self.state.lock().unwrap().fail_next = n;
    }

    /// The next `n` mutating calls take effect, then time out.
    pub fn timeout_next(&self, n: u32) {
        self.state.lock().unwrap().timeout_next = n;
    }

    /// Mutating calls received, including failed and repeated ones.
    pub fn calls(&self) -> u64 {
        self.state.lock().unwrap().calls
    }

    /// The webhook clock (unix seconds) the mock signs with.
    pub fn set_now(&self, secs: u64) {
        self.state.lock().unwrap().now = secs;
    }

    pub fn now(&self) -> u64 {
        self.state.lock().unwrap().now
    }

    /// Webhook deliveries produced so far, oldest first; drains the queue.
    pub fn take_deliveries(&self) -> Vec<Delivery> {
        let mut st = self.state.lock().unwrap();
        let now = st.now;
        std::mem::take(&mut st.deliveries)
            .into_iter()
            .map(|(_, body)| Delivery { signature: webhook::sign(&self.secret, now, &body), body })
            .collect()
    }

    /// Finish a `tok_pending` payment with `outcome`.
    pub fn complete(&self, reference: &str, outcome: Outcome) {
        let mut st = self.state.lock().unwrap();
        let p = st.payments.get_mut(reference).expect("known reference");
        p.status = outcome.clone();
        let p = p.clone();
        Self::emit_payment(&mut st, reference, &p);
        if let Outcome::Captured { amount } = outcome {
            st.settlement.push(SettlementRecord {
                tenant: p.key.tenant,
                kind: SettlementKind::Capture,
                id: p.key.payment,
                amount,
                currency: p.currency,
                reference: reference.to_string(),
            });
        }
    }

    /// Change the settlement report, to test reconciliation.
    pub fn tamper_settlement(&self, f: impl FnOnce(&mut Vec<SettlementRecord>)) {
        f(&mut self.state.lock().unwrap().settlement);
    }

    pub fn status(&self, reference: &str) -> Option<Outcome> {
        self.state.lock().unwrap().payments.get(reference).map(|p| p.status.clone())
    }

    /// Count, fail or time out a mutating call. `Err` before the effect.
    fn enter(st: &mut State) -> Result<bool, PspError> {
        st.calls += 1;
        if st.fail_next > 0 {
            st.fail_next -= 1;
            return Err(PspError::Unavailable("injected failure".into()));
        }
        if st.timeout_next > 0 {
            st.timeout_next -= 1;
            return Ok(true);
        }
        Ok(false)
    }

    fn emit(st: &mut State, ty: &str, object: serde_json::Value) {
        st.next += 1;
        let id = format!("evt_mock_{}", st.next);
        let body = serde_json::json!({"id": id, "type": ty, "data": {"object": object}});
        st.deliveries.push((id, body.to_string().into_bytes()));
    }

    fn meta(key: PaymentKey) -> serde_json::Value {
        serde_json::json!({"h5i_tenant": key.tenant.to_string(), "h5i_payment": key.payment.to_string()})
    }

    fn emit_payment(st: &mut State, reference: &str, p: &MockPayment) {
        let (ty, capturable, received) = match p.status {
            Outcome::Authorized { amount } => ("payment_intent.amount_capturable_updated", amount, 0),
            Outcome::Captured { amount } => ("payment_intent.succeeded", 0, amount),
            Outcome::Declined { .. } => ("payment_intent.payment_failed", 0, 0),
            Outcome::Canceled => ("payment_intent.canceled", 0, 0),
            Outcome::Pending => return,
        };
        let obj = serde_json::json!({
            "id": reference, "object": "payment_intent", "amount": p.amount,
            "amount_capturable": capturable, "amount_received": received,
            "metadata": Self::meta(p.key),
        });
        Self::emit(st, ty, obj);
    }

    fn emit_refund(st: &mut State, reference: &str, p: &MockPayment, refund: u64, outcome: RefundOutcome) {
        let status = match outcome {
            RefundOutcome::Succeeded => "succeeded",
            RefundOutcome::Failed => "failed",
            RefundOutcome::Pending => "pending",
        };
        let mut meta = Self::meta(p.key);
        meta["h5i_refund"] = serde_json::Value::String(refund.to_string());
        let obj = serde_json::json!({"id": format!("re_mock_{refund}"), "object": "refund", "payment_intent": reference, "status": status, "metadata": meta});
        Self::emit(st, "refund.updated", obj);
    }

    fn authorize_now(&self, req: &AuthorizeRequest, key: &str) -> Result<PspPayment, PspError> {
        check_payment_method(&req.payment_method)?;
        if currency_alpha(req.currency).is_none() {
            return Err(PspError::Rejected { code: "invalid_currency".into(), message: format!("{}", req.currency) });
        }
        let mut st = self.state.lock().unwrap();
        if let Some(Cached::Payment(p)) = st.idem.get(key) {
            return Ok(p.clone());
        }
        let lost = Self::enter(&mut st)?;
        st.next += 1;
        let reference = format!("pi_mock_{}", st.next);
        let status = match req.payment_method.as_str() {
            "tok_decline" => Outcome::Declined { reason: "card_declined".into() },
            "tok_pending" => Outcome::Pending,
            _ if req.manual_capture => Outcome::Authorized { amount: req.amount },
            _ => Outcome::Captured { amount: req.amount },
        };
        let p = MockPayment {
            key: req.key,
            amount: req.amount,
            currency: req.currency,
            status: status.clone(),
            refunds_fail: req.payment_method == "tok_refund_fail",
            refunds: Vec::new(),
        };
        if let Outcome::Captured { amount } = status {
            st.settlement.push(SettlementRecord {
                tenant: req.key.tenant,
                kind: SettlementKind::Capture,
                id: req.key.payment,
                amount,
                currency: req.currency,
                reference: reference.clone(),
            });
        }
        Self::emit_payment(&mut st, &reference, &p);
        st.payments.insert(reference.clone(), p);
        let out = PspPayment { reference, outcome: status };
        st.idem.insert(key.to_string(), Cached::Payment(out.clone()));
        if lost { Err(PspError::Timeout) } else { Ok(out) }
    }

    fn capture_now(&self, reference: &str, amount: u64, key: &str) -> Result<Outcome, PspError> {
        let mut st = self.state.lock().unwrap();
        if let Some(Cached::Outcome(o)) = st.idem.get(key) {
            return Ok(o.clone());
        }
        let lost = Self::enter(&mut st)?;
        let p = st.payments.get_mut(reference).ok_or(PspError::NotFound)?;
        let out = match p.status {
            Outcome::Authorized { amount: auth } if amount > 0 && amount <= auth => {
                p.status = Outcome::Captured { amount };
                p.status.clone()
            }
            Outcome::Authorized { .. } => {
                return Err(PspError::Rejected { code: "amount_too_large".into(), message: format!("{amount}") });
            }
            _ => return Err(PspError::Rejected { code: "payment_intent_unexpected_state".into(), message: format!("{:?}", p.status) }),
        };
        let p = p.clone();
        st.settlement.push(SettlementRecord {
            tenant: p.key.tenant,
            kind: SettlementKind::Capture,
            id: p.key.payment,
            amount,
            currency: p.currency,
            reference: reference.to_string(),
        });
        Self::emit_payment(&mut st, reference, &p);
        st.idem.insert(key.to_string(), Cached::Outcome(out.clone()));
        if lost { Err(PspError::Timeout) } else { Ok(out) }
    }

    fn cancel_now(&self, reference: &str, key: &str) -> Result<Outcome, PspError> {
        let mut st = self.state.lock().unwrap();
        if let Some(Cached::Outcome(o)) = st.idem.get(key) {
            return Ok(o.clone());
        }
        let lost = Self::enter(&mut st)?;
        let p = st.payments.get_mut(reference).ok_or(PspError::NotFound)?;
        match p.status {
            Outcome::Authorized { .. } | Outcome::Pending => p.status = Outcome::Canceled,
            Outcome::Canceled => {}
            _ => return Err(PspError::Rejected { code: "payment_intent_unexpected_state".into(), message: format!("{:?}", p.status) }),
        }
        let p = p.clone();
        Self::emit_payment(&mut st, reference, &p);
        st.idem.insert(key.to_string(), Cached::Outcome(Outcome::Canceled));
        if lost { Err(PspError::Timeout) } else { Ok(Outcome::Canceled) }
    }

    fn refund_now(&self, reference: &str, refund: u64, amount: u64, key: &str) -> Result<RefundOutcome, PspError> {
        let mut st = self.state.lock().unwrap();
        if let Some(Cached::Refund(o)) = st.idem.get(key) {
            return Ok(*o);
        }
        let lost = Self::enter(&mut st)?;
        let p = st.payments.get_mut(reference).ok_or(PspError::NotFound)?;
        let Outcome::Captured { amount: captured } = p.status else {
            return Err(PspError::Rejected { code: "charge_not_captured".into(), message: String::new() });
        };
        let done: u64 = p.refunds.iter().filter(|r| r.2 == RefundOutcome::Succeeded).map(|r| r.1).sum();
        let outcome = if p.refunds_fail || done + amount > captured { RefundOutcome::Failed } else { RefundOutcome::Succeeded };
        p.refunds.push((refund, amount, outcome));
        let p = p.clone();
        if outcome == RefundOutcome::Succeeded {
            st.settlement.push(SettlementRecord {
                tenant: p.key.tenant,
                kind: SettlementKind::Refund,
                id: refund,
                amount,
                currency: p.currency,
                reference: reference.to_string(),
            });
        }
        Self::emit_refund(&mut st, reference, &p, refund, outcome);
        st.idem.insert(key.to_string(), Cached::Refund(outcome));
        if lost { Err(PspError::Timeout) } else { Ok(outcome) }
    }
}

impl Psp for MockPsp {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn authorize<'a>(&'a self, req: &'a AuthorizeRequest, key: &'a str) -> BoxFut<'a, Result<PspPayment, PspError>> {
        Box::pin(async move { self.authorize_now(req, key) })
    }

    fn capture<'a>(&'a self, reference: &'a str, amount: u64, key: &'a str) -> BoxFut<'a, Result<Outcome, PspError>> {
        Box::pin(async move { self.capture_now(reference, amount, key) })
    }

    fn cancel<'a>(&'a self, reference: &'a str, key: &'a str) -> BoxFut<'a, Result<Outcome, PspError>> {
        Box::pin(async move { self.cancel_now(reference, key) })
    }

    fn refund<'a>(&'a self, _key: PaymentKey, reference: &'a str, refund: u64, amount: u64, idem: &'a str) -> BoxFut<'a, Result<RefundOutcome, PspError>> {
        Box::pin(async move { self.refund_now(reference, refund, amount, idem) })
    }

    fn retrieve<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Result<PspPayment, PspError>> {
        Box::pin(async move {
            let st = self.state.lock().unwrap();
            let p = st.payments.get(reference).ok_or(PspError::NotFound)?;
            Ok(PspPayment { reference: reference.to_string(), outcome: p.status.clone() })
        })
    }

    fn find<'a>(&'a self, key: PaymentKey) -> BoxFut<'a, Result<Option<PspPayment>, PspError>> {
        Box::pin(async move {
            let st = self.state.lock().unwrap();
            Ok(st
                .payments
                .iter()
                .find(|(_, p)| p.key == key)
                .map(|(r, p)| PspPayment { reference: r.clone(), outcome: p.status.clone() }))
        })
    }

    fn retrieve_refund<'a>(&'a self, reference: &'a str, refund: u64) -> BoxFut<'a, Result<Option<RefundOutcome>, PspError>> {
        Box::pin(async move {
            let st = self.state.lock().unwrap();
            let p = st.payments.get(reference).ok_or(PspError::NotFound)?;
            Ok(p.refunds.iter().find(|r| r.0 == refund).map(|r| r.2))
        })
    }

    fn checkout<'a>(&'a self, req: &'a CheckoutRequest) -> BoxFut<'a, Result<CheckoutSession, PspError>> {
        Box::pin(async move {
            let id = format!("cs_mock_{}_{}", req.key.tenant, req.key.payment);
            Ok(CheckoutSession { url: format!("https://mock-psp.invalid/checkout/{id}"), id })
        })
    }

    fn verify_webhook(&self, header: &str, body: &[u8], now: u64) -> Result<WebhookEvent, WebhookError> {
        webhook::verify(&self.secret, header, body, now)?;
        webhook::parse_event(body)
    }

    fn settlement_report<'a>(&'a self, tenant: u64) -> BoxFut<'a, Result<Vec<SettlementRecord>, PspError>> {
        Box::pin(async move {
            Ok(self.state.lock().unwrap().settlement.iter().filter(|r| r.tenant == tenant).cloned().collect())
        })
    }

    /// One period's lines, selected in memory.
    fn settlement_report_range<'a>(&'a self, tenant: u64, from: u64, to: u64) -> BoxFut<'a, Result<Vec<SettlementRecord>, PspError>> {
        Box::pin(async move {
            let st = self.state.lock().unwrap();
            Ok(st.settlement.iter().filter(|r| r.tenant == tenant && from <= r.id && r.id < to).cloned().collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(payment: u64, pm: &str, manual: bool) -> AuthorizeRequest {
        AuthorizeRequest { key: PaymentKey { tenant: 1, payment }, amount: 1000, currency: 392, manual_capture: manual, payment_method: pm.into() }
    }

    #[tokio::test]
    async fn idempotent_calls_and_lost_responses() {
        let m = MockPsp::new("whsec");
        m.timeout_next(1);
        let r = req(1, "tok_visa", true);
        assert_eq!(m.authorize(&r, "k1").await, Err(PspError::Timeout));
        // The retry with the same key sees the first attempt's payment.
        let p = m.authorize(&r, "k1").await.unwrap();
        assert_eq!(p.outcome, Outcome::Authorized { amount: 1000 });
        assert_eq!(m.find(r.key).await.unwrap().unwrap().reference, p.reference);
        m.fail_next(1);
        assert!(matches!(m.capture(&p.reference, 1000, "c1").await, Err(PspError::Unavailable(_))));
        assert_eq!(m.status(&p.reference), Some(Outcome::Authorized { amount: 1000 }));
        assert_eq!(m.capture(&p.reference, 600, "c1").await, Ok(Outcome::Captured { amount: 600 }));
        assert_eq!(m.capture(&p.reference, 600, "c1").await, Ok(Outcome::Captured { amount: 600 }));
        assert_eq!(m.refund(r.key, &p.reference, 5, 600, "r5").await, Ok(RefundOutcome::Succeeded));
        assert_eq!(m.refund(r.key, &p.reference, 6, 1, "r6").await, Ok(RefundOutcome::Failed));
        let report = m.settlement_report(1).await.unwrap();
        assert_eq!(report.len(), 2);
        let ds = m.take_deliveries();
        assert!(ds.len() >= 3);
        for d in &ds {
            m.verify_webhook(&d.signature, &d.body, m.now()).unwrap();
        }
        let tampered = format!("{}x", String::from_utf8_lossy(&ds[0].body));
        assert_eq!(m.verify_webhook(&ds[0].signature, tampered.as_bytes(), m.now()), Err(WebhookError::BadSignature));
        assert!(m.authorize(&req(2, "4242424242424242", false), "k2").await.is_err());
    }
}
