# h5i-pay

An open-source payment platform on [h5i-app](https://github.com/h5i-dev/h5i):
a double-entry ledger and payment state machine written as pure Rust kernels
whose properties are proven in Lean 4, served over HTTP with every request
in a SERIALIZABLE PostgreSQL transaction. The design is
[docs/design-v0.1.md](docs/design-v0.1.md); progress is in
[docs/PROGRESS.md](docs/PROGRESS.md); what the proofs rest on is in
[docs/proof-assumptions.md](docs/proof-assumptions.md).

## Layout

| Path | Contents |
|---|---|
| `crates/payment-ledger` | ledger kernel (pure Rust, extracted to Lean) |
| `crates/payment-domain` | payment state machine, refunds, webhook dedup, batch policy rules (extracted to Lean) |
| `crates/payment-storage` | h5i-app PostgreSQL adapters, JSON codecs, audits |
| `crates/payment-connectors` | PSP connectors: Mock PSP, Stripe (test mode only), webhook signatures |
| `crates/payment-reconcile` | reconciliation rules and CPU engines |
| `crates/payment-api` | the `h5i-pay` HTTP server and the payment orchestrator |
| `proofs/ledger` | Lake project: extracted ledger, `Ledger/`, `Batch/`, `Models/` |
| `proofs/payments` | Lake project: extracted payment kernel, `Payments/`, `Policies/` |
| `tests/` | integration, concurrency and fault-injection suites |
| `scripts/dev-db.sh` | a local PostgreSQL 17 in docker |

## Dependencies

`h5i-app = "=0.1.4"` and its sub-crates resolve from crates.io, pinned
exactly. `h5i-app-gpu` is not published, so it is a git dependency on
<https://github.com/h5i-dev/h5i> at tag `v0.5.2`.

## Build, test, prove

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings

# PostgreSQL-backed tests (skipped when the variable is unset)
export H5I_APP_TEST_DATABASE_URL=$(scripts/dev-db.sh)
cargo test --workspace

# Proofs: Charon, Aeneas and Lean at the versions h5i v0.5.2 pins
h5i app doctor
h5i app extract crates/payment-ledger crates/payment-domain   # rewrite proofs/*/generated/
h5i app extract --check crates/payment-ledger crates/payment-domain
h5i app check crates/payment-ledger crates/payment-domain     # lake build + axiom gate
h5i app mutate crates/payment-ledger crates/payment-domain    # every listed bug must break a proof
(cd proofs/ledger && lake build) && (cd proofs/payments && lake build)
```

The two Lake projects have the same `require`s, so h5i's shared package
cache (`~/.cache/h5i/lake`) holds one copy of Mathlib for both. On a small
disk, set `H5I_LAKE_CACHE=off` and symlink `proofs/payments/.lake/packages`
to `../../ledger/.lake/packages` instead.

## Run

```sh
export DATABASE_URL=$(scripts/dev-db.sh) H5I_APP_SECRET=dev-secret
OP=$(H5I_APP_ISSUE=1:0 cargo run -q -p payment-api --bin h5i-pay)     # tenant 1, operator
ALICE=$(H5I_APP_ISSUE=1:1 cargo run -q -p payment-api --bin h5i-pay)  # tenant 1, user 1
cargo run -p payment-api --bin h5i-pay &

rpc() { curl -s -H "Authorization: Bearer $1" -H 'content-type: application/json' -d "$2" localhost:8080/v1/ledger; echo; }
rpc $OP    '{"cmd":"open","currency":392}'                                     # {"id":0}
rpc $ALICE '{"cmd":"open","currency":392,"no_overdraft":true}'                 # {"id":1}
rpc $OP    '{"cmd":"transfer","id":1,"debit":0,"credit":1,"amount":1000}'      # posted
rpc $OP    '{"cmd":"transfer","id":1,"debit":0,"credit":1,"amount":1000}'      # exists (replay)
curl -s -H "Authorization: Bearer $OP" localhost:8080/v1/ledger/audit

# Payments (Mock PSP by default; H5I_PAY_PSP=stripe with STRIPE_SECRET_KEY=sk_test_… for Stripe test mode)
MERCHANT=$(H5I_APP_ISSUE=1:7 cargo run -q -p payment-api --bin h5i-pay)
pay() { curl -s -H "Authorization: Bearer $1" -H 'content-type: application/json' -d "$3" localhost:8080$2; echo; }
pay $MERCHANT /v1/payments '{"amount":1200,"currency":"jpy","capture":"manual","payment_method":"tok_visa"}'
pay $MERCHANT /v1/payments/0/capture '{"amount":1000}'
pay $MERCHANT /v1/payments/0/refunds '{"amount":300}'
pay $OP /v1/reconcile '{}'
```
