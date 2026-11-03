# h5i-pay

A payment platform: payments with authorization, capture and refunds, a
double-entry ledger, reconciliation against the PSP, and recovery from
crashes, on [h5i-app](https://github.com/h5i-dev/h5i). The decisions that
move money (the payment state machine, the ledger, the reconciliation
rules) are pure Rust kernels that are extracted to Lean 4 and proven. HTTP,
PostgreSQL, the PSP connectors (a Mock PSP and Stripe in test mode) and the
GPU batch worker are ordinary Rust around them. It runs with synthetic
funds only and never handles card numbers.

## Run

```bash
export DATABASE_URL=$(scripts/dev-db.sh)   # PostgreSQL 17 in Docker on 127.0.0.1:55432
export H5I_APP_SECRET=dev-user-secret H5I_PAY_OPERATOR_SECRET=dev-operator-secret
cargo build -p payment-api --features mock-psp
target/debug/h5i-pay &                      # http://127.0.0.1:8080, Mock PSP
```

The `mock-psp` feature builds in the Mock PSP; without it (the production
image) the server uses Stripe and refuses the mock. The same binary prints
tokens: `H5I_APP_ISSUE=<tenant>:<user>` for a merchant,
`H5I_PAY_ISSUE_OPERATOR=<tenant>:<operator>` for an operator. Set
`DB_CA_FILE` to a CA bundle to connect to PostgreSQL over verified TLS.

## Deploy

`infra/` has the Terraform for AWS: the API on ECS Fargate behind an ALB
with WAF, RDS PostgreSQL (Multi-AZ, encrypted), AWS Batch with a GPU queue
for the reconciliation worker, S3, SQS, Secrets Manager, CloudWatch alarms,
AWS Backup, and the scheduled recovery job. It has been
validated (`terraform validate`) but never applied. See
[infra/README.md](infra/README.md).

## Use

A merchant creates a payment, captures part of it and refunds some:

```bash
MERCHANT=$(H5I_APP_ISSUE=1:7 target/debug/h5i-pay)   # tenant 1, user 7
api() { curl -s -H "Authorization: Bearer $MERCHANT" -H 'content-type: application/json' -d "$2" localhost:8080$1; echo; }
api /v1/payments '{"amount":1200,"currency":"jpy","capture":"manual","payment_method":"tok_visa"}'
api /v1/payments/0/capture '{"amount":1000}'
api /v1/payments/0/refunds '{"amount":300}'
```

The ledger, reconciliation, history, recovery and scoped tokens are in
[docs/api.md](docs/api.md).

## How it decides

A payment is a sequence of kernel commands: `Create`, `BeginAuthorize`
before the PSP is asked, `Record` with the PSP's answer (authorized,
captured, declined, canceled), `AttachRef` for the PSP's reference,
`RequestRefund` and the refund's result, `Event` for webhooks (deduplicated
by event id), and `MarkPosted` once the ledger has the payment's postings.
The ledger posts a capture from the PSP clearing account to the merchant's
payable account, and a refund as a reservation into a holding account that
is then settled or released. Each command runs in its own SERIALIZABLE
PostgreSQL transaction and reads only the rows its read plan names, so
concurrent requests cannot spend the same money twice. A write sent with
an `Idempotency-Key` header acts once; a retry gets the first reply. A
crash between steps leaves work that recovery finds and finishes. See
[docs/design-v0.1.md](docs/design-v0.1.md) (in Japanese) and
[docs/failure-recovery.md](docs/failure-recovery.md).

## Verification

```bash
export H5I_APP_TEST_DATABASE_URL=$(scripts/dev-db.sh)
cargo test --workspace
h5i app check crates/payment-ledger crates/payment-domain crates/payment-reconcile
h5i app mutate crates/payment-ledger crates/payment-domain crates/payment-reconcile
h5i app lint crates/payment-ledger crates/payment-domain crates/payment-reconcile
```

The PostgreSQL tests run when `H5I_APP_TEST_DATABASE_URL` is set; the
`h5i app` commands need Charon, Aeneas and Lean 4.31.0 (`h5i app doctor`).

Lean proves, about the extracted kernels, that the ledger conserves money,
keeps its journal balanced and never overdraws a `no_overdraft` account;
that payments only take allowed steps, never refund more than was
captured and apply a webhook event once; that every change writes one
history entry naming who made it; and that each command decides the same
on the rows its plan reads as on the whole tenant. The mutants are bugs
each of which must break a proof. Trusted: Charon, Aeneas and Lean,
PostgreSQL's SERIALIZABLE isolation, the HTTP layer and authentication,
the PSP, and the clock. See
[docs/proof-assumptions.md](docs/proof-assumptions.md); the full list of
checks is in [docs/development.md](docs/development.md).

## GPU batch

Settlement files are reconciled and batches of requests validated by the
`h5i-pay-batch` worker, on the CPU or, with the `cuda` feature, on an
NVIDIA GPU. The GPU code is Rust (the same rules the CPU runs, compiled to
PTX) and only computes reports; it never commits financial state. The GPU
is the faster engine from about 100,000 rows per side in a long-lived
worker; a one-shot job pays 136 ms for the CUDA context, so the worker
uses the GPU only from 2 million rows. See
[docs/benchmarks.md](docs/benchmarks.md) and
[docs/gpu-toolchain.md](docs/gpu-toolchain.md).

## Performance

End to end through the HTTP API with the Mock PSP, one tenant, PostgreSQL
17 in Docker on the same AWS g5.xlarge (4 vCPU), 2026-11-03:

| | 10k payments | 100k payments |
|---|---:|---:|
| Payments created and captured, 32 clients | 183 /s | 186 /s |
| `POST /v1/payments`, p50 | 18.7 ms | 16.7 ms |
| `GET /v1/payments/{id}`, p50 | 0.77 ms | 0.78 ms |
| Reconcile the whole tenant | 73 ms | 535 ms |
| Paged ledger audit | 159 ms | 1.48 s |

No run had a serialization retry. Details, earlier versions and the
batch engines: [docs/benchmarks.md](docs/benchmarks.md).

## Code

- `crates/payment-ledger/`, `crates/payment-domain/`, `crates/payment-reconcile/`:
  the kernels (ledger, payments, reconciliation rules); `proofs/`: their
  generated Lean and theorems.
- `crates/payment-api/`: the `h5i-pay` server, the orchestrator and the
  scheduled jobs.
- `crates/payment-storage/`: PostgreSQL adapters, audits, recovery queries.
- `crates/payment-connectors/`: the Mock PSP, Stripe, webhook signatures.
- `crates/payment-batch/`, `crates/payment-gpu/`: the batch engines and
  the GPU worker.
- `tests/`, `benchmarks/`: test suites and benchmarks; `infra/`:
  Terraform and Dockerfiles.
- `docs/`: the [design](docs/design-v0.1.md), the [API](docs/api.md),
  [development](docs/development.md), [proofs](docs/proof-assumptions.md),
  [benchmarks](docs/benchmarks.md), [failure recovery](docs/failure-recovery.md),
  the [security review](docs/security-review.md),
  [compliance](docs/compliance.md) and [runbooks](docs/runbooks/README.md).

Progress and history: [docs/PROGRESS.md](docs/PROGRESS.md).

## License

Apache-2.0.
