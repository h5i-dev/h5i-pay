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
- Lean (`proofs/Ledger`, `proofs/Batch`, `proofs/Models`), all on the
  extracted code except `Models/Engine.lean` (a model of the trusted engine):
  `conservation`, `journal_balanced`, `no_overdraft`, `no_overcredit`,
  `posting_balanced`, `replay_is_noop`, `only_owner_or_operator_debits`,
  `Batch.batch_serial`, `Storage.db_inv`, `Models.isolation`,
  `Models.noninterference`, `Models.keyed_replay`, `Models.ledger_isolation`.
  `h5i app check crates/payment-ledger`: 145 theorems in 13 modules on the
  standard axioms only; no `sorry`.
- Mutants (`h5i app mutate crates/payment-ledger`): see the M1 commit message
  for the result.
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

## M2 — Payment Orchestration: in progress

## M3 — GPU Batch Processing: not started

## M4 — AWS Deployment (IaC only): not started

## M5 — Production Hardening (docs): not started

## Open items / decisions

- u32 columns break the Lean that `h5i-app-schema` 0.1.4 renders (its
  generated proofs fail on `int ↑(u32)`), so every kernel column is `u64`.
- Every command loads the whole tenant snapshot (h5i-app's default
  `Store::load`). Correct, but O(rows) per request; a `load_for` with a frame
  theorem is the scaling path (see `docs/proof-assumptions.md`).
