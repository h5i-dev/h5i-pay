# h5i-pay progress log

Status of the milestones in `docs/design-v0.1.md`, kept so work can resume
after a disconnect. Newest entries first within each milestone.

## Environment (verified 2026-10-31)

| Tool | Version / location | Notes |
|---|---|---|
| Rust | 1.99.1 | workspace edition 2024 |
| h5i CLI | 0.5.2, `~/.local/bin/h5i` | built from `/root/h5i-upstream` with `H5I_SKIP_WEB_BUILD=1` (no Node on the host, so the `h5i ui` web bundle is not embedded) |
| h5i-app | 0.5.2 from crates.io, pinned `=0.5.2` | published from h5i commit `02ccdde4ccfc` (#771; includes #769 and #770). `h5i-app-gpu`, `-gpu-backend-nvvm` and the `h5i_app_lib` Lean library come from git at that commit (not on crates.io). Earlier: crates.io 0.1.4 + tag `v0.5.2` (M1–M5), then git rev `a19667c`. The h5i CLI is still the tag-v0.5.2 build |
| Charon / Aeneas | 4bd5a29f6e97 / b86120db3183 at `/opt/aeneas/{charon,aeneas}/bin` | built with `nix build github:AeneasVerif/aeneas/<rev>` (Determinate Nix installer); `h5i app doctor` is green when both are on `PATH` |
| Lean | leanprover/lean4:v4.31.0 via elan (`~/.elan/bin`) | Mathlib cache from `lake exe cache get` |
| PostgreSQL | 17 in docker (`scripts/dev-db.sh`, port 55432) | |
| CUDA | 13.2, NVIDIA A10G (sm_86) | |

`h5i` v0.5.2 has no `init` or `capture` commands (`h5i --help` lists
`box ui browser plugin project app skill completion`), so commits are
plain `git commit`; provenance for the proofs is `h5i app check` /
`h5i app mutate`, whose run records live under `crates/*/.h5i/app/runs/`.

PATH for proofs: `export PATH=/opt/aeneas/aeneas/bin:/opt/aeneas/charon/bin:$HOME/.elan/bin:$PATH`.

## M1 — Verified Ledger: done

- `crates/payment-ledger`: double-entry kernel (accounts with posted
  debits/credits per currency, `no_overdraft`/`no_overcredit` limits,
  client-chosen transfer ids with replay detection, linked batches,
  owner/operator authorization). Extracted with `h5i app extract crates/payment-ledger`.
- `crates/payment-storage`: h5i-app `Kernel`/`Store`/`ReplyCodec`/JSON for the
  ledger (PostgreSQL schema `ledger`), plus `audit()` that re-checks the
  proven invariants on loaded rows.
- `crates/payment-api`: axum server, `POST /v1/ledger`, `GET /v1/ledger/audit`.
- Lean (`proofs/ledger/{Ledger,Batch,Models}`), all on the
  extracted code except `Models/Engine.lean` (a model of the trusted engine):
  `conservation`, `journal_balanced`, `no_overdraft`, `no_overcredit`,
  `posting_balanced`, `replay_is_noop`, `only_owner_or_operator_debits`,
  `Batch.batch_serial`, `Storage.db_inv`, `Models.isolation`,
  `Models.noninterference`, `Models.keyed_replay`, `Models.ledger_isolation`.
  `h5i app check crates/payment-ledger`: 145 theorems in 13 modules on the
  standard axioms only; no `sorry`.
- Mutants (`h5i app mutate crates/payment-ledger`): 5/5 caught
  (self transfer, overdraft, anyone may debit, short credit side, replay
  ignoring the amount).
- Tests (`H5I_APP_TEST_DATABASE_URL=postgres://h5i:h5i@127.0.0.1:55432/h5i`):
  - concurrency: 16 tasks × 40 random transfers with SERIALIZABLE retries
    (1119 retries in one run) and with the tenant lock; concurrent duplicates
    commit once; concurrent linked batches are atomic; PostgreSQL agrees with
    the in-memory reference engine on 300 random commands.
  - fault injection: backend killed mid-transaction (retried once), killed
    before COMMIT (`CommitUnknown`, nothing half-applied, client retry posts
    once), same with an idempotency key (engine retries itself), persistent
    failure (retries exhausted, ledger untouched), unreachable database (503).
  - integration: the ledger over HTTP, idempotency keys, tenant isolation.
- `cargo build`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `lake build` pass.

## M2 — Payment Orchestration: done

- `crates/payment-domain`: payment kernel (statuses CREATED → PROCESSING →
  AUTHORIZED → CAPTURED, CANCELED, FAILED; refunds reserved against the
  capture; PSP references; webhook events deduplicated by id and tolerant of
  reordering) and the stateless batch policy rules (`policy::check`).
- `crates/payment-connectors`: `Psp` trait; `MockPsp` (idempotency keys,
  manual/automatic capture, partial capture, refunds, Stripe-shaped signed
  webhooks, settlement report, fault injection: fail before effect, lose the
  response, pending payments, tampered settlement); `StripePsp` (test keys
  only, live keys refused; PaymentIntents, capture, cancel, refunds, search,
  hosted Checkout; `Stripe-Signature` verification with 300 s tolerance);
  raw card numbers refused before any PSP call.
- `crates/payment-api/src/orchestrator.rs`: create/authorize/capture/cancel/
  refund/checkout, webhooks, recovery (`POST /v1/recover`), ledger postings
  with deterministic transfer ids (capture, refund reserve/settle/release),
  reconciliation inputs (payments vs PSP report, payments vs ledger journal).
- `crates/payment-reconcile`: rules, reference oracle, CPU single-thread
  (M3 adds the other engines' benchmarks).
- Lean (`proofs/payments/{Payments,Policies}`), on the extracted kernel:
  `status_steps_allowed`, `terminal_final`, `captured_final`,
  `refund_safety`, `refunded_le_captured`, `event_dedup`, `reachable_inv`,
  `Policies.check_sound`. `h5i app check crates/payment-domain`: 121
  theorems in 6 modules, standard axioms only. `h5i app mutate
  crates/payment-domain`: 6/6 caught.
- The proofs are now two Lake projects, `proofs/ledger` and
  `proofs/payments`: `h5i app check`'s axiom gate imports every module of a
  project into one Lean environment, and the two extracted kernels both
  declare `h5i_app_sql.*`, so they cannot share one. Both have the same
  `require`s; here `proofs/payments/.lake/packages` is a symlink to the
  ledger's packages and `H5I_LAKE_CACHE=off` (the disk has room for one
  Mathlib); with h5i's default shared cache no symlink is needed.
- Tests: `tests/integration/payments_api.rs` (11 HTTP end-to-end tests with
  the Mock PSP: automatic and manual capture, partial capture and refunds,
  refund limits, declines, cancels, card-number refusal, a pending payment
  completed by 10 concurrent deliveries of one webhook (1 processed, 9
  duplicates), out-of-order webhooks, lost PSP responses recovered for
  payments and refunds, PSP outage, merchant balance too low for a refund,
  reconciliation with tampered settlement under all four CPU engines,
  idempotency key creating one payment); Stripe connector against a local
  fake of the Stripe API (request encoding, declines, refunds, 404s,
  signatures) and a live test-mode round trip that runs only when
  `STRIPE_SECRET_KEY=sk_test_…` is set (no key on this machine, so it was
  skipped).

## M3 — GPU Batch Processing: done

- `crates/payment-reconcile`: four engines with one `Report` — reference
  (`BTreeMap` oracle), CPU single-thread (open addressing), CPU
  multithread (rayon radix partition + per-partition hash join), SIMD
  (AVX2 4-lane probes).
- `crates/payment-batch`: the batch API on h5i-app-gpu's `Backend`;
  policy validation with the verified `policy::check` single-thread,
  rayon, and AVX2 (`policy_simd.rs`); `run_policy` / `run_reconcile`
  with `ExecutionPolicy` (CPU / GPU / Auto with a row threshold, no CPU
  replay after a GPU failure, output length validated); synthetic data.
- `crates/payment-gpu` (feature `cuda`): CUDA C kernels
  (`kernels/batch.cu`: atomic hash-table build, probes, policy check)
  compiled to PTX by nvcc (`compute_75`, JIT on T4/A10G/L4), launched
  through `h5i-app-gpu-backend-nvvm`/`cust` at the pinned Rust-CUDA rev;
  `Gpu` implements `Backend` for both workloads with H2D/kernel/D2H timing.
  `h5i-pay-batch` worker binary (CSV in, JSON report out; Auto selection).
  (Superseded: see "A" under "After M5"; the kernels are now Rust compiled by
  rustc_codegen_nvvm.) libclang (`dnf install clang-libs clang-devel`) is needed for cust.
- `tests/differential`: every CPU engine vs the reference, and with
  `--features gpu` the GPU vs the reference on every row (empty batches,
  block boundaries, hot keys, hash collisions, malformed policies, unsorted
  allowlists, 100k/200k synthetic sets), repeat determinism, partition
  independence, corrupted-output rejection. All pass on the A10G.
- `benchmarks` (`bench all`): 10k/100k/1M/10M rows, p50/p95, throughput,
  H2D/kernel/D2H, peak RSS increase, device memory, $/M rows; results and
  analysis in `docs/benchmarks.md`. Summary: with a warm context the GPU
  wins from ~100k rows (reconciliation 5.2× / 8.6× the 4-thread CPU at
  1M / 10M; policy 3.9× / 3.4×); below ~10k rows, and for one-shot jobs
  under ~2–3M rows (136 ms context + JIT), the CPU wins; policy is
  transfer-bound (kernel 2.55 of 57.6 ms at 10M); SIMD reconciliation is
  slower than scalar on this Zen 2 CPU.

## M4 — AWS Deployment (IaC only): done (files only; nothing applied)

- `infra/terraform`: VPC (public / private / isolated data subnets, NAT,
  VPC endpoints, flow logs), security groups, KMS, Secrets Manager, RDS
  PostgreSQL 17 Multi-AZ, S3 (settlements, batch, Object-Lock audit),
  SQS + DLQ, ECR, ALB + WAF + Route 53, ECS Fargate API (circuit breaker,
  alarm rollback, autoscaling, ADOT sidecar), AWS Batch GPU (g4dn/g5 from
  zero) and Fargate CPU queues, IAM per actor, GitHub OIDC deploy role,
  CloudWatch alarms (ledger invariant, posting refused, reconciliation
  exceptions, 5xx, DLQ, queue lag, RDS), AWS Backup + restore testing.
  `terraform fmt -check` and `terraform validate` pass (Terraform 1.9.8,
  providers downloaded to /tmp and removed; no credentials, no apply).
- `infra/docker`: API and worker Dockerfiles, Batch entrypoint, compose.
  Not built locally (disk); CI builds them.
- `.github/workflows/ci.yml`: Rust (PostgreSQL service), CUDA build, GPU
  runner tests (opt-in), proofs (extract --check, check, mutate),
  Terraform checks, ECR images via OIDC, deploy to `test` with approval.
- `payment-api` reads RDS-managed credentials (`DB_HOST`/`DB_NAME`/
  `DB_CREDENTIALS`); `h5i-pay-batch` prints a JSON summary line for the
  reconciliation-exceptions metric filter.
- Known gap at the time: h5i-app-pg connected without TLS, so
  `rds.force_ssl = 0`. Closed after M5 (SEC-01, below).

## M5 — Production Hardening (docs): done

- `docs/failure-recovery.md`: each failure (database connection lost
  mid-transaction or before COMMIT, races, PSP timeouts and outages, lost
  refund responses, duplicate/reordered/forged webhooks, crash between
  capture and ledger posting, settlement mismatch, GPU failure, bad
  deploy, database loss), what keeps money correct, and the test or proof
  that shows it.
- `docs/security-review.md`: threat model and findings SEC-01..07.
- `docs/compliance.md`: PCI DSS scope (no card data handled: tokens and
  hosted Checkout only), data retention, audit trail, what is still needed
  before real funds.
- `docs/runbooks/`: ledger imbalance, reconciliation exceptions, PSP
  incidents, webhooks, backup/restore, deploy/rollback, batch jobs, secret
  rotation.
- `docs/proof-assumptions.md`: trusted base, gate results, the mutant
  table (first failing proof per mutant, read from the run records), and
  the limits of the GPU and reconciliation evidence.
- README updated with the full layout and commands.

## After M5 (newest first)

### A: GPU kernels compiled from the Rust the CPU and the proofs use

- Toolchain (A1) works natively on this Amazon Linux 2023 host, no
  container: rust-cuda `3a82c35a`, `nightly-2026-04-02`, the prebuilt LLVM
  7.1 and CUDA 12.6's libNVVM (`cuda-nvvm-12-6` etc. from NVIDIA's rhel9
  repo, beside CUDA 13.2; `/usr/local/cuda` put back on 13.2 with
  `alternatives`). The one Ubuntu-only piece was `libedit.so.2` /
  `libbsd.so.0` for the prebuilt LLVM; Ubuntu 20.04's packages (24.04's need
  glibc 2.38) are extracted to `/var/tmp`, pinned by SHA-256. Details and
  the environment file: `docs/gpu-toolchain.md`.
