# Agent brief: implement h5i-pay

You are implementing the payment platform described in `docs/design-v0.1.md`
(Japanese). Read it fully first. This brief records facts already verified so
you do not rediscover them.

## Verified facts about h5i

- The h5i source at tag **v0.5.2** is cloned at `/root/h5i-upstream`
  (from https://github.com/h5i-dev/h5i). Read it there; do not re-clone.
- **h5i CLI v0.5.2** is the root binary of that repo. Build/install it from
  `/root/h5i-upstream` (see its README/install.sh) and use it for provenance
  (`h5i init`, `h5i capture commit`, etc.) in this repo.
- **h5i-app v0.1.4** is the Rust web framework crate at
  `/root/h5i-upstream/crates/h5i-app`. It re-exports `h5i-app-core`,
  `-http`, `-pg`, `-schema`, `-sql`, `-pgsql`, `-json`, `-token` behind
  features (`http`, `postgres`, `schema`, `full`). Kernels are pure Rust,
  extracted to Lean 4 with Aeneas/Charon, served with axum, executed in
  SERIALIZABLE PostgreSQL transactions with retries and idempotency keys.
- `h5i-app-gpu` (`/root/h5i-upstream/crates/h5i-app-gpu`) is the GPU batch
  backend (backend-nvvm, backend-oxide, differential testing).
- Directly relevant examples — study these before designing:
  - `examples/app/ledger/` (kernel, server, proofs, TUTORIAL.md)
  - `examples/app/discount-gpu/` and `examples/app/parallel/` (GPU batch)
  - `examples/app/calculator/TUTORIAL.md` (basics)
  - `docs/design/design-app*.md`, `docs/guides/build-and-verify-a-web-app`
- Lean toolchain used by the examples: `leanprover/lean4:v4.31.0` (see
  `examples/app/ledger/proofs/lean-toolchain`). Lean/elan/lake are NOT yet
  installed on this machine; install elan.

## Environment

- Rust 1.99.1, CUDA 13.2 (`/usr/local/cuda`), NVIDIA A10G GPU, docker, tmux.
- No PostgreSQL running yet; use docker for a local PostgreSQL.
- Workspace: `/root/h5i-pay` (git repo, branch `main`, remote origin).

## How to depend on h5i-app

Check whether `h5i-app = "0.1.4"` resolves from crates.io. If not, use a git
dependency on https://github.com/h5i-dev/h5i at tag `v0.5.2` (or a path
dependency on `/root/h5i-upstream/crates/h5i-app` only as a fallback, and
record that choice in the README). Pin the version.

## Scope and order

Follow the design's milestones. Do M1 → M2 → M3 thoroughly here; M4 (AWS)
means writing Terraform/Docker/CI files only — do NOT create real AWS
resources or spend money. M5 means docs (runbooks, proof assumptions).

- M1 Verified Ledger: double-entry ledger kernel on h5i-app with
  idempotency, PostgreSQL integration, Lean 4 proofs of conservation,
  balanced postings, non-negative balances, idempotency, tenant isolation,
  batch serializability. Tests: concurrent transfers, replay, DB failure.
- M2 Payment Orchestration: payment state machine (create/capture/cancel/
  refund), Mock PSP, Stripe test-mode connector (tokens/hosted checkout only;
  never handle raw card numbers; read the key from env, never commit it),
  webhooks with signature verification + dedup, retry recovery,
  reconciliation. Lean proofs of valid transitions and refund safety.
- M3 GPU Batch: reconciliation and batch policy validation, each with
  CPU single-thread, CPU multithread, SIMD, and GPU implementations using
  h5i-app-gpu; differential tests proving identical output; benchmarks at
  100K/1M/10M records reporting throughput, p50/p95, transfer time, peak
  memory. Report honestly where GPU wins and where it does not.
  GPU never commits financial state.

## Rules

- Follow the repository structure in section 7 of the design, adapted to
  h5i-app conventions (kernel crates kept free of tokio/axum/DB deps).
- Every Lean theorem must actually check (`lake build`), no `sorry` left in
  claimed results. If something cannot be proved, list it in
  `docs/proof-assumptions.md` instead of pretending.
- Run `cargo build`, `cargo test`, `cargo clippy` and `lake build` and make
  them pass before claiming a milestone is done.
- Commit after each milestone with clear messages (use `h5i capture commit`
  once h5i CLI is installed). Do not push, do not force anything.
- Keep a progress log in `docs/PROGRESS.md` (milestone status, what passes,
  what is open) so work can resume after a disconnect.
- Update README.md with build/run/test instructions at the end.
