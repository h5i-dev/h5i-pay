//! Payment service provider (PSP) connectors.
//!
//! A connector turns the orchestrator's requests into PSP API calls and the
//! PSP's answers and webhooks into [`Outcome`]s and [`WebhookEvent`]s. It
//! decides nothing about a payment's state; the payment kernel does.
//!
//! Two connectors exist: `mock::MockPsp` (feature `mock`), an in-process
//! PSP with fault injection for tests, and [`stripe::StripePsp`], Stripe in
//! **test mode only** (it refuses live keys). Neither ever sees a card number: callers
//! pass PSP tokens (`pm_card_visa`, `tok_visa`) or use hosted checkout, and
//! [`check_payment_method`] rejects anything that looks like a PAN.
//!
//! Every mutating call carries an idempotency key derived from our own ids,
//! so a retry after a timeout cannot charge, capture or refund twice.

#[cfg(feature = "mock")]
pub mod mock;
pub mod stripe;
pub mod webhook;

use std::future::Future;
use std::pin::Pin;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a PSP says a payment is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Authorized for `amount`, awaiting capture.
    Authorized { amount: u64 },
    /// Captured for `amount`.
    Captured { amount: u64 },
    Declined { reason: String },
    Canceled,
    /// Not decided yet (3-D Secure, asynchronous methods): wait for a webhook.
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PspPayment {
    /// The PSP's id for the payment (Stripe: `pi_...`).
    pub reference: String,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefundOutcome {
    Succeeded,
    Failed,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PspError {
    /// No answer in time: the request may or may not have taken effect.
    Timeout,
    /// The request certainly failed before the PSP acted on it.
    Unavailable(String),
    /// The PSP refused the request itself (not a card decline).
    Rejected { code: String, message: String },
    /// Not found at the PSP.
    NotFound,
    /// Misconfiguration (missing or live key, bad currency).
    Config(String),
}

impl std::fmt::Display for PspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PspError::Timeout => write!(f, "PSP timeout (outcome unknown)"),
            PspError::Unavailable(m) => write!(f, "PSP unavailable: {m}"),
            PspError::Rejected { code, message } => write!(f, "PSP rejected the request: {code}: {message}"),
            PspError::NotFound => write!(f, "not found at the PSP"),
            PspError::Config(m) => write!(f, "PSP configuration: {m}"),
        }
    }
}

impl std::error::Error for PspError {}

/// Our identity of a payment, sent to the PSP as metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaymentKey {
    pub tenant: u64,
    pub payment: u64,
}

#[derive(Clone, Debug)]
pub struct AuthorizeRequest {
    pub key: PaymentKey,
    pub amount: u64,
    /// ISO 4217 numeric.
    pub currency: u32,
    pub manual_capture: bool,
    /// A PSP token or saved payment method id, never a card number.
    pub payment_method: String,
}

#[derive(Clone, Debug)]
pub struct CheckoutRequest {
    pub key: PaymentKey,
    pub amount: u64,
    pub currency: u32,
    pub manual_capture: bool,
    pub description: String,
    pub success_url: String,
    pub cancel_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutSession {
    pub id: String,
    pub url: String,
}

/// What a verified webhook reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventResult {
    Authorized { amount: u64 },
    Captured { amount: u64 },
    Declined,
    Canceled,
    RefundSucceeded { refund: u64 },
    RefundFailed { refund: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookEvent {
    /// The PSP's event id; deliveries of one event share it.
    pub id: String,
    pub key: PaymentKey,
    /// The PSP's payment reference, if the event names one.
    pub reference: Option<String>,
    pub result: EventResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SettlementKind {
    Capture,
    Refund,
}

/// One line of a PSP settlement report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettlementRecord {
    pub tenant: u64,
    pub kind: SettlementKind,
    /// Our payment id (captures) or refund id (refunds), from metadata.
    pub id: u64,
    pub amount: u64,
    pub currency: u32,
    pub reference: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WebhookError {
    BadSignature,
    Stale,
    Malformed(String),
    /// A well-formed event this system does not act on.
    Ignored(String),
}

impl std::fmt::Display for WebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// A payment service provider.
pub trait Psp: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Create and confirm a payment. `idempotency_key` makes a retry return
    /// the first attempt's payment instead of charging again.
    fn authorize<'a>(&'a self, req: &'a AuthorizeRequest, idempotency_key: &'a str) -> BoxFut<'a, Result<PspPayment, PspError>>;

    fn capture<'a>(&'a self, reference: &'a str, amount: u64, idempotency_key: &'a str) -> BoxFut<'a, Result<Outcome, PspError>>;

