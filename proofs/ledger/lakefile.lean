import Lake
open Lake DSL

-- Pinned to the Aeneas commit h5i extracts with (`h5i app doctor`; the same
-- at tag v0.5.2 and at 02ccdde, which h5i-app 0.5.2 was published from).
require aeneas from git
  "https://github.com/AeneasVerif/aeneas" @ "b86120db3183b0107eb5f2637b11c424cd06ef1c" / "backends/lean"

-- h5i-app's lemmas and tactics for extracted kernels, from the commit h5i-app
-- 0.5.2 was published from (Cargo.toml: crates.io =0.5.2, h5i-app-gpu at 02ccdde).
require h5i_app_lib from git
  "https://github.com/h5i-dev/h5i" @ "02ccdde4ccfc849f09f764ff7d02a31aa7ac9306" / "crates/h5i-app-core/proofs"

package h5i_pay_ledger_proofs

-- Generated Lean: `h5i app extract crates/payment-ledger` and its `schema!` Lean. Do not edit.
lean_lib Generated where
  srcDir := "generated"
  roots := #[`PaymentLedger, `LedgerSchema]

-- Hand-written specifications and proofs.
@[default_target] lean_lib Proofs where
  roots := #[`Ledger, `Batch, `Models]
