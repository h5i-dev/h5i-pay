import Reconcile.Join
/-!
# A period's reconciliation is the whole run restricted to it (G6)

`POST /v1/reconcile` reconciles one period at a time: the internal and the
external rows whose keys fall in the period, and nothing else. A period is
any set of keys `P` (the API's: the ids `from ≤ id < to` of the key's
payment or refund, `payment_reconcile::period`). Every code depends only on
the rows that share the row's key (`rowCode`, `extCode`: both key counts
and the first external row with it), and all rows with a key in `P` are in
the period, so on a period's rows the codes are those of the whole run
(`rowCode_restrict`, `extCode_restrict`). With `join_eq_reference`:
`join_period`, the single-thread join on a period gives each of its rows
the code the whole-tenant run gives that row.

Not claimed: the matched index (`matchIdx`) is an index into the period's
external rows, not the whole run's; the matched record is the same one.
-/
open Aeneas Aeneas.Std Result payment_reconcile payment_reconcile.rules payment_reconcile.Reconcile
  payment_reconcile.Join

namespace payment_reconcile.Period

/-- The rows of `l` whose key is in the period `P`. -/
def restrict (P : U64 → Bool) (l : List Record) : List Record := l.filter (fun x => P x.key)

theorem withKey_restrict (P : U64 → Bool) (l : List Record) (k : U64) (hk : P k = true) :
    withKey (restrict P l) k = withKey l k := by
  unfold withKey restrict
  rw [List.filter_filter]
  apply List.filter_congr
  intro x _
  by_cases h : x.key = k
  · simp [h, hk]
  · simp [h]

theorem find_restrict (P : U64 → Bool) (E : List Record) (k : U64) (hk : P k = true) :
    (restrict P E).find? (fun y => decide (y.key = k)) = E.find? (fun y => decide (y.key = k)) := by
  unfold restrict
  induction E with
  | nil => rfl
  | cons y ys ih =>
    by_cases h : y.key = k
    · have hy : P y.key = true := by rw [h]; exact hk
      simp [List.filter_cons, hy, h, hk]
    · by_cases hy : P y.key = true
      · simp [List.filter_cons, hy, h, ih]
      · simp [List.filter_cons, hy, h, ih]

theorem rowCode_restrict (P : U64 → Bool) (I E : List Record) (x : Record) (hx : P x.key = true) :
    rowCode (restrict P I) (restrict P E) x = rowCode I E x := by
  unfold rowCode
  rw [withKey_restrict P I _ hx, withKey_restrict P E _ hx, find_restrict P E _ hx]

theorem extCode_restrict (P : U64 → Bool) (I E : List Record) (y : Record) (hy : P y.key = true) :
    extCode (restrict P I) (restrict P E) y = extCode I E y := by
  unfold extCode
  rw [withKey_restrict P I _ hy, withKey_restrict P E _ hy]

/-- **A period is the whole run restricted.** For any set of keys `P`, the
single-thread join on the internal and external rows with keys in `P`
gives every one of those rows the code the run on all rows gives it. -/
theorem join_period (P : U64 → Bool) (I E : List Record) (I' E' : alloc.vec.Vec Record)
    (hI : I'.val = restrict P I) (hE : E'.val = restrict P E)
    (hkI : ∀ x ∈ I'.val, x.key ≠ EMPTY) (hkE : ∀ y ∈ E'.val, y.key ≠ EMPTY)
    (hnI : I'.val.length < U32.max) (hnE : E'.val.length < U32.max)
    (hcI : 4 * I'.val.length + 32 ≤ Usize.max) (hcE : 4 * E'.val.length + 32 ≤ Usize.max) :
    join.join I' E' ⦃ out =>
      out.internal.val.map (·.val) = I'.val.map (rowCode I E) ∧
      out.external.val.map (·.val) = E'.val.map (extCode I E) ⦄ := by
  apply WP.spec_mono (join_eq_reference I' E' hkI hkE hnI hnE hcI hcE)
  rintro out ⟨h1, -, h3⟩
  refine ⟨h1.trans ?_, h3.trans ?_⟩
  · rw [hI, hE]
    apply List.map_congr_left
    intro x hx
    exact rowCode_restrict P I E x (List.mem_filter.1 hx).2
  · rw [hI, hE]
    apply List.map_congr_left
    intro y hy
    exact extCode_restrict P I E y (List.mem_filter.1 hy).2

end payment_reconcile.Period