- `crates/payment-gpu/device` (its own workspace, `no_std`, `cuda_std`)
  includes `payment-domain/src/policy.rs` and `payment-reconcile/src/rules.rs`
  with `#[path]` and exports `policy_check`, `recon_build`,
  `recon_internal`, `recon_external`. `build-device.sh` compiles it to
  `kernels/payment_gpu_device.ptx` (committed, compute_75, 14.5 KB, sha256
  `4cedff02…764f`); `--check` rebuilds and compares (two builds were
  byte-identical). `payment-gpu` embeds that PTX; `kernels/batch.cu`,
  `build.rs` and the nvcc step are deleted (A2).
- Reconciliation (A3): `rules.rs` now also holds the per-row probe
  (`find_slot`, `TableRef`, `internal_row`, `external_row`) that the CPU
  single-thread and multithread engines and the GPU all call;
  `classify_internal` lost its match guard (same semantics) so Aeneas takes
  it. `crates/payment-reconcile/h5i-app.toml` extracts the two classifiers
  into the new Lake project `proofs/reconcile`. Proven
  (`proofs/reconcile/Reconcile.lean`, 15 theorems, standard axioms): the
  extracted functions compute decision tables `specInternal` /
  `specExternal`; codes are in range; over whole inputs (counts and first
  external record of the key, what the reference computes),
  `internal_matched_iff`, `external_matched_iff`, `duplicates_first`,
  `exception_reported_once`, `pair_matched_both_sides`. `h5i app mutate
  crates/payment-reconcile`: 3/3 caught (duplicate external ignored, status
  not compared, extra external reported matched). The hash joins
  themselves (CPU `Table`, GPU `recon_build` atomics) stay a trusted,
  tested shell.
