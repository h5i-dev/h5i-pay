# Runbooks

| Alarm or situation | Runbook |
|---|---|
| `ledger-invariant-violated`, `ledger-posting-refused` | [ledger-imbalance.md](ledger-imbalance.md) |
| `unmatched-settlement`, reconciliation exceptions | [reconciliation-exceptions.md](reconciliation-exceptions.md) |
| PSP timeouts, outage, payments stuck in `processing` | [psp-incidents.md](psp-incidents.md) |
| Webhooks failing, duplicated or delayed | [webhooks.md](webhooks.md) |
| Restore, failover, restore drills | [backup-restore.md](backup-restore.md) |
| Deploys, canary, schema migrations, rollback | [deploy.md](deploy.md) |
| `jobs-dlq`, `jobs-lag`, GPU capacity | [batch-jobs.md](batch-jobs.md) |
| Secret rotation, suspected key exposure | [secret-rotation.md](secret-rotation.md) |

Conventions: `$API` is the API URL; `$OP` an operator token of the tenant
(`H5I_APP_ISSUE=<tenant>:0 h5i-pay` with the environment's secret, issued
for the incident and short-lived). Every correction to money is an ordinary
ledger transfer with a recorded reason, never an edit of rows.
