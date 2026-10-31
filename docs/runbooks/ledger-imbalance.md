# Ledger invariant violated / posting refused

**Severity: page.** The audit found stored rows that break what the Lean
proofs guarantee (per-currency debits = credits = journal; no constrained
account past its limit), or the orchestrator could not post a transfer a
payment implies.

## Invariant violated (`ledger-invariant-violated`)

The proofs cover every state the kernel can produce, so a violation means
an assumption broke: rows written outside the kernel (manual SQL, a
migration), a restore of inconsistent data, or an engine/database fault.

1. Stop writes: scale the API to 0 (`aws ecs update-service --desired-count 0 ...`).
   There is no per-tenant switch yet; rotating the HMAC secret revokes
   every tenant's tokens at once (secret-rotation.md, SEC-02).
2. Capture the evidence: `curl -H "Authorization: Bearer $OP" $API/v1/ledger/audit`
   (the `violation` field names the currency or account), a database
   snapshot (`aws rds create-db-snapshot`), and recent deploys and
   migrations.
3. Look for writes outside the engine: PostgreSQL logs
   (`log_min_duration_statement`, connection user other than the app
   role), CloudTrail for `rds:ModifyDBInstance`/restores.
4. Repair only by compensating ledger transfers posted through the API by
   an operator, each with an id in a reserved range and a ticket reference
   in `code`; re-run the audit until `ok: true`. Never edit `accounts` or
   `transfers` rows.
5. Write the incident up against the assumption that failed
   (`docs/proof-assumptions.md`).

## Posting refused (`ledger-posting-refused`)

The payment kernel recorded a state (capture, refund) whose ledger posting
the ledger refused, usually `insufficient_funds` on a merchant payable
account for a refund settled at the PSP without a prior reservation.

1. Find the payment in the API logs (`ledger posting refused`, with
   tenant, payment and refund ids).
2. `POST $API/v1/reconcile` (operator) shows it under `ledger` exceptions.
3. Decide with the merchant: fund the merchant account (operator transfer
   into its payable account), then `POST $API/v1/recover` to re-post.
