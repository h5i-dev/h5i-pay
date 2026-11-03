# The HTTP API, by example

A local walk-through of the API: the ledger, payments, reconciliation,
history and scoped tokens. Every command below runs against a local server
with the Mock PSP (see the README's "Run").

## The server and its database

Set `DB_CA_FILE` to a PEM CA bundle to connect to PostgreSQL over verified
TLS (certificate chain and host name, no plaintext fallback). Deployed, the
API reads the RDS credentials secret and refuses to start without it; the
image ships the Amazon RDS bundle at `/etc/h5i-pay/rds-global-bundle.pem`.
Locally, without `DB_CA_FILE`, it uses plain TCP. The server listens on
`LISTEN` (default `127.0.0.1:8080`); the other variables are documented at
the top of `crates/payment-api/src/main.rs`.

```sh
export DATABASE_URL=$(scripts/dev-db.sh) H5I_APP_SECRET=dev-user-secret H5I_PAY_OPERATOR_SECRET=dev-operator-secret
cargo build -p payment-api --features mock-psp
OP=$(H5I_PAY_ISSUE_OPERATOR=1:1 target/debug/h5i-pay)   # tenant 1, operator 1 (1 h)
ALICE=$(H5I_APP_ISSUE=1:1 target/debug/h5i-pay)         # tenant 1, user 1
MERCHANT=$(H5I_APP_ISSUE=1:7 target/debug/h5i-pay)      # tenant 1, user 7
target/debug/h5i-pay &                                  # Mock PSP: dev and test builds only
```

A `mock-psp` build defaults to the Mock PSP; `H5I_PAY_PSP=stripe` with
`STRIPE_SECRET_KEY=sk_test_…` uses Stripe test mode. A build without the
feature (the production image) defaults to Stripe and refuses the mock.

## The ledger

```sh
rpc() { curl -s -H "Authorization: Bearer $1" -H 'content-type: application/json' -d "$2" localhost:8080/v1/ledger; echo; }
rpc $OP    '{"cmd":"open","currency":392}'                                     # {"id":0}
rpc $ALICE '{"cmd":"open","currency":392,"no_overdraft":true}'                 # {"id":1}
rpc $OP    '{"cmd":"transfer","id":1,"debit":0,"credit":1,"amount":1000}'      # posted
rpc $OP    '{"cmd":"transfer","id":1,"debit":0,"credit":1,"amount":1000}'      # exists (replay)
rpc $OP    '{"cmd":"transfers","account":1}'                                   # account 1's journal, a page of at most 64
curl -s -H "Authorization: Bearer $OP" localhost:8080/v1/ledger/audit           # conservation, balanced journal, limits
```

## Payments, refunds, reconciliation, history

```sh
pay() { curl -s -H "Authorization: Bearer $1" -H 'content-type: application/json' -d "$3" localhost:8080$2; echo; }
pay $MERCHANT /v1/payments '{"amount":1200,"currency":"jpy","capture":"manual","payment_method":"tok_visa"}'
pay $MERCHANT /v1/payments/0/capture '{"amount":1000}'
pay $MERCHANT /v1/payments/0/refunds '{"amount":300}'
pay $OP /v1/reconcile '{}'                                  # ids 0..4095; the next period: /v1/reconcile?from=4096
pay $OP /v1/payments/rpc '{"cmd":"history","payment":0}'    # who changed it, when, how
pay $OP /v1/recover '{}'                                    # finish what crashes and timeouts left half done
curl -s -H "Authorization: Bearer $OP" localhost:8080/v1/reconcile/reports   # the latest nightly batch report, if any
```

The nightly batch report exists after `h5i-pay export`, the batch worker
and `h5i-pay ingest` have run (`docs/runbooks/`). An `Idempotency-Key`
header on a write makes a retry return the first reply instead of acting
twice; keys are per principal.

## Tokens

The API issues v2 HMAC tokens (`v2.<payload>.<mac>`) with scopes
(`payments:read`, `payments:write`, `refunds:write`, `ledger:read`,
`ledger:write` for users; also `reconcile:read`, `recover`,
`tokens:revoke` for operators; `operator:*` is all), a token id and an
issue time; h5i-app's v1 tokens still work. With `H5I_PAY_OIDC_ISSUER` and
`H5I_PAY_OIDC_AUDIENCE` it also accepts RS256/ES256 access tokens from that
issuer (`crates/payment-api/src/oidc.rs` for the claims). Revocation:
[runbooks/secret-rotation.md](runbooks/secret-rotation.md).

```sh
# A read-only merchant token and a reconciliation-only operator token;
# revoke one token, or everything an operator holds.
RO=$(H5I_APP_ISSUE=1:7:3600:payments:read target/debug/h5i-pay)
pay $RO /v1/payments '{"amount":5,"currency":"jpy","payment_method":"tok_visa"}'   # 403 insufficient_scope
RECON=$(H5I_PAY_ISSUE_OPERATOR=1:2:3600:reconcile:read target/debug/h5i-pay)
pay $RECON /v1/reconcile '{}'                                                      # allowed
pay $OP /v1/tokens/revoke '{"actor":{"kind":"operator","id":2},"reason":"left the team"}'
pay $RECON /v1/reconcile '{}'                                                      # 401 once revoked
```
