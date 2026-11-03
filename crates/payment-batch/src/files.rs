//! The batch worker's file formats, shared by `h5i-pay-batch`, the export
//! and ingest jobs in `payment-api`, and the tests.
//!
//! * Records: CSV `key,amount,currency,status`, no header, read on every
//!   thread; or the binary records file ([`crate::binfile`]).
//! * Reconciliation report: JSON `{"engine", "elapsed_ms", "internal":
//!   {code: count}, "external": {...}, "exceptions": [{"side", "code",
//!   "key", "amount"}]}`.
//! * Payout requests (bulk payout screening): CSV
//!   `merchant,recipient,amount,currency,kind`, no header; policy: JSON
//!   `{"min_amount", "max_amount": [4], "currencies": [≤ 8], "kinds_mask",
//!   "merchants": [ascending], "blocked": [≤ 64]}`.

use payment_domain::policy::{Limits, PaymentReq, MAX_CURRENCIES};
use payment_reconcile::{rules, Recon, Record, Report};
use payment_reconcile::Rows;
use std::io::Write;

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

/// Split `data` into about `parts` pieces that end at a newline (or the end).
fn line_chunks(data: &[u8], parts: usize) -> Vec<std::ops::Range<usize>> {
    let step = data.len().div_ceil(parts.max(1)).max(1 << 16);
    let mut out = Vec::new();
    let mut start = 0;
    while start < data.len() {
        let mut end = (start + step).min(data.len());
        if end < data.len() {
            end = data[end..].iter().position(|b| *b == b'\n').map_or(data.len(), |p| end + p + 1);
        }
        out.push(start..end);
        start = end;
    }
    out
}

/// Map `path` (an empty file is an empty slice).
fn map_file(path: &str) -> Res<Option<memmap2::Mmap>> {
    let f = std::fs::File::open(path)?;
    if f.metadata()?.len() == 0 {
        return Ok(None);
    }
    // SAFETY: read-only private mapping of the worker's own input file
    // (see `binfile` on truncation underneath the reader).
    Ok(Some(unsafe { memmap2::MmapOptions::new().map(&f)? }))
}

/// Parse CSV `data` (from `path`) with `row` per non-blank line, the file
/// split at line boundaries across every thread. Errors name the line.
fn parse_lines<T: Send>(path: &str, data: &[u8], row: impl Fn(&mut dyn FnMut(&str) -> Res<u64>) -> Res<T> + Sync) -> Res<Vec<T>> {
    use rayon::prelude::*;
    let chunks = line_chunks(data, rayon::current_num_threads() * 4);
    let parsed: Vec<Res<Vec<T>>> = chunks
        .par_iter()
        .map(|c| {
            let line0 = || data[..c.start].iter().filter(|b| **b == b'\n').count();
            let text = std::str::from_utf8(&data[c.clone()]).map_err(|e| format!("{path}:{}: {e}", line0() + 1))?;
            let mut out = Vec::with_capacity(text.len() / 24);
            for (n, line) in text.split('\n').enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let mut it = line.split(',').map(str::trim);
                let mut field = |name: &str| -> Res<u64> {
                    it.next().ok_or_else(|| format!("missing {name}"))?.parse::<u64>().map_err(|e| format!("{name}: {e}").into())
                };
                out.push(row(&mut field).map_err(|e| format!("{path}:{}: {e}", line0() + n + 1))?);
            }
            Ok(out)
        })
        .collect();
    let mut out = Vec::with_capacity(parsed.iter().map(|p| p.as_ref().map_or(0, Vec::len)).sum());
    for p in parsed {
        out.extend(p?);
    }
    Ok(out)
}

/// Read a records CSV (in parallel, see [`parse_lines`]).
pub fn read_records(path: &str) -> Res<Vec<Record>> {
    let Some(map) = map_file(path)? else { return Ok(Vec::new()) };
    parse_records(path, &map)
}

/// Parse records CSV `data`; `name` labels errors.
pub fn parse_records(name: &str, data: &[u8]) -> Res<Vec<Record>> {
    parse_lines(name, data, |field| {
        let key = field("key")?;
        let amount = field("amount")?;
        let currency = u32::try_from(field("currency")?)?;
        let status = u32::try_from(field("status")?)?;
        Ok(Record { key, amount, currency, status })
    })
}

/// Write a records CSV.
pub fn write_records<R: Rows + ?Sized>(path: &std::path::Path, rows: &R) -> std::io::Result<()> {
    let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    for i in 0..rows.len() {
        let r = rows.get(i);
        writeln!(w, "{},{},{},{}", r.key, r.amount, r.currency, r.status)?;
    }
    w.flush()
}

/// "00" … "99".
const DIGITS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

/// Append `v` in decimal (two digits per step).
#[inline]
fn push_u64(out: &mut Vec<u8>, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v >= 100 {
        let d = (v % 100) as usize * 2;
        v /= 100;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&DIGITS[d..d + 2]);
    }
    if v >= 10 {
        let d = v as usize * 2;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&DIGITS[d..d + 2]);
    } else {
        i -= 1;
        buf[i] = b'0' + v as u8;
    }
    out.extend_from_slice(&buf[i..]);
}

