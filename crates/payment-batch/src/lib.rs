//! The batch API: large batches of independent rows, evaluated by one of
//! several interchangeable engines behind h5i-app-gpu's
//! [`Backend`](h5i_app_gpu::batch::Backend) trait.
//!
//! Two workloads:
//!
//! * **policy validation** — [`PolicyBatch`]: every payment request checked
//!   against a tenant's limits with the verified `payment_domain::policy::check`
//!   (`Policies.check_sound` in Lean); output one decision code per row;
//! * **reconciliation** — `payment_reconcile::Recon` → `Report`.
//!
//! Engines: CPU single-thread, CPU multithread (rayon), CPU SIMD (AVX2), and
//! CUDA (`payment-gpu`, behind its `cuda` feature). [`run_policy`] and
//! [`run_reconcile`] pick one with h5i-app-gpu's `ExecutionPolicy`.
//!
//! A batch result is advice. Policy approval is necessary, not sufficient:
//! balances, budgets and idempotency are decided by the ledger kernel in a
//! SERIALIZABLE transaction afterwards; reconciliation produces a report.
//! No engine here — in particular not the GPU — commits financial state.

pub mod binfile;
pub mod files;
pub mod policy_simd;

use h5i_app_gpu::batch::Backend;
use payment_domain::policy::{self, Limits, PaymentReq};
use payment_reconcile::{Recon, ReconError, Report};
use std::convert::Infallible;

/// A batch of payment requests and the policy they are checked against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyBatch {
    pub limits: Limits,
    /// The merchant allowlist, ascending.
    pub merchants: Vec<u64>,
    pub blocked: Vec<u64>,
    pub n_blocked: usize,
    pub requests: Vec<PaymentReq>,
}

impl PolicyBatch {
    /// The policy outcome of row `i` (the specification).
    pub fn check(&self, i: usize) -> u32 {
        policy::check(&self.limits, &self.merchants, &self.blocked, self.n_blocked, &self.requests[i])
    }
}

/// One thread, the verified `check` per row.
pub struct PolicySingle;
/// rayon, the verified `check` per row.
pub struct PolicyParallel;
/// AVX2, four rows per step (see [`policy_simd`]).
pub struct PolicySimd;

impl Backend<PolicyBatch, (), Vec<u32>> for PolicySingle {
    type Error = Infallible;
    fn run(&mut self, (): (), b: &PolicyBatch) -> Result<Vec<u32>, Infallible> {
        Ok((0..b.requests.len()).map(|i| b.check(i)).collect())
    }
}

impl Backend<PolicyBatch, (), Vec<u32>> for PolicyParallel {
    type Error = Infallible;
    fn run(&mut self, (): (), b: &PolicyBatch) -> Result<Vec<u32>, Infallible> {
        use rayon::prelude::*;
        Ok(b.requests.par_iter().with_min_len(4096).map(|r| policy::check(&b.limits, &b.merchants, &b.blocked, b.n_blocked, r)).collect())
    }
}

impl Backend<PolicyBatch, (), Vec<u32>> for PolicySimd {
    type Error = Infallible;
    fn run(&mut self, (): (), b: &PolicyBatch) -> Result<Vec<u32>, Infallible> {
        Ok(policy_simd::check_all(b))
    }
}

/// A reconciliation engine as an h5i-app-gpu backend.
pub struct ReconCpu(pub payment_reconcile::Engine);

impl Backend<Recon, (), Report> for ReconCpu {
    type Error = ReconError;
    fn run(&mut self, (): (), r: &Recon) -> Result<Report, ReconError> {
        self.0.run(r)
    }
}

pub use h5i_app_gpu::runtime::{ExecutionError, ExecutionPolicy};

/// Run a policy batch on the CPU or, if given and the policy allows, the
/// GPU. Like `h5i_app_gpu::runtime::execute_batch`, `Auto` falls back to
/// the CPU only when the batch is small or no GPU backend exists, never
/// after a GPU failure, and the output must have one row per request.
pub fn run_policy<C, G>(
    exec: ExecutionPolicy,
    cpu: &mut C,
    gpu: Option<&mut G>,
    b: &PolicyBatch,
) -> Result<Vec<u32>, ExecutionError<C::Error, G::Error>>
where
    C: Backend<PolicyBatch, (), Vec<u32>>,
    G: Backend<PolicyBatch, (), Vec<u32>>,
{
    let out = match (exec, gpu) {
        (ExecutionPolicy::Cpu, _) => cpu.run((), b).map_err(ExecutionError::Cpu)?,
        (ExecutionPolicy::Auto { min_gpu_rows }, g) if b.requests.len() < min_gpu_rows || g.is_none() => {
            cpu.run((), b).map_err(ExecutionError::Cpu)?
        }
        (_, Some(g)) => g.run((), b).map_err(ExecutionError::Gpu)?,
        (_, None) => return Err(ExecutionError::GpuUnavailable),
    };
    if out.len() != b.requests.len() {
        return Err(ExecutionError::OutputLength { expected: b.requests.len(), actual: out.len() });
    }
    Ok(out)
}

