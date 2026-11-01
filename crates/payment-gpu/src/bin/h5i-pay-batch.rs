//! `h5i-pay-batch`: the batch worker (AWS Batch job, or locally).
//!
//! ```text
//! h5i-pay-batch reconcile <internal.csv> <external.csv> <report.json> [--engine auto|gpu|parallel|single|simd] [--min-gpu-rows N]
//! h5i-pay-batch policy <requests.csv> <policy.json> <decisions.csv> [--engine auto|gpu|parallel|single|simd] [--min-gpu-rows N]
//! ```
//!
//! File formats: `payment_batch::files`. `reconcile` lists per-code counts
//! and every exception; `h5i-pay ingest` stores the report for operators.
//! `policy` screens a bulk payout file against a tenant's policy with the
//! verified `policy::check` (on the GPU, the same source compiled to PTX):
//! one decision per row (`index,code,name`). Approved rows are candidates;
//! an operator still posts them as ledger transfers, where balances and
//! idempotency are decided. The worker has no database access and no way to
//! move money. The job wrapper (`infra/docker/batch-entrypoint.sh`) copies
//! inputs from and reports to S3.
//!
//! `--engine auto` (the default) uses the GPU when this binary was built
//! with CUDA, a device answers and the batch has at least `--min-gpu-rows`
//! rows (default 2 000 000: a one-shot job pays the CUDA context and JIT,
//! see `docs/benchmarks.md`); otherwise the
//! multithreaded CPU engine. A GPU failure after selection is an error, not
//! a silent CPU retry (h5i-app-gpu's `ExecutionPolicy::Auto` rule).
//! `elapsed_ms` is the engine's time including GPU context creation when
//! the GPU is tried; the JSON log line also has `total_ms`, from process
//! start to the output written (file parsing included).

#[cfg(not(feature = "cuda"))]
use h5i_app_gpu::batch::Backend;
use payment_batch::files::{self, Res};
use payment_batch::{run_policy, run_reconcile, ExecutionPolicy, PolicyParallel, PolicySimd, PolicySingle, ReconCpu};
#[cfg(not(feature = "cuda"))]
use payment_batch::PolicyBatch;
use payment_reconcile::{Engine, Recon};
use std::io::Write;

/// The GPU type when this build has no CUDA: never constructed.
#[cfg(not(feature = "cuda"))]
enum NoGpu {}
#[cfg(not(feature = "cuda"))]
impl Backend<Recon, (), payment_reconcile::Report> for NoGpu {
    type Error = std::convert::Infallible;
    fn run(&mut self, (): (), _: &Recon) -> Result<payment_reconcile::Report, Self::Error> {
        match *self {}
    }
}
#[cfg(not(feature = "cuda"))]
impl Backend<PolicyBatch, (), Vec<u32>> for NoGpu {
    type Error = std::convert::Infallible;
    fn run(&mut self, (): (), _: &PolicyBatch) -> Result<Vec<u32>, Self::Error> {
        match *self {}
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: h5i-pay-batch reconcile <internal.csv> <external.csv> <report.json> [--engine E] [--min-gpu-rows N]\n       h5i-pay-batch policy <requests.csv> <policy.json> <decisions.csv> [--engine E] [--min-gpu-rows N]"
    );
    std::process::exit(2);
}

