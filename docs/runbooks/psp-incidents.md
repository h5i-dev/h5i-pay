# PSP timeouts, outages, stuck payments

Symptoms: HTTP 202 with `"psp_pending": true`, payments in `processing`,
refunds `pending`, `psp_error` (502) in logs.

1. Check the PSP's status page; for Stripe also the request log in the
   Dashboard (test mode) for our idempotency keys
   (`h5i-pay-<tenant>-payment-<id>-<op>`).
2. Nothing is lost: every PSP call carries an idempotency key, the payment
   kernel only records answers, and refunds are reserved before the call.
3. Recovery runs on its own every 5 minutes for every tenant with
   half-done work (`h5i-pay recover`, EventBridge Scheduler
   `<name>-recover`, `infra/terraform/recovery.tf`); its JSON line
   (`"event":"recovery"`) in the API log group gives `tenants_failed` and
   `payments_unresolved`. Alarms: `<name>-recovery-failed` (a tenant could
   not be processed for 30 minutes), `<name>-recovery-unresolved`
   (payments with no PSP record for an hour), `<name>-recovery-not-running`
   (no pass reported in an hour: check the schedule's role and the task's
   stopped reason in ECS). To recover one tenant now:
   `curl -XPOST -H "Authorization: Bearer $OP" $API/v1/recover`.
   It retrieves each `processing` payment (by reference, or by metadata if
   the reference was never recorded), each pending refund, and re-posts
   the ledger. It is idempotent; run it again until `payments_unresolved`
   stops falling.
4. Payments the PSP never received stay `processing` (`payments_unresolved`).
   The scheduled pass never fails them. After confirming at the PSP that
   they do not exist, fail them:
   `POST $API/v1/recover?fail_unsubmitted=true`. Do this only after the PSP
   search index has caught up (Stripe: at least a minute after the outage).
5. Then reconcile (`POST /v1/reconcile`).

Stripe idempotency keys expire after 24 hours; recovery after that relies on
metadata search (`h5i_tenant`, `h5i_payment`, `h5i_refund`), which is why
every PSP object carries them.