- Differential tests (A4) unchanged and passing on the A10G with the
  Rust-compiled PTX: every CPU engine and the GPU against the reference on
  every row (6 tests). They now check the compiler and the hardware, not a
  transcription. `docs/proof-assumptions.md` has the new trusted base.
- Cost on the CPU: routing the CPU joins through the shared `TableRef`
  probe made them about 8% slower until the probe was `#[inline(always)]`;
  now 2–3% (single) and ~5% (multithread) slower than before at 1M rows
  (paired A/B runs of the old and new `bench one recon`, 1M rows, 11 runs:
  single 309 → 316–319 ms, multithread 89–90 → 95 ms).
- GPU timings with the nvvm PTX (`bench all`, first run, the host was
  otherwise busy so its CPU rows are not comparable): reconciliation 1M
  rows p50 18.8 ms (kernels 4.18 ms; nvcc build 16.9 ms / 4.09 ms), 10M
  206 ms (kernels 49.4 vs 48.2 ms); policy 1M 5.2 ms (kernel 0.54 vs 0.29
  ms), 10M 59.4 ms (kernel 5.0 vs 2.55 ms). So the reconciliation kernels
  are as fast as the CUDA C; the policy kernel is about 2× slower than
  nvcc's, which changes the end-to-end time by a few percent because
  transfers dominate. A clean re-run of all benchmarks is part of B4.
