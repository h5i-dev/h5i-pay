# Deploys, canary, schema migrations

## Normal deploy

CI (`.github/workflows/ci.yml`) builds images tagged with the commit SHA
after tests, proofs and Terraform checks pass on `main`, then the
`deploy-test` job (approval through the GitHub `test` environment)
registers a task definition revision and updates the ECS service.

ECS replaces tasks without dropping below the desired count
(`minimum_healthy_percent = 100`). The deployment rolls back automatically
when new tasks fail health checks (circuit breaker) or when the 5xx or
ledger-invariant alarms fire (`alarms { rollback = true }`).

## Canary

For changes to the kernels, the orchestrator or the PSP mapping:

1. Run the new revision as a second ECS service, `api-canary`, with one
   task registered in the same target group (the ALB then sends it about
   1/(n+1) of the traffic). ECS rolling updates cannot pause halfway, so
   the canary is a separate service. Not in the Terraform yet; create it
   from the `api` service's settings with `aws ecs create-service`.
2. Watch for 30 minutes: 5xx rate, `ledger-posting-refused`, reconciliation
   exceptions on test tenants, PSP error rate.
3. Promote (normal deploy of the same revision, then delete `api-canary`)
   or roll back (delete `api-canary`).

## Schema migrations

The kernels' tables are created by `install_schema` (idempotent
`CREATE TABLE IF NOT EXISTS`). A change to a kernel's `schema!` is a
migration:

1. Additive only in one release (new tables or nullable columns); the old
   version must keep working on the new schema, so rollback is a task
   definition change, not a database change.
2. Take a manual snapshot before the deploy.
3. Re-extract and re-prove (`h5i app extract`, `h5i app check`): the
   storage theorems (`Storage.db_inv`) are about the new tables.
4. Run the restore drill on the snapshot with the new version before
   production.
5. Destructive changes (drop, rename) ship one release later, after the
   old version is gone.
