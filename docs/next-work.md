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
  (PROGRESS.md "E"). checkov clean (400 passed, 44 justified skips) and
  in CI; the items needing AWS/Stripe credentials are listed in
  PROGRESS.md "E (rest)" and remain undone.

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

## F. Second round (after A–E)

Same ground rules as above. Do these in order; tick each off here and in
PROGRESS.md as it lands.

1. [x] **Finish SEC-02: operators distinct inside the kernels.** Today every
   operator token maps to kernel principal `(tenant, 0)` and the operator id
   is only logged (`crates/payment-api/src/auth.rs`, `PayAuth::authenticate`).
   Make the kernels' principal carry who acts (user vs operator, and the
   operator id), record the operator id on what operators do (e.g. on the
   ledger transfers and payment records they cause, or an audit field), and
   keep authorization proven: re-state and re-prove
   `only_owner_or_operator_debits`, the payment kernel's operator-only
   commands, and SEC-03's `replay_needs_debit_right` for the new principal;
   add mutants for "any user acts as operator" and "operator id dropped".
   The in-process orchestrator and the recovery job act as a named system
   operator. Update `docs/security-review.md` (close SEC-02 or say exactly
   what remains) and the runbooks.
2. [x] **Per-payment "ledger complete" marker.** `recovery_work`'s anti-join
   on the ledger's transfer ids costs O(captured payments) every pass (852 ms
   at 100k payments with nothing to fix, `docs/benchmarks.md` B4). Record in
   the payment kernel when a payment's ledger postings are all done (set by
   the orchestrator after a successful sync, cleared by anything that needs
   new postings, e.g. a refund), index it, and make recovery O(unfinished).
   Prove that a payment marked complete has no postings missing under the
   kernel's own rules (or state precisely what the marker guarantees), and
   that recovery still finds every payment that needs work. Re-measure.
3. [x] **Pagination.** `List` (payments) and `Accounts` (ledger) return
   everything; ledger batches over 64 transfers fall back to a whole-tenant
   load. Add cursor pagination (by key, with a limit and a hard maximum) to
   the kernel commands and the HTTP routes, with read plans and frame
   theorems for the paged reads, and remove or bound the whole-tenant
   fallback for large batches (e.g. split, or a plan that covers them).
   Update clients in the orchestrator, recovery, export and tests.
4. [x] **Serialization retries.** 8 concurrent clients cause 88–132 retries
   per 200 payments (B4). Find which transactions conflict (log retry
   causes; check the per-key `lock_for` plans, shared rows like system
   accounts and counters, and read plans that read more than they need),
   reduce them (advisory locks on the hot keys, narrower plans, fewer
   shared counters), and re-measure throughput and retries at 10k and 100k
   payments with 8 and 32 clients. Correctness stays SERIALIZABLE.
