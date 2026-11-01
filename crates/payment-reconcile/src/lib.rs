//! Reconciliation: match internal records (from the ledger and payment
//! kernels) against a PSP's settlement report.
//!
//! Every internal record gets a code ([`rules`]): matched, amount /
//! currency / status mismatch, missing at the PSP, or duplicated; every
//! external record gets matched, extra (unknown to us) or duplicated. The
//! result is a report for an operator. Nothing here changes a balance:
//! corrections go through the ledger kernel as ordinary, audited transfers.
//!
//! Four implementations compute the same [`Report`] from the same input:
//!
//! * [`reference`] — `BTreeMap`, written to be obviously right (the oracle);
//! * [`cpu_single`] — one thread, open-addressing hash join;
//! * [`cpu_parallel`] — rayon: radix-partition by key hash, then one hash
//!   join per partition;
//! * [`simd`] — one thread, the probes of four rows at a time with AVX2
//!   gathers (scalar fallback without AVX2).
//!
//! The CUDA implementation is in `payment-gpu`; differential tests compare
//! all of them, field by field, on every row.

pub mod rules;
pub mod inputs;
pub mod join;

pub use rules::Record;
use rules::*;
use std::collections::BTreeMap;

/// Internal and external records to reconcile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recon {
    pub internal: Vec<Record>,
    pub external: Vec<Record>,
}

/// Per-row results, in input order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Code of each internal record.
    pub internal: Vec<u32>,
    /// Index of the lowest-index external record with the same key, or [`rules::NONE`].
    pub matched: Vec<u32>,
    /// Code of each external record.
    pub external: Vec<u32>,
}

impl Report {
    /// How many rows got each code: `(internal, external)`.
    pub fn counts(&self) -> ([u64; CODES], [u64; CODES]) {
        let mut i = [0u64; CODES];
        let mut e = [0u64; CODES];
        for c in &self.internal {
            i[*c as usize] += 1;
        }
        for c in &self.external {
            e[*c as usize] += 1;
        }
        (i, e)
    }

