import PaymentReconcile
import H5iAppLib
/-!
# Reconciliation: what one row's code means

`payment_reconcile::rules::classify_internal` / `classify_external` decide
each row's code from how many internal and external records share its key
and the first external one. Every engine calls them: the reference, CPU
single-thread, CPU multithread, SIMD, and the GPU (the device crate compiles
`rules.rs` itself to PTX). This file is about the Lean that Aeneas extracts
from that source (`generated/PaymentReconcile.lean`).

* `classify_internal_spec`, `classify_external_spec`: the extracted
  functions compute the decision tables `specInternal` / `specExternal`.
* Over whole inputs (`rowCode` / `extCode`: the counts and the first
  external record of a row's key in the internal and external lists, which
  is what the reference engine computes and the hash joins look up):
  `internal_matched_iff`, `external_matched_iff` (when a row is MATCHED),
  `duplicates_first` (a duplicated key is reported as such, whatever the
  fields), `exception_reported_once` (a one-to-one pair with a field
  mismatch gets the mismatch on the internal row and MATCHED on the
  external one), `pair_matched_both_sides`, and `codes_in_range`.

Not proven: that the hash tables (CPU `Table`, the GPU's `recon_build` with
atomics, `find_slot`) return those counts and that first record. That is the
trusted join, checked by differential tests against the reference.
-/
open Aeneas Aeneas.Std Result payment_reconcile payment_reconcile.rules

namespace payment_reconcile.Reconcile

/-! ## The decision tables -/

/-- An internal record's code: 6 duplicate internal, 4 missing external,
5 duplicate external, 2 currency, 1 amount, 3 status mismatch, 0 matched. -/
def specInternal (r : Record) (ic ec : Nat) (first : Option Record) : Nat :=
  if ic > 1 then 6
  else match first with
    | none => 4
    | some e =>
      if ec > 1 then 5
      else if e.currency ≠ r.currency then 2
      else if e.amount ≠ r.amount then 1
      else if e.status ≠ r.status then 3
      else 0

/-- An external record's code: 7 extra, 6 duplicate internal, 5 duplicate external, 0 matched. -/
def specExternal (ic ec : Nat) : Nat :=
  if ic = 0 then 7 else if ic > 1 then 6 else if ec > 1 then 5 else 0

theorem classify_internal_spec (r : Record) (ic ec : U32) (f : Option Record) :
    classify_internal r ic ec f ⦃ c => c.val = specInternal r ic.val ec.val f ⦄ := by
  unfold classify_internal specInternal
  by_cases h1 : ic > 1#u32
  · have : ic.val > 1 := by scalar_tac
    simp [h1, this, DUPLICATE_INTERNAL]
  · have : ¬ ic.val > 1 := by scalar_tac
    simp only [h1, this, if_false]
    cases f with
    | none => simp [MISSING_EXTERNAL]
    | some e =>
      by_cases h2 : ec > 1#u32
      · have : ec.val > 1 := by scalar_tac
        simp [h2, this, DUPLICATE_EXTERNAL]
      · have : ¬ ec.val > 1 := by scalar_tac
        simp only [h2, this, if_false]
        by_cases hc : e.currency = r.currency <;> by_cases ha : e.amount = r.amount <;>
          by_cases hs : e.status = r.status <;>
          simp [hc, ha, hs, CURRENCY_MISMATCH, AMOUNT_MISMATCH, STATUS_MISMATCH, MATCHED]