5. [x] **Prove the hash join.** `docs/proof-assumptions.md` "Not proven":
   the reconciliation join (single-thread open addressing, the multithread
   radix partition, and the GPU's build/probe) is checked only by
   differential tests. Write the join as Rust that Aeneas extracts (at least
   the single-thread engine, and the per-partition join used by the
   multithread and GPU paths if it can share source), and prove it equal to
   the `BTreeMap` reference specification (same code for every row, both
   sides). If the GPU's atomic build cannot be covered, prove the
   sequential algorithm it implements and say so. Mutants for wrong
   duplicate counting and wrong first-row choice. Update proof-assumptions.

After F, re-run the full checks and the B4 end-to-end benchmarks, update
`docs/benchmarks.md`, and report what was proven, measured, and left open.

## G. Third round (the items F left open)

Same ground rules. Tick each off here and in PROGRESS.md as it lands.

1. [x] **Operator audit history.** The `operator` audit field on ledger
   transfers and on `payments`, `refunds`, `psp_events`, `psp_refs` keeps
   only the last writer. Record every change: an append-only history (who:
   user or operator id; what: the command or event; when: the transaction's
   clock; before/after status or amounts) written in the same transaction
   as the change, by the kernels' own writes so it cannot be skipped. Prove
   that every state change the kernel makes appends exactly one history
   entry with the acting principal, and that history is never rewritten
   (only appended). Expose it read-only to the tenant's operators (paged).
   Mutants: a change without an entry, a wrong actor, an entry overwritten.
2. [x] **Idempotency namespaces per actor.** All operators share the `u0`
   idempotency namespace, so two operators (or an operator and a system
   job) using the same key collide. Key idempotency by the full principal
   (`User(id)` / `Operator(id)`, system operators included), keep an
   operator from replaying another's key, and keep the system jobs'
   deterministic keys working (they act as their named system operators).
   Migrate existing keys. Tests for the collision and for replays.
3. [x] **Scoped tokens, identity provider, revocation.**
   - Scopes: tokens carry scopes (e.g. `payments:read`, `payments:write`,
     `refunds:write`, `ledger:read`, `ledger:write`, `reconcile:read`,
     `recover`, `operator:*`); every route checks its scope before the
     kernel is called; operator tokens are limited to the scopes issued.
   - Identity provider: accept OIDC/JWT access tokens from a configured
     issuer (JWKS fetched and cached, `iss`/`aud`/`exp`/`nbf` checked,
     RS256/ES256 only, claims mapped to tenant, actor and scopes) next to
     the HMAC tokens; test with a local issuer (generated keys) — no
     external IdP is needed for the tests. Terraform: optional Cognito user
     pool or issuer URL as a variable.
   - Revocation: a token id (`jti`) on every token and a revocation list in
     PostgreSQL checked on each request (with a short in-process cache and
     a bounded staleness you state); revoke-by-token, by-actor and
     by-issued-before; operator routes to revoke; runbook updated.
   - Keep the h5i-app token format compatible or version it; if h5i-app
     itself needs a change, do it upstream as before.
   Update `docs/security-review.md` and `docs/runbooks/secret-rotation.md`.
4. [x] **Page the Transfers journal.** Done (PROGRESS.md "G4"). The ledger's Transfers journal read
   is not paged. Cursor pagination with a limit and a hard maximum, a read
   plan and frame theorem as in F3, the HTTP route and every caller
   (reconcile inputs, export, audit) moved to paged reads.
5. [x] **The remaining retries without losing throughput.** Done (PROGRESS.md "G5"). `MarkPosted`
   and `Create` conflict on the same index pages; serializing them removed
   the retries but cost 20% of throughput. Find a way that keeps both:
   e.g. take `MarkPosted` off the payment's hot path (mark asynchronously
   or in the next command's transaction, batch marks per tenant in the
   recovery job), a separate marker table keyed so it shares no pages with
   new payments, or index/key layout changes. Keep F2's proven guarantees
   (state exactly what the marker means if its timing changes). Measure
   at 10k/100k with 8 and 32 clients, interleaving the old and new builds
   on the same host load.
6. [x] **`POST /v1/reconcile` without a whole-tenant load.** Done (PROGRESS.md "G6"). It loads both
   kernels' full snapshots and the PSP report (~600 ms at 100k payments).
   Make it incremental or bounded: reconcile a time window or a settlement
   period (the PSP report is per period), read paged, stream through the
   engine, and page the exceptions in the reply; or keep a maintained
   reconciliation state updated as payments and settlements change. The
   result for a period must equal the whole-tenant reconciliation
   restricted to it (test against the full run on random data). Measure at
   10k/100k.

After G (done, PROGRESS.md "After G", `docs/benchmarks.md` "After G"): re-run all checks and the B4 end-to-end benchmarks (interleaved
with the previous build if the host is loaded), update `docs/benchmarks.md`,
`docs/security-review.md`, `docs/proof-assumptions.md`, and report what was
proven, measured and left open.

## H. Win back what G cost

Same ground rules. Tick each off here and in PROGRESS.md as it lands.

1. [x] **No extra round trip for the database clock.** Since G1 the engines
   use `Clock::Database`, which runs `SELECT transaction_timestamp()` as a
   separate statement in every kernel transaction; the after-G bisect puts
   the whole 19–23% throughput drop (≈183 → 142 payments/s) at G1. Read the
   clock without an extra round trip: e.g. fold it into the first query of
   the load, or let the write use `now()`/`transaction_timestamp()`. If
   this needs h5i-app-pg changes, make them upstream (below). The history's
   `at` must stay the transaction's own clock, monotonic per tenant as
   before; keep G1's proofs. Measure G1's two costs separately (clock vs
   history row) and report both.