fn main() -> Res<()> {
    let start = std::time::Instant::now();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 || (args[1] != "reconcile" && args[1] != "policy") {
        usage();
    }
    let mut engine = "auto".to_string();
    // One process per job pays the CUDA context and PTX JIT (~140-170 ms) every
    // time; below about 2M rows the 4-thread CPU finishes first (docs/benchmarks.md).
    let mut min_gpu_rows = 2_000_000usize;
    let mut i = 5;
    while i < args.len() {
        if i + 1 >= args.len() {
            return Err(format!("{} needs a value", args[i]).into());
        }
        match args[i].as_str() {
            "--engine" => engine = args[i + 1].clone(),
            "--min-gpu-rows" => min_gpu_rows = args[i + 1].parse()?,
            other => return Err(format!("unknown flag {other}").into()),
        }
        i += 2;
    }
    if args[1] == "policy" {
        return policy(&args, &engine, min_gpu_rows, start);
    }
    let r = Recon { internal: files::read_records(&args[2])?, external: files::read_records(&args[3])? };
    let t = std::time::Instant::now();
    let (report, used) = match engine.as_str() {
        "single" | "parallel" | "simd" => {
            let e = match engine.as_str() {
                "single" => Engine::Single,
                "simd" => Engine::Simd,
                _ => Engine::Parallel,
            };
            (e.run(&r)?, engine.clone())
        }
        "auto" | "gpu" => {
            let exec = if engine == "gpu" { ExecutionPolicy::Gpu } else { ExecutionPolicy::Auto { min_gpu_rows } };
            #[cfg(feature = "cuda")]
            {
                let mut gpu = payment_gpu::try_gpu();
                let rows = r.internal.len() + r.external.len();
                let used = if gpu.is_some() && (engine == "gpu" || rows >= min_gpu_rows) { "gpu" } else { "parallel" };
                let rep = run_reconcile(exec, &mut ReconCpu(Engine::Parallel), gpu.as_mut(), &r).map_err(|e| format!("{e:?}"))?;
                (rep, used.to_string())
            }
            #[cfg(not(feature = "cuda"))]
            {
                let rep = run_reconcile::<_, NoGpu>(exec, &mut ReconCpu(Engine::Parallel), None, &r).map_err(|e| format!("{e:?}"))?;
                (rep, "parallel".to_string())
            }
        }
        other => return Err(format!("unknown engine {other}").into()),
    };
    let elapsed_ms = t.elapsed().as_secs_f64() * 1e3;
    files::write_report(&args[4], &r, &report, &used, elapsed_ms)?;
    // One JSON line for the logs (CloudWatch metric filter on exceptions_count).
    println!(
        "{{\"event\":\"reconciliation\",\"engine\":\"{used}\",\"internal_rows\":{},\"external_rows\":{},\"exceptions_count\":{},\"elapsed_ms\":{elapsed_ms:.3},\"total_ms\":{:.3}}}",
        r.internal.len(),
        r.external.len(),
        report.exceptions(),
        start.elapsed().as_secs_f64() * 1e3
    );
    Ok(())
}

fn policy(args: &[String], engine: &str, min_gpu_rows: usize, start: std::time::Instant) -> Res<()> {
    let mut b = files::read_policy(&args[3])?;
    b.requests = files::read_requests(&args[2])?;
    let t = std::time::Instant::now();
    let (codes, used) = match engine {
        "single" => (run_policy::<_, PolicySingle>(ExecutionPolicy::Cpu, &mut PolicySingle, None, &b).map_err(|e| format!("{e:?}"))?, "single"),
        "parallel" => (run_policy::<_, PolicySingle>(ExecutionPolicy::Cpu, &mut PolicyParallel, None, &b).map_err(|e| format!("{e:?}"))?, "parallel"),
        "simd" => (run_policy::<_, PolicySingle>(ExecutionPolicy::Cpu, &mut PolicySimd, None, &b).map_err(|e| format!("{e:?}"))?, "simd"),
        "auto" | "gpu" => {
            let exec = if engine == "gpu" { ExecutionPolicy::Gpu } else { ExecutionPolicy::Auto { min_gpu_rows } };
            #[cfg(feature = "cuda")]
            {
                let mut gpu = payment_gpu::try_gpu();
                let used = if gpu.is_some() && (engine == "gpu" || b.requests.len() >= min_gpu_rows) { "gpu" } else { "parallel" };
                (run_policy(exec, &mut PolicyParallel, gpu.as_mut(), &b).map_err(|e| format!("{e:?}"))?, used)
            }
            #[cfg(not(feature = "cuda"))]
            {
                (run_policy::<_, NoGpu>(exec, &mut PolicyParallel, None, &b).map_err(|e| format!("{e:?}"))?, "parallel")
            }
        }
        other => return Err(format!("unknown engine {other}").into()),
    };
    let elapsed_ms = t.elapsed().as_secs_f64() * 1e3;
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[4])?);
    let mut counts = [0u64; files::POLICY_NAMES.len()];
    for (i, c) in codes.iter().enumerate() {
        let name = files::POLICY_NAMES.get(*c as usize).copied().unwrap_or("unknown");
        if let Some(n) = counts.get_mut(*c as usize) {
            *n += 1;
        }
        writeln!(out, "{i},{c},{name}")?;
    }
    out.flush()?;
    let summary = files::POLICY_NAMES.iter().zip(counts).map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(",");
    println!(
        "{{\"event\":\"payout_screening\",\"engine\":\"{used}\",\"rows\":{},\"decisions\":{{{summary}}},\"elapsed_ms\":{elapsed_ms:.3},\"total_ms\":{:.3}}}",
        codes.len(),
        start.elapsed().as_secs_f64() * 1e3
    );
    Ok(())
}