- CI: the proofs job extracts, checks and mutates `crates/payment-reconcile`
  too; a new `device-ptx` job (Ubuntu 24.04, CUDA 12.6 container) builds
  the backend and runs `build-device.sh --check`. Written, not run here.

### B2: recovery selects only unfinished work

- `payment_storage::recovery_work` (one read-only query over every
  tenant, outside the kernels like `tenants_to_recover` before it) returns,
  per tenant, the payments to ask the PSP about (`PROCESSING`, `AUTHORIZED`
  with automatic capture), those with a pending refund, and `CAPTURED`
  payments with a ledger posting missing: an anti-join on the ledger's
  deterministic transfer ids by the rule `sync_ledger` follows
  (`payment_storage::needs_ledger_sync`, the same rule in Rust). Settled
  payments are no longer re-synced on every pass. Partial indexes on the
  unfinished and captured payments; `payments_merchant`,
  `refunds_payment` and `accounts_owner` indexes back the B1 plans.
- `Orchestrator::recover_work` handles one tenant's work with at most
  `PSP_CONCURRENCY` (8) PSP calls or ledger syncs at once, re-reading each
  payment through the kernel first (work finished meanwhile is skipped);
  `jobs::recover_all` runs `TENANT_CONCURRENCY` (4) tenants at once.
  `POST /v1/recover` finds its tenant's work from the two kernels'
  snapshots (`find_work`, same rule) and runs the same executor.
