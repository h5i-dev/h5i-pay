# Benchmarks

Two parts: **end to end** (B4: the API, the scheduled recovery and the
batch worker through PostgreSQL, files and the PSP adapter, before and
after the B1–B3 data-path changes), and the **batch engines** alone (M3,
re-run after the GPU kernels became Rust in A). The end-to-end numbers say
where the time goes; read them first.

## End to end (B4)

Harness: `benchmarks/src/bin/e2e.rs` (`e2e seed` / `e2e measure`), raw
results in [`benchmarks/results/e2e-g5.xlarge.json`](../benchmarks/results/e2e-g5.xlarge.json).
One tenant with 1k, 10k or 100k captured payments (one in ten partly
refunded), seeded through the real orchestrator; each measurement runs on a
fresh copy of that database. The API runs in-process with the Mock PSP
(no network); PostgreSQL 17 in Docker on the same host. "Before" is the
same harness built at `e88c83f` (whole-tenant load per command, the old
recovery and orchestrator); "after" is `685abb0` (B1–B3). The seeded
databases have the B1 indexes in both cases (they do not help whole-tenant
loads). Caveat: another project's Lean builds kept 1–2 of the 4 vCPUs busy
during these runs (load average 3–6), so absolute times are pessimistic;
the before/after ratios are measured under the same conditions.

| | 1k before | 1k after | 10k before | 10k after | 100k after |
|---|---:|---:|---:|---:|---:|
| `POST /v1/payments` (auto capture), p50 ms | 52.9 | 17.6 | 461.2 | 15.9 | 19.4 |
| … p95 ms | 55.8 | 18.2 | 528.0 | 17.8 | 23.5 |
| … kernel transactions per payment | 11 | 7 | 11 | 7 | 7 |
| `GET /v1/payments/{id}`, p50 ms (transactions) | 13.24 (3) | 0.83 (1) | 113.5 (3) | 0.83 (1) | 0.96 (1) |
| `POST …/refunds`, p50 ms (transactions) | 60.3 (13) | 19.3 (7) | 522.5 (13) | 19.5 (7) | 22.5 (7) |
| 8 concurrent clients, payments/s | 18.3 | 164.0 | 2.2 | 158.5 | 123.2 |
| … serialization retries (whole run) | 0 | 132 | 0 | 108 | 88 |
| recovery pass, 5 payments to fix: ms | 31,805 | 65 | 1,771,638 (29.5 min) | 619 | 1,002 |
| … kernel transactions / PSP calls | 6,319 / 5 | 45 / 5 | 52,219 / 5 | 45 / 5 | 45 / 5 |
| recovery pass, nothing to fix: ms (transactions) | 33,034 (6,311) | 10 (0) | – | 78 (0) | 852 (0) |
| `POST /v1/reconcile` (both reconciliations), p50 ms | 7.8 | 10.3 | 68.5 | 65.4 | 583.1 |

Before, every command loaded the whole tenant, so a payment cost
O(tenant) and the time grew linearly with the tenant (53 → 461 ms from 1k
to 10k payments); writes took the tenant lock, so concurrency did not help
(2.2 payments/s at 10k). After, a payment costs the same at 1k, 10k and
100k (16–19 ms, seven SERIALIZABLE transactions, each reading its keys);
concurrent writes queue on per-key locks and retry on conflicts instead
(88–132 retries per 200 payments). Before at 100k was not measured: at the
measured linear growth a single payment would take about 4.6 s.