    /// Rows that need an operator: everything not matched.
    pub fn exceptions(&self) -> usize {
        self.internal.iter().filter(|c| **c != MATCHED).count() + self.external.iter().filter(|c| **c != MATCHED).count()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconError {
    /// A record uses the reserved key `u64::MAX`.
    ReservedKey { external: bool, index: usize },
    /// More rows than a `u32` index can name.
    TooLarge,
}

impl std::fmt::Display for ReconError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ReconError {}

/// Reject inputs every implementation would mishandle.
pub fn validate(r: &Recon) -> Result<(), ReconError> {
    if r.internal.len() >= u32::MAX as usize || r.external.len() >= u32::MAX as usize {
        return Err(ReconError::TooLarge);
    }
    for (external, rows) in [(false, &r.internal), (true, &r.external)] {
        if let Some(index) = rows.iter().position(|x| x.key == EMPTY) {
            return Err(ReconError::ReservedKey { external, index });
        }
    }
    Ok(())
}

/// The oracle.
pub fn reference(r: &Recon) -> Result<Report, ReconError> {
    validate(r)?;
    let mut ext: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (j, e) in r.external.iter().enumerate() {
        ext.entry(e.key).or_default().push(j);
    }
    let mut int: BTreeMap<u64, usize> = BTreeMap::new();
    for i in &r.internal {
        *int.entry(i.key).or_default() += 1;
    }
    let mut out = Report::default();
    for x in &r.internal {
        let es = ext.get(&x.key);
        let first = es.map(|v| v[0]);
        out.internal.push(classify_internal(
            x,
            int[&x.key] as u32,
            es.map_or(0, |v| v.len() as u32),
            first.map(|j| &r.external[j]),
        ));
        out.matched.push(first.map_or(NONE, |j| j as u32));
    }
    for e in &r.external {
        out.external.push(classify_external(int.get(&e.key).copied().unwrap_or(0) as u32, ext[&e.key].len() as u32));
    }
    Ok(out)
}

/// An open-addressing table of keys, with how many rows share each key and
/// the lowest row index among them.
pub struct Table {
    mask: usize,
    keys: Vec<u64>,
    count: Vec<u32>,
    first: Vec<u32>,
}

impl Table {
    /// Rows `idx` of `rows` (in increasing order), keyed by their key.
    pub fn build(rows: &[Record], idx: impl Iterator<Item = usize>, n: usize) -> Table {
        let cap = capacity(n);
        let mut t = Table { mask: cap - 1, keys: vec![EMPTY; cap], count: vec![0; cap], first: vec![NONE; cap] };
        for i in idx {
            let k = rows[i].key;
            let mut s = (hash(k) as usize) & t.mask;
            loop {
                if t.keys[s] == k {
                    break;
                }
                if t.keys[s] == EMPTY {
                    t.keys[s] = k;
                    t.first[s] = i as u32;
                    break;
                }
                s = (s + 1) & t.mask;
            }
            t.count[s] += 1;
        }
        t
    }

    #[inline(always)]
    pub fn find(&self, k: u64) -> Option<usize> {
        find_slot(&self.keys, self.mask as u64, k)
    }

    /// The table as the shared per-row functions see it.
    #[inline(always)]
    pub fn as_ref(&self) -> TableRef<'_> {
        TableRef { keys: &self.keys, count: &self.count, first: &self.first, mask: self.mask as u64 }
    }

    /// `(count, first)` for key `k`; `(0, NONE)` if absent.
    #[inline(always)]
    pub fn get(&self, k: u64) -> (u32, u32) {
        self.as_ref().get(k)
    }
}

/// One thread, one hash table per side: [`join::join`], the code
/// `proofs/reconcile/Reconcile/Join.lean` proves equal to [`reference`].
pub fn cpu_single(r: &Recon) -> Result<Report, ReconError> {
    validate(r)?;
    let out = join::join(&r.internal, &r.external);
    Ok(Report { internal: out.internal, matched: out.matched, external: out.external })
}

/// A shared output buffer written at disjoint indices from several threads.
struct Disjoint<T>(*mut T, usize);
unsafe impl<T: Send> Send for Disjoint<T> {}
unsafe impl<T: Send> Sync for Disjoint<T> {}
impl<T> Disjoint<T> {
    fn new(v: &mut [T]) -> Self {
        Disjoint(v.as_mut_ptr(), v.len())
    }
    /// # Safety
    /// No two threads write the same `i`, and nobody reads until all writes are done.
    unsafe fn write(&self, i: usize, x: T) {
        assert!(i < self.1);
        unsafe { self.0.add(i).write(x) }
    }
}

/// Row indices grouped by partition (stable: increasing within a partition),
/// with the start of each partition. Two-pass parallel radix partition.
fn partition(rows: &[Record], parts: usize) -> (Vec<u32>, Vec<usize>) {
    use rayon::prelude::*;
    let part = |k: u64| ((hash(k) >> 40) as usize) % parts;
    let chunk = rows.len().div_ceil(rayon::current_num_threads() * 4).max(4096);
    // Pass 1: a histogram per chunk.
    let hist: Vec<Vec<usize>> = rows
        .par_chunks(chunk)
        .map(|c| {
            let mut h = vec![0usize; parts];
            for r in c {
                h[part(r.key)] += 1;
            }
            h
        })
        .collect();
    // Offsets: partition-major, then chunk.
    let mut offsets = vec![vec![0usize; parts]; hist.len()];
    let mut starts = vec![0usize; parts + 1];
    let mut acc = 0;
    for p in 0..parts {
        starts[p] = acc;
        for (c, h) in hist.iter().enumerate() {
            offsets[c][p] = acc;
            acc += h[p];
        }
    }
    starts[parts] = acc;
    // Pass 2: scatter, each chunk into its own ranges.
    let mut out = vec![0u32; rows.len()];
    let dst = Disjoint::new(&mut out);
    rows.par_chunks(chunk).zip(offsets.into_par_iter()).enumerate().for_each(|(c, (rs, mut off))| {
        let base = c * chunk;
        for (k, r) in rs.iter().enumerate() {
            let p = part(r.key);
            // SAFETY: the offsets of different (chunk, partition) pairs are disjoint ranges.
            unsafe { dst.write(off[p], (base + k) as u32) };
            off[p] += 1;
        }
    });
    (out, starts)
}

/// rayon: partition both sides by key hash, then reconcile each partition
/// independently. Equal keys always land in the same partition.
pub fn cpu_parallel(r: &Recon) -> Result<Report, ReconError> {
    use rayon::prelude::*;
    validate(r)?;
    let parts = (rayon::current_num_threads() * 8).next_power_of_two();
    let ((ip, is), (ep, es)) = rayon::join(|| partition(&r.internal, parts), || partition(&r.external, parts));
    let mut out = Report { internal: vec![0; r.internal.len()], matched: vec![0; r.internal.len()], external: vec![0; r.external.len()] };
    let (oi, om, oe) = (Disjoint::new(&mut out.internal), Disjoint::new(&mut out.matched), Disjoint::new(&mut out.external));
    (0..parts).into_par_iter().for_each(|p| {
        let irows = &ip[is[p]..is[p + 1]];
        let erows = &ep[es[p]..es[p + 1]];
        // The partition's rows in increasing index order, joined by the
        // proven single-thread join; local indices map back through
        // `irows`/`erows`. Every key's rows are all in this partition, so
        // the counts and the first external row are the whole input's.
        let int: Vec<Record> = irows.iter().map(|&i| r.internal[i as usize]).collect();
        let ext: Vec<Record> = erows.iter().map(|&j| r.external[j as usize]).collect();
        let j = join::join(&int, &ext);
        for (l, &i) in irows.iter().enumerate() {
            let m = j.matched[l];
            let m = if m == NONE { NONE } else { erows[m as usize] };
            // SAFETY: every row index is in exactly one partition.
            unsafe {
                oi.write(i as usize, j.internal[l]);
                om.write(i as usize, m);
            }
        }
        for (l, &e) in erows.iter().enumerate() {
            // SAFETY: as above.
            unsafe { oe.write(e as usize, j.external[l]) };
        }
    });
    Ok(out)
}

pub mod simd;
pub use simd::simd;

/// The implementations, by name (for benchmarks and the API).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Reference,
    Single,
    Parallel,
    Simd,
}

