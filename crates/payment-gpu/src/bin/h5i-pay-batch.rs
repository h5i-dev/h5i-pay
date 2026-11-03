//! `h5i-pay-batch`: the batch worker (AWS Batch job, or locally).
//!
//! ```text
//! h5i-pay-batch reconcile <internal> <external> <report.json> [--engine E] [--min-gpu-rows N]
//! h5i-pay-batch jobs <jobs.txt|-> [--engine E] [--min-gpu-rows N]
//! h5i-pay-batch convert <in> <out>
//! h5i-pay-batch policy <requests.csv> <policy.json> <decisions.csv> [--engine E] [--min-gpu-rows N]
//! ```
//!
//! `E` is `auto` (default), `gpu`, `parallel`, `single` or `simd`.
//!
//! Inputs are records CSV or binary records files (`payment_batch::binfile`,
//! told apart by their first bytes); file formats: `payment_batch::files`.
//! With two binary files the worker maps them and hands the columns to the
//! engine as they are: the GPU copies them to the device through pinned
//! staging buffers, the parallel CPU engine reads them in place. `reconcile`
//! lists per-code counts and every exception; `h5i-pay ingest` stores the
//! report for operators.
//!
//! `jobs` is the long-lived mode: one job per line (`<internal> <external>
//! <report.json>`, separated by spaces or tabs, from a file or stdin), all in
//! one process, so the CUDA context, the PTX JIT and the device and pinned
//! buffers are set up once per run, not once per file. A failed job is
//! logged and the others still run; the exit status is non-zero if any
//! failed. `convert` turns a records CSV (for instance a PSP settlement
//! report) into a binary records file, or a binary one back into CSV.
//!
//! `policy` screens a bulk payout file against a tenant's policy with the
//! verified `policy::check` (on the GPU, the same source compiled to PTX):
//! one decision per row (`index,code,name`). Approved rows are candidates;
//! an operator still posts them as ledger transfers, where balances and
//! idempotency are decided. The worker has no database access and no way to
//! move money. The job wrapper (`infra/docker/batch-entrypoint.sh`) copies
//! inputs from and reports to S3.
//!
//! `--engine auto` uses the GPU when this binary was built with CUDA, a
//! device answers and the job has at least `--min-gpu-rows` rows (both
//! sides; default 6 000 000 for a one-shot `reconcile`, which pays the CUDA
//! context, and 200 000 for `jobs`, where it is paid once; see
//! `docs/benchmarks.md`); otherwise the multithreaded CPU engine. A GPU
//! failure after selection is an error, not a silent CPU retry
//! (h5i-app-gpu's `ExecutionPolicy::Auto` rule). `elapsed_ms` is the
//! engine's time, including GPU context creation in the job that created
//! it. The JSON log line per job also has the phases (`open_ms`: map and
//! check headers, or parse CSV; `verify_ms`: data checksums; `engine_ms`;
//! `report_ms`; for the GPU `context_ms`, `h2d_ms`, `kernel_ms`, `d2h_ms`)
//! and `total_ms`, from the start of the job to the report written.

#[cfg(not(feature = "cuda"))]
use h5i_app_gpu::batch::Backend;
use payment_batch::binfile;
use payment_batch::files::{self, Res};
use payment_batch::{run_policy, run_reconcile, ExecutionPolicy, PolicyParallel, PolicySimd, PolicySingle, ReconCpu};
#[cfg(not(feature = "cuda"))]
use payment_batch::PolicyBatch;
use payment_reconcile::{Cols, Engine, Recon, Record};
use std::io::{BufRead, Write};
use std::time::Instant;

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

#[cfg(feature = "cuda")]
type Gpu = payment_gpu::Gpu;
#[cfg(not(feature = "cuda"))]
type Gpu = NoGpu;