    /// Void an authorization.
    fn cancel<'a>(&'a self, reference: &'a str, idempotency_key: &'a str) -> BoxFut<'a, Result<Outcome, PspError>>;

    fn refund<'a>(
        &'a self,
        key: PaymentKey,
        reference: &'a str,
        refund: u64,
        amount: u64,
        idempotency_key: &'a str,
    ) -> BoxFut<'a, Result<RefundOutcome, PspError>>;

    /// The PSP's current view of a payment (recovery after a timeout).
    fn retrieve<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Result<PspPayment, PspError>>;

    /// Find a payment by our ids, when the reference was never recorded
    /// (the authorize call timed out).
    fn find<'a>(&'a self, key: PaymentKey) -> BoxFut<'a, Result<Option<PspPayment>, PspError>>;

    /// The PSP's view of one of our refunds, if it exists there.
    fn retrieve_refund<'a>(&'a self, reference: &'a str, refund: u64) -> BoxFut<'a, Result<Option<RefundOutcome>, PspError>>;

    /// A hosted payment page: the customer enters card details at the PSP.
    fn checkout<'a>(&'a self, req: &'a CheckoutRequest) -> BoxFut<'a, Result<CheckoutSession, PspError>>;

    /// Verify the signature and parse a webhook delivery.
    fn verify_webhook(&self, signature_header: &str, body: &[u8], now_secs: u64) -> Result<WebhookEvent, WebhookError>;

    /// Settled captures and refunds of a tenant, for reconciliation.
    fn settlement_report<'a>(&'a self, tenant: u64) -> BoxFut<'a, Result<Vec<SettlementRecord>, PspError>>;
}

/// Refuse anything that could be a raw card number (PCI DSS scope): 12 to
/// 19 digits, possibly separated by spaces or dashes, that pass Luhn.
/// Payment methods must be PSP tokens.
pub fn check_payment_method(pm: &str) -> Result<(), PspError> {
    let digits: Vec<u32> = pm.chars().filter(|c| c.is_ascii_digit()).filter_map(|c| c.to_digit(10)).collect();
    let only_card_chars = pm.chars().all(|c| c.is_ascii_digit() || c == ' ' || c == '-');
    if (only_card_chars && digits.len() >= 12) || (digits.len() >= 12 && luhn(&digits)) {
        return Err(PspError::Config(
            "raw card numbers are never accepted; pass a PSP token or use hosted checkout".into(),
        ));
    }
    if pm.is_empty() || pm.len() > 255 || !pm.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(PspError::Config("payment_method must be a PSP token like pm_card_visa".into()));
    }
    Ok(())
}

fn luhn(digits: &[u32]) -> bool {
    let mut sum = 0;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut d = *d;
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    sum % 10 == 0
}

/// ISO 4217 numeric to alphabetic, for the currencies h5i-pay supports.
pub fn currency_alpha(numeric: u32) -> Option<&'static str> {
    Some(match numeric {
        36 => "aud",
        124 => "cad",
        156 => "cny",
        344 => "hkd",
        392 => "jpy",
        410 => "krw",
        554 => "nzd",
        702 => "sgd",
        756 => "chf",
        826 => "gbp",
        840 => "usd",
        978 => "eur",
        _ => return None,
    })
}

pub fn currency_numeric(alpha: &str) -> Option<u32> {
    let a = alpha.to_ascii_lowercase();
    [36, 124, 156, 344, 392, 410, 554, 702, 756, 826, 840, 978].into_iter().find(|n| currency_alpha(*n) == Some(a.as_str()))
}

/// The idempotency key for one PSP operation on one of our objects.
pub fn idempotency_key(tenant: u64, object: &str, id: u64, op: &str) -> String {
    format!("h5i-pay-{tenant}-{object}-{id}-{op}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_numbers_are_refused() {
        for pan in ["4242424242424242", "4242 4242 4242 4242", "4242-4242-4242-4242", "5555555555554444", "tok4242424242424242"] {
            assert!(check_payment_method(pan).is_err(), "{pan}");
        }
        for ok in ["pm_card_visa", "tok_visa", "pm_1PqXyZ2eZvKYlo2C", "tok_decline"] {
            assert!(check_payment_method(ok).is_ok(), "{ok}");
        }
        assert!(check_payment_method("").is_err());
        assert!(check_payment_method("pm card").is_err());
    }

    #[test]
    fn currencies_roundtrip() {
        for n in [392, 840, 978, 826] {
            assert_eq!(currency_numeric(currency_alpha(n).unwrap()), Some(n));
        }
        assert_eq!(currency_alpha(1), None);
    }
}
