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
    let mut out = Vec::with_capacity(s.payments.len() + s.refunds.len());
    // A refund's currency is its payment's: one map, not a scan per refund.
    let currency: std::collections::HashMap<u64, u32> = s.payments.iter().map(|p| (p.id, p.currency)).collect();
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
        let currency = currency.get(&r.payment).copied().unwrap_or(0);
        out.push(Record { key: key(KIND_REFUND, r.id), amount: r.amount, currency, status });
    }
    out
}
