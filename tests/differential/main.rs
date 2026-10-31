//! Differential tests: every batch engine against the reference, row by row.
//!
//! CPU engines always run. The CUDA engine runs with `--features gpu` (and
//! fails, rather than skips, if the build has CUDA but no device answers:
//! an explicit GPU test must not pass vacuously).
//!
//! Compared on every row: reconciliation codes, matched indices and
//! external codes; policy decision codes. Inputs cover empty batches, block
//! boundaries (255/256/257 rows for 256-thread blocks), hot keys (one key on
//! thousands of rows: atomic contention), all-missing and all-extra sides,
//! hash-colliding keys, malformed policies, unsorted allowlists, and the
//! synthetic workloads the benchmarks use.

use h5i_app_gpu::batch::Backend;
use h5i_app_gpu::differential::compare;
use payment_batch::{synth, PolicyBatch, PolicyParallel, PolicySimd, PolicySingle, ReconCpu};
use payment_reconcile::rules::{key, EMPTY, KIND_CAPTURE};
use payment_reconcile::{reference, Engine, Recon, Record, Report};

fn rec(k: u64, amount: u64) -> Record {
    Record { key: k, amount, currency: 392, status: 0 }
}

/// Reconciliation inputs with known hard cases.
fn recon_cases() -> Vec<(String, Recon)> {
    let mut cases = vec![
        ("empty".to_string(), Recon::default()),
        ("internal only".into(), Recon { internal: (0..300).map(|i| rec(key(KIND_CAPTURE, i), i)).collect(), external: vec![] }),
        ("external only".into(), Recon { internal: vec![], external: (0..300).map(|i| rec(key(KIND_CAPTURE, i), i)).collect() }),
        (
            "hot key".into(),
            Recon {
                internal: (0..5_000).map(|i| rec(if i % 2 == 0 { 42 } else { i }, 1)).collect(),
                external: (0..5_000).map(|i| rec(if i % 3 == 0 { 42 } else { i + 1 }, 1)).collect(),
            },
        ),
        (
            // Keys that differ only in bits the hash folds together.
            "colliding".into(),
            Recon {
                internal: (0..4_096u64).map(|i| rec((i << 32) | ((i * 0x9e37_79b1) & 0xffff_ffff), i)).collect(),
                external: (0..4_096u64).rev().map(|i| rec((i << 32) | ((i * 0x9e37_79b1) & 0xffff_ffff), i + (i % 5 == 0) as u64)).collect(),
            },
        ),
    ];
    for n in [1usize, 255, 256, 257, 1_025, 100_000] {
        cases.push((format!("synthetic {n}"), synth::recon(n, n as u64)));
    }
    cases
}

fn policy_cases() -> Vec<(String, PolicyBatch)> {
    let mut cases = Vec::new();
    for n in [0usize, 1, 3, 4, 5, 255, 256, 257, 4_099, 200_000] {
        cases.push((format!("synthetic {n}"), synth::policy_batch(n, 77 + n as u64)));
    }
    let mut bad = synth::policy_batch(300, 5);
    bad.n_blocked = 65;
    cases.push(("too many blocked".into(), bad));
    let mut bad = synth::policy_batch(300, 6);
    bad.limits.n_currencies = 9;
    cases.push(("too many currencies".into(), bad));
    let mut unsorted = synth::policy_batch(10_000, 7);
    unsorted.merchants.reverse();
    cases.push(("unsorted allowlist".into(), unsorted));
    let mut none = synth::policy_batch(1_000, 8);
    none.merchants.clear();
    none.blocked.clear();
    none.n_blocked = 0;
    cases.push(("empty lists".into(), none));
    cases
}

#[test]
fn cpu_reconcile_engines_match_the_reference() {
    for (name, r) in recon_cases() {
        let want = reference(&r).unwrap();
        for e in [Engine::Single, Engine::Parallel, Engine::Simd] {
            let mut cpu = ReconCpu(e);
            assert_eq!(cpu.run((), &r).unwrap(), want, "{name}: {e:?}");
            compare(&mut ReconCpu(Engine::Reference), &mut cpu, (), &r, PartialEq::eq).unwrap();
        }
    }
}

#[test]
fn cpu_policy_engines_match_the_verified_check() {
    for (name, b) in policy_cases() {
        let want: Vec<u32> = (0..b.requests.len()).map(|i| b.check(i)).collect();
        assert_eq!(PolicySingle.run((), &b).unwrap(), want, "{name}");
        assert_eq!(PolicyParallel.run((), &b).unwrap(), want, "{name}");
        assert_eq!(PolicySimd.run((), &b).unwrap(), want, "{name}");
    }
}

