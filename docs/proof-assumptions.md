# Proof assumptions and trust boundary

What the Lean proofs in `proofs/` establish, what they rest on, and what is
not proven. Every theorem named here builds with `lake build` and passes
`h5i app check` (standard axioms only: `propext`, `Classical.choice`,
`Quot.sound`; no `sorry`, `native_decide` or `axiom`).

## What is proven, and about what code

| Area | Theorems | About |
|---|---|---|
| Ledger | `payment_ledger.Theorems.conservation`, `journal_balanced`, `no_overdraft`, `no_overcredit`, `posting_balanced`, `replay_is_noop`, `replay_needs_debit_right`, `only_owner_or_operator_debits`, `reachable_inv` | the Lean that Charon+Aeneas extract from `crates/payment-ledger/src/lib.rs` (`proofs/ledger/generated/PaymentLedger.lean`) |
| Batch | `payment_ledger.Batch.batch_serial` | the extracted `batch` loop |
| Storage | `payment_ledger.Storage.db_inv` | the extracted `sql_writes`/`decode` and h5i-app's SQL model (`H5iAppLib.Pg`) |
| Engine model | `Models.isolation`, `noninterference`, `others_commute`, `keyed_replay`, `keyed_conflict`, `ledger_isolation` | a hand-written Lean model of the engine (`proofs/ledger/Models/Engine.lean`) |
| Payments | `payment_domain.Theorems.status_steps_allowed`, `terminal_final`, `captured_final`, `refund_safety`, `refunded_le_captured`, `event_dedup`, `reachable_inv` | the extracted `crates/payment-domain/src/lib.rs` (`proofs/payments/generated/PaymentDomain.lean`) |
| Policies | `payment_domain.Policies.check_sound` | the extracted `crates/payment-domain/src/policy.rs` |
| Reconciliation | `payment_reconcile.Reconcile.classify_internal_spec`, `classify_external_spec`, `codes_in_range`, `internal_matched_iff`, `external_matched_iff`, `duplicates_first`, `exception_reported_once`, `pair_matched_both_sides` | the extracted `crates/payment-reconcile/src/rules.rs` (`classify_internal`, `classify_external`), the per-row decision every engine calls, the GPU included (`proofs/reconcile/generated/PaymentReconcile.lean`) |
| Read plans | `payment_ledger.Plan.reads_abs`, `payment_ledger.Frame.frame`, `load_for_sound`; `payment_domain.Plan.reads_abs`, `payment_domain.Frame.frame`, `load_for_sound`; `*.Schema.decode_load` (generated) | the extracted `reads` of each kernel (`Store::load_for` runs it) and the extracted `transition`: on what each command's plan loads, `transition` gives its whole-tenant result |

Gate results (h5i-app 0.5.2, with the read plans): `h5i app check
crates/payment-ledger` — 227 theorems in 13 modules; `h5i app check
crates/payment-domain` — 228 theorems in 8 modules; both on the three
standard axioms only. (Before the read plans: 145 and 122.)

Ledger `currency` and `code`, and payment `currency` and `status`, are
`u32` columns. They are stored as `BIGINT` like `u64` columns; on load,
h5i-app-schema's `from_val` refuses a value outside `0..=u32::MAX`
(`u32_from_val` in `generated/*Schema.lean`), so `db_inv` covers them.

