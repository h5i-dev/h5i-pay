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
    records(&s.payments, &s.refunds, &[])
}

/// [`internal_records`] of the given rows: captures of `payments`, then
/// `refunds`, each refund in its payment's currency (found among
/// `payments` and `more`, 0 if neither has it). A `Range` page passes its
/// payments, its refunds and the refunds' payments (G6).
pub fn records(payments: &[d::Payment], refunds: &[d::Refund], more: &[d::Payment]) -> Vec<Record> {
    let mut out = Vec::with_capacity(payments.len() + refunds.len());
    // A refund's currency is its payment's: one map, not a scan per refund.
    let currency: std::collections::HashMap<u64, u32> = payments.iter().chain(more).map(|p| (p.id, p.currency)).collect();
    for p in payments {
        if p.status == d::CAPTURED {
            out.push(Record { key: key(KIND_CAPTURE, p.id), amount: p.captured, currency: p.currency, status: SETTLED });
        }
    }
    for r in refunds {
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
