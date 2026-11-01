# Secret rotation

| Secret | Effect of rotation | Procedure |
|---|---|---|
| `h5i-app-secret` (user-token HMAC) | none if done in two steps; tokens signed with a dropped value stop working | see below; on suspected exposure skip the overlap (whoever holds it can mint any user of any tenant) |
| `h5i-pay-operator-secret` (operator-token HMAC) | as above, for operator tokens | see below; on suspected exposure skip the overlap (whoever holds it can act as any tenant's operator) |
| `stripe-secret-key` (test) | PSP calls fail until tasks restart | roll the key in the Stripe Dashboard, put the new `sk_test_` value, force a new deployment |
| `stripe-webhook-secret` | deliveries fail signature checks until updated | Stripe lets an endpoint have two signing secrets during a roll: add the new one at Stripe, update the secret, deploy, then expire the old one (the verifier accepts any matching `v1` entry) |
| RDS master credentials | managed by RDS (`manage_master_user_password`) | rotate in RDS; tasks read `DB_CREDENTIALS` at start, so force a new deployment |
| `mock-webhook-secret` | test only | regenerate with Terraform (`-replace=random_password.mock_webhook`) |

## Token keys

Both token secrets are JSON `{"current": "...", "previous": "..."}`. The
API signs with `current` and accepts `previous` too; outside a rotation the
two are equal. Terraform writes only the initial value and ignores later
changes. User and operator keys must all differ (the API refuses to start
otherwise) and be at least 32 bytes.

Planned rotation, no downtime (`$S` is `<name>/h5i-app-secret` or
`<name>/h5i-pay-operator-secret`):

1. `OLD=$(aws secretsmanager get-secret-value --secret-id $S --query SecretString --output text | jq -r .current)`;
   `NEW=$(openssl rand -hex 32)`;
   `aws secretsmanager put-secret-value --secret-id $S --secret-string "$(jq -n --arg c "$NEW" --arg p "$OLD" '{current:$c,previous:$p}')"`.
2. `aws ecs update-service --cluster <name> --service <name>-api --force-new-deployment`;
   wait for the deployment to finish. Old tokens still verify; new ones use `NEW`.
3. Reissue tokens with `NEW`. Wait out the longest token lifetime signed
   with `OLD` (user tokens from `H5I_APP_ISSUE`: 24 h; operator tokens: 12 h).
4. Put `{current: NEW, previous: NEW}` and force a new deployment again.
   Tokens signed with `OLD` now get 401.

Suspected exposure: put `{current: NEW, previous: NEW}` at once and deploy;
every token signed with the old value is refused as soon as the tasks
restart. The scheduled recovery job reads neither key.

After any rotation: `GET /healthz`, one test payment on a test tenant,
`GET /v1/ledger/audit`.
