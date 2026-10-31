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
| `crates/payment-reconcile` | reconciliation rules and CPU engines (reference, single-thread, multithread, AVX2) |
| `crates/payment-batch` | batch API on h5i-app-gpu: policy validation and reconciliation with CPU/GPU/Auto selection |
| `crates/payment-gpu` | CUDA kernels (`kernels/batch.cu`, feature `cuda`) and the `h5i-pay-batch` worker |
| `crates/payment-api` | the `h5i-pay` HTTP server and the payment orchestrator |
| `proofs/ledger` | Lake project: extracted ledger, `Ledger/`, `Batch/`, `Models/` |
| `proofs/payments` | Lake project: extracted payment kernel, `Payments/`, `Policies/` |
| `tests/` | integration, concurrency, fault-injection and CPU/GPU differential suites |
| `benchmarks/` | `bench` binary; results in `benchmarks/results/`, analysis in [docs/benchmarks.md](docs/benchmarks.md) |
| `infra/` | Terraform for AWS and Dockerfiles ([infra/README.md](infra/README.md)); never applied |
| `.github/workflows/ci.yml` | CI: Rust, CUDA build, proofs, Terraform checks, images, test deploy |
| `scripts/dev-db.sh` | a local PostgreSQL 17 in docker |

## Documents

- [docs/design-v0.1.md](docs/design-v0.1.md): design (Japanese)
- [docs/PROGRESS.md](docs/PROGRESS.md): milestone status and open items
- [docs/proof-assumptions.md](docs/proof-assumptions.md): what is proven and what the proofs assume
- [docs/benchmarks.md](docs/benchmarks.md): CPU vs GPU results, including where the GPU loses
- [docs/failure-recovery.md](docs/failure-recovery.md): failure modes and recovery
- [docs/security-review.md](docs/security-review.md): threat model and open findings
- [docs/compliance.md](docs/compliance.md): PCI DSS scope and readiness
- [docs/runbooks/](docs/runbooks/README.md): operational runbooks

h5i-pay runs in test mode with synthetic funds only (Mock PSP or Stripe
test keys). It never handles raw card numbers.

## Dependencies

Every h5i-app crate (`h5i-app`, `-core`, `-pg`, `-http`, `-json`, `-schema`,
`-sql`, `-gpu`, `-gpu-backend-nvvm`) is a git dependency on
<https://github.com/h5i-dev/h5i> at one commit, `a19667cbbb5a1be14c8d16579eb00e679910dc57`
(main after #769, Lean lemmas for `u32` schema columns, and #770, verified
TLS in h5i-app-pg). The crates there are version 0.1.4 and are not on
crates.io. The Lean library `h5i_app_lib` in `proofs/*/lakefile.lean` is
required from the same commit; keep the three pins identical.

## Build, test, prove

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings

# PostgreSQL-backed tests (skipped when the variable is unset)
export H5I_APP_TEST_DATABASE_URL=$(scripts/dev-db.sh)
cargo test --workspace

# TLS-only PostgreSQL for tests/tls (h5i's script; prints the three variables)
eval "$(bash /path/to/h5i/scripts/app/pg-tls.sh /tmp/pg-tls | sed 's/^/export /')"
cargo test -p h5i-pay-tests --test tls
# Proofs: Charon, Aeneas and Lean at the versions h5i pins
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

## GPU batch and benchmarks

Needs the CUDA toolkit (`nvcc`), an NVIDIA driver and libclang. Without the
`cuda` feature everything builds and runs on the CPU only.

```sh
cargo clippy --workspace --all-targets --features payment-gpu/cuda,h5i-pay-tests/gpu,h5i-pay-benchmarks/gpu -- -D warnings
cargo test --release -p h5i-pay-tests --features gpu --test differential   # GPU vs the reference on every row
cargo build --release -p h5i-pay-benchmarks --features gpu
./target/release/bench all --out benchmarks/results/<instance>.json

cargo run --release -p payment-gpu --features cuda --bin h5i-pay-batch -- \
  reconcile internal.csv external.csv report.json --engine auto
```

The GPU computes reports and candidate decisions; it never commits
financial state.

## Run

Set `DB_CA_FILE` to a PEM CA bundle to connect to PostgreSQL over verified
TLS (certificate chain and host name, no plaintext fallback). Deployed, the
API reads the RDS credentials secret and refuses to start without it; the
image ships the Amazon RDS bundle at `/etc/h5i-pay/rds-global-bundle.pem`.
Locally, without `DB_CA_FILE`, it uses plain TCP:

```sh
export DATABASE_URL=$(scripts/dev-db.sh) H5I_APP_SECRET=dev-secret
OP=$(H5I_APP_ISSUE=1:0 cargo run -q -p payment-api --features mock-psp --bin h5i-pay)     # tenant 1, operator
ALICE=$(H5I_APP_ISSUE=1:1 cargo run -q -p payment-api --features mock-psp --bin h5i-pay)  # tenant 1, user 1
cargo run -p payment-api --features mock-psp --bin h5i-pay &   # Mock PSP: dev and test builds only

rpc() { curl -s -H "Authorization: Bearer $1" -H 'content-type: application/json' -d "$2" localhost:8080/v1/ledger; echo; }
rpc $OP    '{"cmd":"open","currency":392}'                                     # {"id":0}
rpc $ALICE '{"cmd":"open","currency":392,"no_overdraft":true}'                 # {"id":1}
rpc $OP    '{"cmd":"transfer","id":1,"debit":0,"credit":1,"amount":1000}'      # posted
rpc $OP    '{"cmd":"transfer","id":1,"debit":0,"credit":1,"amount":1000}'      # exists (replay)
curl -s -H "Authorization: Bearer $OP" localhost:8080/v1/ledger/audit

# Payments. A mock-psp build defaults to the Mock PSP; H5I_PAY_PSP=stripe with STRIPE_SECRET_KEY=sk_test_…
# for Stripe test mode. A build without the feature (the production image) defaults to stripe and refuses mock.
MERCHANT=$(H5I_APP_ISSUE=1:7 cargo run -q -p payment-api --features mock-psp --bin h5i-pay)
pay() { curl -s -H "Authorization: Bearer $1" -H 'content-type: application/json' -d "$3" localhost:8080$2; echo; }
pay $MERCHANT /v1/payments '{"amount":1200,"currency":"jpy","capture":"manual","payment_method":"tok_visa"}'
pay $MERCHANT /v1/payments/0/capture '{"amount":1000}'
pay $MERCHANT /v1/payments/0/refunds '{"amount":300}'
pay $OP /v1/reconcile '{}'
```
