# Reconciliation exceptions

`POST $API/v1/reconcile` (or the Batch worker report in
`s3://<batch-bucket>/reports/<job>.json`) lists rows that do not match.

Since G6 the API reconciles one period of ids per call: `?from=<id>`
(default 0) and `&to=<id>` (default and at most `from + 4096`), ids of our
payments (captures) and refunds. The reply's `period.next` is the next
period's `from` (`null` for the period that is still open: newer payments
can still get ids in it). To go over a tenant, start at `from=0` and follow
`period.next`; to check what is new, start at the last closed period. Each
section lists 100 exceptions from `exceptions_from` (`exceptions_limit` up
to 1000) and gives `exceptions_next` for the next page. A PSP line with an
id nobody has appears as `extra_external` in the period of that id.

| Code | Meaning | Usual cause | Action |
|---|---|---|---|
| `missing_external` | We recorded a capture/refund the PSP report lacks | report cut-off; capture recorded but not settled yet; PSP reversal | wait one settlement cycle; then query the PSP object; if it never captured, cancel/refund through the API |
| `extra_external` | The PSP settled something we do not know | a payment made outside h5i-pay; a lost webhook and a failed recovery | `POST /v1/recover`; if still unknown, investigate at the PSP; record a correcting transfer only after approval |
| `amount_mismatch` | Same object, different amount | partial capture recorded wrongly; fees netted by the PSP | compare PSP object and `GET /v1/payments/{id}`; correct with a ledger transfer and a ticket |
| `currency_mismatch` | Same object, different currency | integration bug | stop the integration; investigate |
| `status_mismatch` | Settled at the PSP, pending here (refunds) | webhook not yet processed | `POST /v1/recover` |
| `duplicate_*` | The key appears twice on one side | duplicated report lines; replayed import | dedupe the input; if internal, investigate (should be impossible: payment ids are unique) |

Rules: the report never moves money. Every correction is a ledger transfer
posted by an operator with an approval reference, exported to the audit
bucket. Re-run reconciliation after corrections; the `ledger` section
(payments vs journal) must be empty.