/// The same selection for reconciliation; the report must cover every row.
pub fn run_reconcile<C, G>(
    exec: ExecutionPolicy,
    cpu: &mut C,
    gpu: Option<&mut G>,
    r: &Recon,
) -> Result<Report, ExecutionError<C::Error, G::Error>>
where
    C: Backend<Recon, (), Report>,
    G: Backend<Recon, (), Report>,
{
    let rows = r.internal.len() + r.external.len();
    let out = match (exec, gpu) {
        (ExecutionPolicy::Cpu, _) => cpu.run((), r).map_err(ExecutionError::Cpu)?,
        (ExecutionPolicy::Auto { min_gpu_rows }, g) if rows < min_gpu_rows || g.is_none() => cpu.run((), r).map_err(ExecutionError::Cpu)?,
        (_, Some(g)) => g.run((), r).map_err(ExecutionError::Gpu)?,
        (_, None) => return Err(ExecutionError::GpuUnavailable),
    };
    let got = out.internal.len().min(out.matched.len()) + out.external.len();
    if out.internal.len() != r.internal.len() || out.matched.len() != r.internal.len() || out.external.len() != r.external.len() {
        return Err(ExecutionError::OutputLength { expected: rows, actual: got });
    }
    Ok(out)
}

/// Deterministic synthetic inputs for tests and benchmarks.
pub mod synth {
    use super::*;
    use payment_reconcile::rules::{key, KIND_CAPTURE, KIND_REFUND};
    use payment_reconcile::Record;

    pub struct Rng(pub u64);
    impl Rng {
        pub fn next_u64(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        pub fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n.max(1)
        }
    }

    /// A tenant policy: 4 currencies, 2 000 merchants, 32 blocked recipients.
    pub fn policy_batch(n: usize, seed: u64) -> PolicyBatch {
        let mut rng = Rng(seed | 1);
        let mut currencies = [0u32; policy::MAX_CURRENCIES];
        currencies[..4].copy_from_slice(&[392, 840, 978, 826]);
        let limits = Limits { min_amount: 50, max_amount: [1_000_000, 50_000, 0, 10_000_000], currencies, n_currencies: 4, kinds_mask: 0b1011 };
        let merchants: Vec<u64> = (0..2_000).map(|i| 1_000 + 3 * i).collect();
        let blocked: Vec<u64> = (0..32).map(|i| 900_000 + 7 * i).collect();
        let requests = (0..n)
            .map(|_| {
                let merchant = if rng.below(50) == 0 { 2 + rng.below(999) } else { merchants[rng.below(merchants.len() as u64) as usize] };
                let recipient = match rng.below(100) {
                    0 => blocked[rng.below(32) as usize],
                    1 => merchant,
                    _ => 1_000_000 + rng.below(1 << 30),
                };
                let amount = match rng.below(100) {
                    0 => 0,
                    1 => rng.below(50),
                    2 => 2_000_000 + rng.below(1 << 20),
                    _ => 50 + rng.below(40_000),
                };
                PaymentReq {
                    merchant,
                    recipient,
                    amount,
                    currency: [392, 840, 978, 826, 156][(rng.below(100) / 24).min(4) as usize],
                    kind: [0, 0, 0, 1, 3, 2, 7][rng.below(7) as usize],
                }
            })
            .collect();
        PolicyBatch { limits, merchants, blocked, n_blocked: 32, requests }
    }

