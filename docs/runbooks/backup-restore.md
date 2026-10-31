# Backup, restore, failover

What exists (Terraform): RDS automated backups (14 days), AWS Backup daily
recovery points (35 days) in a KMS-encrypted vault, a weekly AWS Backup
restore test into a throwaway instance, Multi-AZ for automatic failover.

## Restore drill (monthly; also after every schema migration)

1. Restore the latest recovery point to a new instance in the data subnets
   (AWS Backup console, or `aws backup start-restore-job`), or use the
   instance the weekly restore test created.
2. Run an API task against it with `DATABASE_URL` pointing at the restored
   instance and `H5I_PAY_PSP=mock` (never a real PSP against restored data).
3. For every tenant: `GET /v1/ledger/audit` must return `ok: true`, and
   `POST /v1/reconcile` must show no `ledger` exceptions. The audit checks
   the same invariants the proofs establish, on the restored rows.
4. Record the recovery point time, restore duration (RTO) and data age
   (RPO) in the drill log; delete the instance.

## Failover drill (quarterly)

`aws rds reboot-db-instance --db-instance-identifier <id> --force-failover`
during low traffic. Expected: requests in flight fail with 503 and are
retried by clients with their idempotency keys; the engine reconnects; the
audit stays `ok`. The fault-injection tests exercise the same paths
(killed connections, unknown commit).

## Real restore after data loss

1. Freeze writes (API desired count 0).
2. Restore to a point in time before the incident.
3. Audit as in the drill.
4. Replay what happened after the restore point from the PSP: run
   `POST /v1/recover` and reconciliation for every tenant; PSP objects carry
   our ids in metadata, so captures and refunds after the restore point
   show up as `extra_external` and are re-recorded through the API.
5. Unfreeze.