- The posting kinds and `system_transfer_id` moved to
  `payment_storage::ledger` (the orchestrator re-exports them) so the query
  and the orchestrator share one definition.
- Test `recovery_job`: four tenants (two lost PSP answers, one healthy,
  one `CAPTURED` with its capture posting lost); the query selects exactly
  the three with work and the right payment per category, one pass fixes
  all (2 resolved, 1 ledger sync), audits pass, `find_work` agrees with the
  query on the healthy tenant, and the next pass selects no tenant. Timings
  are in B4.

### B1: per-command reads with frame theorems (SEC-06)

- Both kernels have an extracted `reads(actor, cmd) -> Option<Vec<h5i_app_sql::Read>>`
  (one declarative read per table); `LedgerStore::load_for` and
  `PaymentsStore::load_for` run it with `schema_load_plan`. Ledger: `Open`
  reads the counter row; `Transfer` its transfer id and two accounts;
  `Batch` (≤ 64 transfers, `MAX_PLANNED_BATCH`) every id and account it
  names, longer batches the whole tenant; `Account` its row; `Accounts` all
  rows for an operator, the caller's own otherwise; `Transfers` the journal
  for an operator, nothing otherwise. Payments: every command by key, `List`
  by merchant (all for an operator), `Refunds` the payment and its refunds,
  `GetRefund` the refund and, with `IN (SELECT payment ...)`, its payment.
- Proofs (`proofs/ledger/Ledger/{Plan,Frame}.lean`,
  `proofs/payments/Payments/{Plan,Frame}.lean`): `Plan.reads_abs` (the
  extracted `reads` computes the plan `readsA` written in Lean),
  `Frame.frame` (on `Snapshot.load plan s`, what loading the plan decodes to
  by the generated `decode_load`, `transition` gives exactly its result on
  `s`) and `Frame.load_for_sound` (the two together). The ledger batch case
  carries the slice through the batch loop: each transfer's writes touch
  only rows the slice keeps, so the slice stays a slice
  (`keeps_applyAll`, `batch_loop_congr`); it assumes `HasRoom s`. The
  payments frame needs no assumption.
- Writes take per-key advisory locks (`lock_for`: accounts and transfer id
  of a transfer, the counter for `Open`/`Create`, the payment for payment
  commands) instead of the tenant lock; reads take none.
- Gates: `h5i app check crates/payment-ledger` 224 theorems in 13 modules,
  `crates/payment-domain` 222 in 8, standard axioms only.
  `h5i app mutate`: ledger 7/7, payments 8/8 (new mutants: a plan that
  misses the credit account, a plan on the wrong column, a `Record` plan
  that misses the refund, a `Refunds` plan by refund id; each is caught by
  `Plan.reads_abs`, i.e. the Rust plan no longer matches the plan the
  frame theorem is proved for).
- Tests against PostgreSQL (`tests/concurrency`):
  `load_for_matches_the_whole_tenant` (400 random ledger commands, batches
  up to 70 legs) and `payments_load_for_matches_the_whole_tenant` (500
  random payment commands): at every step `transition` on the rows
  `snapshot_for` returns equals `transition` on the whole tenant. The
  existing `postgres_agrees_with_the_reference_engine` now also runs
  through `load_for`.