fn usage() -> ! {
    eprintln!(
        "usage: h5i-pay-batch reconcile <internal> <external> <report.json> [--engine E] [--min-gpu-rows N]\n       h5i-pay-batch jobs <jobs.txt|-> [--engine E] [--min-gpu-rows N]\n       h5i-pay-batch convert <in> <out>\n       h5i-pay-batch policy <requests.csv> <policy.json> <decisions.csv> [--engine E] [--min-gpu-rows N]"
    );
    std::process::exit(2);
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// The process-wide engine state: the GPU, created on first use.
struct Worker {
    engine: String,
    min_gpu_rows: usize,
    /// `None`: not tried yet; `Some(None)`: no device (or no CUDA build).
    gpu: Option<Option<Gpu>>,
}

impl Worker {
    /// The GPU, if this job should use it; the context time if this call
    /// created it.
    fn gpu_for(&mut self, rows: usize) -> Res<(Option<&mut Gpu>, f64)> {
        let wanted = match self.engine.as_str() {
            "gpu" => true,
            "auto" => rows >= self.min_gpu_rows,
            _ => false,
        };
        if !wanted {
            return Ok((None, 0.0));
        }
        let mut context_ms = 0.0;
        if self.gpu.is_none() {
            let t = Instant::now();
            #[cfg(feature = "cuda")]
            {
                self.gpu = Some(payment_gpu::try_gpu());
            }
            #[cfg(not(feature = "cuda"))]
            {
                self.gpu = Some(None);
            }
            context_ms = ms(t);
        }
        let g = self.gpu.as_mut().and_then(Option::as_mut);
        if g.is_none() && self.engine == "gpu" {
            return Err("--engine gpu: no CUDA device (or a build without CUDA)".into());
        }
        Ok((g, context_ms))
    }

    /// One reconciliation job; returns its JSON log line.
    fn reconcile(&mut self, internal: &str, external: &str, report: &str) -> Res<String> {
        let start = Instant::now();
        if !["auto", "gpu", "parallel", "single", "simd"].contains(&self.engine.as_str()) {
            return Err(format!("unknown engine {}", self.engine).into());
        }
        if binfile::is_records_file(internal)? && binfile::is_records_file(external)? {
            return self.reconcile_bin(internal, external, report, start);
        }
        // CSV (or one of each): rows in memory.
        let t = Instant::now();
        let read = |p: &str| -> Res<Vec<Record>> {
            if binfile::is_records_file(p)? {
                let f = binfile::Mapped::open(p)?.verify(p)?;
                let c = f.cols();
                Ok((0..c.len()).map(|i| c.get(i)).collect())
            } else {
                files::read_records(p)
            }
        };
        let r = Recon { internal: read(internal)?, external: read(external)? };
        let open_ms = ms(t);
        let t = Instant::now();
        let rows = r.internal.len() + r.external.len();
        let (rep, used, context_ms) = match self.engine.as_str() {
            "single" => (Engine::Single.run(&r)?, "single", 0.0),
            "simd" => (Engine::Simd.run(&r)?, "simd", 0.0),
            "parallel" => (Engine::Parallel.run(&r)?, "parallel", 0.0),
            _ => {
                let exec = if self.engine == "gpu" { ExecutionPolicy::Gpu } else { ExecutionPolicy::Auto { min_gpu_rows: self.min_gpu_rows } };
                let (g, context_ms) = self.gpu_for(rows)?;
                let used = if g.is_some() { "gpu" } else { "parallel" };
                (run_reconcile(exec, &mut ReconCpu(Engine::Parallel), g, &r).map_err(|e| format!("{e:?}"))?, used, context_ms)
            }
        };
        let elapsed_ms = ms(t);
        let t = Instant::now();
        files::write_report(report, &r, &rep, used, elapsed_ms)?;
        let report_ms = ms(t);
        Ok(format!(
            "{{\"event\":\"reconciliation\",\"format\":\"csv\",\"engine\":\"{used}\",\"internal_rows\":{},\"external_rows\":{},\"exceptions_count\":{},\"elapsed_ms\":{elapsed_ms:.3},\"open_ms\":{open_ms:.3},\"context_ms\":{context_ms:.3},\"engine_ms\":{:.3},\"report_ms\":{report_ms:.3},\"total_ms\":{:.3}}}",
            r.internal.len(),
            r.external.len(),
            rep.exceptions(),
            elapsed_ms - context_ms,
            ms(start)
        ))
    }

    /// Two binary records files: mapped, verified, columns to the engine.
    fn reconcile_bin(&mut self, internal: &str, external: &str, report: &str, start: Instant) -> Res<String> {
        let t = Instant::now();
        let (mi, me) = (binfile::Mapped::open(internal)?, binfile::Mapped::open(external)?);
        let open_ms = ms(t);
        let t = Instant::now();
        let (fi, fe) = (mi.verify(internal)?, me.verify(external)?);
        let verify_ms = ms(t);
        let (ci, ce) = (fi.cols(), fe.cols());
        let rows = ci.len() + ce.len();
        let t = Instant::now();
        // Write the report from either engine's codes; (exceptions, report_ms).
        fn write<C: Copy + Into<u32> + Sync>(report: &str, ci: &Cols<'_>, ce: &Cols<'_>, codes: (&[C], &[C]), used: &str, elapsed_ms: f64) -> Res<(u64, f64)> {
            let t = Instant::now();
            let exceptions = files::write_report_rows(report, ci, ce, codes.0, codes.1, used, elapsed_ms)?;
            Ok((exceptions, ms(t)))
        }
        let (used, context_ms, elapsed_ms, (exceptions, report_ms), gpu) = match self.engine.as_str() {
            "single" | "simd" => {
                let r = Recon { internal: (0..ci.len()).map(|i| ci.get(i)).collect(), external: (0..ce.len()).map(|i| ce.get(i)).collect() };
                let rep = if self.engine == "single" { Engine::Single.run(&r)? } else { Engine::Simd.run(&r)? };
                let elapsed_ms = ms(t);
                let used = self.engine.clone();
                let w = write(report, &ci, &ce, (&rep.internal, &rep.external), &used, elapsed_ms)?;
                (used, 0.0, elapsed_ms, w, String::new())
            }
            _ => {
                let (g, context_ms) = self.gpu_for(rows)?;
                match g {
                    #[cfg(feature = "cuda")]
                    Some(g) => {
                        let out = g.reconcile_cols(ci, ce, false).map_err(|e| format!("gpu: {e}"))?;
                        let tm = out.timings;
                        let elapsed_ms = ms(t);
                        let w = write(report, &ci, &ce, (out.internal, out.external), "gpu", elapsed_ms)?;
                        let phases = format!(
                            ",\"gpu\":{{\"validate_ms\":{:.3},\"alloc_ms\":{:.3},\"h2d_ms\":{:.3},\"kernel_ms\":{:.3},\"after_h2d_ms\":{:.3},\"d2h_ms\":{:.3},\"h2d_bytes\":{},\"device_bytes\":{}}}",
                            tm.validate_ms, tm.alloc_ms, tm.h2d_ms, tm.kernel_ms, tm.after_h2d_ms, tm.d2h_ms, tm.h2d_bytes, tm.device_bytes
                        );
                        ("gpu".to_string(), context_ms, elapsed_ms, w, phases)
                    }
                    _ => {
                        let rep = payment_reconcile::cpu_parallel_cols(ci, ce, false)?;
                        let elapsed_ms = ms(t);
                        let w = write(report, &ci, &ce, (&rep.internal, &rep.external), "parallel", elapsed_ms)?;
                        ("parallel".to_string(), context_ms, elapsed_ms, w, String::new())
                    }
                }
            }
        };
        Ok(format!(
            "{{\"event\":\"reconciliation\",\"format\":\"binary\",\"engine\":\"{used}\",\"internal_rows\":{},\"external_rows\":{},\"exceptions_count\":{exceptions},\"elapsed_ms\":{elapsed_ms:.3},\"open_ms\":{open_ms:.3},\"verify_ms\":{verify_ms:.3},\"context_ms\":{context_ms:.3},\"engine_ms\":{:.3},\"report_ms\":{report_ms:.3}{gpu},\"total_ms\":{:.3}}}",
            ci.len(),
            ce.len(),
            elapsed_ms - context_ms,
            ms(start)
        ))
    }
}

fn flags(args: &[String], from: usize, min_gpu_rows: usize) -> Res<(String, usize)> {
    let mut engine = "auto".to_string();
    let mut min = min_gpu_rows;
    let mut i = from;
    while i < args.len() {
        if i + 1 >= args.len() {
            return Err(format!("{} needs a value", args[i]).into());
        }
        match args[i].as_str() {
            "--engine" => engine = args[i + 1].clone(),
            "--min-gpu-rows" => min = args[i + 1].parse()?,
            other => return Err(format!("unknown flag {other}").into()),
        }
        i += 2;
    }
    Ok((engine, min))
}

fn main() -> Res<()> {
    let start = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // One process per job pays the CUDA context, the PTX JIT and the
        // buffers (~170 ms); below about 3M rows per side the 4-thread CPU
        // finishes first (J1, docs/benchmarks.md).
        Some("reconcile") if args.len() >= 5 => {
            let (engine, min_gpu_rows) = flags(&args, 5, 6_000_000)?;
            let mut w = Worker { engine, min_gpu_rows, gpu: None };
            println!("{}", w.reconcile(&args[2], &args[3], &args[4])?);
            Ok(())
        }
        // The context is paid once per run: the GPU's warm crossover.
        Some("jobs") if args.len() >= 3 => {
            let (engine, min_gpu_rows) = flags(&args, 3, 200_000)?;
            jobs(&args[2], Worker { engine, min_gpu_rows, gpu: None })
        }
        Some("convert") if args.len() == 4 => convert(&args[2], &args[3]),
        Some("policy") if args.len() >= 5 => {
            let (engine, min_gpu_rows) = flags(&args, 5, 2_000_000)?;
            policy(&args, &engine, min_gpu_rows, start)
        }
        _ => usage(),
    }
}

