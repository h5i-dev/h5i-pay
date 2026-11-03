//! `h5i-pay-batch` as a process: the binary records file and the CSV give
//! the same report (byte for byte after `engine` and `elapsed_ms`) on every
//! engine, in one-shot and `jobs` mode; `convert` round-trips; bad files
//! are refused. With `--features cuda` the GPU engine is included (and must
//! find a device).

use payment_batch::{binfile, files, synth};
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_h5i-pay-batch");

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("h5i-pay-worker-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(BIN).args(args).output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// The report without its `engine` and `elapsed_ms` fields.
fn body(p: &Path) -> String {
    let r = std::fs::read_to_string(p).unwrap();
    r[r.find("\"internal\":").expect("report body")..].to_string()
}

fn engines() -> Vec<&'static str> {
    let mut e = vec!["parallel", "single", "simd"];
    if payment_gpu::BUILT_WITH_CUDA {
        e.push("gpu");
    }
    e
}

#[test]
fn binary_and_csv_give_the_same_report() {
    let d = dir("same");
    let r = synth::recon(300_000, 21);
    files::write_records(&d.join("i.csv"), &r.internal).unwrap();
    files::write_records(&d.join("e.csv"), &r.external).unwrap();
    binfile::write(&d.join("i.h5ib"), &r.internal).unwrap();
    binfile::write(&d.join("e.h5ib"), &r.external).unwrap();
    let want = d.join("want.json");
    let rep = payment_reconcile::reference(&r).unwrap();
    files::write_report(s(&want), &r, &rep, "reference", 0.0).unwrap();
    let want = body(&want);
    assert!(want.contains("amount_mismatch") && want.contains("extra_external"));
    let mut jobs = String::new();
    for engine in engines() {
        for fmt in ["csv", "h5ib"] {
            let out = d.join(format!("{engine}-{fmt}.json"));
            let (ok, log) = run(&["reconcile", s(&d.join(format!("i.{fmt}"))), s(&d.join(format!("e.{fmt}"))), s(&out), "--engine", engine]);
            assert!(ok, "{engine} {fmt}: {log}");
            assert!(log.contains(&format!("\"engine\":\"{engine}\"")), "{log}");
            assert_eq!(body(&out), want, "{engine} {fmt}");
            jobs.push_str(&format!("{}\t{} {}\n", s(&d.join(format!("i.{fmt}"))), s(&d.join(format!("e.{fmt}"))), s(&d.join(format!("job-{engine}-{fmt}.json")))));
        }
    }
    // Mixed formats, and `auto` (CPU below --min-gpu-rows).
    let out = d.join("mixed.json");
    let (ok, log) = run(&["reconcile", s(&d.join("i.h5ib")), s(&d.join("e.csv")), s(&out), "--min-gpu-rows", "100000000"]);
    assert!(ok && log.contains("\"engine\":\"parallel\""), "{log}");
    assert_eq!(body(&out), want);
    // `jobs`: every pair in one process, per engine.
    for engine in engines() {
        let list = d.join(format!("jobs-{engine}.txt"));
        let mine: String = jobs.lines().filter(|l| l.contains(&format!("job-{engine}-"))).map(|l| format!("{l}\n")).collect();
        std::fs::write(&list, format!("# comment\n\n{mine}")).unwrap();
        let (ok, log) = run(&["jobs", s(&list), "--engine", engine]);
        assert!(ok, "{log}");
        assert!(log.contains("\"event\":\"reconciliation_jobs\",\"jobs\":2,\"failed\":0"), "{log}");
        for fmt in ["csv", "h5ib"] {
            assert_eq!(body(&d.join(format!("job-{engine}-{fmt}.json"))), want, "jobs {engine} {fmt}");
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn convert_round_trips_and_bad_files_are_refused() {
    let d = dir("convert");
    let r = synth::recon(50_000, 5);
    files::write_records(&d.join("e.csv"), &r.external).unwrap();
    let (ok, log) = run(&["convert", s(&d.join("e.csv")), s(&d.join("e.h5ib"))]);
    assert!(ok && log.contains("\"to\":\"binary\""), "{log}");
    let f = binfile::Mapped::open(s(&d.join("e.h5ib"))).unwrap().verify(s(&d.join("e.h5ib"))).unwrap();
    let c = f.cols();
    assert_eq!((0..c.len()).map(|i| c.get(i)).collect::<Vec<_>>(), r.external);
    let (ok, log) = run(&["convert", s(&d.join("e.h5ib")), s(&d.join("back.csv"))]);
    assert!(ok && log.contains("\"to\":\"csv\""), "{log}");
    assert_eq!(std::fs::read(d.join("back.csv")).unwrap(), std::fs::read(d.join("e.csv")).unwrap());

    // A truncated file, a flipped byte, and a CSV error with its line number.
    let good = std::fs::read(d.join("e.h5ib")).unwrap();
    std::fs::write(d.join("short.h5ib"), &good[..good.len() - 4]).unwrap();
    let mut flipped = good.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    std::fs::write(d.join("flipped.h5ib"), &flipped).unwrap();
    std::fs::write(d.join("bad.csv"), "1,2,392,0\n\n3,x,392,0\n").unwrap();
    for (bad, msg) in [("short.h5ib", "Length"), ("flipped.h5ib", "DataChecksum"), ("bad.csv", "bad.csv:3: amount")] {
        for engine in engines() {
            let (ok, log) = run(&["reconcile", s(&d.join(bad)), s(&d.join("e.h5ib")), s(&d.join("x.json")), "--engine", engine]);
            assert!(!ok && log.contains(msg), "{bad} {engine}: {log}");
        }
    }
    // In `jobs` mode a bad job fails the run, the others still run.
    let list = d.join("jobs.txt");
    std::fs::write(&list, format!("{} {} {}\n{} {} {}\n", s(&d.join("short.h5ib")), s(&d.join("e.h5ib")), s(&d.join("j1.json")), s(&d.join("e.h5ib")), s(&d.join("e.h5ib")), s(&d.join("j2.json")))).unwrap();
    let (ok, log) = run(&["jobs", s(&list), "--engine", "parallel"]);
    assert!(!ok && log.contains("reconciliation_failed") && log.contains("\"jobs\":1,\"failed\":1"), "{log}");
    assert!(d.join("j2.json").exists() && !d.join("j1.json").exists());
    let _ = std::fs::remove_dir_all(&d);
}
