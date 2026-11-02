//! The h5i-pay HTTP server.
//!
//! Every state change goes through one of the two verified kernels, run by
//! h5i-app's PostgreSQL engine in a SERIALIZABLE transaction with retries:
//!
//! * `POST /v1/ledger` — the double-entry ledger (`payment_ledger`);
//!   `GET /v1/ledger/audit` re-checks its proven invariants on the rows;
//! * `/v1/payments…`, `/v1/checkout` — the payment orchestrator
//!   ([`orchestrator`]), which records PSP outcomes through the payment
//!   kernel (`payment_domain`) and posts the matching ledger transfers;
//! * `POST /v1/webhooks/{psp}` — signed PSP webhooks, deduplicated by the kernel;
//! * `POST /v1/recover` — finishes payments and refunds left half done (one
//!   tenant); `h5i-pay recover` does it for every tenant, on a schedule
//!   ([`jobs::recover_all`]);
//! * `POST /v1/reconcile` — compares payments with the PSP's settlement
//!   report and with the ledger journal; a report only, it moves no money.
//!
//! Authentication ([`auth`], G3): v2 HMAC tokens with scopes and a token id
//! (h5i-app's v1 tokens still accepted), signed with one key for users and
//! another for operators, or access tokens from a configured OIDC issuer
//! ([`oidc`]); every route checks a scope before a kernel runs, and every
//! token is checked against the revocation list ([`revocation`]).
//! `Idempotency-Key` makes a request run at most once.

pub mod auth;
pub mod batch;
pub mod jobs;
pub mod oidc;
pub mod orchestrator;
pub mod revocation;
pub mod routes;

pub use auth::Keys;
pub use routes::{router, AppState};
