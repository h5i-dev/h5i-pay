# Failure modes and recovery (M5)

What can fail, what keeps money correct when it does, and which test shows
it. "Proof" means a Lean theorem on the extracted kernel
(`docs/proof-assumptions.md`); "test" names a test under `tests/` or a
crate's unit tests.

| Failure | What happens | What keeps it correct | Evidence |
|---|---|---|---|
| Connection to PostgreSQL dies mid-transaction | The attempt rolls back; the engine retries from `BEGIN` on a new connection | SERIALIZABLE transaction per request; a kernel's write set commits whole or not at all | test `fault_injection::killed_mid_transaction_is_retried_once` |
| Connection dies after the writes, before `COMMIT` (outcome unknown) | Without an idempotency key the client gets `CommitUnknown` (HTTP 503); with one the engine retries itself | Ledger transfer ids are idempotent: a retried transfer is `Exists`, never a second posting. Idempotency keys replay the stored reply | tests `unknown_commit_then_client_retry`, `unknown_commit_with_idempotency_key_is_retried_once`; proof `replay_is_noop`, model `Models.keyed_replay` |
| PostgreSQL unreachable or retries exhausted | 503; nothing committed | Infrastructure errors are distinct from kernel refusals; no reply is invented | tests `persistent_failure_leaves_the_ledger_untouched`, `unreachable_database` |
| Two requests race on the same accounts | One serializes after the other (or retries) | SERIALIZABLE isolation; the kernel decides on the snapshot it read | tests `concurrent_transfers_*` (1,119 serialization retries in one run), `concurrent_duplicates_commit_once`, `concurrent_batches_are_atomic`; proofs `conservation`, `no_overdraft` over every interleaving the engine serializes |
| PSP times out after charging (response lost) | Payment stays `PROCESSING`, HTTP 202 | PSP idempotency key `h5i-pay-<tenant>-payment-<id>-authorize`; recovery (scheduled every 5 minutes for every tenant, or `POST /v1/recover` for one) asks the PSP (by reference or by metadata) and records the answer | tests `payments_api::timeouts_are_recovered`, `recovery_job::scheduled_recovery_covers_every_tenant` |
| PSP unavailable before it saw the request | Payment stays `PROCESSING` | Recovery finds nothing at the PSP; an operator may fail it (`fail_unsubmitted=true`) since no money moved | same test, last part |
| Refund response lost | Refund stays `PENDING`, reserved in the payment kernel and the ledger | Reservation means the in-flight amount cannot be refunded twice; recovery retrieves the refund by metadata | test `timeouts_are_recovered`; proof `refund_safety` |
| Merchant cannot cover a refund | 409 before any PSP call; refund recorded `FAILED` | Ledger reservation (`no_overdraft` merchant account) happens before the PSP is asked | test `refund_needs_merchant_funds_in_the_ledger`; proof `no_overdraft` |
| Webhook delivered twice / concurrently | One effect | Kernel deduplicates on the event id (`Command::Event`) | test `pending_payment_completes_by_webhook_once` (10 concurrent: 1 processed, 9 duplicates); proof `event_dedup` |
| Webhooks out of order | Later state wins; the stale event is recorded without effect | Status transitions only along `Allowed`; captured is final | test `webhooks_out_of_order`; proofs `status_steps_allowed`, `captured_final` |
| Forged or replayed webhook | 400 | HMAC-SHA256 over `t.body`, constant-time compare, 300 s tolerance | tests in `payment-connectors::webhook`, `pending_payment_completes_by_webhook_once` |
| API process killed (SIGKILL) while waiting for the PSP | The request dies with the process; the payment stays `PROCESSING` (or the refund `PENDING`, reserved), nothing half applied | Each kernel step commits alone; the PSP call carries a deterministic idempotency key; a client retry with the same `Idempotency-Key`, or the scheduled `h5i-pay recover`, finishes it. A payment the PSP never saw stays `PROCESSING` until an operator fails it | `payment-api/tests/process_kill.rs`: the real production binary against PostgreSQL and a fake Stripe that holds the request open, killed after the charge (retry: same payment, one charge, posted once), during a refund (scheduled job settles it; the reserved amount cannot be refunded again meanwhile), before the PSP acted (job leaves it; operator fails it; no money moved); ledger audit and both reconciliations clean afterwards |
| Crash between recording a capture and posting the ledger | Payment `CAPTURED`, ledger behind | Postings are derived from the payment state with deterministic ids; `recover` and every later sync re-post; `POST /v1/reconcile` compares payments with the journal | test `automatic_capture_posts_the_ledger` (`ledger.exceptions = 0`), `reconciliation_finds_discrepancies` |
| PSP settlement disagrees with us | Exceptions in the reconciliation report | A report only; corrections are separate, audited ledger transfers | test `reconciliation_finds_discrepancies` |
| GPU absent, out of memory, or wrong | `Auto` uses the CPU when no GPU exists; a GPU error after selection is an error, never a silent CPU retry; output with the wrong shape is rejected | h5i-app-gpu `ExecutionPolicy`; outputs are reports and candidate decisions, never commits | tests `corrupted_output_is_rejected`, `gpu_*` in `tests/differential` |
| Bad deployment | Circuit breaker and alarms roll the ECS service back | `deployment_circuit_breaker`, `alarms { rollback = true }` on 5xx and ledger-invariant alarms | `infra/terraform/compute.tf`; `docs/runbooks/deploy.md` |
| Database lost | Restore from AWS Backup / RDS snapshot, then audit | Weekly automated restore tests; `GET /v1/ledger/audit` re-checks the proven invariants on restored rows | `docs/runbooks/backup-restore.md` |

## Not covered yet

- No chaos test on a real Multi-AZ failover; the runbook describes the
  manual drill.
- Scheduled recovery (`h5i-pay recover` every 5 minutes,
  `infra/terraform/recovery.tf`) is tested in-process
  (`recovery_job::scheduled_recovery_covers_every_tenant`) and the binary
  was run against a local database; the EventBridge schedule itself has
  not run on AWS.
