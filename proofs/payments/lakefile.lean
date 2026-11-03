import Lake
open Lake DSL

-- Pinned to the Aeneas commit h5i extracts with (`h5i app doctor`; the same
-- at tag v0.5.2 and at 02ccdde, which h5i-app 0.5.2 was published from).
require aeneas from git
  "https://github.com/AeneasVerif/aeneas" @ "b86120db3183b0107eb5f2637b11c424cd06ef1c" / "backends/lean"

-- h5i-app's lemmas and tactics for extracted kernels, from the h5i commit every
-- h5i-app crate comes from (Cargo.toml: branch `app/pipeline-errors`).
require h5i_app_lib from git
  "https://github.com/h5i-dev/h5i" @ "9ec26b510cbcbbe0bea703e63239b0e6a515e33d" / "crates/h5i-app-core/proofs"

package h5i_pay_payments_proofs

-- Generated Lean: `h5i app extract crates/payment-domain` and its `schema!` Lean. Do not edit.
lean_lib Generated where
  srcDir := "generated"
  roots := #[`PaymentDomain, `PaymentsSchema]
  -- The history table's 17-column row decode (`HistoryEntry.from_row_spec`)
  -- needs more than the default 200000 heartbeats.
  leanOptions := #[⟨`maxHeartbeats, (1000000 : Nat)⟩]

-- Hand-written specifications and proofs.
@[default_target] lean_lib Proofs where
  roots := #[`Payments, `Policies]