The specifications were tested against deliberately wrong kernels
(`h5i app mutate`, mutants listed in each crate's `h5i-app.toml`); every
mutant broke a proof. The table lists the first proof that failed, taken
from the `lean_build` stage log in the run record
(`crates/*/.h5i/app/runs/*-mutation-*/run.json`). Lake stops at the first
failing module, so theorems downstream of it (for example `conservation`
for the ledger mutants) were not re-checked in the trial; they depend on
the failing step lemma and cannot be proved without it.

| Kernel | Mutant (`h5i-app.toml` name) | First failing proof |
|---|---|---|
| ledger | `self_transfer_allowed` | `Commands.transfer_spec` |
| ledger | `overdraft_allowed` | `Commands.post_spec` |
| ledger | `anyone_may_debit` | `Commands.may_debit_spec` |
| ledger | `credit_side_short` | `Commands.post_spec` |
| ledger | `replay_ignores_amount` | `Commands.same_transfer_spec` |
| ledger | `replay_answers_anyone` | `Commands.transfer_spec` |
| payments | `refund_past_capture` | `Commands.request_refund_spec` |
| payments | `capture_beyond_authorization` | `Commands.record_captured_spec` |
| payments | `decline_after_capture` | `Commands.record_declined_spec` |
| payments | `webhook_not_deduplicated` | `Commands.event_spec` |
| payments | `refund_success_without_release` | `Commands.record_refund_spec` |
| policy | `blocked_recipient_approved` | `Policies.check_sound` |
| ledger plan | `transfer_plan_misses_credit_account` | `Plan.accounts_of_eq` |
| ledger plan | `user_accounts_plan_wrong_column` | `Plan.reads_abs` |
| payments plan | `record_plan_misses_refund` | `Plan.result_reads_abs` |
| payments plan | `refunds_plan_reads_by_refund_id` | `Plan.reads_abs` |

The plan mutants are caught where the extracted `reads` stops computing
the plan `readsA` that `Frame.frame` is proved for; a wrong `readsA`
itself would fail `Frame.frame`.

"Reachable" (`Spec.Reachable`) means: reached from the empty ledger by any
sequence of commands that the kernel accepted, from any callers, in any
order. The engine runs each request in a SERIALIZABLE transaction, so the
committed history is such a sequence.

## Assumptions

1. **Translation.** Charon and Aeneas (pinned commits, see `docs/PROGRESS.md`)
   translate the kernel faithfully, and rustc compiles it as written. The
   extraction is checked fresh with `h5i app extract --check`.
2. **Engine contract (trusted Rust, `h5i-app-pg` 0.1.4).** Each request loads
   only the caller's tenant (`TenantId(actor.org)`), runs `transition` once
   per attempt inside one SERIALIZABLE transaction, writes the returned write
   set through `sql_writes`, and commits; serialization failures and dropped
   connections restart from `BEGIN`. With an idempotency key a request runs at
   most once per (tenant, scope, key). `Models/Engine.lean` states this as a
   model and proves its consequences; that the Rust engine implements the
   model is assumed. Tests check it against PostgreSQL
   (`tests/concurrency`, `tests/fault-injection`).
3. **PostgreSQL.** Behaves as `H5iAppLib.Pg` models it (tenant-scoped keyed
   upserts and selects) and provides SERIALIZABLE isolation.
4. **Room in memory.** `Spec.HasRoom`: a loaded snapshot has fewer than
   `usize::MAX − 3·8192` rows. Needed so that `Vec::push` in a maximal batch
   cannot overflow; true on any 64-bit host. The ledger's frame theorem uses
   it for batches (so `apply` between a batch's transfers cannot overflow on
   the whole tenant either).
4a. **Declarative reads.** `Store::load_for` loads each table by one
   `h5i_app_sql::Read` of the extracted plan (`schema_load_plan`). Trusted:
   h5i-app-pgsql's `compile_read` turns a read into SQL that returns
   `H5iAppLib.Query.Read.rows` of the tenant's tables in key order (h5i
   differential-tests it against PostgreSQL and the Lean definitions; it is
   not proven), and `Plan.Read.abs` is a faithful reading of the extracted
   `h5i_app_sql::Read` (a structural map, five lines per kernel). Here,
   `tests/concurrency` (`load_for_matches_the_whole_tenant`,
   `payments_load_for_matches_the_whole_tenant`) runs `transition` on the
   rows PostgreSQL returns for each plan and on the whole tenant, for 400 and
   500 random commands, and compares the results.
5. **The shell.** JSON decoding, HMAC token checking (`h5i-app-http`), the
   orchestrator in `payment-api` and the PSP connectors are trusted. The
   kernel's theorems hold for every command, so a decoding bug can do no more
   than a client could ask for.
6. **Identity.** The kernels' principal is `Actor::User(id)` or
   `Actor::Operator(id)` of a tenant (`payment_storage::*::{user, operator}`).
   Only a token signed with the operator key (`H5I_PAY_OPERATOR_SECRET`)
   becomes an operator, under the id in the token (≥ 1, below 2^62);
   user-key tokens for user 0 are refused (`payment_api::auth`). Whoever
   holds the operator key can act as any tenant's operator under any
   non-system id; whoever holds the user key can act as any user. Ids from
   2^62 up are the system's in-process operators (orchestrator, recovery,
   export) and are trusted to be used only by that code. Given the
   principal, the kernels prove who may do what and that every row records
   the acting operator's id (`transfers_record_operator`,
   `writes_record_operator`); that the principal names the right person is
   the token's job.

