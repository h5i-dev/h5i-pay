# Secret rotation

| Secret | Effect of rotation | Procedure |
|---|---|---|
| `h5i-app-secret` (user-token HMAC) | none if done in two steps; tokens signed with a dropped value stop working | see below; on suspected exposure skip the overlap (whoever holds it can mint any user of any tenant) |
| `h5i-pay-operator-secret` (operator-token HMAC) | as above, for operator tokens | see below; on suspected exposure skip the overlap (whoever holds it can act as any tenant's operator) |
| `stripe-secret-key` (test) | PSP calls fail until tasks restart | roll the key in the Stripe Dashboard, put the new `sk_test_` value, force a new deployment |
| `stripe-webhook-secret` | deliveries fail signature checks until updated | Stripe lets an endpoint have two signing secrets during a roll: add the new one at Stripe, update the secret, deploy, then expire the old one (the verifier accepts any matching `v1` entry) |
| RDS master credentials | managed by RDS (`manage_master_user_password`) | rotate in RDS; tasks read `DB_CREDENTIALS` at start, so force a new deployment |
| `mock-webhook-secret` | test only | regenerate with Terraform (`-replace=random_password.mock_webhook`) |
| one token, operator or tenant (G3) | that token (or that actor's, or the tenant's tokens issued before a date) refused within 2 s on every server | `POST /v1/tokens/revoke`, below; no key rotation needed |
| identity-provider signing keys (G3) | none if the old key stays published while its tokens live | rotate at the IdP, below |

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

## Revoking tokens (G3)

One leaked token, one person's tokens, or a whole tenant's can be revoked
without rotating a key. Revocation needs an operator token with the
`tokens:revoke` scope of that tenant (`H5I_PAY_ISSUE_OPERATOR=<tenant>:<id>:3600:tokens:revoke`).
Every request is checked against the list (`ops.revocations`); a
revocation takes effect at once on the server that recorded it and on
every other server within `H5I_PAY_REVOCATION_REFRESH_MS` (2 s by default)
plus one query; a server that cannot read the list for five intervals
(10 s) refuses every token with 503 until it can.

| What | Request (`POST /v1/tokens/revoke`) | Covers |
|---|---|---|
| one token | `{"jti": "<token id>", "expires_at": <exp>, "reason": "…"}` | that v2 or IdP token (the `jti` is in the `operator request` log line of each use) |
| one actor | `{"actor": {"kind": "operator", "id": 5}, "reason": "…"}` (or `"kind": "user"`) | every token of that operator (user) issued until now, or until `"before": <unix seconds>`; v1 tokens of that actor always |
| the tenant | `{"before": <unix seconds>, "reason": "…"}` | every token of the tenant issued then or earlier, the revoking operator's own included (issue a new one afterwards); all v1 tokens of the tenant |

`GET /v1/tokens/revocations` lists them, newest first. Entries are never
deleted; a token entry is dropped from memory once its `expires_at`
passed. Revocation does not reach the identity provider: also disable the
user there (Cognito: `admin-user-global-sign-out`, then disable), or new
tokens it issues are accepted again.

## Identity provider (G3)

With `H5I_PAY_OIDC_ISSUER` and `H5I_PAY_OIDC_AUDIENCE` set (Terraform:
`oidc_issuer_url`/`oidc_audience`, or `create_cognito_user_pool`), the API
also accepts the issuer's RS256/ES256 access tokens. Its signing keys are
the issuer's: rotate them there. The API re-reads the JWKS when it is older
than ten minutes, or when a token names a key id it does not know (at most
every 30 s), so a new key works within 30 s of being published; keep the
old key published until the tokens signed with it have expired. If the
issuer's keys may be compromised, remove `H5I_PAY_OIDC_ISSUER` and deploy
(HMAC tokens keep working), or revoke by date for the affected tenants.
