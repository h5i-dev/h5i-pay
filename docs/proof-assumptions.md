# Proof assumptions and trust boundary

What the Lean proofs in `proofs/` establish, what they rest on, and what is
not proven. Every theorem named here builds with `lake build` and passes
`h5i app check` (standard axioms only: `propext`, `Classical.choice`,
`Quot.sound`; no `sorry`, `native_decide` or `axiom`).

## What is proven, and about what code

| Area | Theorems | About |
|---|---|---|
| Ledger | `payment_ledger.Theorems.conservation`, `journal_balanced`, `no_overdraft`, `no_overcredit`, `posting_balanced`, `replay_is_noop`, `only_owner_or_operator_debits`, `reachable_inv` | the Lean that Charon+Aeneas extract from `crates/payment-ledger/src/lib.rs` (`proofs/generated/PaymentLedger.lean`) |
| Batch | `payment_ledger.Batch.batch_serial` | the extracted `batch` loop |
| Storage | `payment_ledger.Storage.db_inv` | the extracted `sql_writes`/`decode` and h5i-app's SQL model (`H5iAppLib.Pg`) |
| Engine model | `Models.isolation`, `noninterference`, `others_commute`, `keyed_replay`, `keyed_conflict`, `ledger_isolation` | a hand-written Lean model of the engine (`proofs/Models/Engine.lean`) |

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
