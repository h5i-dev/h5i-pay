//! `bench`: measure the batch engines.
//!
//! ```text
//! bench one <recon|policy> <single|parallel|simd|gpu> <rows> [runs]   # one measurement, JSON on stdout
//! bench all [--sizes 100000,1000000,10000000] [--out benchmarks/results/latest.json]
//! ```
//!
//! `all` runs every (workload, engine, size) in a fresh child process, so
//! the peak-memory figure of one engine is not inflated by another, and
//! writes the JSON results and a Markdown table.
//!
//! One run is one call of the engine on the whole batch, input already in
//! host memory. Reported: p50/p95 of the run time over `runs` runs (after
//! one warm-up), throughput at p50 (rows/s, counting both sides for
//! reconciliation), for the GPU the host→device / kernel / device→host
//! split (p50), the peak RSS increase of the first run over the input
//! (`/proc/self/clear_refs` reset after the input is built; for the GPU
//! after the CUDA context exists, so it includes the PTX JIT), the device
//! memory the GPU engine allocated, and cost per million rows at the
//! g5.xlarge on-demand price. Every engine's output is checked against
//! `cpu_single` / the verified `check` on the same input, so a fast wrong
//! engine fails the benchmark instead of winning it.

use h5i_app_gpu::batch::Backend;
use payment_batch::{synth, PolicyParallel, PolicySimd, PolicySingle, ReconCpu};
use payment_reconcile::Engine;
use serde_json::{json, Value};
use std::time::Instant;

/// g5.xlarge (4 vCPU AMD EPYC 7R32, 1× A10G), us-east-1 on-demand, USD/hour.
const PRICE_PER_HOUR: f64 = 1.006;

fn rss_kib(field: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i]
}

#[derive(Default)]
struct Split {
    h2d: Vec<f64>,
    kernel: Vec<f64>,
    d2h: Vec<f64>,
    device_bytes: usize,
}

// Without the `gpu` feature the GPU arm (and its use of `split`) is compiled out.
#[cfg_attr(not(feature = "gpu"), allow(unused_mut, unused_variables))]
fn one(workload: &str, engine: &str, rows: usize, runs: usize) -> Value {
    let seed = 0x5eed ^ rows as u64;
    let mut times = Vec::new();
    let mut split = Split::default();
    let counted;
    let input_bytes;
    let peak_kib;
    match workload {
        "recon" => {
            let r = synth::recon(rows, seed);
            counted = r.internal.len() + r.external.len();
            input_bytes = counted * std::mem::size_of::<payment_reconcile::Record>();
            #[cfg(feature = "gpu")]
            let mut g = if engine == "gpu" { Some(payment_gpu::Gpu::new().expect("CUDA device")) } else { None };
            let mut run = |split: &mut Split| -> payment_reconcile::Report {
                match engine {
                    "single" => ReconCpu(Engine::Single).run((), &r).unwrap(),
                    "parallel" => ReconCpu(Engine::Parallel).run((), &r).unwrap(),
                    "simd" => ReconCpu(Engine::Simd).run((), &r).unwrap(),
                    #[cfg(feature = "gpu")]
                    "gpu" => {
                        let g = g.as_mut().unwrap();
                        let out = g.run((), &r).unwrap();
                        split.h2d.push(g.last.h2d_ms);
                        split.kernel.push(f64::from(g.last.kernel_ms));
                        split.d2h.push(g.last.d2h_ms);
                        split.device_bytes = g.last.device_bytes;
                        out
                    }
                    other => panic!("unknown engine {other}"),
                }
            };
            // First run: peak memory over the input (the GPU context exists
            // already; the PTX JIT happens here), then the correctness check.
            let mut warm = Split::default();
            reset_peak();
            let base = rss_kib("VmRSS:");
            let first = run(&mut warm);
            peak_kib = rss_kib("VmHWM:").saturating_sub(base);
            assert!(first == payment_reconcile::cpu_single(&r).unwrap(), "{engine} output differs from cpu_single");
            drop(first);
            for _ in 0..runs {
                let t = Instant::now();
                let out = run(&mut split);
                times.push(t.elapsed().as_secs_f64() * 1e3);
                drop(out);
            }
        }
        "policy" => {
            let b = synth::policy_batch(rows, seed);
            counted = b.requests.len();
            input_bytes = counted * std::mem::size_of::<payment_domain::policy::PaymentReq>();
            #[cfg(feature = "gpu")]
            let mut g = if engine == "gpu" { Some(payment_gpu::Gpu::new().expect("CUDA device")) } else { None };
            let mut run = |split: &mut Split| -> Vec<u32> {
                match engine {
                    "single" => PolicySingle.run((), &b).unwrap(),
                    "parallel" => PolicyParallel.run((), &b).unwrap(),
                    "simd" => PolicySimd.run((), &b).unwrap(),
                    #[cfg(feature = "gpu")]
                    "gpu" => {
                        let g = g.as_mut().unwrap();
                        let out = g.run((), &b).unwrap();
                        split.h2d.push(g.last.h2d_ms);
                        split.kernel.push(f64::from(g.last.kernel_ms));
                        split.d2h.push(g.last.d2h_ms);
                        split.device_bytes = g.last.device_bytes;
                        out
                    }
                    other => panic!("unknown engine {other}"),
                }
            };
            let mut warm = Split::default();
            reset_peak();
            let base = rss_kib("VmRSS:");
            let first = run(&mut warm);
            peak_kib = rss_kib("VmHWM:").saturating_sub(base);
            assert!(first == PolicySingle.run((), &b).unwrap(), "{engine} output differs from the verified check");
            drop(first);
            for _ in 0..runs {
                let t = Instant::now();
                let out = run(&mut split);
                times.push(t.elapsed().as_secs_f64() * 1e3);
                drop(out);
            }
        }
        other => panic!("unknown workload {other}"),
    }
    report(workload, engine, rows, counted, input_bytes, times, split, peak_kib)
}