theorem classify_external_spec (ic ec : U32) :
    classify_external ic ec ⦃ c => c.val = specExternal ic.val ec.val ⦄ := by
  unfold classify_external specExternal
  by_cases h0 : ic = 0#u32
  · have : ic.val = 0 := by simp [h0]
    simp [h0, this, EXTRA_EXTERNAL]
  · have h0' : ic.val ≠ 0 := by intro h; exact h0 (by scalar_tac)
    simp only [h0, h0', if_false]
    by_cases h1 : ic > 1#u32
    · have : ic.val > 1 := by scalar_tac
      simp [h1, this, DUPLICATE_INTERNAL]
    · have : ¬ ic.val > 1 := by scalar_tac
      simp only [h1, this, if_false]
      by_cases h2 : ec > 1#u32
      · have : ec.val > 1 := by scalar_tac
        simp [h2, this, DUPLICATE_EXTERNAL]
      · have : ¬ ec.val > 1 := by scalar_tac
        simp [h2, this, MATCHED]

/-- Every code is one of the eight (`rules::CODES`), so it indexes the report's counters. -/
theorem codes_in_range (r : Record) (ic ec : U32) (f : Option Record) :
    classify_internal r ic ec f ⦃ c => c.val < 8 ⦄ ∧ classify_external ic ec ⦃ c => c.val < 8 ⦄ := by
  constructor
  · apply WP.spec_mono (classify_internal_spec r ic ec f)
    intro c h; rw [h]; unfold specInternal
    split
    · omega
    · split
      · omega
      · split_ifs <;> omega
  · apply WP.spec_mono (classify_external_spec ic ec)
    intro c h; rw [h]; unfold specExternal; split_ifs <;> omega

/-! ## Over whole inputs -/

/-- The records of `l` with key `k`. -/
def withKey (l : List Record) (k : U64) : List Record := l.filter (fun x => decide (x.key = k))

/-- What an engine computes for internal record `x`: the counts of its key on
both sides and the first external record with it. -/
def rowCode (I E : List Record) (x : Record) : Nat :=
  specInternal x (withKey I x.key).length (withKey E x.key).length (E.find? (fun y => decide (y.key = x.key)))

def extCode (I E : List Record) (y : Record) : Nat :=
  specExternal (withKey I y.key).length (withKey E y.key).length

theorem find_eq_head (E : List Record) (k : U64) :
    E.find? (fun y => decide (y.key = k)) = (withKey E k).head? := by
  unfold withKey
  induction E with
  | nil => rfl
  | cons y ys ih =>
    by_cases h : y.key = k
    · simp [List.find?_cons, List.filter_cons, h]
    · simp only [List.find?_cons, List.filter_cons, h, decide_false]; exact ih

theorem mem_withKey {I : List Record} {x : Record} (h : x ∈ I) : 1 ≤ (withKey I x.key).length := by
  unfold withKey
  have : x ∈ I.filter (fun z => decide (z.key = x.key)) := List.mem_filter.2 ⟨h, by simp⟩
  exact List.length_pos_of_mem this

/-- **When an internal row is MATCHED**: its key is unique on both sides and
the one external record agrees in currency, amount and status. -/
theorem internal_matched_iff (I E : List Record) (x : Record) (hx : x ∈ I) :
    rowCode I E x = 0 ↔ (withKey I x.key).length = 1 ∧
      ∃ e, withKey E x.key = [e] ∧ e.currency = x.currency ∧ e.amount = x.amount ∧ e.status = x.status := by
  have h1 := mem_withKey hx
  unfold rowCode specInternal
  rw [find_eq_head]
  constructor
  · intro h
    split at h
    · omega
    · rename_i hic
      cases hw : withKey E x.key with
      | nil => simp [hw] at h
      | cons e rest =>
        simp only [hw, List.head?_cons, List.length_cons] at h
        split_ifs at h with h2 hc ha hs
        refine ⟨by omega, e, ?_, UScalar.eq_of_val_eq (by simpa using hc), UScalar.eq_of_val_eq (by simpa using ha),
          UScalar.eq_of_val_eq (by simpa using hs)⟩
        have : rest.length = 0 := by omega
        simp [List.length_eq_zero_iff.1 this]
  · rintro ⟨hi, e, he, hc, ha, hs⟩
    simp [hi, he, hc, ha, hs]

/-- **When an external row is MATCHED**: exactly one internal record and no
other external record share its key. -/
theorem external_matched_iff (I E : List Record) (y : Record) (hy : y ∈ E) :
    extCode I E y = 0 ↔ (withKey I y.key).length = 1 ∧ (withKey E y.key).length = 1 := by
  have h1 := mem_withKey hy
  unfold extCode specExternal
  constructor
  · intro h; split_ifs at h <;> omega
  · rintro ⟨hi, he⟩; simp [hi, he]

/-- **Duplicates first.** A key that two internal records share is reported
as a duplicate on every row with it, on both sides, whatever the fields. -/
theorem duplicates_first (I E : List Record) (x : Record) (h : (withKey I x.key).length > 1) :
    rowCode I E x = 6 ∧ ∀ y ∈ E, y.key = x.key → extCode I E y = 6 := by
  refine ⟨by simp [rowCode, specInternal, h], fun y _ hk => ?_⟩
  have : (withKey I y.key).length > 1 := by rw [hk]; exact h
  unfold extCode specExternal
  rw [if_neg (by omega), if_pos this]

/-- **An exception is reported once.** A one-to-one pair that disagrees in
a field gets the mismatch code on the internal row and MATCHED on the
external row, so the report counts it once. -/
theorem exception_reported_once (I E : List Record) (x y : Record)
    (hI : withKey I x.key = [x]) (hE : withKey E x.key = [y]) (hk : y.key = x.key)
    (hd : ¬ (y.currency = x.currency ∧ y.amount = x.amount ∧ y.status = x.status)) :
    (rowCode I E x = 1 ∨ rowCode I E x = 2 ∨ rowCode I E x = 3) ∧ extCode I E y = 0 := by
  constructor
  · unfold rowCode specInternal
    rw [find_eq_head, hI, hE]
    simp only [List.length_singleton, List.head?_cons]
    by_cases hc : y.currency = x.currency <;> by_cases ha : y.amount = x.amount <;>
      by_cases hs : y.status = x.status <;> simp_all
  · simp [extCode, specExternal, hk, hI, hE]

/-- **A matched pair is matched on both sides.** -/
theorem pair_matched_both_sides (I E : List Record) (x : Record) (hx : x ∈ I) (h : rowCode I E x = 0) :
    ∃ y ∈ E, y.key = x.key ∧ extCode I E y = 0 := by
  obtain ⟨hi, e, he, -⟩ := (internal_matched_iff I E x hx).1 h
  have hm : e ∈ withKey E x.key := by rw [he]; simp
  unfold withKey at hm
  obtain ⟨hmE, hkey⟩ := List.mem_filter.1 hm
  have hkey : e.key = x.key := by simpa using hkey
  refine ⟨e, hmE, hkey, ?_⟩
  simp [extCode, specExternal, hkey, hi, he]

end payment_reconcile.Reconcile
