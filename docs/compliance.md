# Compliance assessment (M5)

A working assessment to prepare an independent review. It is not legal
advice, and nothing here has been reviewed by counsel or a QSA.

## Operating mode

h5i-pay runs in **test mode with synthetic funds**: the Mock PSP, or Stripe
with test keys (live keys are refused by the code). No real money moves
and no real cardholder data is processed. Moving to real funds needs the
decisions below first.

## PCI DSS scope

- **Card data never enters h5i-pay.** Payments are confirmed with PSP
  tokens or PaymentMethod ids, or collected on the PSP's hosted page
  (Stripe Checkout, `POST /v1/checkout`). The API refuses PAN-like input
  before any PSP call (`check_payment_method`), so a client mistake does not
  bring cardholder data into scope by accident (it is rejected, not logged).
- With hosted payment pages only, a merchant integration is typically
  eligible for the smallest self-assessment (SAQ A); with API-confirmed
  tokens it is typically SAQ A-EP. Which applies depends on how merchants
  integrate and must be confirmed with the acquirer and a QSA.
- AWS being PCI DSS compliant covers AWS's part of the shared
  responsibility only; the application, its configuration and operations
  are this project's responsibility.

## Regulatory questions to settle before real funds (Japan, design context)

- **Who holds the money.** If h5i-pay (rather than the PSP) holds
  merchants' or customers' funds between capture and payout, that may be a
  funds transfer business (資金移動業) or a prepaid-means issue under the
  Payment Services Act (資金決済法); a payment agency model where the PSP
  settles directly to merchants may avoid it. The ledger supports both; the
  operating model decides.
- **Merchant onboarding (KYC/AML)**: not implemented; the Act on Prevention
  of Transfer of Criminal Proceeds may apply depending on the model.
- **Personal information (APPI; GDPR for EU data subjects)**: h5i-pay
  stores ids and amounts, no names or card data; PSP references and webhook
  payloads may contain personal data at the PSP. Retention and deletion
  policies are needed.
- **Instalment sales act (割賦販売法) card security obligations** apply to
  merchants handling card payments; tokenization/hosted pages are the
  intended control.

## Records and audit

| Requirement | Mechanism |
|---|---|
| Immutable transaction history | Ledger journal rows are written once (`PutTransfer` only for new ids; proofs show balances equal the journal) |
| Audit exports | S3 bucket with Object Lock (compliance mode, `audit_retention_days`, default 400 days) |
| Access logs | ALB access logs, CloudWatch Logs (KMS, 90 days), VPC flow logs |
| Change control | Git history, CI gates (tests, proofs, mutants), deploy approvals through the GitHub `test` environment |
| Reconciliation | Daily `POST /v1/reconcile` (payments vs PSP report, payments vs ledger) or the Batch worker; exceptions alarm |

## Readiness for an independent risk review

Ready to hand over: design (`docs/design-v0.1.md`), proofs and what they
assume (`docs/proof-assumptions.md`), failure handling
(`docs/failure-recovery.md`), security review (`docs/security-review.md`),
runbooks (`docs/runbooks/`), benchmarks (`docs/benchmarks.md`),
infrastructure (`infra/`).

Open before real funds: SEC-02 (operator
identity), the operating-model
and licensing decision above, merchant onboarding, a payout flow, and an
actual restore drill and failover drill on AWS.
