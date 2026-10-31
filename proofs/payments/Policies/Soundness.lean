import PaymentDomain
import H5iAppLib
/-!
# Policy soundness

`payment_domain::policy::check` is the stateless rule set that the batch
validator runs on CPU threads, SIMD lanes and the GPU. Soundness: it
approves a request only if every rule holds. The converse (that it approves
every request satisfying the rules) is not claimed for the merchant
allowlist, whose binary search assumes a sorted list: on an unsorted list it
may miss a merchant and refuse, never wrongly approve.
-/
open Aeneas Aeneas.Std Result payment_domain H5iAppLib

namespace payment_domain.Policies

@[step] theorem contains_sorted_spec (sorted : Slice U64) (x : U64) :
    policy.contains_sorted sorted x ⦃ b => b = true → x ∈ sorted.val ⦄ := by
  unfold policy.contains_sorted policy.contains_sorted_loop
  apply loop.spec_decr_nat (measure := fun (p : Usize × Usize) => p.2.val - p.1.val)
    (inv := fun p => p.1.val ≤ p.2.val ∧ p.2.val ≤ sorted.length)
  · rintro ⟨lo, hi⟩ ⟨hle, hlen⟩
    simp only at hle hlen
    unfold policy.contains_sorted_loop.body
    dsimp only
    split
    · step*
      all_goals first
        | (intro _; rw [← ‹v = x›, v_post]; exact List.getElem_mem _)
        | (refine ⟨?_, ?_, ?_⟩ <;> scalar_tac)
    · simp
  · simp [Slice.length]

@[step] theorem contains_linear_spec (xs : Slice U64) (n : Usize) (x : U64) :
    policy.contains_linear xs n x ⦃ b => b = false → x ∉ xs.val.take n.val ⦄ := by
  unfold policy.contains_linear policy.contains_linear_loop
  apply loop.spec_decr_nat (measure := fun (i : Usize) => xs.length - i.val)
    (inv := fun i => ∀ j : Nat, j < i.val → (h : j < xs.length) → xs.val[j] ≠ x)
  · intro i hi
    unfold policy.contains_linear_loop.body
    step*
    · refine ⟨fun j hj hl => ?_, by simp only [Slice.length] at *; scalar_tac⟩
      by_cases e : j = i.val
      · subst e; rw [← i2_post]; assumption
      · exact hi j (by scalar_tac) hl
    all_goals
      intro _ hm
      obtain ⟨j, hj, he⟩ := List.mem_iff_getElem.1 hm
      simp only [List.length_take] at hj
      rw [List.getElem_take] at he
      exact hi j (by simp only [Slice.len, Slice.length] at *; scalar_tac) (by simp only [Slice.length]; omega) he
  · intro j hj; simp at hj

@[step] theorem currency_allowed_spec (l : policy.Limits) (c : U32) :
    policy.currency_allowed l c ⦃ b => b = true → c ∈ l.currencies.val.take l.n_currencies.val ⦄ := by
  unfold policy.currency_allowed policy.currency_allowed_loop
  apply loop.spec_decr_nat (measure := fun (i : Usize) => 8 - i.val) (inv := fun i => i.val ≤ 8)
  · intro i hi
    unfold policy.currency_allowed_loop.body
    step*
    · simp only [policy.MAX_CURRENCIES] at *; scalar_tac
    · have hn : i.val < l.n_currencies.val := by
        have := U32.cast_Usize_val_eq l.n_currencies; rw [i1_post] at *; scalar_tac
      have hl := l.currencies.property
      rw [← ‹i2 = c›, i2_post]
      intro _
      apply List.mem_iff_getElem.2
      refine ⟨i.val, ?_, ?_⟩
      · rw [List.length_take, Nat.lt_min]
        have h8 : i.val < 8 := by
          have := ‹i < policy.MAX_CURRENCIES›; unfold policy.MAX_CURRENCIES at this; scalar_tac
        exact ⟨hn, by rw [hl]; simpa using h8⟩
      · rw [List.getElem_take]
  · scalar_tac

/-- **Policy soundness.** `check` approves a request only if: the amount is
positive and at least the minimum, the kind is known and allowed, the amount
is within that kind's limit, the currency is allowed, the merchant is on
the allowlist, the request does not pay the merchant itself, and the
recipient is not blocked. -/
theorem check_sound (l : policy.Limits) (merchants blocked : Slice U64) (nb : Usize) (r : policy.PaymentReq) :
    policy.check l merchants blocked nb r ⦃ d => d = policy.APPROVED →
      r.amount.val ≠ 0 ∧ l.min_amount.val ≤ r.amount.val ∧ r.kind.val < 4 ∧
      (l.kinds_mask.val >>> r.kind.val) % 2 = 1 ∧
      (∃ cap ∈ l.max_amount.val[r.kind.val]?, r.amount.val ≤ cap.val) ∧
      r.currency ∈ l.currencies.val.take l.n_currencies.val ∧
      r.merchant ∈ merchants.val ∧ r.recipient ≠ r.merchant ∧
      r.recipient ∉ blocked.val.take nb.val ⦄ := by
  unfold policy.check
  step*
  all_goals try (intro h; simp [policy.APPROVED, policy.BAD_POLICY, policy.ZERO_AMOUNT, policy.BELOW_MINIMUM,
    policy.UNKNOWN_KIND, policy.KIND_NOT_ALLOWED, policy.ABOVE_LIMIT, policy.CURRENCY_NOT_ALLOWED,
    policy.MERCHANT_NOT_ALLOWED, policy.SELF_PAYMENT, policy.RECIPIENT_BLOCKED] at h; done)
  all_goals
    have hk : r.kind.val < 4 := by
      have h1 : i2.val = r.kind.val := by rw [i2_post]; exact U32.cast_Usize_val_eq r.kind
      have h2 : ¬ (i2.val ≥ 4) := by simpa [policy.KINDS] using ‹¬i2 ≥ policy.KINDS›
      omega
  · scalar_tac
  · have := U32.cast_Usize_val_eq r.kind
    have := l.max_amount.property
    rw [i5_post]; simp only [Array.length] at *; scalar_tac
  · intro _
    have h5 : i5.val = r.kind.val := by have := U32.cast_Usize_val_eq r.kind; rw [i5_post]; scalar_tac
    have hbit : i4.val ≠ 0 := fun e => ‹¬i4 = 0#u32› (by scalar_tac)
    have h1 : (1#u32 : U32).val = 1 := rfl
    rw [i4_post, UScalar.val_and, i3_post, h1, Nat.and_one_is_mod] at hbit
    have hlen := l.max_amount.property
    refine ⟨by scalar_tac, by scalar_tac, hk, by simp at hbit; omega, ⟨i6, ?_, by scalar_tac⟩,
      b_post ‹_›, b1_post ‹_›, ‹_›, b2_post (by simpa using ‹¬b2 = true›)⟩
    rw [← h5, List.getElem?_eq_getElem (by rw [hlen, h5]; simpa using hk), i6_post]; rfl

end payment_domain.Policies
