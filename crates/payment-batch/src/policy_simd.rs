//! `payment_domain::policy::check` for four requests at once, with AVX2.
//!
//! Each rule becomes a lane mask (all ones where the rule fails), and the
//! decision is the first failing rule in `check`'s order, built by blending
//! the codes from the last rule to the first. The merchant allowlist is a
//! branch-free binary search per lane (gathers), visiting the same indices
//! as the scalar search, so it agrees even on an unsorted list. Rows that do
//! not fill a register, and hosts without AVX2, use the scalar `check`.

use crate::PolicyBatch;
use payment_domain::policy::{self, MAX_BLOCKED, MAX_CURRENCIES};

pub fn check_all(b: &PolicyBatch) -> Vec<u32> {
    let n = b.requests.len();
    let mut out = vec![0u32; n];
    if (b.limits.n_currencies as usize) > MAX_CURRENCIES || b.n_blocked > MAX_BLOCKED || b.n_blocked > b.blocked.len() {
        out.fill(policy::BAD_POLICY);
        return out;
    }
    let mut done = 0;
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        let full = n / 4 * 4;
        // SAFETY: AVX2 is present; every gather reads inside its slice.
        unsafe { avx2::check4s(b, &mut out[..full]) };
        done = full;
    }
    for (i, o) in out.iter_mut().enumerate().skip(done) {
        *o = b.check(i);
    }
    out
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use crate::PolicyBatch;
    use payment_domain::policy::{self, PaymentReq};
    use std::arch::x86_64::*;

    const SIGN: i64 = i64::MIN;

    /// Unsigned `a > b` per 64-bit lane.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn ugt(a: __m256i, b: __m256i) -> __m256i {
        let s = _mm256_set1_epi64x(SIGN);
        _mm256_cmpgt_epi64(_mm256_xor_si256(a, s), _mm256_xor_si256(b, s))
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn not(a: __m256i) -> __m256i {
        _mm256_xor_si256(a, _mm256_set1_epi64x(-1))
    }

    #[target_feature(enable = "avx2")]
    pub unsafe fn check4s(b: &PolicyBatch, out: &mut [u32]) {
        unsafe {
            assert_eq!(std::mem::size_of::<PaymentReq>(), 32);
            let base = b.requests.as_ptr() as *const i64;
            let l = &b.limits;
            let zero = _mm256_setzero_si256();
            let min = _mm256_set1_epi64x(l.min_amount as i64);
            let three = _mm256_set1_epi64x(3);
            let kmask = _mm256_set1_epi64x(i64::from(l.kinds_mask));
            let one = _mm256_set1_epi64x(1);
            let lo32 = _mm256_set1_epi64x(0xffff_ffff);
            let caps = l.max_amount.as_ptr() as *const i64;
            let nm = b.merchants.len() as i64;
            let mbase = b.merchants.as_ptr() as *const i64;
            for (q, chunk) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let i = q * 4;
                let w = |f: i64| _mm256_i64gather_epi64::<8>(base, _mm256_setr_epi64x(i as i64 * 4 + f, (i as i64 + 1) * 4 + f, (i as i64 + 2) * 4 + f, (i as i64 + 3) * 4 + f));
                let merchant = w(0);
                let recipient = w(1);
                let amount = w(2);
                let w3 = w(3);
                let currency = _mm256_and_si256(w3, lo32);
                let kind = _mm256_srli_epi64::<32>(w3);

                let f_zero = _mm256_cmpeq_epi64(amount, zero);
                let f_below = ugt(min, amount);
                let f_unknown = _mm256_cmpgt_epi64(kind, three);
                let f_kind = _mm256_cmpeq_epi64(_mm256_and_si256(_mm256_srlv_epi64(kmask, kind), one), zero);
                let cap = _mm256_i64gather_epi64::<8>(caps, _mm256_and_si256(kind, three));
                let f_above = ugt(amount, cap);
                let mut cur_ok = zero;
                for c in &l.currencies[..l.n_currencies as usize] {
                    cur_ok = _mm256_or_si256(cur_ok, _mm256_cmpeq_epi64(currency, _mm256_set1_epi64x(i64::from(*c))));
                }
                let f_currency = not(cur_ok);
                // Binary search, lane by lane in lockstep.
                let mut lo = zero;
                let mut hi = _mm256_set1_epi64x(nm);
                let mut found = zero;
                let mut active = _mm256_cmpgt_epi64(hi, lo);
                while _mm256_testz_si256(active, active) == 0 {
                    let mid = _mm256_add_epi64(lo, _mm256_srli_epi64::<1>(_mm256_sub_epi64(hi, lo)));
                    let v = _mm256_mask_i64gather_epi64::<8>(zero, mbase, mid, active);
                    let eq = _mm256_and_si256(_mm256_cmpeq_epi64(v, merchant), active);
                    found = _mm256_or_si256(found, eq);
                    let rest = _mm256_andnot_si256(eq, active);
                    let lt = _mm256_and_si256(ugt(merchant, v), rest);
                    let gt = _mm256_andnot_si256(lt, rest);
                    lo = _mm256_blendv_epi8(lo, _mm256_add_epi64(mid, one), lt);
                    hi = _mm256_blendv_epi8(hi, mid, gt);
                    active = _mm256_and_si256(rest, _mm256_cmpgt_epi64(hi, lo));
                }
                let f_merchant = not(found);
                let f_self = _mm256_cmpeq_epi64(recipient, merchant);
                let mut f_blocked = zero;
                for x in &b.blocked[..b.n_blocked] {
                    f_blocked = _mm256_or_si256(f_blocked, _mm256_cmpeq_epi64(recipient, _mm256_set1_epi64x(*x as i64)));
                }
                // First failing rule wins: blend from the last rule to the first.
                let mut code = _mm256_set1_epi64x(i64::from(policy::APPROVED));
                for (f, c) in [
                    (f_blocked, policy::RECIPIENT_BLOCKED),
                    (f_self, policy::SELF_PAYMENT),
                    (f_merchant, policy::MERCHANT_NOT_ALLOWED),
                    (f_currency, policy::CURRENCY_NOT_ALLOWED),
                    (f_above, policy::ABOVE_LIMIT),
                    (f_kind, policy::KIND_NOT_ALLOWED),
                    (f_unknown, policy::UNKNOWN_KIND),
                    (f_below, policy::BELOW_MINIMUM),
                    (f_zero, policy::ZERO_AMOUNT),
                ] {
                    code = _mm256_blendv_epi8(code, _mm256_set1_epi64x(i64::from(c)), f);
                }
                let mut lanes = [0i64; 4];
                _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, code);
                for (o, v) in chunk.iter_mut().zip(lanes) {
                    *o = v as u32;
                }
            }
        }
    }
}
