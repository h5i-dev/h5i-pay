//! Reconciliation with AVX2: the same hash join as `cpu_single`, but the
//! table probes for four rows run in the lanes of one 256-bit register
//! (vertical vectorization: gather four slots, compare four keys, advance
//! only the lanes that have not hit their key or an empty slot). Hashing is
//! vectorized too; the hash uses 32×32→64 multiplies so AVX2 can compute it.
//! Building the tables stays scalar (inserts conflict with each other).
//!
//! Without AVX2 at run time it falls back to the scalar loop, so the result
//! is always the same `Report`.

use crate::rules::*;
use crate::{validate, Recon, ReconError, Report, Table};

/// Slots of `keys` for each probe key, or `None`. Scalar reference for the lanes.
fn probe_scalar(t: &Table, k: u64) -> Option<usize> {
    t.find(k)
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use crate::rules::{Record, EMPTY};
    use crate::Table;
    use std::arch::x86_64::*;

    /// The probe of four keys at once. Returns the slot per lane or -1.
    #[target_feature(enable = "avx2")]
    pub unsafe fn probe4(t: &Table, k: __m256i) -> [i64; 4] {
        unsafe {
            let lo = _mm256_and_si256(k, _mm256_set1_epi64x(0xffff_ffff));
            let hi = _mm256_srli_epi64::<32>(k);
            let a = _mm256_xor_si256(
                _mm256_mul_epu32(lo, _mm256_set1_epi64x(0x9e37_79b1)),
                _mm256_mul_epu32(hi, _mm256_set1_epi64x(0x85eb_ca77)),
            );
            let h = _mm256_xor_si256(_mm256_xor_si256(a, _mm256_srli_epi64::<29>(a)), _mm256_srli_epi64::<17>(k));
            let mask = _mm256_set1_epi64x(t.mask as i64);
            let one = _mm256_set1_epi64x(1);
            let empty = _mm256_set1_epi64x(EMPTY as i64);
            let mut slot = _mm256_and_si256(h, mask);
            let mut result = _mm256_set1_epi64x(-1);
            // Lanes still searching: all ones.
            let mut active = _mm256_set1_epi64x(-1);
            let base = t.keys.as_ptr() as *const i64;
            loop {
                let got = _mm256_mask_i64gather_epi64::<8>(empty, base, slot, active);
                let hit = _mm256_and_si256(_mm256_cmpeq_epi64(got, k), active);
                let miss = _mm256_and_si256(_mm256_cmpeq_epi64(got, empty), active);
                result = _mm256_blendv_epi8(result, slot, hit);
                active = _mm256_andnot_si256(_mm256_or_si256(hit, miss), active);
                if _mm256_testz_si256(active, active) == 1 {
                    break;
                }
                slot = _mm256_and_si256(_mm256_add_epi64(slot, one), mask);
            }
            let mut out = [0i64; 4];
            _mm256_storeu_si256(out.as_mut_ptr() as *mut __m256i, result);
            out
        }
    }

    /// Keys of four consecutive records (stride 24 bytes = 3 words).
    #[target_feature(enable = "avx2")]
    pub unsafe fn keys4(rows: &[Record], i: usize) -> __m256i {
        unsafe {
            let idx = _mm256_setr_epi64x(0, 3, 6, 9);
            _mm256_i64gather_epi64::<8>(rows.as_ptr().add(i) as *const i64, idx)
        }
    }
}

/// One thread, AVX2 probes.
pub fn simd(r: &Recon) -> Result<Report, ReconError> {
    validate(r)?;
    let ext = Table::build(&r.external, 0..r.external.len(), r.external.len());
    let int = Table::build(&r.internal, 0..r.internal.len(), r.internal.len());
    let mut out = Report { internal: vec![0; r.internal.len()], matched: vec![NONE; r.internal.len()], external: vec![0; r.external.len()] };
    let at = |t: &Table, s: Option<usize>| match s {
        Some(s) => (t.count[s], t.first[s]),
        None => (0, NONE),
    };
    let internal_row = |out: &mut Report, i: usize, si: Option<usize>, se: Option<usize>| {
        let (ic, _) = at(&int, si);
        let (ec, ef) = at(&ext, se);
        let first = if ec == 0 { None } else { Some(&r.external[ef as usize]) };
        out.internal[i] = classify_internal(&r.internal[i], ic, ec, first);
        out.matched[i] = ef;
    };
    let external_row = |out: &mut Report, j: usize, si: Option<usize>, se: Option<usize>| {
        out.external[j] = classify_external(at(&int, si).0, at(&ext, se).0);
    };
    let lanes = |v: [i64; 4]| v.map(|x| if x < 0 { None } else { Some(x as usize) });
    let mut done_i = 0;
    let mut done_e = 0;
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        let n = r.internal.len() / 4 * 4;
        while done_i < n {
            // SAFETY: AVX2 is present, and rows done_i..done_i+4 exist.
            let (si, se) = unsafe {
                let k = avx2::keys4(&r.internal, done_i);
                (lanes(avx2::probe4(&int, k)), lanes(avx2::probe4(&ext, k)))
            };
            for l in 0..4 {
                internal_row(&mut out, done_i + l, si[l], se[l]);
            }
            done_i += 4;
        }
        let m = r.external.len() / 4 * 4;
        while done_e < m {
            // SAFETY: as above.
            let (si, se) = unsafe {
                let k = avx2::keys4(&r.external, done_e);
                (lanes(avx2::probe4(&int, k)), lanes(avx2::probe4(&ext, k)))
            };
            for l in 0..4 {
                external_row(&mut out, done_e + l, si[l], se[l]);
            }
            done_e += 4;
        }
    }
    for i in done_i..r.internal.len() {
        let k = r.internal[i].key;
        internal_row(&mut out, i, probe_scalar(&int, k), probe_scalar(&ext, k));
    }
    for j in done_e..r.external.len() {
        let k = r.external[j].key;
        external_row(&mut out, j, probe_scalar(&int, k), probe_scalar(&ext, k));
    }
    Ok(out)
}