    /// `n` internal records and a PSP report for them with realistic
    /// discrepancies: ~1% amount, ~0.2% currency, ~0.5% status mismatches,
    /// ~0.5% missing, ~0.5% extra, ~0.1% duplicated on each side. The
    /// external side is shuffled (reports do not come in our order).
    pub fn recon(n: usize, seed: u64) -> Recon {
        let mut rng = Rng(seed | 1);
        let mut internal = Vec::with_capacity(n);
        let mut external = Vec::with_capacity(n + n / 100);
        for i in 0..n as u64 {
            let kind = if rng.below(10) == 0 { KIND_REFUND } else { KIND_CAPTURE };
            let r = Record { key: key(kind, i * 7 + 1), amount: 100 + rng.below(1_000_000), currency: 392, status: 0 };
            internal.push(r);
            if rng.below(1000) == 0 {
                internal.push(r);
            }
            let mut e = r;
            match rng.below(1000) {
                0..=9 => e.amount += 1 + rng.below(100),
                10..=11 => e.currency = 840,
                12..=16 => e.status = 1,
                17..=21 => continue,
                22 => external.push(e),
                _ => {}
            }
            external.push(e);
            if rng.below(200) == 0 {
                external.push(Record { key: key(KIND_CAPTURE, (1 << 50) + rng.below(1 << 40)), amount: rng.below(1000), currency: 392, status: 0 });
            }
        }
        // Fisher–Yates.
        for i in (1..external.len()).rev() {
            let j = rng.below(i as u64 + 1) as usize;
            external.swap(i, j);
        }
        Recon { internal, external }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h5i_app_gpu::differential::{check_batch, compare};
    use std::num::NonZeroUsize;

    #[test]
    fn policy_engines_agree() {
        for n in [0usize, 1, 3, 4, 5, 127, 4_096, 50_000] {
            let b = synth::policy_batch(n, n as u64 + 9);
            let want: Vec<u32> = (0..n).map(|i| b.check(i)).collect();
            assert_eq!(PolicySingle.run((), &b).unwrap(), want);
            assert_eq!(PolicyParallel.run((), &b).unwrap(), want);
            assert_eq!(PolicySimd.run((), &b).unwrap(), want, "n={n}");
            compare(&mut PolicySingle, &mut PolicySimd, (), &b, PartialEq::eq).unwrap();
        }
        // Every decision code occurs in the synthetic data.
        let b = synth::policy_batch(100_000, 1);
        let out = PolicySimd.run((), &b).unwrap();
        for code in [policy::APPROVED, policy::ZERO_AMOUNT, policy::BELOW_MINIMUM, policy::UNKNOWN_KIND, policy::KIND_NOT_ALLOWED,
            policy::ABOVE_LIMIT, policy::CURRENCY_NOT_ALLOWED, policy::MERCHANT_NOT_ALLOWED, policy::SELF_PAYMENT, policy::RECIPIENT_BLOCKED]
        {
            assert!(out.contains(&code), "code {code} missing");
        }
    }

    #[test]
    fn malformed_policies_fail_closed_everywhere() {
        let mut b = synth::policy_batch(9, 3);
        b.n_blocked = 33;
        b.blocked.truncate(32);
        for out in [PolicySingle.run((), &b).unwrap(), PolicySimd.run((), &b).unwrap(), PolicyParallel.run((), &b).unwrap()] {
            assert!(out.iter().all(|c| *c == policy::BAD_POLICY));
        }
        let mut b = synth::policy_batch(9, 3);
        b.limits.n_currencies = 9;
        assert!(PolicySimd.run((), &b).unwrap().iter().all(|c| *c == policy::BAD_POLICY));
        // An unsorted allowlist only ever refuses more, never approves more.
        let mut b = synth::policy_batch(5_000, 4);
        b.merchants.reverse();
        let s = PolicySingle.run((), &b).unwrap();
        assert_eq!(PolicySimd.run((), &b).unwrap(), s);
    }

    /// The row-wise policy check is a batch computation in h5i-app-gpu's
    /// sense: partitions and repeats give the same output.
    #[test]
    fn policy_is_partition_independent() {
        struct Rows(PolicyBatch);
        impl Backend<[PaymentReq], (), Vec<u32>> for Rows {
            type Error = Infallible;
            fn run(&mut self, (): (), rows: &[PaymentReq]) -> Result<Vec<u32>, Infallible> {
                let b = PolicyBatch { requests: rows.to_vec(), ..self.0.clone() };
                PolicySimd.run((), &b)
            }
        }
        let b = synth::policy_batch(1_029, 5);
        let rows = b.requests.clone();
        check_batch(&mut Rows(b), &rows, NonZeroUsize::new(128).unwrap()).unwrap();
    }

    #[test]
    fn reconcile_engines_agree_on_synthetic_reports() {
        let r = synth::recon(200_000, 11);
        let want = payment_reconcile::reference(&r).unwrap();
        for e in [payment_reconcile::Engine::Single, payment_reconcile::Engine::Parallel, payment_reconcile::Engine::Simd] {
            assert_eq!(ReconCpu(e).run((), &r).unwrap(), want, "{e:?}");
        }
        let (ci, ce) = want.counts();
        assert!(ci[..7].iter().all(|c| *c > 0), "{ci:?}");
        assert!(ce[payment_reconcile::rules::EXTRA_EXTERNAL as usize] > 0, "{ce:?}");
    }

    #[test]
    fn selection() {
        let b = synth::policy_batch(10, 2);
        let none: Option<&mut PolicySimd> = None;
        let out = run_policy(ExecutionPolicy::Auto { min_gpu_rows: 100 }, &mut PolicySingle, none, &b).unwrap();
        assert_eq!(out.len(), 10);
        let none: Option<&mut PolicySimd> = None;
        assert!(matches!(run_policy(ExecutionPolicy::Gpu, &mut PolicySingle, none, &b), Err(ExecutionError::GpuUnavailable)));
    }
}
