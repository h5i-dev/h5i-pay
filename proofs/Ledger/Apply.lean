import Ledger.Spec
import LedgerSchema
/-!
# Committing a write set

The kernel's `apply` computes `Spec.applyAll`. `schema!` generated `apply`
and the `put` loops with their lemmas (`LedgerSchema.lean`), so only the
dispatch over `Write` is left.
-/
open Aeneas Aeneas.Std Result payment_ledger payment_ledger.Spec payment_ledger.Schema H5iAppLib

namespace payment_ledger.ApplyProofs

theorem write_clone (w : Write) : Write.Insts.CoreCloneClone.clone w = ok w := by
  cases w <;> rfl

theorem apply_write_spec (s : Snapshot) (w : Write) (h : Snapshot.size s < Usize.max) :
    apply_write s w ⦃ s' => Snapshot.toSt s' = applyWrite (Snapshot.toSt s) w ∧
      Snapshot.size s' ≤ Snapshot.size s + 1 ⦄ := by
  unfold Snapshot.size at *
  unfold apply_write
  cases w <;> step* <;> simp_all [Snapshot.toSt, applyWrite, alloc.vec.Vec.length] <;> grind [upsert_length]

/-- The kernel's `apply` computes `Spec.applyAll`, as long as no table can
overflow (each write adds at most one row). -/
theorem apply_spec (s : Snapshot) (ws : alloc.vec.Vec Write) (h : Snapshot.size s + ws.length < Usize.max) :
    apply s ws ⦃ s' => Snapshot.toSt s' = applyAll (Snapshot.toSt s) ws.val ⦄ :=
  apply_spec' Snapshot.toSt applyWrite apply_write_spec write_clone s ws h

/-- Rows in the state's keyed tables. -/
def rows (s : St) : Nat := s.accounts.length + s.transfers.length

theorem rows_toSt (s : Snapshot) : rows (Snapshot.toSt s) = Snapshot.size s := by
  simp [rows, Snapshot.toSt, Snapshot.size, alloc.vec.Vec.length]

/-- Each write adds at most one row. -/
theorem rows_applyAll (s : St) (ws : List Write) : rows (applyAll s ws) ≤ rows s + ws.length := by
  induction ws generalizing s with
  | nil => simp [applyAll]
  | cons w ws ih =>
    have := ih (applyWrite s w)
    have : rows (applyWrite s w) ≤ rows s + 1 := by
      cases w <;> simp only [applyWrite, rows] <;> grind [upsert_length]
    simp only [applyAll, List.foldl_cons, List.length_cons] at *
    omega

end payment_ledger.ApplyProofs
