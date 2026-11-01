//! The batch worker's file formats, shared by `h5i-pay-batch`, the export
//! and ingest jobs in `payment-api`, and the tests.
//!
//! * Records: CSV `key,amount,currency,status`, no header.
//! * Reconciliation report: JSON `{"engine", "elapsed_ms", "internal":
//!   {code: count}, "external": {...}, "exceptions": [{"side", "code",
//!   "key", "amount"}]}`.
//! * Payout requests (bulk payout screening): CSV
//!   `merchant,recipient,amount,currency,kind`, no header; policy: JSON
//!   `{"min_amount", "max_amount": [4], "currencies": [≤ 8], "kinds_mask",
//!   "merchants": [ascending], "blocked": [≤ 64]}`.

use payment_domain::policy::{Limits, PaymentReq, MAX_CURRENCIES};
use payment_reconcile::{rules, Recon, Record, Report};
use std::io::{BufRead, Write};

pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub const CODE_NAMES: [&str; rules::CODES] =
    ["matched", "amount_mismatch", "currency_mismatch", "status_mismatch", "missing_external", "duplicate_external", "duplicate_internal", "extra_external"];

pub const POLICY_NAMES: [&str; 11] = [
    "approved",
    "zero_amount",
    "below_minimum",
    "unknown_kind",
    "kind_not_allowed",
    "above_limit",
    "currency_not_allowed",
    "merchant_not_allowed",
    "self_payment",
    "recipient_blocked",
    "bad_policy",
];

fn fields<'a>(path: &'a str, n: usize, line: &'a str) -> impl FnMut(&str) -> Res<u64> + 'a {
    let mut it = line.split(',').map(str::trim);
    move |name: &str| -> Res<u64> {
        it.next().ok_or_else(|| format!("{path}:{}: missing {name}", n + 1))?.parse::<u64>().map_err(|e| format!("{path}:{}: {name}: {e}", n + 1).into())
    }
}

/// Read a records CSV.
pub fn read_records(path: &str) -> Res<Vec<Record>> {
    let f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut out = Vec::new();
    for (n, line) in f.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let mut field = fields(path, n, &line);
        let key = field("key")?;
        let amount = field("amount")?;
        let currency = u32::try_from(field("currency")?)?;
        let status = u32::try_from(field("status")?)?;
        out.push(Record { key, amount, currency, status });
    }
    Ok(out)
}

/// Write a records CSV.
pub fn write_records(path: &std::path::Path, rows: &[Record]) -> std::io::Result<()> {
    let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
    for r in rows {
        writeln!(w, "{},{},{},{}", r.key, r.amount, r.currency, r.status)?;
    }
    w.flush()
}

/// Write a reconciliation report.
pub fn write_report(path: &str, r: &Recon, report: &Report, engine: &str, elapsed_ms: f64) -> Res<()> {
    let (ci, ce) = report.counts();
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    let counts = |c: [u64; rules::CODES]| CODE_NAMES.iter().zip(c).map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(",");
    write!(out, "{{\"engine\":\"{engine}\",\"elapsed_ms\":{elapsed_ms:.3},\"internal\":{{{}}},\"external\":{{{}}},\"exceptions\":[", counts(ci), counts(ce))?;
    let mut first = true;
    let mut emit = |side: &str, rec: &Record, code: u32| -> Res<()> {
        if !first {
            write!(out, ",")?;
        }
        first = false;
        write!(out, "{{\"side\":\"{side}\",\"code\":\"{}\",\"key\":{},\"amount\":{}}}", CODE_NAMES[code as usize], rec.key, rec.amount)?;
        Ok(())
    };
    for (x, c) in r.internal.iter().zip(&report.internal) {
        if *c != rules::MATCHED {
            emit("internal", x, *c)?;
        }
    }
    for (x, c) in r.external.iter().zip(&report.external) {
        if *c != rules::MATCHED {
            emit("external", x, *c)?;
        }
    }
    writeln!(out, "]}}")?;
    out.flush()?;
    Ok(())
}

/// Read payout requests.
pub fn read_requests(path: &str) -> Res<Vec<PaymentReq>> {
    let f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut out = Vec::new();
    for (n, line) in f.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let mut field = fields(path, n, &line);
        let merchant = field("merchant")?;
        let recipient = field("recipient")?;
        let amount = field("amount")?;
        let currency = u32::try_from(field("currency")?)?;
        let kind = u32::try_from(field("kind")?)?;
        out.push(PaymentReq { merchant, recipient, amount, currency, kind });
    }
    Ok(out)
}

/// Read a policy file into a batch's limits and lists (requests empty).
pub fn read_policy(path: &str) -> Res<crate::PolicyBatch> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let nums = |k: &str| -> Res<Vec<u64>> {
        v[k].as_array().ok_or_else(|| format!("{path}: {k} must be an array"))?.iter().map(|x| x.as_u64().ok_or_else(|| format!("{path}: {k}: not a number").into())).collect()
    };
    let max = nums("max_amount")?;
    let currencies = nums("currencies")?;
    if max.len() != payment_domain::policy::KINDS || currencies.len() > MAX_CURRENCIES {
        return Err(format!("{path}: max_amount needs {} entries, currencies at most {MAX_CURRENCIES}", payment_domain::policy::KINDS).into());
    }
    let mut limits = Limits {
        min_amount: v["min_amount"].as_u64().ok_or_else(|| format!("{path}: min_amount"))?,
        n_currencies: currencies.len() as u32,
        kinds_mask: u32::try_from(v["kinds_mask"].as_u64().ok_or_else(|| format!("{path}: kinds_mask"))?)?,
        ..Default::default()
    };
    limits.max_amount.copy_from_slice(&max);
    for (i, c) in currencies.iter().enumerate() {
        limits.currencies[i] = u32::try_from(*c)?;
    }
    let blocked = nums("blocked")?;
    Ok(crate::PolicyBatch { limits, merchants: nums("merchants")?, n_blocked: blocked.len(), blocked, requests: Vec::new() })
}
