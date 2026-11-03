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
| Reconciliation join | `payment_reconcile.Join.probe_spec`, `insert_spec`, `build_spec`, `lookup_spec`, `internal_code_spec`, `join_eq_reference`, `join_rows` | the extracted `crates/payment-reconcile/src/join.rs`, the single-thread hash join (`cpu_single`, and `cpu_parallel` per partition), equal row for row to the reference's counts and first external record |
| History (G1) | `payment_ledger.History.every_change_logged`, `history_appended`; `payment_domain.Theorems.every_change_logged`, `history_appended` | the extracted `post`/`open` (ledger: each account row followed by its entry) and the extracted `transition` = `decide_command` + `logged` (payments: one entry per change of a payment); in every reachable state the history is only appended to, under fresh keys |
| Operator reads (G4) | `payment_ledger.Theorems.operator_reads` | the extracted `transition`: `Transfers` (an account's journal page), `AccountRange` and `History` succeed only for operators and write nothing; their pages are covered by `Frame.frame` like every other command (`journal_page_congr`, `account_range_congr`) |
| Read-only commands (G6, payments) | `payment_domain.ReadOnly.sound`, `payment_domain.Theorems.range_operator` | the extracted `read_only` and `transition` of the payment kernel: `Get`, `List`, `GetRefund`, `Refunds`, `GetRef`, `View`, `History`, `Range` and `RefundRange` write nothing when accepted; only operators read a `Range` or `RefundRange` page |
| Reconciliation periods (G6) | `payment_reconcile.Period.join_period`, `rowCode_restrict`, `extCode_restrict` | the extracted single-thread join: on the internal and external rows whose keys are in any set `P`, every row gets the code the run on all rows gives it |
| Read-only commands (G4) | `payment_ledger.ReadOnly.sound` | the extracted `read_only` and `transition`: every command `read_only` names writes nothing when accepted, for every actor and state (h5i-app's `H5iAppLib.ReadOnly.Sound`), so the engine may run it `SERIALIZABLE READ ONLY DEFERRABLE` |
| Read plans | `payment_ledger.Plan.reads_abs`, `payment_ledger.Frame.frame`, `load_for_sound`; `payment_domain.Plan.reads_abs`, `payment_domain.Frame.frame`, `load_for_sound`; `*.Schema.decode_load` (generated) | the extracted `reads` of each kernel (`Store::load_for` runs it) and the extracted `transition`: on what each command's plan loads, `transition` gives its whole-tenant result |

Gate results (after I1, h5i `9ec26b5`): `h5i app check
crates/payment-ledger` — 375 theorems in 15 modules; `h5i app check
crates/payment-domain` — 450 theorems in 10 modules; `h5i app check
crates/payment-reconcile` — 69 in 3; all on the three standard axioms
only. (After G: 376, 451, 69; after G1: 317, 405, 64; after F: 257, 293
and 64; with the first read plans: 227 and 228; before them: 145 and 122.)

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
| ledger plan | `journal_plan_wrong_column` (G4) | `Plan.reads_abs` |
| ledger frame | `account_range_past_end` (G4) | `Frame.id_range_spec` |
| ledger | `journal_open_to_users` (G4) | `Invariants.journal_page_writes` (and `journal_page_operator`) |
| ledger | `transfer_declared_read_only` (G4) | `ReadOnly.sound` |
| ledger frame | `transfer_range_past_end` (G6) | `Frame.transfer_ids_spec` |
| payments plan | `range_plan_misses_refund_payments` (G6) | `Plan.reads_abs` |
| payments | `range_open_to_users` (G6) | `Commands.range_page_writes` (the proof follows the operator check) |
| payments | `create_declared_read_only` (G6) | `ReadOnly.decide_reads_nothing` |

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
   `usize::MAX − 5·8192` rows (five writes per transfer since G1). Needed so that `Vec::push` in a maximal batch
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
   Since H2 an id or sequence range `[from, end)` is a range read
   (`seq_range`): `col >= from AND col < end`, or `col >= from OR col < end`
   when it crosses 2^63. h5i-app stores a `u64` as its 64 bits, so ids from
   2^63 up (every system transfer) are negative `BIGINT`s, and `Query.Pred.ge`
   / `.lt` compare in that signed order (`IntOf`, `intOfVal`). Proven in each
   kernel's `Frame.lean`: `intOf_int` (a `u64` column's `BIGINT` is `n` below
   2^63 and `n − 2^64` above) and `rangeA_holds_of` (every row whose column
   holds a `u64` in `[from, end)` satisfies the read, for every `from < end ≤
   u64::MAX`, the wrap included), which is all `Frame.frame` uses;
   `Plan.seq_range_spec` pins the extracted `seq_range` to that read. That
   the read returns no row outside the range is not proven (nothing relies
   on it). `tests/integration/sign_ranges.rs` runs `TransferRange` and
   `Range` against PostgreSQL on ids on both sides of 2^63 and up to
   `u64::MAX`; the mutant `range_plan_ignores_wrap` (no `OR` for a range
   across 2^63) is caught by the gate.
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
   the token's job. Since G3 the token also limits what the principal may
   ask (scopes, checked by the routes before a kernel runs), and a revoked
   token is refused; both are shell checks (`payment_api::routes::Authed`),
   not proven: a kernel theorem holds for every principal the shell passes,
   whatever its scopes. Access tokens from the identity provider are
   trusted as far as the issuer and its keys are (`payment_api::oidc`).

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
   the journal itself. Since G5 the scheduled recovery finds unmarked
   payments through `open_payments`. Since I1 two triggers on `payments`
   only insert there (a payment inserted unmarked, a marker cleared), so
   it holds every payment with `ledger_done = false` and, until
   `prune_open_payments` removes them at the start of each scheduled pass,
   marked ones too; `recovery_work` re-checks `NOT ledger_done` on each row.
   The pruning deletes a row only with its payment locked `FOR SHARE` and
   seen marked (`READ COMMITTED`), so a concurrent clearing of the marker
   either commits first (the re-check keeps the row) or waits and then
   inserts it again. Not proven; tested in
   `payments_load_for_matches_the_whole_tenant` (superset, then equal
   after pruning) and `pruning_keeps_every_unmarked_payment` (pruning in a
   loop while 8 tasks clear and set markers; it caught a pruning without
   the row lock in 3 of 6 runs: a race, not a proof). A row missing from
   `open_payments` would delay that payment's recovery until `POST
   /v1/recover` or reconciliation finds it.

8. **The history's clock and its storage (G1).** Both kernels prove that
   every change writes exactly one history entry per changed payment or
   account, naming the principal and `a.at`, and that in every reachable
   state the history is only appended to (`every_change_logged`,
   `history_appended` in each kernel). Trusted: that `a.at` is the
   transaction's clock. The engine stamps it before every attempt
   (`Kernel::stamp`) from `Clock::Database`, PostgreSQL's
   `transaction_timestamp()` in microseconds (`payment_storage::engines_with`
   sets it); the kernel only copies it. Since H1 h5i-app-pg sends that
   `SELECT` in the same flight as the load and sends a transaction's writes
   pipelined, in plan order (h5i PR #773, main `ae87ef7`); trusted with the
   rest of the engine: `transaction_timestamp()` is fixed for the
   transaction whatever statement runs first, and PostgreSQL runs a
   connection's pipelined statements in the order they were sent. It is not monotone across
   transactions (two transactions that start in one order may commit in the
   other; `n` gives the order per payment or account). That nothing outside
   the kernels rewrites the history is PostgreSQL's: triggers on both
   `history` tables refuse `UPDATE`, `DELETE` and `TRUNCATE` from any role
   (`payment_storage::append_only_history`; `payments_api::every_change_is_in_the_history`
   checks it), so a stray `ON CONFLICT DO UPDATE` would abort the
   transaction rather than overwrite an entry. A superuser can still drop
   the triggers. Payments and accounts created before G1 start with
   `changes = 0`: their history begins with their first change after the
   migration. The payments proof project raises Lean's `maxHeartbeats` for
   the generated schema (the 17-column `HistoryEntry` row decode); nothing
   else changes in what is checked.

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
  (the hash-table build with atomics). Since J1 the same holds for the
  column entry points (`recon_build_cols`, `recon_internal_cols`,
  `recon_external_cols`: one byte per code, `rules::internal_row_cols`)
  and the host's staged, two-stream copies (`Gpu::reconcile_cols`), which
  `gpu_columns_match_the_reference` checks row by row. The SIMD policy
  engine is a separate implementation checked against `check` by tests.
- **The batch worker's input files are parsed by trusted code.**
  `payment_batch::binfile` (header, layout, length and checksum checks, the
  column views of the mapping) and the records-CSV parser are tested
  (truncated, oversized, misaligned, corrupted and random inputs) and
  fuzzed (`crates/payment-batch/fuzz`), not proven. The checksum detects
  corruption, not tampering. The engines still refuse the reserved key
  (`validate_cols`), so a file that passes the format checks cannot break
  the join's preconditions.
- **The hash joins: the single-thread join is proven, the others are
  tested.** `join.rs` (open addressing, linear probing from `rules::hash`
  masked to the capacity, per key a row count and the lowest row index) is
  extracted, and `payment_reconcile.Join.join_eq_reference` proves that it
  gives every internal row `rowCode` (both key counts and the first
  external record) and the first external index (`NONE` if none), and every
  external row `extCode`: the per-row answer of the `BTreeMap` reference
  (`reference` itself is not extracted; it reads those definitions off a
  map of key to row list). Preconditions: no key is `EMPTY`, fewer than
  `u32::MAX` rows a side (`validate`), `4n + 32` fits a `usize`. The proof
  does not depend on where probing starts. `cpu_single` is this code;
  `cpu_parallel` runs it on each partition. Not proven, and checked by the
  differential tests (every engine against the reference on every row):
  - `cpu_parallel`'s radix partition and the mapping back to global
    indices (that a partition holds all rows of its keys, in increasing
    order, so local first rows are the global ones), on records or, since
    J1, on columns (`cpu_parallel_cols`), with a partition count that grows
    with the input (`partitions`);
  - the SIMD engine's `Table` and vector probe;
  - the GPU's `recon_build` (CAS on the key, `atomicAdd` on the count,
    `atomicMin` on the first index) and its probe `find_slot`. They
    implement the proven sequential algorithm (same start, same probe
    order, same per-key count and lowest index), but the concurrent build
    is not covered: that every interleaving ends in a table with distinct
    keys and full probe paths is not proven.
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
  of at most 64 sequence numbers of one merchant or owner. Since G4 the
  ledger journal (`Transfers`) is one page of history entries of one
  account, and `AccountRange` one page of account ids, both operator-only
  and since I2 of at most 4096 (`MAX_RANGE`; before, 64): the system's own
  readers (the paged audit, reconciliation, recovery) read pages of 1024
  (`payment_storage::ledger::JOURNAL_PAGE`). Over HTTP these pages stay at
  most 64: `payment_storage::ledger::http_limit` turns a larger limit into
  0, which the kernel refuses (`BadPage`). That bound is in the HTTP
  decoding, not proven (tested in `ledger_api`);
  the audit, `find_work` and `reconcile_inputs` read the ledger through
  them. Since G6 the payment kernel's `Range` and `RefundRange` (ids of
  payments, or of refunds with their payments) and the ledger's
  `TransferRange` serve reconciliation and the export, one period of at
  most 4096 ids per `POST /v1/reconcile`; since H2 each reads its ids as one
  range (`MAX_RANGE` = 4096 ids), so a period is one read per table.
  Since I3 `Orchestrator::find_work` (`POST /v1/recover`) reads the tenant
  by pages of 4096 ids too: per page `Range`, `RefundRange` and the
  `TransferRange`s of the four posting kinds, each its own read-only
  transaction. On a state that does not change meanwhile it returns what
  the whole-tenant version (`find_work_whole`) returns; not proven, tested
  on random states with pages of 1, 2, 3, 7 and 4096 ids
  (`find_work_matches_the_whole_tenant`). Its journal check reads
  transfers by id where the whole-tenant version read the journals of the
  PSP clearing and refund holding accounts; these agree because every
  system posting is in the journal of one of those accounts (it debits or
  credits one) and a transfer is in the journals of both its accounts
  (`every_change_logged`). Whole-tenant snapshots remain only in
  `find_work_whole` and `reconcile_inputs` (the references the tests
  compare with; no route calls them) and in the benchmark's row count.
- **A reconciliation period is a range of ids, not of clock time.** The
  result for a period equals the whole-tenant result restricted to the
  keys with ids in it (`Period.join_period` for the single-thread join;
  the other engines are tested against it, and the period's inputs are
  tested to be exactly the whole run's rows in the period). Trusted: that
  the shell reads every row of the period (all `Range` and `RefundRange` pages up to `to`
  or the counter, both `TransferRange` kinds over the whole period, and
  the PSP's lines with those ids), which the tests check against the
  whole-tenant run. Pages are separate read-only transactions, so a
  period read while payments change is not one instant: a payment changed
  between two pages shows its state at its own page. A period is `closed`
  only when ids past it existed; an open period can gain rows. The PSP
  side: the Stripe connector has no way to select by our ids, so its
  `settlement_report_range` still reads the whole report and filters it
  (`created` filters by time would need times we do not store).
- **The paged audit is per account, not one snapshot.** Each account is
  checked against its journal up to the `changes` its row had when read,
  in separate transactions. A transfer committed between two accounts'
  reads shows on one side only; the audit finds it past the other
  account's bound and reports it in flight. The audit's claim is
  therefore "every account equals the sum of its own journal, and every
  transfer has both legs", not "the tenant total was zero at one instant".
- **Pre-G1 journal entries are reconstructed.** The G4 backfill gives
  transfers older than the history entries with time 0 and command
  `CMD_MIGRATED`; their actor is the transfer's operator or, for user
  transfers, the debit account's owner (who had to be the caller since
  SEC-03). This attribution is a migration rule, not a recorded fact.
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