/// Write a reconciliation report.
pub fn write_report(path: &str, r: &Recon, report: &Report, engine: &str, elapsed_ms: f64) -> Res<()> {
    write_report_rows(path, &r.internal[..], &r.external[..], &report.internal, &report.external, engine, elapsed_ms)?;
    Ok(())
}

/// Write a reconciliation report from each side's rows and codes; returns
/// the number of exceptions. The exceptions are formatted on every thread,
/// in chunks, and written in row order (internal side first), so the bytes
/// are those of a sequential writer.
pub fn write_report_rows<R: Rows + ?Sized, C: Copy + Into<u32> + Sync>(
    path: &str,
    internal: &R,
    external: &R,
    codes_i: &[C],
    codes_e: &[C],
    engine: &str,
    elapsed_ms: f64,
) -> Res<u64> {
    use rayon::prelude::*;
    if codes_i.len() != internal.len() || codes_e.len() != external.len() {
        return Err("report: one code per row expected".into());
    }
    const CHUNK: usize = 1 << 16;
    type Part = Res<([u64; rules::CODES], Vec<u8>)>;
    let side = |name: &'static str, rows: &R, codes: &[C]| -> Vec<Part> {
        codes
            .par_chunks(CHUNK)
            .enumerate()
            .map(|(c, cs)| {
                let mut counts = [0u64; rules::CODES];
                let mut matched = 0u64;
                let mut out = Vec::new();
                // Most rows match: skip 16 at a time when all of them do.
                for (g, group) in cs.chunks(16).enumerate() {
                    if group.iter().fold(0u32, |a, c| a | (*c).into()) == 0 {
                        matched += group.len() as u64;
                        continue;
                    }
                    for (k, code) in group.iter().enumerate() {
                        let code: u32 = (*code).into();
                        if code == rules::MATCHED {
                            matched += 1;
                            continue;
                        }
                        let name_of = CODE_NAMES.get(code as usize).ok_or_else(|| format!("report: bad code {code}"))?;
                        counts[code as usize] += 1;
                        let i = c * CHUNK + g * 16 + k;
                        out.extend_from_slice(b",{\"side\":\"");
                        out.extend_from_slice(name.as_bytes());
                        out.extend_from_slice(b"\",\"code\":\"");
                        out.extend_from_slice(name_of.as_bytes());
                        out.extend_from_slice(b"\",\"key\":");
                        push_u64(&mut out, rows.key(i));
                        out.extend_from_slice(b",\"amount\":");
                        push_u64(&mut out, rows.amount(i));
                        out.push(b'}');
                    }
                }
                counts[rules::MATCHED as usize] = matched;
                Ok((counts, out))
            })
            .collect()
    };
    let (pi, pe) = rayon::join(|| side("internal", internal, codes_i), || side("external", external, codes_e));
    let total = |parts: &[Part]| -> Res<[u64; rules::CODES]> {
        let mut t = [0u64; rules::CODES];
        for p in parts {
            let (c, _) = p.as_ref().map_err(|e| e.to_string())?;
            for (a, b) in t.iter_mut().zip(c) {
                *a += b;
            }
        }
        Ok(t)
    };
    let (ci, ce) = (total(&pi)?, total(&pe)?);
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    let counts = |c: [u64; rules::CODES]| CODE_NAMES.iter().zip(c).map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(",");
    write!(out, "{{\"engine\":\"{engine}\",\"elapsed_ms\":{elapsed_ms:.3},\"internal\":{{{}}},\"external\":{{{}}},\"exceptions\":[", counts(ci), counts(ce))?;
    let mut first = true;
    for p in pi.iter().chain(&pe) {
        let (_, bytes) = p.as_ref().map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            continue;
        }
        // Every entry starts with a comma; drop the very first one.
        out.write_all(if first { &bytes[1..] } else { bytes })?;
        first = false;
    }
    writeln!(out, "]}}")?;
    out.flush()?;
    Ok(ci[1..].iter().chain(&ce[1..]).sum())
}

/// Read payout requests.
pub fn read_requests(path: &str) -> Res<Vec<PaymentReq>> {
    let Some(map) = map_file(path)? else { return Ok(Vec::new()) };
    parse_lines(path, &map, |field| {
        let merchant = field("merchant")?;
        let recipient = field("recipient")?;
        let amount = field("amount")?;
        let currency = u32::try_from(field("currency")?)?;
        let kind = u32::try_from(field("kind")?)?;
        Ok(PaymentReq { merchant, recipient, amount, currency, kind })
    })
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

#[cfg(test)]
mod tests {
    #[test]
    fn push_u64_is_decimal() {
        for v in [0u64, 1, 9, 10, 11, 99, 100, 101, 999, 1000, 12_345, 99_999_999, u64::MAX, u64::MAX - 1, 1 << 56, 10u64.pow(19)] {
            let mut out = b"x".to_vec();
            super::push_u64(&mut out, v);
            assert_eq!(out, format!("x{v}").into_bytes());
        }
    }
}