impl Engine {
    pub fn run(self, r: &Recon) -> Result<Report, ReconError> {
        match self {
            Engine::Reference => reference(r),
            Engine::Single => cpu_single(r),
            Engine::Parallel => cpu_parallel(r),
            Engine::Simd => simd(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: u64, amount: u64, currency: u32, status: u32) -> Record {
        Record { key, amount, currency, status }
    }

    #[test]
    fn every_code() {
        let r = Recon {
            internal: vec![
                rec(1, 100, 392, 0), // matched
                rec(2, 100, 392, 0), // amount
                rec(3, 100, 392, 0), // currency
                rec(4, 100, 392, 0), // status
                rec(5, 100, 392, 0), // missing
                rec(6, 100, 392, 0), // duplicate external
                rec(7, 100, 392, 0), // duplicate internal
                rec(7, 100, 392, 0),
            ],
            external: vec![
                rec(1, 100, 392, 0),
                rec(2, 99, 392, 0),
                rec(3, 100, 840, 0),
                rec(4, 100, 392, 1),
                rec(6, 100, 392, 0),
                rec(6, 100, 392, 0),
                rec(7, 100, 392, 0),
                rec(8, 5, 392, 0), // extra
            ],
        };
        let want = Report {
            internal: vec![MATCHED, AMOUNT_MISMATCH, CURRENCY_MISMATCH, STATUS_MISMATCH, MISSING_EXTERNAL, DUPLICATE_EXTERNAL, DUPLICATE_INTERNAL, DUPLICATE_INTERNAL],
            matched: vec![0, 1, 2, 3, NONE, 4, 6, 6],
            external: vec![MATCHED, MATCHED, MATCHED, MATCHED, DUPLICATE_EXTERNAL, DUPLICATE_EXTERNAL, DUPLICATE_INTERNAL, EXTRA_EXTERNAL],
        };
        for e in [Engine::Reference, Engine::Single, Engine::Parallel, Engine::Simd] {
            assert_eq!(e.run(&r).unwrap(), want, "{e:?}");
        }
        assert_eq!(want.exceptions(), 7 + 4);
    }

    #[test]
    fn reserved_key_is_refused() {
        let r = Recon { internal: vec![rec(EMPTY, 1, 1, 0)], external: vec![] };
        for e in [Engine::Reference, Engine::Single, Engine::Parallel, Engine::Simd] {
            assert_eq!(e.run(&r), Err(ReconError::ReservedKey { external: false, index: 0 }));
        }
    }

    #[test]
    fn random_inputs_agree() {
        let mut s = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: u64| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s % n
        };
        for size in [0usize, 1, 7, 100, 5_000, 70_000] {
            let keys = (size as u64 / 2).max(1);
            let mut r = Recon::default();
            for _ in 0..size {
                r.internal.push(rec(key(1 + next(2), next(keys)), next(4), 392 + 448 * next(2) as u32, next(2) as u32));
                r.external.push(rec(key(1 + next(2), next(keys)), next(4), 392 + 448 * next(2) as u32, next(2) as u32));
            }
            let want = reference(&r).unwrap();
            assert_eq!(cpu_single(&r).unwrap(), want, "single {size}");
            assert_eq!(cpu_parallel(&r).unwrap(), want, "parallel {size}");
            assert_eq!(simd(&r).unwrap(), want, "simd {size}");
        }
    }
}
