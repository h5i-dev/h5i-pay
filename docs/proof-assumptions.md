# Proof assumptions and trust boundary

What the Lean proofs in `proofs/` establish, what they rest on, and what is
not proven. Every theorem named here builds with `lake build` and passes
`h5i app check` (standard axioms only: `propext`, `Classical.choice`,
`Quot.sound`; no `sorry`, `native_decide` or `axiom`).

## What is proven, and about what code

| Area | Theorems | About |
|---|---|---|
| Ledger | `payment_ledger.Theorems.conservation`, `journal_balanced`, `no_overdraft`, `no_overcredit`, `posting_balanced`, `replay_is_noop`, `only_owner_or_operator_debits`, `reachable_inv` | the Lean that Charon+Aeneas extract from `crates/payment-ledger/src/lib.rs` (`proofs/ledger/generated/PaymentLedger.lean`) |
| Batch | `payment_ledger.Batch.batch_serial` | the extracted `batch` loop |
| Storage | `payment_ledger.Storage.db_inv` | the extracted `sql_writes`/`decode` and h5i-app's SQL model (`H5iAppLib.Pg`) |
| Engine model | `Models.isolation`, `noninterference`, `others_commute`, `keyed_replay`, `keyed_conflict`, `ledger_isolation` | a hand-written Lean model of the engine (`proofs/ledger/Models/Engine.lean`) |
| Payments | `payment_domain.Theorems.status_steps_allowed`, `terminal_final`, `captured_final`, `refund_safety`, `refunded_le_captured`, `event_dedup`, `reachable_inv` | the extracted `crates/payment-domain/src/lib.rs` (`proofs/payments/generated/PaymentDomain.lean`) |
| Policies | `payment_domain.Policies.check_sound` | the extracted `crates/payment-domain/src/policy.rs` |

Gate results at the M5 commit: `h5i app check crates/payment-ledger` — 145
theorems in 11 modules; `h5i app check crates/payment-domain` — 121
theorems in 6 modules; both on the three standard axioms only.

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
| payments | `refund_past_capture` | `Commands.request_refund_spec` |
| payments | `capture_beyond_authorization` | `Commands.record_captured_spec` |
| payments | `decline_after_capture` | `Commands.record_declined_spec` |
| payments | `webhook_not_deduplicated` | `Commands.event_spec` |
| payments | `refund_success_without_release` | `Commands.record_refund_spec` |
| policy | `blocked_recipient_approved` | `Policies.check_sound` |

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
   cannot overflow; true on any 64-bit host.
5. **The shell.** JSON decoding, HMAC token checking (`h5i-app-http`), the
   orchestrator in `payment-api` and the PSP connectors are trusted. The
   kernel's theorems hold for every command, so a decoding bug can do no more
   than a client could ask for.
6. **Identity.** User 0 of a tenant is its operator (`payment_storage::*::principal`);
   whoever holds the HMAC secret can mint any principal.

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
  tests on the inputs they cover, not proven (see `docs/benchmarks.md`).
  The CPU engines and the CUDA kernels are separate implementations of the
  Rust rules: the CUDA C in `crates/payment-gpu/kernels/batch.cu` is a
  transcription, not compiled from the extracted Rust. Trusted for the GPU
  path: nvcc, the PTX JIT, the CUDA driver and hardware, `cust`, the buffer
  layouts (asserted sizes), and the launch shell. Only the policy rules the
  CPU engines run are the verified function; the SIMD policy engine is a
  separate implementation checked against it by tests.
- **Reconciliation rules are not proven.** `payment_reconcile::rules` is
  tested (every code, random inputs, four engines against an oracle), not
  extracted.
- **Tenant isolation inside PostgreSQL** is proven for the SQL model and the
  engine model, not for the real engine or database.
- **The `Exists` replay path does not check the caller.** A principal who
  re-sends a transfer id that exists learns that it exists (and its own
  payload matched). It cannot change anything.
- **Liveness.** Nothing proves that a valid command eventually succeeds; under
  contention the engine can give up after `max_attempts` (tests show this
  path leaves the ledger untouched).
- **Account counters are u64.** A transfer that would overflow an account's
  posted debits or credits is refused (`Overflow`), not wrapped; the proofs
  rely on the refusal, not on a bound.
- **Whole-tenant loads.** Every command reads the tenant's full snapshot. A
  smaller `Store::load_for` would need a frame theorem that `transition`
  decides the same on the smaller snapshot; none is written yet.
