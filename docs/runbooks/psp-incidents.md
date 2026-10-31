# PSP timeouts, outages, stuck payments

Symptoms: HTTP 202 with `"psp_pending": true`, payments in `processing`,
refunds `pending`, `psp_error` (502) in logs.

1. Check the PSP's status page; for Stripe also the request log in the
   Dashboard (test mode) for our idempotency keys
   (`h5i-pay-<tenant>-payment-<id>-<op>`).
2. Nothing is lost: every PSP call carries an idempotency key, the payment
   kernel only records answers, and refunds are reserved before the call.
3. When the PSP answers again, run recovery per tenant:
   `curl -XPOST -H "Authorization: Bearer $OP" $API/v1/recover`.
   It retrieves each `processing` payment (by reference, or by metadata if
   the reference was never recorded), each pending refund, and re-posts
   the ledger. It is idempotent; run it again until `payments_unresolved`
   stops falling.
4. Payments the PSP never received stay `processing` (`payments_unresolved`).
   After confirming at the PSP that they do not exist, fail them:
   `POST $API/v1/recover?fail_unsubmitted=true`. Do this only after the PSP
   search index has caught up (Stripe: at least a minute after the outage).
5. Then reconcile (`POST /v1/reconcile`).

Stripe idempotency keys expire after 24 hours; recovery after that relies on
metadata search (`h5i_tenant`, `h5i_payment`, `h5i_refund`), which is why
every PSP object carries them.
