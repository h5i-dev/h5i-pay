# Batch engine benchmarks (M3)

Four implementations of each batch workload, measured on the same machine
and the same inputs, with every engine's output checked against the
reference before it is timed. Raw results:
[`benchmarks/results/g5.xlarge.json`](../benchmarks/results/g5.xlarge.json)
(table: [`g5.xlarge.md`](../benchmarks/results/g5.xlarge.md)).

## Setup

| | |
|---|---|
| Instance | AWS g5.xlarge: 4 vCPU AMD EPYC 7R32 (2 cores × SMT), 16 GiB, 1× NVIDIA A10G 24 GiB, driver 595.112, CUDA 13.2 |
| Price used for cost | $1.006/hour, g5.xlarge on-demand in us-east-1 ([Vantage](https://instances.vantage.sh/aws/ec2/g5.xlarge), [devzero](https://www.devzero.io/instances/aws/g5.xlarge)) |
| Build | `cargo build --release` (Rust 1.99.1, `debug = 1`), kernels `nvcc -O3 -arch=compute_75` (JIT to sm_86) |
| Runs | one warm-up run, then 21 (≤ 200k rows), 11 (≤ 2M) or 5 (10M) timed runs; p50 and p95 of whole-batch time |
| Input | synthetic, deterministic (`payment_batch::synth`); input already in host memory |
| Command | `cargo run --release -p h5i-pay-benchmarks --features gpu -- all --out benchmarks/results/g5.xlarge.json` |

Engines:

- **single** — one thread: `cpu_single` (open-addressing hash join) and the
  verified `policy::check` per row.
- **parallel** — rayon on 4 threads: radix-partition by key hash, then one
  hash join per partition; `check` with `par_iter`.
- **simd** — one thread, AVX2: reconciliation probes four rows per step
  with 64-bit gathers; policy evaluates every rule for four rows as lane
  masks and blends the first failing code.
- **gpu** — CUDA kernels through h5i-app-gpu's `backend-nvvm` (`cust`):
  hash tables built with `atomicCAS`/`atomicAdd`/`atomicMin`, one thread
  per row for probes and policy checks. The time includes allocation,
  host→device copies (pageable memory), table initialization, all kernels
  and device→host copies. It does not include creating the CUDA context and
  JIT-compiling the PTX, which costs **136 ms once per process**
  (`bench init`, three runs: 137.8, 136.3, 135.6 ms).

Rows: for reconciliation, *rows* is the internal side; the external side
has about the same number (discrepancies add or drop ~1%), and throughput
counts both sides. The synthetic report has about 1% amount, 0.2%
currency and 0.5% status mismatches, 0.5% missing, 0.5% extra and 0.1%
duplicated rows, and is shuffled. The policy batch draws from a policy with
4 currencies, a 2,000-merchant allowlist and 32 blocked recipients, and
contains every decision code.

## Results

### Reconciliation

| rows | engine | p50 ms | p95 ms | Mrows/s | vs single | H2D / kernel / D2H ms | peak RSS +MiB | device MiB | $ / M rows |
|---:|---|---:|---:|---:|---:|---|---:|---:|---:|
| 10 000 | single | 0.90 | 0.96 | 22.2 | 1.00× | – | 1 | – | 0.000013 |
| 10 000 | parallel | **0.35** | 0.40 | 57.4 | 2.58× | – | 1 | – | 0.000005 |
| 10 000 | simd | 1.22 | 1.31 | 16.4 | 0.74× | – | 1 | – | 0.000017 |
| 10 000 | gpu | 0.39 | 0.41 | 51.7 | 2.33× | 0.19 / 0.02 / 0.05 | 1 | 2 | 0.000005 |
| 100 000 | single | 12.75 | 13.08 | 15.7 | 1.00× | – | 9 | – | 0.000018 |
| 100 000 | parallel | 3.67 | 4.15 | 54.5 | 3.47× | – | 3 | – | 0.000005 |
| 100 000 | simd | 17.89 | 18.87 | 11.2 | 0.71× | – | 9 | – | 0.000025 |
| 100 000 | gpu | **2.16** | 2.21 | 92.7 | 5.90× | 0.95 / 0.13 / 0.27 | 1 | 14 | 0.000003 |
| 1 000 000 | single | 308.66 | 325.51 | 6.5 | 1.00× | – | 75 | – | 0.000043 |
| 1 000 000 | parallel | 88.57 | 90.52 | 22.6 | 3.48× | – | 27 | – | 0.000012 |
| 1 000 000 | simd | 477.23 | 481.33 | 4.2 | 0.65× | – | 75 | – | 0.000067 |
| 1 000 000 | gpu | **16.87** | 22.29 | 118.7 | 18.30× | 4.97 / 4.09 / 1.87 | 12 | 121 | 0.000002 |
| 10 000 000 | single | 3964.33 | 3999.51 | 5.0 | 1.00× | – | 1138 | – | 0.000055 |
| 10 000 000 | parallel | 1726.26 | 1815.62 | 11.6 | 2.30× | – | 319 | – | 0.000024 |
| 10 000 000 | simd | 5834.50 | 5855.56 | 3.4 | 0.68× | – | 1139 | – | 0.000081 |
| 10 000 000 | gpu | **199.78** | 200.34 | 100.2 | 19.84× | 39.82 / 48.18 / 68.56 | 115 | 1597 | 0.000003 |

### Batch policy validation

| rows | engine | p50 ms | p95 ms | Mrows/s | vs single | H2D / kernel / D2H ms | peak RSS +MiB | device MiB | $ / M rows |
|---:|---|---:|---:|---:|---:|---|---:|---:|---:|
| 10 000 | single | 0.62 | 0.64 | 16.0 | 1.00× | – | 0 | – | 0.000017 |
| 10 000 | parallel | 0.41 | 0.46 | 24.6 | 1.54× | – | 0 | – | 0.000011 |
| 10 000 | simd | 0.32 | 0.33 | 30.9 | 1.93× | – | 0 | – | 0.000009 |
| 10 000 | gpu | **0.29** | 0.32 | 34.7 | 2.16× | 0.16 / 0.02 / 0.02 | 0 | 0 | 0.000008 |
| 100 000 | single | 6.09 | 7.15 | 16.4 | 1.00× | – | 1 | – | 0.000017 |
| 100 000 | parallel | 1.94 | 2.78 | 51.4 | 3.13× | – | 1 | – | 0.000005 |
| 100 000 | simd | 3.25 | 3.30 | 30.8 | 1.88× | – | 0 | – | 0.000009 |
| 100 000 | gpu | **0.77** | 0.82 | 129.3 | 7.88× | 0.45 / 0.06 / 0.09 | 1 | 3 | 0.000002 |
| 1 000 000 | single | 59.93 | 63.19 | 16.7 | 1.00× | – | 4 | – | 0.000017 |
| 1 000 000 | parallel | 18.94 | 20.59 | 52.8 | 3.16× | – | 4 | – | 0.000005 |
| 1 000 000 | simd | 34.69 | 37.27 | 28.8 | 1.73× | – | 4 | – | 0.000010 |
| 1 000 000 | gpu | **4.86** | 6.24 | 205.9 | 12.34× | 3.55 / 0.29 / 0.63 | 4 | 34 | 0.000001 |
| 10 000 000 | single | 617.84 | 621.51 | 16.2 | 1.00× | – | 38 | – | 0.000017 |
| 10 000 000 | parallel | 193.07 | 203.67 | 51.8 | 3.20× | – | 38 | – | 0.000005 |
| 10 000 000 | simd | 366.67 | 368.14 | 27.3 | 1.69× | – | 38 | – | 0.000010 |
| 10 000 000 | gpu | **57.59** | 57.68 | 173.7 | 10.73× | 29.98 / 2.55 / 23.32 | 38 | 343 | 0.000002 |

"peak RSS +MiB" is the increase of the process's peak resident set during
the first run over the resident set after the input was built (for the GPU
after the context existed). For policy validation the increase is the
output vector itself (4 bytes per request), the same in every engine.

## What the numbers say

**Where the GPU wins.** With the CUDA context already up (a long-lived
worker), the GPU is the fastest engine for reconciliation from about
100,000 rows per side (1.7× the 4-thread CPU engine at 100k, 5.2× at 1M,
8.6× at 10M) and for policy validation from about 100,000 requests (2.5×,
3.9×, 3.4×). At 10,000 rows all engines finish in under a millisecond and
the 4-thread CPU engine matches or beats the GPU for reconciliation.

**Where it does not.**

- *One-shot jobs pay 136 ms for the CUDA context and PTX JIT.* An AWS Batch
  job that starts a process per settlement file is faster on the CPU below
  roughly 2–3 million rows per side: at 1M rows the GPU job takes
  17 + 136 ≈ 153 ms against 89 ms on 4 CPU threads; at 10M it takes
  200 + 136 ≈ 336 ms against 1,726 ms. The worker therefore uses the GPU
  only above `--min-gpu-rows` (default 200,000) and only on a GPU instance.
- *Policy validation is transfer-bound.* At 10M requests the kernel runs
  2.55 ms of the 57.6 ms; the rest is copying 320 MB in and 40 MB out
  through pageable memory. The GPU only pays off when requests arrive in
  large batches, and would gain most from pinned buffers or from data that
  already lives on the device.
- *Reconciliation's device→host copy is the largest part at 10M*
  (68.6 ms for three 40 MB output columns, about 1.7 GB/s from pageable
  memory). Narrower codes (u8) and pinned buffers are the obvious next
  steps; neither is implemented.
- *The SIMD reconciliation engine is slower than the scalar one*
  (0.65–0.74×). Its probe loop gathers four table slots at a time; on this
  Zen 2 CPU those gathers are microcoded and, it appears, slower than the
  scalar loads an out-of-order core already overlaps. The SIMD policy engine helps
  (1.7–1.9×), because there the work is comparisons on values already in
  registers. Both are kept as measured baselines, not recommendations.
- *Four vCPUs are two cores.* The parallel engines scale 3.1–3.5× on
  in-cache sizes and 2.3× for 10M-row reconciliation, which is bound by
  memory bandwidth and the partition pass. A larger CPU instance would
  narrow the GPU's lead; it was not measured.
- *The batch is rarely the bottleneck end to end.* `POST /v1/reconcile`
  loads the tenant's payments and ledger snapshots from PostgreSQL and
  fetches the PSP report before any engine runs; for tenants of the sizes
  above, that dominates.
- *Cost per million rows is negligible for every engine* (below $0.0001 at
  the g5.xlarge price). What costs money is a GPU instance that idles: the
  AWS Batch compute environment scales GPU capacity from zero per job.

**Memory.** The GPU engine keeps host memory flat (+115 MiB at 10M rows,
mostly the output columns) and uses 1.6 GiB of device memory at 10M rows
per side (inputs, two hash tables at ≤ 50% load, outputs). The single-thread
CPU engine needs +1.1 GiB of host memory for the same input.

## Correctness of the comparison

Before timing, each engine's output is compared with `cpu_single` (itself
checked against the `BTreeMap` reference in `tests/differential`) or with
the verified `policy::check`; the benchmark aborts on any difference. The
differential suite (`cargo test -p h5i-pay-tests --features gpu --test
differential`) additionally compares every engine, row by row, on empty
batches, 255/256/257-row block boundaries, a hot key shared by thousands of
rows (atomic contention), hash-colliding keys, all-missing and all-extra
sides, malformed policies and unsorted allowlists, and checks repeat
determinism and partition independence (h5i-app-gpu `check_batch`). This is
evidence for the inputs tested, not a proof of CPU/GPU equivalence
(`docs/proof-assumptions.md`).

The GPU never commits financial state: reconciliation output is a report,
and an approved policy decision is only a candidate that the ledger kernel
still has to accept in a SERIALIZABLE transaction.
