#!/usr/bin/env python3
"""Summarize benchmarks/j1-batch.sh output: medians (and min-max) per rows,
format, mode, cache and engine; GPU/CPU ratio of the end-to-end total.

  benchmarks/j1-summary.py <out.jsonl>...

Long-lived rows exclude the first job of each process (it pays the CUDA
context, the JIT and the buffer allocation) and are listed separately as
"first job"."""
import json
import statistics
import sys
from collections import defaultdict

rows = [json.loads(l) for f in sys.argv[1:] for l in open(f) if l.strip()]
groups = defaultdict(list)
for r in rows:
    mode = r["mode"]
    if mode == "long-lived":
        mode = "long-lived, first job" if r["job"] == 1 else "long-lived, later jobs"
    groups[(r["rows"], r["format"], mode, r["cache"], r["engine"])].append(r)


def med(xs):
    return statistics.median(xs) if xs else float("nan")


def phase(rs, key, sub=None):
    vals = [(r[sub][key] if sub else r.get(key, 0.0)) for r in rs if (sub is None or sub in r)]
    return med(vals) if vals else None


def fmt(x):
    return "–" if x is None else f"{x:,.0f}" if x >= 100 else f"{x:.1f}"


print("| rows/side | input | mode | cache | engine | n | read ms | context ms | engine ms | (h2d / kernels / after / d2h) | report ms | total ms (min–max) | GPU/CPU |")
print("|---:|---|---|---|---|---:|---:|---:|---:|---|---:|---:|---:|")
keys = sorted(groups, key=lambda k: (k[0], k[1] != "binary", k[2], k[3], k[4] != "gpu"))
for k in keys:
    rs = groups[k]
    n, f, mode, cache, eng = k
    total = [r["total_ms"] for r in rs]
    read = med([r["open_ms"] + r.get("verify_ms", 0.0) for r in rs])
    gpu = ""
    if eng == "gpu" and "gpu" in rs[0]:
        gpu = " / ".join(fmt(phase(rs, x, "gpu")) for x in ["h2d_ms", "kernel_ms", "after_h2d_ms", "d2h_ms"])
    ratio = ""
    if eng == "gpu":
        cpu = groups.get((n, f, mode, cache, "parallel"))
        if cpu:
            ratio = f"{med([r['total_ms'] for r in cpu]) / med(total):.1f}×"
    print(
        f"| {n/1e6:g}M | {f} | {mode} | {cache} | {eng} | {len(rs)} | {fmt(read)} | {fmt(phase(rs, 'context_ms'))} | {fmt(phase(rs, 'engine_ms'))} | {gpu} | {fmt(phase(rs, 'report_ms'))} | {fmt(med(total))} ({fmt(min(total))}–{fmt(max(total))}) | {ratio} |"
    )
