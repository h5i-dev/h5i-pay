# h5i-pay progress log

Status of the milestones in `docs/design-v0.1.md`, kept so work can resume
after a disconnect. Newest entries first within each milestone.

## Environment (verified 2026-10-31)

| Tool | Version / location | Notes |
|---|---|---|
| Rust | 1.99.1 | workspace edition 2024 |
| h5i CLI | 0.5.2, `~/.local/bin/h5i` | built from `/root/h5i-upstream` with `H5I_SKIP_WEB_BUILD=1` (no Node on the host, so the `h5i ui` web bundle is not embedded) |
| h5i-app | 0.1.4 from crates.io, pinned `=0.1.4` | `h5i-app-gpu` is not on crates.io: git dependency on h5i tag `v0.5.2` |
| Charon / Aeneas | 4bd5a29f6e97 / b86120db3183 at `/opt/aeneas/{charon,aeneas}/bin` | built with `nix build github:AeneasVerif/aeneas/<rev>` (Determinate Nix installer); `h5i app doctor` is green when both are on `PATH` |
| Lean | leanprover/lean4:v4.31.0 via elan (`~/.elan/bin`) | Mathlib cache from `lake exe cache get` |
| PostgreSQL | 17 in docker (`scripts/dev-db.sh`, port 55432) | |
| CUDA | 13.2, NVIDIA A10G (sm_86) | |

`h5i` v0.5.2 has no `init` or `capture` commands (`h5i --help` lists
`box ui browser plugin project app skill completion`), so commits are
plain `git commit`; provenance for the proofs is `h5i app check` /
`h5i app mutate`, whose records live under `proofs/.lake/h5i`.

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

## M3 — GPU Batch Processing: not started

## M4 — AWS Deployment (IaC only): not started

## M5 — Production Hardening (docs): not started

## Open items / decisions

- u32 columns break the Lean that `h5i-app-schema` 0.1.4 renders (its
  generated proofs fail on `int ↑(u32)`), so every kernel column is `u64`.
- Every command loads the whole tenant snapshot (h5i-app's default
  `Store::load`). Correct, but O(rows) per request; a `load_for` with a frame
  theorem is the scaling path (see `docs/proof-assumptions.md`).
