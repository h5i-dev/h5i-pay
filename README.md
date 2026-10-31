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
| `crates/payment-storage` | h5i-app PostgreSQL adapters, JSON codecs, audits |
| `crates/payment-api` | the `h5i-pay` HTTP server |
| `proofs/` | one Lake project: `generated/` (extracted), `Ledger/`, `Batch/`, `Models/` |
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
h5i app extract crates/payment-ledger     # rewrite proofs/generated/
h5i app check crates/payment-ledger       # lake build + axiom gate
h5i app mutate crates/payment-ledger      # every listed bug must break a proof
(cd proofs && lake build)
```

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
```
