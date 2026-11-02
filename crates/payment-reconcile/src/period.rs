//! Reconciliation by period (G6).
//!
//! A period is a range of ids, `from ≤ id < to`: the payment id of a
//! capture's key, the refund id of a refund's (`rules::key`). Payments and
//! refunds take their ids from one counter in the order they are created,
//! so a period is the money that entered the system between two counter
//! values. Every code depends only on the rows that share a key, and a key
//! is in exactly one period, so the result for a period is the whole-tenant
//! result restricted to it: `payment_reconcile.Period.join_period` proves
//! it of the single-thread join for any set of keys, and the other engines
//! are checked against that one.

use crate::rules::Record;
use crate::Recon;

/// The id a key names: its payment or refund.
pub fn id_of(key: u64) -> u64 {
    key & ((1 << 56) - 1)
}

/// Is `key`'s id in `[from, to)`?
pub fn contains(from: u64, to: u64, key: u64) -> bool {
    let id = id_of(key);
    from <= id && id < to
}

/// The rows of `rows` whose key is in the period.
pub fn restrict_rows(rows: &[Record], from: u64, to: u64) -> Vec<Record> {
    rows.iter().copied().filter(|r| contains(from, to, r.key)).collect()
}

/// Both sides of `r` restricted to the period.
pub fn restrict(r: &Recon, from: u64, to: u64) -> Recon {
    Recon { internal: restrict_rows(&r.internal, from, to), external: restrict_rows(&r.external, from, to) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{key, KIND_CAPTURE, KIND_REFUND};
    use crate::Engine;

    /// Random rows over few keys (so duplicates and mismatches are common):
    /// every engine on a random period gives each of its rows the code
    /// the whole run gives that row.
    #[test]
    fn a_period_is_the_whole_run_restricted() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for _ in 0..200 {
            let rows = |n: u64, next: &mut dyn FnMut(u64) -> u64| -> Vec<Record> {
                (0..n)
                    .map(|_| Record {
                        key: key(if next(2) == 0 { KIND_CAPTURE } else { KIND_REFUND }, next(40)),
                        amount: 100 + next(3),
                        currency: 392 + next(2) as u32,
                        status: next(2) as u32,
                    })
                    .collect()
            };
            let (ni, ne) = (next(60), next(60));
            let whole = Recon { internal: rows(ni, &mut next), external: rows(ne, &mut next) };
            let from = next(40);
            let to = from + next(41 - from);
            let part = restrict(&whole, from, to);
            let all = Engine::Reference.run(&whole).unwrap();
            let code_of = |rows: &[Record], codes: &[u32], r: &Record| codes[rows.iter().position(|y| y == r).unwrap()];
            for e in [Engine::Reference, Engine::Single, Engine::Parallel, Engine::Simd] {
                let rep = e.run(&part).unwrap();
                for (i, r) in part.internal.iter().enumerate() {
                    assert_eq!(rep.internal[i], code_of(&whole.internal, &all.internal, r), "{e:?} internal {r:?}");
                }
                for (j, r) in part.external.iter().enumerate() {
                    assert_eq!(rep.external[j], code_of(&whole.external, &all.external, r), "{e:?} external {r:?}");
                }
            }
        }
    }
}