fn jobs(list: &str, mut w: Worker) -> Res<()> {
    let start = Instant::now();
    let input: Box<dyn BufRead> = if list == "-" { Box::new(std::io::stdin().lock()) } else { Box::new(std::io::BufReader::new(std::fs::File::open(list)?)) };
    let (mut ok, mut failed) = (0u64, 0u64);
    for (n, line) in input.lines().enumerate() {
        let line = line?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() || parts[0].starts_with('#') {
            continue;
        }
        let res = match parts[..] {
            [i, e, r] => w.reconcile(i, e, r),
            _ => Err(format!("{list}:{}: expected <internal> <external> <report.json>", n + 1).into()),
        };
        match res {
            Ok(log) => {
                ok += 1;
                println!("{log}");
            }
            Err(e) => {
                failed += 1;
                let msg = serde_json::Value::String(e.to_string());
                println!("{{\"event\":\"reconciliation_failed\",\"job\":{},\"error\":{msg}}}", serde_json::Value::String(line.clone()));
            }
        }
        std::io::stdout().flush()?;
    }
    println!("{{\"event\":\"reconciliation_jobs\",\"jobs\":{ok},\"failed\":{failed},\"total_ms\":{:.3}}}", ms(start));
    if failed > 0 {
        return Err(format!("{failed} job(s) failed").into());
    }
    Ok(())
}

fn convert(input: &str, output: &str) -> Res<()> {
    let start = Instant::now();
    let (to, rows) = if binfile::is_records_file(input)? {
        let f = binfile::Mapped::open(input)?.verify(input)?;
        files::write_records(std::path::Path::new(output), &f.cols())?;
        ("csv", f.rows())
    } else {
        let rows = files::read_records(input)?;
        binfile::write(std::path::Path::new(output), &rows)?;
        ("binary", rows.len())
    };
    println!("{{\"event\":\"convert\",\"to\":\"{to}\",\"rows\":{rows},\"total_ms\":{:.3}}}", ms(start));
    Ok(())
}

fn policy(args: &[String], engine: &str, min_gpu_rows: usize, start: Instant) -> Res<()> {
    let mut b = files::read_policy(&args[3])?;
    b.requests = files::read_requests(&args[2])?;
    let t = Instant::now();
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