7. **The ledger marker (F2).** The payment kernel proves that only an
   operator's `MarkPosted` sets `ledger_done`, only on the state it names
   (a settled payment — captured, failed or canceled — with that captured
   amount and exactly the payment's current refund rows), and
   that every other command writing the payment or one of its refunds
   clears it (`marked_only_as_seen`, `changes_clear_ledger_done`). That the
   ledger really holds the postings when the orchestrator sends
   `MarkPosted` is the orchestrator's (trusted) part: `sync_ledger` sends
   it only after every batch was accepted (`Posted`/`Exists`). The ledger
   never deletes or rewrites a transfer, so a marked payment's postings stay
   unless rows are changed outside the engines; the scheduled recovery
   trusts the marker, and `POST /v1/recover` and reconciliation also check
   the journal itself.

## Not proven (known limits)

- **The orchestrator is not verified.** The payment kernel guarantees that
  whatever is recorded is a valid transition with consistent amounts; the
  orchestrator decides *what* to record from PSP answers. A wrong mapping
  from a PSP answer to a `PspResult` would be a valid, wrong record.
  Reconciliation against the PSP report is the check on that.
- **Ledger and payments are separate kernels.** The ledger postings for a
  payment are made after the payment kernel commits, with deterministic
  transfer ids; they converge (recovery re-posts) but are not atomic with
  the payment record. Reconciliation of payments against the ledger journal
  reports any gap. Merging the two kernels into one transaction is the
  alternative, at the cost of one larger kernel and proof.
- **Webhook event ids are hashed to 64 bits** (SHA-256 prefix of
  `psp\0event_id`). Two different events colliding would make the second
  one a "duplicate"; the chance is about n²/2⁶⁵ for n events per tenant.
- **Policy completeness** is not claimed: `check_sound` says approval implies
  every rule holds. The merchant allowlist uses binary search, which may
  miss a merchant on an unsorted list (refusing, never wrongly approving).
- **CPU/GPU equivalence** of the batch engines is checked by differential
  tests, not proven. Since A (2026-11) the GPU runs the same Rust: the
  device crate (`crates/payment-gpu/device`) includes `policy.rs` and
  `rules.rs` verbatim and is compiled to PTX by Rust-CUDA's
  `rustc_codegen_nvvm` (`docs/gpu-toolchain.md`); the CUDA C transcription
  is gone. The policy decision and the reconciliation classification on the
  GPU are therefore the verified functions, compiled by a different
  compiler. Trusted for the GPU path: the nightly rustc front end,
  `rustc_codegen_nvvm` and `cuda_std` (rust-cuda `3a82c35a`), LLVM 7.1,
  libNVVM 12.6, the PTX JIT, the CUDA driver and hardware, `cust`, the
  committed PTX being the output of `build-device.sh` (`--check` rebuilds
  and compares), and the device crate's launch shell and `recon_build`
  (the hash-table build with atomics). The SIMD policy engine is a
  separate implementation checked against `check` by tests.
- **The hash joins are not proven.** `classify_internal` /
  `classify_external` are proven (`proofs/reconcile`); that each engine's
  hash table (CPU `Table`, the GPU's `recon_build`, `find_slot`) hands
  them the right counts and first external record is tested (every engine
  against the `BTreeMap` reference on every row), not proven.
- **Tenant isolation inside PostgreSQL** is proven for the SQL model and the
  engine model, not for the real engine or database.
- **Replays and authorized callers.** Since SEC-03 only a caller who may
  debit the debit account reaches the journal lookup
  (`replay_needs_debit_right`); such a caller still learns from
  `IdConflict` that an id it reuses was taken.
- **Liveness.** Nothing proves that a valid command eventually succeeds; under
  contention the engine can give up after `max_attempts` (tests show this
  path leaves the ledger untouched).
- **Account counters are u64.** A transfer that would overflow an account's
  posted debits or credits is refused (`Overflow`), not wrapped; the proofs
  rely on the refusal, not on a bound.
- **No kernel command loads the whole tenant** (F3). Ledger batches are at
  most 64 transfers (`MAX_BATCH`; longer ones are refused before reading,
  since an `OR` chain of a few hundred terms already costs PostgreSQL
  tens of milliseconds of planning), and `List`/`Accounts` read one page
  of at most 64 sequence numbers of one merchant or owner. Whole-tenant
  snapshots remain only where taken on purpose: `reconcile_inputs`,
  `audit`, `find_work` and the export.
- **Pages are numbered by per-merchant and per-owner counters** (`seq`,
  `merchant_counters`, `owner_counters`). The frame theorems say a page's
  plan gives the whole-tenant reply; they do not say the numbers are
  dense. They are by construction (each `Create`/`Open` takes the
  counter's value and increments it; existing rows were numbered by id at
  migration), so a page may be short only at the end; `next` comes from
  the counter, not from counting rows.
- **The frame theorems are about one command's read.** They say the
  partial load gives the whole-tenant result; that the server then commits
  that result rests, as before, on SERIALIZABLE: a concurrent write to a row
  outside the plan cannot change the decision, and one inside it makes
  PostgreSQL abort and the engine retry.
