# Next work: A–E

Work plan after M1–M5, SEC-01, SEC-05, u32 columns, scheduled recovery and
the API-kill test. Written from a review of the code at `b56a776`; every
claim below was checked against the files named. Keep `docs/PROGRESS.md`
current as items land, and tick items off here.

Ground rules:
- Commit each logical step only with its checks passing (cargo build/test/
  clippy, `h5i app extract --check`, `h5i app check`, `h5i app mutate`, lake
  build, GPU differential where touched), and push to `origin/main`.
- Do not claim more than you measured or proved. If something is not
  feasible, say so in PROGRESS.md with the evidence, and move on.
- If h5i-app itself needs a fix, make it in `/root/h5i-upstream` on a branch,
  push it to `github.com:h5i-dev/h5i` over SSH (`git@github.com:h5i-dev/h5i.git`)
  and say so in PROGRESS.md; do not vendor patched copies into h5i-pay.

## D. Finish SEC-02 (do first: the uncommitted work breaks deployment)

- [x] **Done.** Operator key in Secrets Manager/Terraform/task/compose/CI/runbook;
  operator TTL also enforced at verification; the operator id stays out of
  the kernels by decision (documented). See PROGRESS.md "SEC-02".

Uncommitted: `crates/payment-api/src/auth.rs` (separate user and operator
HMAC keys, `_PREVIOUS` rotation keys, operator TTL ≤ 12 h, user 0 refused
on user tokens) and edits in `payment-api/src/{lib,main,routes}.rs` and
`tests/*`.

- `main.rs` now requires `H5I_PAY_OPERATOR_SECRET`, but
  `infra/terraform/compute.tf` (~line 206) injects only `H5I_APP_SECRET`
  and `infra/docker/docker-compose.yml` (~line 23) sets only
  `H5I_APP_SECRET`: a deployed API would not start. Add the operator secret
  to Secrets Manager/Terraform, the task definition, compose, CI
  (`.github/workflows/ci.yml` ~line 75), and `docs/runbooks/secret-rotation.md`.
- Every operator still maps to kernel principal `(tenant, 0)`: the operator
  id is only logged. Decide whether the kernels or their audit trail should
  carry the operator id (a kernel change with proofs re-checked); at least
  document the limit.
- Update `docs/security-review.md` (SEC-02 row) and PROGRESS.md.

## B. Data path and efficiency (the real bottleneck)

1. [x] **Done** (PROGRESS.md "B1"). Whole-tenant load per command. `PaymentsStore`/`LedgerStore`
   (`payment-storage/src/{payments,ledger}.rs`) implement only `load`; the
   h5i-app-pg default `load_for` loads every row of the tenant, so every
   command is O(tenant rows) (SEC-06). Implement `load_for` per command
   with the frame theorem h5i-app requires (see h5i-app-pg `Store::load_for`
   docs and upstream examples), proofs included.
2. [x] **Done** (PROGRESS.md "B2"). Recovery (`orchestrator.rs::recover`, `jobs.rs::recover_all`): one
   `List` (whole tenant), then sequentially for every payment GetRef + a PSP
   call, and `sync_ledger` for every CAPTURED payment on every pass even if
   nothing changed — O(payments × tenant rows) per pass plus up to one PSP
   call per payment. Select only unfinished work (by status/index), bound
   PSP concurrency, stop re-syncing settled payments (or detect that
   quickly), and process tenants concurrently with a limit.
3. [x] **Done** (PROGRESS.md "B3"). Online flow: about ten kernel commands per payment (Create,
   BeginAuthorize, Get, PSP, AttachRef, Record, sync_ledger, then Get /
   Refunds / GetRef for the view), each a SERIALIZABLE transaction with a
   whole-tenant load. Reduce round trips (combined commands/replies where
   the kernel can return what the view needs), and post ledger transfers
   with `Command::Batch` instead of one command per transfer/refund.
4. [x] **Done** (PROGRESS.md "B4", `docs/benchmarks.md`). End-to-end benchmarks. `benchmarks/` times only `Backend::run` on
   synthetic in-memory data; the worker's `elapsed_ms` starts after the CSV
   read. Add measurements that include PostgreSQL load, PSP report fetch,
   CSV/S3, compute and output, for the API reconcile path, the worker, and
   recovery at realistic tenant sizes, before and after B1–B3. Rewrite the
   conclusions in `docs/benchmarks.md` from those numbers.

