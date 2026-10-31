import Lake
open Lake DSL

-- Pinned to the Aeneas commit h5i v0.5.2 extracts with (`h5i app doctor`).
require aeneas from git
  "https://github.com/AeneasVerif/aeneas" @ "b86120db3183b0107eb5f2637b11c424cd06ef1c" / "backends/lean"

-- h5i-app's lemmas and tactics for extracted kernels, at the h5i release
-- whose h5i-app crates (0.1.4) the Rust code depends on.
require h5i_app_lib from git
  "https://github.com/h5i-dev/h5i" @ "v0.5.2" / "crates/h5i-app-core/proofs"

package h5i_pay_proofs

-- Generated Lean: the extracted kernels (`h5i app extract crates/payment-ledger`,
-- `h5i app extract crates/payment-domain`) and their `schema!` Lean. Do not edit.
lean_lib Generated where
  srcDir := "generated"
  roots := #[`PaymentLedger, `LedgerSchema, `PaymentDomain, `PaymentsSchema]

-- Hand-written specifications and proofs.
@[default_target] lean_lib Proofs where
  roots := #[`Ledger, `Batch, `Payments, `Policies, `Models]
