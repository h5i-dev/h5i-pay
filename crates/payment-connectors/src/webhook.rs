//! Webhook signatures (Stripe's scheme) and the Stripe-shaped event parser
//! shared by the Stripe connector and the Mock PSP.
//!
//! A delivery carries `Stripe-Signature: t=<unix secs>,v1=<hex>` where the
//! hex is HMAC-SHA256(secret, "<t>.<body>"). A delivery is accepted only if
//! some `v1` matches (constant-time) and `t` is within the tolerance, which
//! bounds replay of a captured delivery. Deduplication of accepted events is
//! the payment kernel's job (`Command::Event`).

use crate::{EventResult, PaymentKey, WebhookError, WebhookEvent};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Seconds a signed delivery stays valid (Stripe's default).
pub const TOLERANCE_SECS: u64 = 300;

fn mac(secret: &[u8], t: u64, body: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes any key length");
    m.update(t.to_string().as_bytes());
    m.update(b".");
    m.update(body);
    m.finalize().into_bytes().to_vec()
}

/// The signature header for `body` at time `t`.
pub fn sign(secret: &str, t: u64, body: &[u8]) -> String {
    format!("t={t},v1={}", hex::encode(mac(secret.as_bytes(), t, body)))
}

/// Check a signature header against `body`.
pub fn verify(secret: &str, header: &str, body: &[u8], now_secs: u64) -> Result<(), WebhookError> {
    let mut t: Option<u64> = None;
    let mut sigs: Vec<Vec<u8>> = Vec::new();
    for part in header.split(',') {
        let Some((k, v)) = part.trim().split_once('=') else { continue };
        match k {
            "t" => t = v.parse().ok(),
            "v1" => {
                if let Ok(b) = hex::decode(v) {
                    sigs.push(b);
                }
            }
            _ => {}
        }
    }
    let t = t.ok_or(WebhookError::BadSignature)?;
    let expected = mac(secret.as_bytes(), t, body);
    if !sigs.iter().any(|s| ct_eq(s, &expected)) {
        return Err(WebhookError::BadSignature);
    }
    if now_secs.abs_diff(t) > TOLERANCE_SECS {
        return Err(WebhookError::Stale);
    }
    Ok(())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn meta_u64(obj: &serde_json::Value, key: &str) -> Option<u64> {
    obj.get("metadata")?.get(key)?.as_str()?.parse().ok()
}

/// Parse a verified Stripe-shaped event into what the payment kernel needs.
pub fn parse_event(body: &[u8]) -> Result<WebhookEvent, WebhookError> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| WebhookError::Malformed(e.to_string()))?;
    let id = v.get("id").and_then(|x| x.as_str()).ok_or_else(|| WebhookError::Malformed("no id".into()))?.to_string();
    let ty = v.get("type").and_then(|x| x.as_str()).ok_or_else(|| WebhookError::Malformed("no type".into()))?;
    let obj = v.get("data").and_then(|d| d.get("object")).ok_or_else(|| WebhookError::Malformed("no data.object".into()))?;
    let amount = |k: &str| obj.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let missing = || WebhookError::Ignored(format!("{ty} without h5i metadata"));
    let tenant = meta_u64(obj, "h5i_tenant").ok_or_else(missing)?;
    let payment = meta_u64(obj, "h5i_payment").ok_or_else(missing)?;
    let refund_id = || meta_u64(obj, "h5i_refund").ok_or_else(missing);
    let status = obj.get("status").and_then(|x| x.as_str()).unwrap_or("");
    let result = match ty {
        "payment_intent.amount_capturable_updated" => EventResult::Authorized { amount: amount("amount_capturable") },
        "payment_intent.succeeded" => EventResult::Captured { amount: amount("amount_received") },
        "payment_intent.payment_failed" => EventResult::Declined,
        "payment_intent.canceled" => EventResult::Canceled,
        "refund.created" | "refund.updated" | "refund.failed" => match status {
            "succeeded" => EventResult::RefundSucceeded { refund: refund_id()? },
            "failed" | "canceled" => EventResult::RefundFailed { refund: refund_id()? },
            _ => return Err(WebhookError::Ignored(format!("{ty} with status {status}"))),
        },
        other => return Err(WebhookError::Ignored(other.to_string())),
    };
    let reference = match ty {
        t if t.starts_with("payment_intent.") => obj.get("id").and_then(|x| x.as_str()).map(String::from),
        _ => obj.get("payment_intent").and_then(|x| x.as_str()).map(String::from),
    };
    Ok(WebhookEvent { id, key: PaymentKey { tenant, payment }, reference, result })
}

/// The 64-bit id the payment kernel deduplicates on: the first 8 bytes of
/// SHA-256 of the PSP's name and event id. Two different events colliding
/// needs about 2^32 events of one tenant before it becomes likely.
pub fn event_id(psp: &str, event: &str) -> u64 {
    use sha2::Digest;
    let d = Sha256::digest(format!("{psp}\0{event}").as_bytes());
    u64::from_le_bytes(d[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let body = br#"{"id":"evt_1"}"#;
        let h = sign("whsec_test", 1000, body);
        assert_eq!(verify("whsec_test", &h, body, 1000), Ok(()));
        assert_eq!(verify("whsec_test", &h, body, 1000 + TOLERANCE_SECS + 1), Err(WebhookError::Stale));
        assert_eq!(verify("whsec_other", &h, body, 1000), Err(WebhookError::BadSignature));
        assert_eq!(verify("whsec_test", &h, br#"{"id":"evt_2"}"#, 1000), Err(WebhookError::BadSignature));
        assert_eq!(verify("whsec_test", "garbage", body, 1000), Err(WebhookError::BadSignature));
        // Several v1 entries (secret rotation): any match is enough.
        let two = format!("{},v1=00ff", h);
        assert_eq!(verify("whsec_test", &two, body, 1000), Ok(()));
    }

    #[test]
    fn events_parse() {
        let body = serde_json::json!({
            "id": "evt_9", "type": "payment_intent.succeeded",
            "data": {"object": {"id": "pi_1", "amount_received": 1200, "status": "succeeded",
                     "metadata": {"h5i_tenant": "4", "h5i_payment": "17"}}}
        });
        let e = parse_event(body.to_string().as_bytes()).unwrap();
        assert_eq!(e.key, PaymentKey { tenant: 4, payment: 17 });
        assert_eq!(e.result, EventResult::Captured { amount: 1200 });
        assert_eq!(e.reference.as_deref(), Some("pi_1"));
        let refund = serde_json::json!({
            "id": "evt_10", "type": "refund.updated",
            "data": {"object": {"id": "re_1", "payment_intent": "pi_1", "status": "failed",
                     "metadata": {"h5i_tenant": "4", "h5i_payment": "17", "h5i_refund": "30"}}}
        });
        assert_eq!(parse_event(refund.to_string().as_bytes()).unwrap().result, EventResult::RefundFailed { refund: 30 });
        let other = serde_json::json!({"id": "evt_11", "type": "customer.created", "data": {"object": {"metadata": {"h5i_tenant": "1", "h5i_payment": "1"}}}});
        assert!(matches!(parse_event(other.to_string().as_bytes()), Err(WebhookError::Ignored(_))));
        assert_ne!(event_id("stripe", "evt_1"), event_id("mock", "evt_1"));
    }
}
