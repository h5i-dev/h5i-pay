//! `h5i-pay-batch`: the batch worker (AWS Batch job, or locally).
//!
//! ```text
//! h5i-pay-batch reconcile <internal.csv> <external.csv> <report.json> [--engine auto|gpu|parallel|single|simd] [--min-gpu-rows N]
//! ```
//!
//! CSV rows are `key,amount,currency,status` (no header). The report lists
//! per-code counts and every exception. It is a report for an operator:
//! the worker has no database access and no way to move money. The job
//! wrapper (`infra/docker/batch-entrypoint.sh`) copies inputs from and the
//! report to S3.
//!
//! `--engine auto` (the default) uses the GPU when this binary was built
//! with CUDA, a device answers and the batch has at least `--min-gpu-rows`
//! rows (default 200 000, from `docs/benchmarks.md`); otherwise the
//! multithreaded CPU engine. A GPU failure after selection is an error, not
//! a silent CPU retry (h5i-app-gpu's `ExecutionPolicy::Auto` rule).

#[cfg(not(feature = "cuda"))]
use h5i_app_gpu::batch::Backend;
use payment_batch::{run_reconcile, ExecutionPolicy, ReconCpu};
use payment_reconcile::{rules, Engine, Recon, Record};
use std::io::{BufRead, Write};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn read(path: &str) -> Res<Vec<Record>> {
    let f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut out = Vec::new();
    for (n, line) in f.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let mut it = line.split(',').map(str::trim);
        let mut field = |name: &str| -> Res<u64> {
            it.next().ok_or_else(|| format!("{path}:{}: missing {name}", n + 1))?.parse::<u64>().map_err(|e| format!("{path}:{}: {name}: {e}", n + 1).into())
        };
        let key = field("key")?;
        let amount = field("amount")?;
        let currency = u32::try_from(field("currency")?)?;
        let status = u32::try_from(field("status")?)?;
        out.push(Record { key, amount, currency, status });
    }
    Ok(out)
}

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

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 || args[1] != "reconcile" {
        eprintln!("usage: h5i-pay-batch reconcile <internal.csv> <external.csv> <report.json> [--engine auto|gpu|parallel|single|simd] [--min-gpu-rows N]");
        std::process::exit(2);
    }
    let mut engine = "auto".to_string();
    let mut min_gpu_rows = 200_000usize;
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
    let r = Recon { internal: read(&args[2])?, external: read(&args[3])? };
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
    let (ci, ce) = report.counts();
    let names = ["matched", "amount_mismatch", "currency_mismatch", "status_mismatch", "missing_external", "duplicate_external", "duplicate_internal", "extra_external"];
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[4])?);
    let counts = |c: [u64; rules::CODES]| names.iter().zip(c).map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(",");
    write!(out, "{{\"engine\":\"{used}\",\"elapsed_ms\":{elapsed_ms:.3},\"internal\":{{{}}},\"external\":{{{}}},\"exceptions\":[", counts(ci), counts(ce))?;
    let mut first = true;
    let mut emit = |side: &str, rec: &Record, code: u32| -> Res<()> {
        if !first {
            write!(out, ",")?;
        }
        first = false;
        write!(out, "{{\"side\":\"{side}\",\"code\":\"{}\",\"key\":{},\"amount\":{}}}", names[code as usize], rec.key, rec.amount)?;
        Ok(())
    };
    for (x, c) in r.internal.iter().zip(&report.internal) {
        if *c != rules::MATCHED {
            emit("internal", x, *c)?;
        }
    }
    for (x, c) in r.external.iter().zip(&report.external) {
        if *c != rules::MATCHED {
            emit("external", x, *c)?;
        }
    }
    writeln!(out, "]}}")?;
    eprintln!("{used}: {} internal, {} external rows, {} exceptions in {elapsed_ms:.1} ms", r.internal.len(), r.external.len(), report.exceptions());
    Ok(())
}
