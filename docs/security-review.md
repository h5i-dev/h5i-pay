# Security review (M5)

Scope: the code in this repository at the M5 commit, and the AWS layout in
`infra/`. This is the project's own review, written to hand to an
independent reviewer; it is not that review.

## Assets and trust boundaries

| Asset | Where | Protection |
|---|---|---|
| Ledger and payment state | PostgreSQL, one schema per kernel, every row keyed by tenant | all writes through verified kernels; SERIALIZABLE; tenant scoping in h5i-app-pg's SQL |
| Bearer-token HMAC secret | Secrets Manager → ECS task env | possession mints any principal of any tenant (see SEC-02) |
| PSP keys | Secrets Manager; Stripe **test** keys only | `StripePsp::new` refuses keys not starting `sk_test_` / `rk_test_` |
| Webhook secrets | Secrets Manager | HMAC-SHA256 verification, 300 s tolerance |
| Card data | never present | tokens or hosted Checkout only; `check_payment_method` refuses PAN-like input before any PSP call |
| Audit exports | S3 with Object Lock (compliance mode) | KMS, TLS-only bucket policy |

Trust boundaries: the internet → ALB/WAF → API (HMAC token) → kernels
(verified) → engine and PostgreSQL (trusted, modelled) ; API → PSP (TLS,
idempotency keys) ; PSP → webhook endpoint (signature) ; Batch workers →
S3 only.

## Findings

| ID | Severity | Finding | Status |
|---|---|---|---|
| SEC-01 | Medium | **No TLS between the API and PostgreSQL** (until 2026-10-31: h5i-app-pg 0.1.4 used `NoTls`, so `rds.force_ssl` was 0). | **Closed.** h5i-app-pg's `tls` feature (h5i #770, released in h5i-app 0.5.2): `payment-storage` uses `pool_tls` with `Roots::PemFile($DB_CA_FILE)`, i.e. rustls with the chain checked against the Amazon RDS global CA bundle (vendored at `infra/docker/rds-global-bundle.pem`, shipped in the API image at `/etc/h5i-pay/rds-global-bundle.pem`) and the host name against the certificate (libpq `verify-full`); `sslmode` is forced to `require` with no plaintext fallback, and `sslmode=disable` is an error. The API refuses to start with `DB_CREDENTIALS` and no `DB_CA_FILE`. RDS has `rds.force_ssl = 1`. Evidence: `tests/tls` against TLS-only servers (default and TLS 1.2): payments, refunds, reconciliation and audit over TLS with every server-side connection TLS per `pg_stat_ssl`; plain TCP refused by the server; a certificate from another CA refused (`UnknownIssuer`); IP instead of the certified name refused (`NotValidForName`); bad CA files refused at startup. The API image was also run against the TLS-only server. Residual: SCRAM channel binding is offered (RSA/ECDSA certificates) but not required; add `channel_binding=require` once verified against RDS's certificate type. |
| SEC-02 | Medium | **Identity is coarse.** A token is (tenant, user); user 0 is the tenant operator, who may debit any account and record PSP results. Whoever holds `H5I_APP_SECRET` can mint any principal. | Accepted for test mode. Before real funds: separate operator credentials (service identity, not a user id), scoped tokens, rotation runbook (`docs/runbooks/secret-rotation.md`). |
| SEC-03 | Low | **Transfer-id replay reveals existence.** `Transfer` with an id that exists and the same payload answers `Exists` to any caller of the tenant, without checking the debit authorization. | Documented in `docs/proof-assumptions.md`. Option: check `may_debit` on the replay path too (and re-prove `replay_is_noop` with that hypothesis). |
| SEC-04 | Low | **Webhook replay window and id hashing.** A captured, validly signed delivery replayed within 300 s is accepted, then deduplicated by event id (no second effect). Event ids are hashed to 64 bits; a collision would drop an event as a duplicate (≈ n²/2⁶⁵). | Accepted; reconciliation catches a dropped event. |
| SEC-05 | Medium | **Mock PSP in a deployed environment.** `H5I_PAY_PSP=mock` accepts any token and settles instantly; webhooks to `/v1/webhooks/mock` are verified with the mock secret. | **Closed.** The Mock PSP is compiled only with `payment-connectors/mock` (feature `mock-psp` of `payment-api`), off by default. The production API image is built without it: `H5I_PAY_PSP=mock` stops the binary at startup, the default PSP is `stripe`, and the binary contains no Mock PSP code (checked with `strings`). CI builds that binary alone and fails unless it refuses `mock`. Synthetic-funds test environments run a separate `<sha>-mock` image (`API_FEATURES=mock-psp`); Terraform selects it only for `psp_mode = "mock"`, injects the mock webhook secret only then, and rejects `psp_mode = "mock"` for any `prod*` environment. The deploy job keeps the variant the task definition already uses. |
| SEC-06 | Low | **Whole-tenant snapshot per request.** Every command loads the tenant's whole state; a very large tenant makes every request slow (self-inflicted DoS, and a cost amplifier). | WAF rate limit per IP; tenant size alarms are not defined yet. Scaling path: `Store::load_for` with frame theorems. |
| SEC-07 | Low | **Floating image tag.** The ADOT collector sidecar uses `latest`. | Pin by digest before production. |
| SEC-08 | Info | **Orchestrator is trusted.** It maps PSP answers to kernel commands; a mapping bug records a valid but wrong state. | Reconciliation against the PSP report, and payments against the ledger journal, every day. |
| SEC-09 | Info | **Dependencies from git.** `h5i-app-gpu` and its nvvm backend come from github.com/h5i-dev/h5i at a pinned commit (`02ccdde…`, the one h5i-app 0.5.2 was published from; they are not on crates.io), and `cust` from Rust-CUDA at a pinned revision. The other h5i-app crates are crates.io `=0.5.2`. | Cargo.lock pins commits; add `cargo deny` (bans, advisories) to CI. |

## Checks done

- Card numbers: `check_payment_method` refuses 12–19 digit strings (with or
  without separators) and anything Luhn-valid; tested in
  `payment-connectors` and end to end (`declines_cancels_and_card_numbers`
  shows no PSP call is made).
- Live keys: refused at construction (`stripe::tests::live_keys_are_refused`).
- Tenant isolation: proven for the engine model and the SQL model; tested
  over HTTP (`tenants_are_isolated`): another tenant's operator sees no
  accounts and cannot name another tenant's account ids.
- Authorization: proven `only_owner_or_operator_debits`; tested
  (`ledger_over_http`, merchant cannot read or refund another merchant's
  payment in `manual_capture_partial_refunds_and_limits`).
- Reserved ids and codes: clients cannot use system transfer ids (top bit)
  or system account codes (≥ 1000) (`ledger_over_http`).
- Webhook signatures: wrong secret, tampered body, stale timestamp and
  malformed headers are refused (`webhook::tests::signatures`, mock tests).
- Infrastructure: no public subnets for compute or data; RDS not publicly
  accessible; S3 public access blocked and TLS-only; KMS everywhere; IMDSv2
  on Batch instances; no long-lived CI keys (OIDC, environment-scoped).

## Not reviewed

- The h5i-app framework itself (engine, HTTP, token parsing) beyond reading
  the parts h5i-pay uses; its own trust document is
  `docs/design/design-app-trust.md` in the h5i repository.
- Side channels, denial-of-service beyond WAF rate limiting, and the
  Stripe account configuration.
