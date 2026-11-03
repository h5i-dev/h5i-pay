import Lake
open Lake DSL

-- The same pins as proofs/ledger and proofs/payments.
require aeneas from git
  "https://github.com/AeneasVerif/aeneas" @ "b86120db3183b0107eb5f2637b11c424cd06ef1c" / "backends/lean"

require h5i_app_lib from git
  "https://github.com/h5i-dev/h5i" @ "7b91863063707d16c339b029d6d5543b89642de8" / "crates/h5i-app-core/proofs"

package h5i_pay_reconcile_proofs

-- Generated Lean: `h5i app extract crates/payment-reconcile`. Do not edit.
lean_lib Generated where
  srcDir := "generated"
  roots := #[`PaymentReconcile]

-- Hand-written specifications and proofs.
@[default_target] lean_lib Proofs where
  roots := #[`Reconcile, `Reconcile.Join, `Reconcile.Period]