### SEC-02: separate user and operator token keys

- `crates/payment-api/src/auth.rs`: user tokens are signed with
  `H5I_APP_SECRET`, operator tokens with `H5I_PAY_OPERATOR_SECRET`; the API
  refuses equal keys, and keys under 32 bytes on the deployed
  (`DB_CREDENTIALS`) path. A user-key token for user 0 gets 401, so holding
  the user key no longer makes an operator. Operator tokens name the operator
  (id ≥ 1, logged per request with the tenant as `operator request`) and
  are refused at verification if they expire more than 12 h (+60 s skew)
  after the time of use, whoever signed them (`issue_operator` also refuses
  a longer TTL). `…_PREVIOUS` values are accepted during a rotation.
  `H5I_PAY_ISSUE_OPERATOR=<tenant>:<id>[:<ttl>]` issues operator tokens with
  the operator key alone; `H5I_APP_ISSUE` issues user tokens (user ≥ 1) with
  the user key alone. `h5i-pay recover` needs neither key.
- Deployment: Terraform has a second secret `h5i-pay-operator-secret`; both
  token secrets are JSON `{current, previous}` (initially equal; Terraform
  ignores later changes) and the task injects the four variables by JSON
  key (`<arn>:current::`, `<arn>:previous::`). The execution role may read
  the new secret. Compose and the CI "refuses mock" step set both keys.
  `docs/runbooks/secret-rotation.md` has the two-step rotation. Not run on
  AWS: ECS JSON-key injection is written from the AWS docs and checked only
  by `terraform validate`.
