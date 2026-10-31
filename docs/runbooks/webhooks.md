# Webhooks

Endpoint: `POST $API/v1/webhooks/<psp>` (`stripe` or `mock`), authenticated
by the `Stripe-Signature` header (HMAC-SHA256, 300 s tolerance).

| Symptom | Meaning | Action |
|---|---|---|
| Many 400 `invalid webhook` | wrong secret, clock skew > 300 s, or forged traffic | compare `STRIPE_WEBHOOK_SECRET` with the endpoint's signing secret; check NTP on tasks; if forged, WAF block |
| `duplicate` replies | the PSP retried; the kernel already processed the event | none; expected |
| `stale` replies | the event arrived after a later state (out of order) | none; recorded without effect |
| `unknown_payment` | event for a payment we do not have | reconciliation will show it as `extra_external` |
| Events not arriving | endpoint disabled at the PSP, ALB/WAF blocking | re-enable; then `POST /v1/recover` covers the gap |

Webhooks are an optimisation of recovery, not the only path: synchronous
PSP responses and `/v1/recover` reach the same states.
