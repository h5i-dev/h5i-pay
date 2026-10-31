//! Building reconciliation inputs from h5i-pay's own records.

use crate::rules::{key, Record, KIND_CAPTURE, KIND_REFUND};
use payment_domain as d;

pub const SETTLED: u32 = 0;
pub const PENDING: u32 = 1;

/// What the payment kernel says should appear in the PSP's settlement
/// report: every captured payment (its captured amount) and every refund
/// that succeeded, plus pending refunds (so a PSP that already settled one
/// shows up as a status mismatch, not as an extra line).
pub fn internal_records(s: &d::Snapshot) -> Vec<Record> {
    let mut out = Vec::new();
    for p in &s.payments {
        if p.status == d::CAPTURED {
            out.push(Record { key: key(KIND_CAPTURE, p.id), amount: p.captured, currency: p.currency, status: SETTLED });
        }
    }
    for r in &s.refunds {
        let status = match r.status {
            d::REFUND_SUCCEEDED => SETTLED,
            d::REFUND_PENDING => PENDING,
            _ => continue,
        };
        let currency = s.payments.iter().find(|p| p.id == r.payment).map_or(0, |p| p.currency);
        out.push(Record { key: key(KIND_REFUND, r.id), amount: r.amount, currency, status });
    }
    out
}