- Decision on the operator id in the kernels: not done. Kernel `Principal`
  is `{org, user, operator}` and operator-created rows record `actor.user`
  (a payment's `merchant`), so putting the operator id in `user` would
  collide with user ids; it needs a new field and re-checked authorization
  proofs. The per-operator trail is the API log. Documented in
  `docs/security-review.md` (SEC-02 now "partly closed") and
  `docs/proof-assumptions.md`.
- Verified: `cargo test --workspace` (with PostgreSQL and both TLS servers),
  clippy with and without `mock-psp`, `terraform fmt -check` and `validate`,
  the production binary refuses `mock` and refuses to start without the
  operator key. Kernels untouched, so no proof re-run was needed.

### h5i-app 0.5.2 from crates.io

- h5i-app 0.5.2 is on crates.io (published from h5i `02ccdde`, a
  descendant of `a19667c` that only bumps versions). The seven published
  crates are now `=0.5.2` from crates.io; `h5i-app-gpu`, the nvvm backend
  and both `h5i_app_lib` requires are at git `02ccdde`. Cargo.lock has
  one copy of each h5i-app crate. The crate's git tag `v0.5.2` is not used:
  it is the CLI release and has h5i-app 0.1.4, without TLS or u32 support.
- Re-extraction changed only `Source:` path comments. Verified:
  extract --check, check (145 / 122 theorems), mutate (5/5, 6/6),
  lake build, `cargo test --workspace` (67), clippy (default, mock-psp,
  CUDA), GPU differential tests, `cargo build --locked`.

### The API process killed mid-request

- `crates/payment-api/tests/process_kill.rs` runs the `h5i-pay` binary as
  the production image builds it (no Mock PSP; `H5I_PAY_PSP=stripe`)
  against its own scratch PostgreSQL database and a fake Stripe API in the
  test process (`STRIPE_API_BASE`), which implements PaymentIntents,
  search, refunds and Stripe's idempotency-key replay, and can hold one
  request open before or after acting. The test SIGKILLs the process
  while it waits:
  - after the charge: the payment is left `PROCESSING`, no reference, no
    ledger entry; a client retry with the same `Idempotency-Key` on a new
    process returns the same payment, captured, with the PSP's original
    charge (one PSP effect) and one ledger posting; the recovery job then
    has nothing to do;
  - after the PSP made a refund: the refund stays pending and reserved
    (a second refund past the remaining amount gets 409); `h5i-pay
    recover`, with no API running, settles it;
  - before the PSP acted: the job reports it unresolved and leaves it; the
    operator's `fail_unsubmitted=true` fails it; no money moved.
  Each ends with the ledger audit and both reconciliations clean. Passed
  8 runs in a row. Making the PSP idempotency key non-deterministic makes
  the first test fail, so it does check that money moves at most once.

### Scheduled recovery

- `h5i-pay recover` (same binary and task definition as the API, command
  override): one pass over every tenant `payment_storage::tenants_to_recover`
  finds (payments `PROCESSING`, `AUTHORIZED` with automatic capture,
  `CAPTURED`, or with a refund pending), running `Orchestrator::recover`
  per tenant. A failing tenant does not stop the others; exit 1 if any
  failed. It never passes `fail_unsubmitted`. Prints one JSON line
  (`"event":"recovery"`). `tenants_to_recover` is the only cross-tenant
  query: read-only transaction, reads `tenant_id` only, outside the
  kernels; it also works over verified TLS.
- `infra/terraform/recovery.tf`: EventBridge Scheduler every 5 minutes
  (`recover_schedule`), Fargate RunTask of the API family's latest
  revision (so it follows CI deploys) in the private subnets with the API
  security group; a scheduler role limited to RunTask on that family in
  this cluster plus PassRole of the task's two roles; metric filters and
  alarms (tenants failed, payments unresolved for an hour, no pass in an
  hour). `terraform validate` passes. Terraform `required_version` is now
  `>= 1.9.0` (the SEC-05 cross-variable validation needs it).
- Test `recovery_job::scheduled_recovery_covers_every_tenant` (own scratch
  database, since the job sees all tenants): three tenants, two with a
  lost PSP response; one pass resolves both, audits pass, a second pass
  changes nothing; an empty database has nothing to recover. The binary
  was also run against the dev database (65 tenants, 97 payments re-synced,
  exit 0).

### SEC-05 closed: the Mock PSP is out of production builds

- `payment-connectors` feature `mock` gates `mock::MockPsp`;
  `payment-api` feature `mock-psp` enables it. Both are off by default.
  Without it, `H5I_PAY_PSP=mock` stops the binary at startup and the
  default PSP is `stripe`. With it, the default stays `mock`, and a warning
  is logged.
- `Dockerfile.api` has `ARG API_FEATURES=""`: the production image is built
  without the Mock PSP, and CI also pushes `<sha>-mock` (built with
  `API_FEATURES=mock-psp`). Terraform runs the `-mock` image and injects
  the mock webhook secret only for `psp_mode = "mock"`, and rejects
  `psp_mode = "mock"` in `prod*` environments (cross-variable validation;
  checked with a scratch plan: `prod-1`+`mock` rejected, `test`/`staging`
  +`mock` and `prod`+`stripe` accepted). The deploy job keeps the image
  variant matching the task's `H5I_PAY_PSP`.
- CI builds `payment-api` alone and fails unless the binary refuses
  `mock`, and runs clippy on the `mock-psp` path too.
- Checked here: the production binary refuses `mock`, defaults to Stripe
  (`STRIPE_SECRET_KEY is not set`), and `strings` finds no Mock PSP
  markers in it (2 in the `mock-psp` build); both image variants were
  built and run.

### u32 columns

- Ledger: `Account.currency`, `Account.code`, `Transfer.currency`,
  `Transfer.code`, `TransferReq.code`, `Command::Open { currency, code }`.
  Payments: `Payment.currency`, `Payment.status`, `Refund.status`, the
  status constants and `Reply::Status`. Amounts, ids and counters stay
  `u64`.
- No migration: h5i-app-schema stores `u32` as `BIGINT` and range-checks it
  on load (`u32_from_val`).
- Proof changes: `journalRow`/`open_spec` take `U32`; `post_spec` closes
  `a.currency = b.currency` with `UScalar.eq_of_val_eq`; `Payments.Commands`
  adds `u32_val_eq` (h5i_app_lib has only `u64_val_eq`) and retypes
  `with_status_spec`/`create_spec`.
- Side effect: the orchestrator used to pass `currency as u32` to the PSP
  from a `u64` kernel field, so a currency of 2³²+392 would have been
  charged as 392 (JPY) while the kernel stored another value. The casts
  are gone; the API answers 400 for a currency past `u32::MAX` (checked
  by hand on `/v1/payments` and `/v1/ledger`).
- Verified: extract --check, check (145 / 122 theorems), mutate (5/5,
  6/6), lake build, `cargo test --workspace` (including `tests/tls`),
  clippy with and without CUDA, GPU differential tests.

### SEC-01 closed: verified TLS to PostgreSQL

- `payment-storage` enables h5i-app-pg's `tls` feature. `DbTls::from_env`
  reads `DB_CA_FILE`; when set, every pool uses `pool_tls` with
  `Roots::PemFile` (chain and host name verified, `sslmode` forced to
  `require`, `sslmode=disable` an error, no plaintext fallback). A missing
  or certificate-less bundle fails at startup. The API refuses to start on
  the RDS-secret path (`DB_CREDENTIALS`) without `DB_CA_FILE`; with
  `DATABASE_URL` and no CA it uses plain TCP (local development).
- The Amazon RDS global CA bundle (114 CAs, sha256 `90a6f1a7…f21b`) is
  vendored at `infra/docker/rds-global-bundle.pem`
  (`infra/docker/update-rds-ca.sh` refreshes it) and shipped in the API
  image at `/etc/h5i-pay/rds-global-bundle.pem`; the ECS task sets
  `DB_CA_FILE` to it. Not added to the worker image: the batch worker has
  no database credentials or network path to RDS.
- Terraform: `rds.force_ssl = 1`. `terraform fmt -check` and `validate` pass.
- `tests/tls` (7 tests) against h5i's `scripts/app/pg-tls.sh` servers
  (TLS-only `pg_hba.conf`; ports 55433 default protocols, 55434 TLS 1.2):
  payment, capture, refund, reconciliation and audit over TLS with every
  server-side connection TLS (`pg_stat_ssl`); plain TCP refused by the
  server; a certificate from another CA refused (`UnknownIssuer`); the IP
  instead of the certified name refused (`NotValidForName`);
  `sslmode=disable` refused; bad CA files refused.
- The API image was built locally (90.7 MB) and run against the TLS-only
  server: with the test CA it served `/v1/ledger` and `audit` with
  `db_tls=true`; with the shipped RDS bundle the handshake failed; without
  `DB_CA_FILE` it refused to start.
- CI starts the TLS servers with h5i's script at the pinned rev and fails if
  the TLS suite skipped.

### h5i-app from git rev a19667c

- Every h5i-app crate (including `h5i-app-gpu` and
  `h5i-app-gpu-backend-nvvm`) and both `h5i_app_lib` Lake requires now come
  from h5i `a19667cbbb5a1be14c8d16579eb00e679910dc57`. The h5i_app_lib Lean
  sources and the Aeneas pin are identical to v0.5.2; the re-extraction
  changed only the `Source:` path comments in `proofs/*/generated/`.
- Verified: `h5i app extract --check` (both up to date), `h5i app check`
  (ledger 145 theorems / 11 modules, payments 121 / 6, standard axioms
  only), `h5i app mutate` (5/5 and 6/6 caught), `lake build` in both
  projects, `cargo test --workspace`, clippy with and without CUDA, GPU
  differential tests.

## Open items / decisions

- Not done or not verifiable here:
  - SEC-02 is partly closed (separate operator key); the kernels still see
    every operator as user 0. See `docs/security-review.md`.
  - The recovery schedule is written and validated but has not run on AWS.
  - Docker images are built locally now. The worker image did not build
    before (Ubuntu 24.04 has no `awscli` package); fixed by installing AWS
    CLI v2.39.0 from its release zip, pinned by SHA-256 after a PGP check.
    The worker image (793 MB) ran a 200k-row reconciliation on the A10G
    and on CPU with identical reports. Not pushed to any registry.
  - The Stripe test-mode round trip was skipped (no `STRIPE_SECRET_KEY`).
  - Terraform was validated, never planned against or applied to AWS.

- u32 columns broke the Lean that `h5i-app-schema` 0.1.4 rendered, so M1–M5
  used `u64` everywhere. Fixed upstream (#769); see "u32 columns" above.
- Every command but ledger batches over 64 transfers now reads only its
  plan's rows (B1). Listings (`List`, `Accounts`) are not paginated.
