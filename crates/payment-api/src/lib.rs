//! The h5i-pay HTTP server.
//!
//! Every state change goes through a verified kernel, run by h5i-app's
//! PostgreSQL engine in a SERIALIZABLE transaction with retries:
//!
//! * `POST /v1/ledger` — the double-entry ledger (`payment_ledger`);
//! * `GET /v1/ledger/audit` — re-checks the proven invariants on the
//!   committed rows (operators only), for monitoring.
//!
//! Authentication is h5i-app's HMAC bearer token (`v1.<tenant>.<user>.<exp>.<mac>`);
//! user 0 of a tenant is its operator. `Idempotency-Key` makes any request
//! run at most once.

pub mod routes;

pub use routes::{router, AppState};
