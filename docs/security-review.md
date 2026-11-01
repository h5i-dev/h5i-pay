# Security review (M5)

Scope: the code in this repository at the M5 commit, and the AWS layout in
`infra/`. This is the project's own review, written to hand to an
independent reviewer; it is not that review.

## Assets and trust boundaries

| Asset | Where | Protection |
|---|---|---|
| Ledger and payment state | PostgreSQL, one schema per kernel, every row keyed by tenant | all writes through verified kernels; SERIALIZABLE; tenant scoping in h5i-app-pg's SQL |
| User-token HMAC key (`h5i-app-secret`) | Secrets Manager → ECS task env | possession mints any user (id ≥ 1) of any tenant; not an operator (SEC-02) |
| Operator-token HMAC key (`h5i-pay-operator-secret`) | Secrets Manager → ECS task env | possession mints any tenant's operator under any non-system operator id (recorded on what it writes); tokens live ≤ 12 h (SEC-02) |
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
| SEC-02 | Medium | **Identity is coarse.** A token is (tenant, user); user 0 is the tenant operator, who may debit any account and record PSP results. Until 2026-11: whoever held `H5I_APP_SECRET` could mint any principal, operators included; the kernels saw every operator as `(tenant, 0)`. | **Closed for kernel identity and audit (F1); open items listed.** Tokens: user and operator tokens are signed with different keys (`H5I_APP_SECRET`, `H5I_PAY_OPERATOR_SECRET`; startup refuses equal keys, and keys under 32 bytes when deployed); a user-key token for user 0 is refused; operator tokens carry the operator's own id (1 to 2^62 − 1), live ≤ 12 h (+60 s skew) whoever signed them, and each key has a `…_PREVIOUS` value for rotation (`docs/runbooks/secret-rotation.md`). Kernels: the principal is now `{org, actor: User(id) \| Operator(id)}` in both kernels, so a user can never be taken for an operator whatever its id, and operators are told apart. Every row a command writes records the acting operator's id (`operator` on ledger `transfers`, and on `payments`, `refunds`, `psp_events`, `psp_refs`; 0 when a user acted; 0 also on rows written before the column existed). In-process actors are named system operators with ids from 2^62 up that no token can carry: the orchestrator (2^62+1), the scheduled recovery job (2^62+2), the export (2^62+3); `POST /v1/recover` acts as the calling operator. Proven (`h5i app check`): `only_owner_or_operator_debits` and `replay_needs_debit_right` restated for the new principal (the debiting actor is `Operator _` or `User owner`), the payment kernel's `operator_only` (`Record`, `Event`, `AttachRef` succeed only for `Operator _`), and that every row written records the caller's operator id (`transfers_record_operator`, `writes_record_operator`); mutants `user_acts_as_operator` and `operator_id_dropped` are caught in both kernels. Evidence over HTTP: `ledger_api::the_journal_names_the_operator`, operator ids asserted in `payments_api` (orchestrator, recovery by operator 1). Still open: the `operator` column says who last wrote a payment or refund row, not its history (the journal and the API log `operator request` keep history); operators share one idempotency-key namespace per tenant (`u0`, kept for compatibility); scoped tokens (read-only, per-command), an identity provider instead of shared HMAC keys, and revoking one operator without rotating the key. |
| SEC-03 | Low | **Transfer-id replay reveals existence.** `Transfer` with an id that existed answered `Exists` (same payload) or `IdConflict` to any caller of the tenant, before checking the debit authorization. | **Closed.** The kernel checks `SameAccount`, the debit account and `may_debit` before it looks the id up, so a caller who may not debit the account gets `Forbidden` (or `NotFound`) whether or not the id was used. Proven: `Theorems.replay_needs_debit_right` (an `Exists` reply implies the caller may debit) and `replay_is_noop`, now for callers with that right; mutant `replay_answers_anyone` is caught; kernel test `replay_does_not_reveal_ids_to_others`. Residual: a caller who may debit the account still learns that an id it reuses was taken (`IdConflict`); ids are per tenant and that caller is authorized on the debit side. |
| SEC-04 | Low | **Webhook replay window and id hashing.** A captured, validly signed delivery replayed within 300 s is accepted, then deduplicated by event id (no second effect). Event ids are hashed to 64 bits; a collision would drop an event as a duplicate (≈ n²/2⁶⁵). | Accepted; reconciliation catches a dropped event. |
| SEC-05 | Medium | **Mock PSP in a deployed environment.** `H5I_PAY_PSP=mock` accepts any token and settles instantly; webhooks to `/v1/webhooks/mock` are verified with the mock secret. | **Closed.** The Mock PSP is compiled only with `payment-connectors/mock` (feature `mock-psp` of `payment-api`), off by default. The production API image is built without it: `H5I_PAY_PSP=mock` stops the binary at startup, the default PSP is `stripe`, and the binary contains no Mock PSP code (checked with `strings`). CI builds that binary alone and fails unless it refuses `mock`. Synthetic-funds test environments run a separate `<sha>-mock` image (`API_FEATURES=mock-psp`); Terraform selects it only for `psp_mode = "mock"`, injects the mock webhook secret only then, and rejects `psp_mode = "mock"` for any `prod*` environment. The deploy job keeps the variant the task definition already uses. |
| SEC-06 | Low | **Whole-tenant snapshot per request.** Every command loaded the tenant's whole state; a very large tenant made every request slow (self-inflicted DoS, and a cost amplifier). | **Closed for every command but ledger batches over 64 transfers.** `LedgerStore::load_for` and `PaymentsStore::load_for` load only the rows of each command's read plan (`reads` in each kernel, extracted; `Frame.frame` proves `transition` decides the same on them), with indexes for the non-key plans (accounts by owner, payments by merchant, refunds by payment). Writes take per-key advisory locks instead of the tenant lock. Merchant listings (`List`, `Accounts`) still return every row of the caller: unpaginated, so a merchant with many payments makes its own listing slow. The scheduled recovery reads only unfinished payments. |
| SEC-07 | Low | **Floating image tag.** The ADOT collector sidecar used `latest`. | **Closed.** Pinned to `v0.51.0@sha256:955e240b…e07d` (the index `latest` pointed at on 2026-11-01, read with `docker buildx imagetools inspect`). Bump tag and digest together. |
| SEC-08 | Info | **Orchestrator is trusted.** It maps PSP answers to kernel commands; a mapping bug records a valid but wrong state. | Reconciliation against the PSP report, and payments against the ledger journal, every day. |
| SEC-09 | Info | **Dependencies from git.** `h5i-app-gpu` and its nvvm backend come from github.com/h5i-dev/h5i at a pinned commit, `cust` and the device crate's `cuda_std` from Rust-CUDA at a pinned revision. | **Closed.** `deny.toml` + CI `cargo-deny-action`: advisories (yanked denied), licenses (allow-list), bans (no wildcards), sources (crates.io and the two pinned git hosts only). `cargo deny check` passes locally (cargo-deny 0.20.2). One advisory ignored with its reason: RUSTSEC-2026-0173 (`proc-macro-error2` unmaintained; a build-time proc macro of hax-lib via libcrux-hmac in h5i-app-http; no fixed version exists). The device crate's own lockfile is not covered (it is not in the workspace). |

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