## A. GPU the way h5i-app intends: one Rust source for CPU, proofs and GPU

- [x] **Done** A1–A4 (PROGRESS.md "A"; toolchain in `docs/gpu-toolchain.md`).
  The clean benchmark re-run is folded into B4.

`crates/payment-gpu/kernels/batch.cu` is hand-written CUDA C transcribing
`payment_domain::policy::check` and `payment_reconcile::rules` and the hash
join; the only link to the Rust is differential tests and runtime size
asserts. Upstream (`/root/h5i-upstream/examples/app/discount-gpu`,
`examples/app/parallel`, `crates/h5i-app-gpu`, `docs/design/design-app-executors.md`)
compiles the same Rust source to PTX with rustc_codegen_nvvm: a `#![no_std]`
device crate that `#[path]`-includes the kernel source the host uses and
Aeneas extracts.

1. Get the pinned toolchain working here: rust-cuda rev `3a82c35a…`,
   nightly-2026-04-02, prebuilt LLVM 7.1, libNVVM/CUDA 12.6 (install a 12.6
   toolkit side by side with 13.2; the 595 driver runs older toolkits), and
   the Ubuntu-only bits of `backend-nvvm/setup.sh` and
   `discount-gpu/build-device.sh`. If the host cannot, build the device
   crate in an Ubuntu 24.04 container and ship the PTX; record what was
   needed.
2. Policy: a device crate that runs the verified `policy::check` source;
   delete the CUDA C policy kernel.
3. Reconciliation: `payment_reconcile::rules` is not extracted or proven at
   all. Make the per-row classification a kernel-style Rust function that
   Aeneas extracts, prove what it should guarantee, and run that source on
   the GPU. The parallel hash join may stay a trusted (tested) shell, as in
   upstream, but the classification must be the shared, proven source.
4. Keep the differential tests (now they check the compiler/GPU, not a
   transcription). Update `docs/proof-assumptions.md` with the new trusted
   base, and re-run the benchmarks.

## C. Connect the batch path to the system

- [x] **Done** (PROGRESS.md "C"): export, nightly Step Functions run,
  ingest into `ops` + `GET /v1/reconcile/reports`; bulk payout screening
  through `run_policy`; `/v1/reconcile` stays CPU by decision.

- Nothing produces the worker's `internal/*.csv`, nothing submits Batch
  jobs (runbook only), and nothing reads the JSON report back. Add the
  export (from the payments/ledger state), scheduled submission, and
  ingestion of exceptions into an operator-visible place (the audited
  correction transfers stay manual, per `docs/runbooks/reconciliation-exceptions.md`).
- `run_policy` / batch policy validation is used only in tests and
  benchmarks. Wire it into a real path where batches exist (e.g. bulk
  payouts), or remove it.
- `POST /v1/reconcile` allows CPU engines only; decide whether it should use
  `run_reconcile` with `ExecutionPolicy` or stay CPU and leave large runs to
  the worker.

## E. Unverified or unexecuted

- [x] Recovery interval agrees (5 minutes); SEC-03, SEC-07, SEC-09 closed
  (PROGRESS.md "E"). What needs credentials: see the last bullet there.

- Recovery interval: PROGRESS.md says every 5 minutes, `infra/terraform/recovery.tf`
  defaults to `rate(1 hour)`; make them agree.
- Not run anywhere: Terraform plan/apply, the recovery schedule on AWS,
  image push, the Stripe test-mode round trip (no `STRIPE_SECRET_KEY`). Do
  what is possible without credentials (e.g. `terraform plan` against a
  local/mock backend is not meaningful; at least `tflint`/`checkov` if
  available) and leave the rest clearly marked as needing credentials.
- Remaining findings: SEC-03 (replay reveals existence), SEC-07 (ADOT
  `latest` tag; pin by digest), SEC-09 (add `cargo deny` to CI).

## Order

D, then B1–B2, then A, then B3–B4 and C, then E. Work autonomously; do not
wait for confirmation between items.
