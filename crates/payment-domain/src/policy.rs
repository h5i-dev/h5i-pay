//! Stateless batch policy rules for payment requests.
//!
//! One pure function, [`check`], decides each request independently of every
//! other row, so a batch can be evaluated on one CPU thread, many threads,
//! SIMD lanes or a GPU with identical results. It uses only `core` (no
//! allocation), so a device build can include this file verbatim.
//!
//! These checks are necessary, not sufficient: a request that passes is a
//! *candidate*. Anything that depends on shared state (balances, cumulative
//! budgets, idempotency) is decided afterwards by the ledger kernel inside a
//! SERIALIZABLE transaction. A batch result never moves money by itself.

/// Kinds of payment request a policy can limit separately.
pub const KINDS: usize = 4;
/// Currencies a policy may allow.
pub const MAX_CURRENCIES: usize = 8;
/// Recipients a policy may block (scanned linearly).
pub const MAX_BLOCKED: usize = 64;

/// One payment request, fixed width so it packs into a device buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct PaymentReq {
    pub merchant: u64,
    pub recipient: u64,
    pub amount: u64,
    pub currency: u32,
    pub kind: u32,
}

/// A tenant's limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Limits {
    pub min_amount: u64,
    /// Largest amount per request, per kind.
    pub max_amount: [u64; KINDS],
    pub currencies: [u32; MAX_CURRENCIES],
    pub n_currencies: u32,
    /// Bit `k` set: kind `k` is allowed.
    pub kinds_mask: u32,
}

pub const APPROVED: u32 = 0;
pub const ZERO_AMOUNT: u32 = 1;
pub const BELOW_MINIMUM: u32 = 2;
pub const UNKNOWN_KIND: u32 = 3;
pub const KIND_NOT_ALLOWED: u32 = 4;
pub const ABOVE_LIMIT: u32 = 5;
pub const CURRENCY_NOT_ALLOWED: u32 = 6;
pub const MERCHANT_NOT_ALLOWED: u32 = 7;
pub const SELF_PAYMENT: u32 = 8;
pub const RECIPIENT_BLOCKED: u32 = 9;
/// The policy itself is malformed (too many currencies or blocked recipients).
pub const BAD_POLICY: u32 = 10;

/// Is `x` in `sorted` (ascending)? Binary search. A `true` answer is always
/// a real match; on an unsorted list it may miss one, which only refuses.
pub fn contains_sorted(sorted: &[u64], x: u64) -> bool {
    let mut lo: usize = 0;
    let mut hi: usize = sorted.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let v = sorted[mid];
        if v == x {
            return true;
        } else if v < x {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    false
}

/// Is `x` among the first `n` entries of `xs`? Linear scan.
pub fn contains_linear(xs: &[u64], n: usize, x: u64) -> bool {
    let mut i: usize = 0;
    while i < n && i < xs.len() {
        if xs[i] == x {
            return true;
        }
        i += 1;
    }
    false
}

fn currency_allowed(l: &Limits, c: u32) -> bool {
    let mut i: usize = 0;
    while i < (l.n_currencies as usize) && i < MAX_CURRENCIES {
        if l.currencies[i] == c {
            return true;
        }
        i += 1;
    }
    false
}

/// The policy decision for one request: [`APPROVED`] or the first failed
/// rule, in this order. `merchants` is the merchant allowlist (ascending),
/// `blocked` the first `n_blocked` blocked recipients.
pub fn check(l: &Limits, merchants: &[u64], blocked: &[u64], n_blocked: usize, r: &PaymentReq) -> u32 {
    if (l.n_currencies as usize) > MAX_CURRENCIES || n_blocked > MAX_BLOCKED || n_blocked > blocked.len() {
        return BAD_POLICY;
    }
    if r.amount == 0 {
        return ZERO_AMOUNT;
    }
    if r.amount < l.min_amount {
        return BELOW_MINIMUM;
    }
    if (r.kind as usize) >= KINDS {
        return UNKNOWN_KIND;
    }
    if (l.kinds_mask >> r.kind) & 1 == 0 {
        return KIND_NOT_ALLOWED;
    }
    if r.amount > l.max_amount[r.kind as usize] {
        return ABOVE_LIMIT;
    }
    if !currency_allowed(l, r.currency) {
        return CURRENCY_NOT_ALLOWED;
    }
    if !contains_sorted(merchants, r.merchant) {
        return MERCHANT_NOT_ALLOWED;
    }
    if r.recipient == r.merchant {
        return SELF_PAYMENT;
    }
    if contains_linear(blocked, n_blocked, r.recipient) {
        return RECIPIENT_BLOCKED;
    }
    APPROVED
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        let mut currencies = [0; MAX_CURRENCIES];
        currencies[0] = 392;
        currencies[1] = 840;
        Limits { min_amount: 50, max_amount: [10_000, 500, 0, 1_000_000], currencies, n_currencies: 2, kinds_mask: 0b1011 }
    }

    #[test]
    fn rules_in_order() {
        let l = limits();
        let m = [3u64, 7, 9, 100];
        let b = [55u64, 66];
        let ok = PaymentReq { merchant: 7, recipient: 8, amount: 100, currency: 392, kind: 0 };
        assert_eq!(check(&l, &m, &b, 2, &ok), APPROVED);
        let with = |f: &dyn Fn(&mut PaymentReq)| {
            let mut r = ok;
            f(&mut r);
            check(&l, &m, &b, 2, &r)
        };
        assert_eq!(with(&|r| r.amount = 0), ZERO_AMOUNT);
        assert_eq!(with(&|r| r.amount = 49), BELOW_MINIMUM);
        assert_eq!(with(&|r| r.kind = 4), UNKNOWN_KIND);
        assert_eq!(with(&|r| r.kind = 2), KIND_NOT_ALLOWED);
        assert_eq!(with(&|r| { r.kind = 1; r.amount = 501 }), ABOVE_LIMIT);
        assert_eq!(with(&|r| r.currency = 978), CURRENCY_NOT_ALLOWED);
        assert_eq!(with(&|r| r.merchant = 8), MERCHANT_NOT_ALLOWED);
        assert_eq!(with(&|r| r.recipient = 7), SELF_PAYMENT);
        assert_eq!(with(&|r| r.recipient = 66), RECIPIENT_BLOCKED);
        // Only the first n_blocked entries count.
        assert_eq!(check(&l, &m, &b, 1, &PaymentReq { recipient: 66, ..ok }), APPROVED);
        assert_eq!(check(&l, &m, &b, 3, &ok), BAD_POLICY);
    }

    #[test]
    fn binary_search_agrees_with_linear_on_sorted_input() {
        let v: Vec<u64> = (0..200).map(|i| i * 3 + 1).collect();
        for x in 0..700 {
            assert_eq!(contains_sorted(&v, x), v.contains(&x), "{x}");
        }
        assert!(!contains_sorted(&[], 1));
    }
}