The recovery pass before re-synced every captured payment's ledger postings
on every pass (6,311 transactions with nothing to do at 1k; at 10k one pass
took 29.5 minutes and 52,219 transactions, measured in a separate run with
`E2E_RECOVERY_PASSES=1`, longer than its own 5-minute schedule). After, one
read-only query selects the unfinished work. It still costs O(captured
payments) inside PostgreSQL (the anti-join on the ledger's transfer ids):
10 ms at 1k, 78 ms at 10k, 852 ms at 100k payments with nothing to fix.
Since F2 a per-payment "ledger complete" marker makes it O(unfinished):
7.8 ms at 100k with nothing to fix (see "After F" below).

`POST /v1/reconcile` is unchanged by B1–B3 and is O(tenant) by nature: it
loads both kernels' full snapshots and the PSP report, classifies on one
CPU thread and renders every exception (110k at 100k payments, since the
Mock PSP of the measurement does not know the seeded payments). 583 ms at
100k payments is the reason large tenants go to the nightly batch job.

### The batch worker, with its files

`h5i-pay-batch reconcile` on CSV files in the page cache (local disk, not
S3), 1% amount mismatches and 0.5% missing rows; `elapsed_ms` is the
engine (including CUDA context creation for the GPU), `total_ms` the whole
process (CSV parsing and report writing included):

| rows per side | engine | engine ms | process ms |
|---:|---|---:|---:|
| 1M | gpu | 266 | 899 |
| 1M | parallel | 100 | 738 |
| 10M | gpu | 477 | 6,695 |
| 10M | parallel | 2,219 | 8,974 |

Parsing the two CSV files is most of a job: about 0.6 s at 1M rows and
6.2 s at 10M (single-threaded `str::parse`). The GPU saves 1.7 s of 9 s at
10M rows; at 1M a one-shot job is faster on the CPU, which is why the
worker's `--min-gpu-rows` default is now 2,000,000. `h5i-pay export`
(PostgreSQL snapshot to CSV) took 0.09 s at 10k and 0.51 s at 100k
payments. S3 transfer times were not measured (no AWS account here); at
the 10M-row size the two files are 580 MB.

Bulk payout screening (`h5i-pay-batch policy`, 300k requests): GPU 209 ms
engine time, almost all context creation; 4-thread CPU 2.5 ms.

**Conclusions.** For the online path the bottleneck was the data path,
not computation: B1–B3 made a payment 3× faster at 1k payments and 29×
at 10k, and independent of tenant size. For the batch path the GPU is the
fastest engine from about 100k rows when its context is warm, but a
one-shot job spends most of its time parsing text; binary inputs (or a
long-lived worker) would matter more than any engine change.

## Batch engines (M3, re-run after A)

Four implementations of each batch workload, measured on the same machine
and the same inputs, with every engine's output checked against the
reference before it is timed. Raw results:
[`benchmarks/results/g5.xlarge.json`](../benchmarks/results/g5.xlarge.json)
(table: [`g5.xlarge.md`](../benchmarks/results/g5.xlarge.md); M3, quiet
host, CUDA C kernels from nvcc).

**After A** (kernels compiled from the Rust source by rustc_codegen_nvvm):
[`g5.xlarge-nvvm.json`](../benchmarks/results/g5.xlarge-nvvm.json)
([table](../benchmarks/results/g5.xlarge-nvvm.md)), same command, but with
2 of 4 vCPUs busy with another workload, so its CPU rows are slower than
M3's and only the GPU rows compare. GPU p50, nvcc → nvvm:
reconciliation 1M 16.9 → 17.4 ms (kernels 4.09 → 4.19), 10M 199.8 → 202.0
ms (kernels 48.2 → 49.4); policy 1M 4.86 → 5.21 ms (kernel 0.29 → 0.54),
10M 57.6 → 62.3 ms (kernel 2.55 → 5.00). The reconciliation kernels are
as fast as the CUDA C; the policy kernel takes twice as long (the Rust
`check` with its bounds-checked slices and binary search), which moves
the end-to-end policy time by 7% because transfers dominate. Context and
JIT: 150 ms (`bench init`; 136 ms with the nvcc PTX). The tables below are
M3's.

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
  only above `--min-gpu-rows` (default 2,000,000 since B4; it was 200,000)
  and only on a GPU instance. With CSV parsing included the break-even
  moves further up (see "The batch worker, with its files").
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
- *The batch is rarely the bottleneck end to end*: measured in B4 above
  (CSV parsing in the worker, snapshot loading in `POST /v1/reconcile`).
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