#[allow(clippy::too_many_arguments)]
fn report(workload: &str, engine: &str, rows: usize, counted: usize, input_bytes: usize, mut times: Vec<f64>, mut split: Split, peak_kib: u64) -> Value {
    let p50 = pct(&mut times, 0.5);
    let p95 = pct(&mut times, 0.95);
    let mut gpu = Value::Null;
    if !split.kernel.is_empty() {
        gpu = json!({
            "h2d_ms_p50": pct(&mut split.h2d, 0.5),
            "kernel_ms_p50": pct(&mut split.kernel, 0.5),
            "d2h_ms_p50": pct(&mut split.d2h, 0.5),
            "device_mib": split.device_bytes as f64 / (1 << 20) as f64,
        });
    }
    json!({
        "workload": workload,
        "engine": engine,
        "rows": rows,
        "counted_rows": counted,
        "input_mib": input_bytes as f64 / (1 << 20) as f64,
        "runs": times.len(),
        "p50_ms": p50,
        "p95_ms": p95,
        "rows_per_s": counted as f64 / (p50 / 1e3),
        "peak_rss_increase_mib": peak_kib as f64 / 1024.0,
        "usd_per_million_rows": PRICE_PER_HOUR / 3600.0 * (p50 / 1e3) / (counted as f64 / 1e6),
        "gpu": gpu,
        "threads": rayon::current_num_threads(),
    })
}

fn runs_for(rows: usize) -> usize {
    match rows {
        0..=200_000 => 21,
        200_001..=2_000_000 => 11,
        _ => 5,
    }
}

