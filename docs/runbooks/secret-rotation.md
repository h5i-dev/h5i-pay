# Secret rotation

| Secret | Effect of rotation | Procedure |
|---|---|---|
| `h5i-app-secret` (bearer-token HMAC) | every issued token becomes invalid | put a new value in Secrets Manager, force a new ECS deployment, reissue tokens; on suspected exposure do this immediately (whoever holds it can mint any principal) |
| `stripe-secret-key` (test) | PSP calls fail until tasks restart | roll the key in the Stripe Dashboard, put the new `sk_test_` value, force a new deployment |
| `stripe-webhook-secret` | deliveries fail signature checks until updated | Stripe lets an endpoint have two signing secrets during a roll: add the new one at Stripe, update the secret, deploy, then expire the old one (the verifier accepts any matching `v1` entry) |
| RDS master credentials | managed by RDS (`manage_master_user_password`) | rotate in RDS; tasks read `DB_CREDENTIALS` at start, so force a new deployment |
| `mock-webhook-secret` | test only | regenerate with Terraform (`-replace=random_password.mock_webhook`) |

After any rotation: `GET /healthz`, one test payment on a test tenant,
`GET /v1/ledger/audit`.
