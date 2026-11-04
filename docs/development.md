# Development: build, test, prove

How to build h5i-pay, run every check CI runs, and rebuild the proofs and
the GPU code. The README has the short version.

## Dependencies

Every h5i-app crate (`h5i-app`, `-core`, `-pg`, `-http`, `-json`,
`-schema`, `-sql`, `-gpu`, `-gpu-backend-nvvm`) and the Lean library
`h5i_app_lib` in `proofs/*/lakefile.lean` come from one commit of
<https://github.com/h5i-dev/h5i> by git rev: branch `app/pipeline-errors`
(`9ec26b5`), one commit on h5i main `3ab10b3` (h5i-app 0.5.3), which has
range comparisons in reads (PR #775) and the database clock and the writes
pipelined (PR #773). Once it is merged, pin the main commit; once h5i-app
is released with it, go back to a crates.io version. Keep the Cargo revs,
the Lake revs and manifests, and CI's `H5I_REV` in step.

## Build and test

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features payment-api/mock-psp -- -D warnings

# PostgreSQL-backed tests (skipped when the variable is unset). CI starts its
# server with the same script; its settings (max_connections=200) live there.
export H5I_APP_TEST_DATABASE_URL=$(scripts/dev-db.sh)
cargo test --workspace

# TLS-only PostgreSQL for tests/tls (h5i's script; prints the three variables)
eval "$(bash /path/to/h5i/scripts/app/pg-tls.sh /tmp/pg-tls | sed 's/^/export /')"
cargo test -p h5i-pay-tests --test tls

cargo deny check
```

`scripts/dev-db.sh` starts (or reuses) PostgreSQL 17 in Docker as
`h5i-pay-pg` on `127.0.0.1:55432` and prints its URL. The suites in
`tests/` are `integration` (HTTP API and PostgreSQL), `concurrency`,
`fault_injection`, `tls` and `differential` (CPU against GPU).

## Proofs

Charon, Aeneas and Lean at the versions h5i pins (`h5i app doctor` checks
them). For each kernel (`crates/payment-ledger`, `crates/payment-domain`,
`crates/payment-reconcile`):

```sh
h5i app doctor
h5i app extract crates/payment-ledger            # rewrite proofs/ledger/generated/
h5i app extract --check crates/payment-ledger    # the generated Lean is current
h5i app lint crates/payment-ledger
h5i app check crates/payment-ledger              # lake build + axiom gate
h5i app mutate crates/payment-ledger             # every listed bug must break a proof
```

The mutants are listed in each crate's `h5i-app.toml`. The Lake projects
(`proofs/ledger`, `proofs/payments`, `proofs/reconcile`) have the same
`require`s, so h5i's shared package cache (`~/.cache/h5i/lake`) holds one
copy of Mathlib for all of them. On a small disk, set
`H5I_LAKE_CACHE=off` and symlink `proofs/payments/.lake/packages` to
`../../ledger/.lake/packages` instead. What the theorems say and assume:
[proof-assumptions.md](proof-assumptions.md).

All three kernels at once (what the README's "Verified" summarizes):

```sh
h5i app check crates/payment-ledger crates/payment-domain crates/payment-reconcile
h5i app mutate crates/payment-ledger crates/payment-domain crates/payment-reconcile
h5i app lint crates/payment-ledger crates/payment-domain crates/payment-reconcile
```

The mutants are bugs each of which must break a proof.

## GPU code and benchmarks

The GPU engines need the CUDA toolkit headers and libraries for `cust`,
an NVIDIA driver and libclang; not nvcc (the PTX is committed).
Rebuilding the PTX needs the pinned Rust-CUDA toolchain:
[gpu-toolchain.md](gpu-toolchain.md), `crates/payment-gpu/build-device.sh
[--check]`. Without the `cuda` feature everything builds and runs on the
CPU only.

```sh
cargo clippy --all-targets -p payment-gpu -p h5i-pay-tests -p h5i-pay-benchmarks \
  --features payment-gpu/cuda,h5i-pay-tests/gpu,h5i-pay-benchmarks/gpu -- -D warnings
cargo test --release -p h5i-pay-tests --features gpu --test differential   # GPU vs the reference on every row
crates/payment-gpu/build-device.sh --check                                  # the committed PTX is current
cargo build --release -p h5i-pay-benchmarks --features gpu
./target/release/bench all --out benchmarks/results/<instance>.json

cargo run --release -p payment-gpu --features cuda --bin h5i-pay-batch -- \
  reconcile internal.csv external.csv report.json --engine auto
```

The worker's file formats (records CSV, the binary records file, the
report, the `jobs` list) are in [batch-files.md](batch-files.md). With a
release build in `target/release`:

```sh
cargo build --release -p payment-gpu --features cuda --bin h5i-pay-batch
cargo build --release -p h5i-pay-benchmarks --features gpu --bin bench
target/release/bench gen-files 1000000 /tmp/j1 --csv                   # synthetic i/e, .h5ib and .csv
target/release/h5i-pay-batch convert /tmp/j1/e.csv /tmp/j1/e2.h5ib     # CSV -> binary (and back)
target/release/h5i-pay-batch reconcile /tmp/j1/i.h5ib /tmp/j1/e2.h5ib /tmp/j1/report.json --engine gpu
echo '/tmp/j1/i.h5ib /tmp/j1/e.h5ib /tmp/j1/r2.json' | target/release/h5i-pay-batch jobs - --engine auto
# J1's measurements: inputs on a real disk (cold runs evict them), deleted afterwards.
benchmarks/j1-batch.sh target/release/h5i-pay-batch target/release/bench /mnt/scratch/h5i-pay-bench 10000000 5 j1.jsonl
benchmarks/j1-summary.py j1.jsonl
```

The binary format's parsers have `cargo fuzz` targets (`records_file`,
`records_csv`) in `crates/payment-batch/fuzz`, outside the workspace
because libFuzzer needs a nightly toolchain:

```sh
cd crates/payment-batch && cargo +nightly fuzz run records_file -- -max_total_time=240
```

The end-to-end benchmark is `benchmarks/src/bin/e2e.rs` (`e2e seed`,
`e2e measure`); the results and how they were taken are in
[benchmarks.md](benchmarks.md).

## Layout

| Path | Contents |
|---|---|
| `crates/payment-ledger` | ledger kernel (pure Rust, extracted to Lean) |
| `crates/payment-domain` | payment state machine, refunds, webhook dedup, batch policy rules (extracted to Lean) |
| `crates/payment-storage` | h5i-app PostgreSQL adapters, JSON codecs, audits, recovery queries |
| `crates/payment-connectors` | PSP connectors: Mock PSP, Stripe (test mode only), webhook signatures |
| `crates/payment-reconcile` | reconciliation rules and CPU engines (reference, single-thread, multithread, AVX2), extracted to Lean |
| `crates/payment-batch` | batch API on h5i-app-gpu: policy validation and reconciliation with CPU/GPU/Auto selection |
| `crates/payment-gpu` | GPU engines (feature `cuda`): the Rust device crate `device/` (the same `policy.rs` and `rules.rs` the CPU runs, compiled to PTX by rustc_codegen_nvvm, committed as `kernels/payment_gpu_device.ptx`) and the `h5i-pay-batch` worker |
| `crates/payment-api` | the `h5i-pay` HTTP server, the payment orchestrator and the scheduled jobs |
| `proofs/ledger`, `proofs/payments`, `proofs/reconcile` | Lake projects: the extracted kernels (`generated/`) and the theorems |
| `tests/` | integration, concurrency, fault-injection, TLS and CPU/GPU differential suites |
| `benchmarks/` | `bench` and `e2e` binaries; results in `benchmarks/results/` |
| `infra/` | Terraform for AWS and Dockerfiles ([infra/README.md](../infra/README.md)) |
| `.github/workflows/ci.yml` | CI: Rust, CUDA build, proofs, Terraform checks, images, test deploy |
| `scripts/dev-db.sh` | a local PostgreSQL 17 in Docker |