fn markdown(results: &[Value]) -> String {
    let mut s = String::new();
    for w in ["recon", "policy"] {
        s.push_str(&format!(
            "\n### {}\n\n| rows | engine | p50 ms | p95 ms | Mrows/s | vs single | H2D / kernel / D2H ms | peak RSS +MiB | device MiB | $ / M rows |\n|---:|---|---:|---:|---:|---:|---|---:|---:|---:|\n",
            if w == "recon" { "Reconciliation (internal + external rows)" } else { "Batch policy validation" }
        ));
        for r in results.iter().filter(|r| r["workload"] == w) {
            let single = results
                .iter()
                .find(|x| x["workload"] == w && x["rows"] == r["rows"] && x["engine"] == "single")
                .and_then(|x| x["p50_ms"].as_f64())
                .unwrap_or(f64::NAN);
            let p50 = r["p50_ms"].as_f64().unwrap();
            let g = &r["gpu"];
            let split = if g.is_null() {
                "–".to_string()
            } else {
                format!("{:.2} / {:.2} / {:.2}", g["h2d_ms_p50"].as_f64().unwrap(), g["kernel_ms_p50"].as_f64().unwrap(), g["d2h_ms_p50"].as_f64().unwrap())
            };
            s.push_str(&format!(
                "| {} | {} | {:.2} | {:.2} | {:.1} | {:.2}× | {} | {:.0} | {} | {:.6} |\n",
                r["rows"],
                r["engine"].as_str().unwrap(),
                p50,
                r["p95_ms"].as_f64().unwrap(),
                r["rows_per_s"].as_f64().unwrap() / 1e6,
                single / p50,
                split,
                r["peak_rss_increase_mib"].as_f64().unwrap(),
                if g.is_null() { "–".to_string() } else { format!("{:.0}", g["device_mib"].as_f64().unwrap()) },
                r["usd_per_million_rows"].as_f64().unwrap(),
            ));
        }
    }
    s
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("one") => {
            let rows: usize = args[4].parse().unwrap();
            let runs = args.get(5).map(|r| r.parse().unwrap()).unwrap_or_else(|| runs_for(rows));
            println!("{}", one(&args[2], &args[3], rows, runs));
        }
        #[cfg(feature = "gpu")]
        Some("init") => {
            // One-time cost of a GPU worker: CUDA context and PTX JIT.
            let t = Instant::now();
            let g = payment_gpu::Gpu::new().expect("CUDA device");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let (free, total) = payment_gpu::Gpu::mem_info().unwrap();
            drop(g);
            println!("{}", json!({"gpu_init_ms": ms, "device_free_mib": free >> 20, "device_total_mib": total >> 20}));
        }
        Some("gen-files") => {
            // J1: synthetic reconciliation inputs as files for the worker:
            // <dir>/i.h5ib, e.h5ib (binary records) and, with --csv, i.csv, e.csv.
            let rows: usize = args[2].parse().unwrap();
            let dir = std::path::Path::new(&args[3]);
            std::fs::create_dir_all(dir).unwrap();
            let t = Instant::now();
            let r = payment_batch::synth::recon(rows, rows as u64);
            payment_batch::binfile::write(&dir.join("i.h5ib"), &r.internal).unwrap();
            payment_batch::binfile::write(&dir.join("e.h5ib"), &r.external).unwrap();
            if args.get(4).map(String::as_str) == Some("--csv") {
                payment_batch::files::write_records(&dir.join("i.csv"), &r.internal).unwrap();
                payment_batch::files::write_records(&dir.join("e.csv"), &r.external).unwrap();
            }
            println!("{}", json!({"rows": rows, "internal": r.internal.len(), "external": r.external.len(), "ms": t.elapsed().as_secs_f64() * 1e3}));
        }
        Some("all") => {
            let mut sizes = vec![10_000usize, 100_000, 1_000_000, 10_000_000];
            let mut out = "benchmarks/results/latest.json".to_string();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--sizes" => {
                        sizes = args[i + 1].split(',').map(|x| x.parse().unwrap()).collect();
                        i += 2;
                    }
                    "--out" => {
                        out = args[i + 1].clone();
                        i += 2;
                    }
                    other => panic!("unknown flag {other}"),
                }
            }
            let mut engines = vec!["single", "parallel", "simd"];
            if payment_gpu::BUILT_WITH_CUDA {
                engines.push("gpu");
            }
            let me = std::env::current_exe().unwrap();
            let mut results = Vec::new();
            for w in ["recon", "policy"] {
                for &n in &sizes {
                    for e in &engines {
                        let o = std::process::Command::new(&me).args(["one", w, e, &n.to_string()]).output().unwrap();
                        assert!(o.status.success(), "{w} {e} {n}: {}", String::from_utf8_lossy(&o.stderr));
                        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
                        eprintln!("{w:6} {e:8} {n:>10}  p50 {:>10.2} ms  {:>8.1} Mrows/s", v["p50_ms"].as_f64().unwrap(), v["rows_per_s"].as_f64().unwrap() / 1e6);
                        results.push(v);
                    }
                }
            }
            let doc = json!({ "machine": machine(), "results": results });
            std::fs::write(&out, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
            print!("{}", markdown(&results));
        }
        _ => {
            eprintln!("usage: bench one <recon|policy> <single|parallel|simd|gpu> <rows> [runs] | bench all [--sizes a,b,c] [--out file]");
            std::process::exit(2);
        }
    }
}

fn machine() -> Value {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_default();
    let gpu = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name,driver_version", "--format=csv,noheader"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    json!({ "cpu": cpu, "threads": rayon::current_num_threads(), "gpu": gpu, "price_usd_per_hour": PRICE_PER_HOUR, "instance": "g5.xlarge" })
}