/// A backend whose output is truncated or of the wrong size is rejected by
/// the batch API before anyone reads it.
#[test]
fn corrupted_output_is_rejected() {
    struct Short;
    impl Backend<Recon, (), Report> for Short {
        type Error = std::convert::Infallible;
        fn run(&mut self, (): (), r: &Recon) -> Result<Report, Self::Error> {
            let mut rep = reference(r).unwrap();
            rep.internal.pop();
            Ok(rep)
        }
    }
    impl Backend<PolicyBatch, (), Vec<u32>> for Short {
        type Error = std::convert::Infallible;
        fn run(&mut self, (): (), b: &PolicyBatch) -> Result<Vec<u32>, Self::Error> {
            Ok(vec![0; b.requests.len() + 1])
        }
    }
    let r = synth::recon(100, 1);
    let res = payment_batch::run_reconcile(payment_batch::ExecutionPolicy::Gpu, &mut ReconCpu(Engine::Single), Some(&mut Short), &r);
    assert!(matches!(res, Err(payment_batch::ExecutionError::OutputLength { .. })));
    let b = synth::policy_batch(100, 1);
    let res = payment_batch::run_policy(payment_batch::ExecutionPolicy::Gpu, &mut PolicySingle, Some(&mut Short), &b);
    assert!(matches!(res, Err(payment_batch::ExecutionError::OutputLength { .. })));
    // A reserved key is refused, not silently misreported.
    let bad = Recon { internal: vec![rec(EMPTY, 1)], external: vec![] };
    assert!(ReconCpu(Engine::Simd).run((), &bad).is_err());
}

#[cfg(feature = "gpu")]
mod gpu {
    use super::*;
    use payment_gpu::Gpu;

    fn gpu() -> Gpu {
        Gpu::new().expect("the `gpu` feature is on, so a CUDA device must answer")
    }

    #[test]
    fn gpu_reconcile_matches_the_reference() {
        let mut g = gpu();
        for (name, r) in recon_cases() {
            let want = reference(&r).unwrap();
            let got = g.run((), &r).unwrap();
            assert_eq!(got, want, "{name}");
            // Repeat determinism (atomics: the lowest index must win every time).
            assert_eq!(g.run((), &r).unwrap(), want, "{name} (repeat)");
        }
        let bad = Recon { internal: vec![], external: vec![rec(EMPTY, 1)] };
        assert!(g.run((), &bad).is_err(), "reserved key must be an error on the GPU too");
    }

    #[test]
    fn gpu_policy_matches_the_verified_check() {
        let mut g = gpu();
        for (name, b) in policy_cases() {
            let want: Vec<u32> = (0..b.requests.len()).map(|i| b.check(i)).collect();
            assert_eq!(g.run((), &b).unwrap(), want, "{name}");
            compare(&mut PolicySingle, &mut g, (), &b, PartialEq::eq).unwrap();
        }
        // Partitions and repeats agree (h5i-app-gpu's batch contract).
        let b = synth::policy_batch(1_029, 3);
        struct Rows<'a>(&'a mut Gpu, PolicyBatch);
        impl Backend<[payment_domain::policy::PaymentReq], (), Vec<u32>> for Rows<'_> {
            type Error = String;
            fn run(&mut self, (): (), rows: &[payment_domain::policy::PaymentReq]) -> Result<Vec<u32>, Self::Error> {
                let b = PolicyBatch { requests: rows.to_vec(), ..self.1.clone() };
                self.0.run((), &b).map_err(|e| e.to_string())
            }
        }
        let rows = b.requests.clone();
        h5i_app_gpu::differential::check_batch(&mut Rows(&mut g, b), &rows, std::num::NonZeroUsize::new(128).unwrap()).unwrap();
    }

    #[test]
    fn gpu_selection_through_the_batch_api() {
        use payment_batch::{run_policy, run_reconcile, ExecutionPolicy};
        let mut g = gpu();
        let r = synth::recon(50_000, 9);
        let want = reference(&r).unwrap();
        let got = run_reconcile(ExecutionPolicy::Gpu, &mut ReconCpu(Engine::Single), Some(&mut g), &r).unwrap();
        assert_eq!(got, want);
        let small = synth::policy_batch(10, 2);
        let out = run_policy(ExecutionPolicy::Auto { min_gpu_rows: 1_000 }, &mut PolicySingle, Some(&mut g), &small).unwrap();
        assert_eq!(out, PolicySingle.run((), &small).unwrap());
    }
}
