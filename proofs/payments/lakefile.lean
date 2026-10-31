import Lake
open Lake DSL

-- Pinned to the Aeneas commit h5i extracts with (`h5i app doctor`; the same
-- at v0.5.2 and at a19667cbbb5a).
require aeneas from git
  "https://github.com/AeneasVerif/aeneas" @ "b86120db3183b0107eb5f2637b11c424cd06ef1c" / "backends/lean"

-- h5i-app's lemmas and tactics for extracted kernels, at the h5i commit
-- the Rust code takes every h5i-app crate from (Cargo.toml).
require h5i_app_lib from git
  "https://github.com/h5i-dev/h5i" @ "a19667cbbb5a1be14c8d16579eb00e679910dc57" / "crates/h5i-app-core/proofs"

package h5i_pay_payments_proofs

-- Generated Lean: `h5i app extract crates/payment-domain` and its `schema!` Lean. Do not edit.
lean_lib Generated where
  srcDir := "generated"
  roots := #[`PaymentDomain, `PaymentsSchema]

-- Hand-written specifications and proofs.
@[default_target] lean_lib Proofs where
  roots := #[`Payments, `Policies]