2. [x] **Range comparisons in h5i-app reads.** h5i-app's read predicates
   have no range comparison, so a page is an `OR` of 64 keys and a G6
   period (≤ 4096 ids) costs 64 `Range` + 64 `RefundRange` + 128
   `TransferRange` kernel transactions (290–420 ms per call; whole tenant
   at 100k: 9.6 s vs 509 ms before G). Add range predicates (`>=`, `<`,
   and an ordered limit if needed) to h5i-app's read language
   (`h5i_app_sql::Pred`/`Read`, the reference `eval`, `h5i_app_pgsql`'s
   compiler, the Lean `H5iAppLib.Query` semantics and the differential
   test against PostgreSQL), so a page or a period is one read per table.
   Do it in `/root/h5i-upstream` on a branch from current `main`, with
   h5i's own checks (its CI script `scripts/app/ci-rust-tests.sh`, lean
   job, examples) passing, push the branch over SSH
   (`git@github.com:h5i-dev/h5i.git`) and say so in PROGRESS.md. Until it
   is merged and released, h5i-pay may depend on that branch's commit by
   git rev (all h5i-app crates and `h5i_app_lib` from the same rev). Then
   move F3/G4/G6 paging (List, Accounts, Transfers journal, Range,
   RefundRange, TransferRange) to range reads, with plans and frame
   theorems re-proven, and re-measure reconcile (one call and whole
   tenant), paging, and the paged audit.

After H: re-run all checks and the B4 end-to-end benchmarks interleaved
with `74225cd`, update `docs/benchmarks.md` and PROGRESS.md, and report
what was proven, measured and left open.

## I. After the range PR, and a readable README

Same ground rules. Tick each off here and in PROGRESS.md as it lands.

0. [x] **Pin to h5i main.** h5i PR #775 squash-merged `app/range-compare`
   into main as `7b91863` (`a98bcbd` is not an ancestor of main). Pin every
   h5i-app crate and `h5i_app_lib` to `7b91863` (same rev everywhere,
   lake-manifest updated) and re-run the checks; nothing else should change.
1. [x] **`Create` retries.** Since H1 the remaining serialization retries
   (about 5–42 per run, ~1% of transactions) are `Create` aborts in
   PostgreSQL's log. Find the conflicting reads/writes (new payments share
   the tail page of the primary key and the per-merchant sequence; check
   the read plan of `Create`, the sequence/counter rows, `open_payments`
   triggers, idempotency rows), and remove them without losing
   throughput (e.g. narrower `Create` plan, per-merchant advisory lock,
   id allocation that does not read a shared counter). Measure retries and
   throughput at 10k/100k with 8 and 32 clients, interleaved with `d71881b`.
2. [x] **Larger pages for the paged audit.** At 100k the paged audit reads
   about 3,400 journal pages of 64 (4.6 s). Let the audit (and other
   internal readers: export, reconcile, recovery) use larger range pages
   (e.g. up to 4096) while the HTTP page limit stays bounded; plans and
   frame theorems as before. Re-measure the audit at 1k/10k/100k.
3. [ ] **`find_work` without a whole-tenant load.** `POST /v1/recover`'s
   `find_work` still loads the whole tenant's payments. Read only the
   unfinished work (the `open_payments` / `payments_refunding` sets and
   range reads), keep it at least as thorough as now (it also catches a
   ledger restored from an older backup: keep that check, by range pages),
   prove or test that it finds exactly what the whole-tenant version
   found (differential test on random states), and measure at 10k/100k.
4. [ ] **README in the style of h5i-ai-gateway.** Rewrite `README.md`
   after <https://github.com/h5i-dev/h5i-ai-gateway/blob/main/README.md>:
   - one short opening paragraph: what h5i-pay is, that the money-moving
     decisions are a pure Rust kernel extracted to Lean 4 and proven on
     h5i-app, and what is ordinary Rust around it;
   - then short sections, each a few sentences and one code block at
     most: **Run** (local, PostgreSQL in Docker, Mock PSP),
     **Deploy** (AWS via Terraform, one paragraph, link to
     `infra/README.md`), **Use** (create a payment, capture, refund with
     curl and a token), **How it decides** (the kernel commands of a
     payment and the ledger postings, SERIALIZABLE, idempotency; link to
     design docs), **Verification** (the commands: cargo test, h5i app
     check/mutate/lint, what is proven in two or three sentences, what is
     trusted; link to `docs/proof-assumptions.md`), **GPU batch** (one
     paragraph, link to `docs/benchmarks.md` and `docs/gpu-toolchain.md`),
     **Performance** (one small table of the current end-to-end numbers
     with the hardware and date; link to `docs/benchmarks.md`), **Code**
     (one line per crate/dir), **License**;
   - plain sentences, no long bullet walls, no history of milestones (that
     stays in `docs/PROGRESS.md`), no internal plan items; every number
     must come from `docs/benchmarks.md`; every command must work as
     written (run them).
   Move anything removed from the README that is still needed into the
   right doc and link it.

After I: re-run all checks and the B4 end-to-end benchmarks interleaved
with `d71881b`, update `docs/benchmarks.md` and PROGRESS.md, and report.
