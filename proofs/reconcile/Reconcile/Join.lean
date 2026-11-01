import Reconcile
/-!
# The single-thread hash join computes the reference's answer

`payment_reconcile::join` (`crates/payment-reconcile/src/join.rs`) is the
single-thread engine (`cpu_single`): an open-addressing table per side,
linear probing from `start k cap`, holding per key how many rows have it and
the lowest row index among them. This file proves (`join_eq_reference`)
that `join.join I E` gives

* every internal row `x` the code `rowCode I E x` and the matched index
  `matchIdx E x.key` (the index of the first external row with its key, or
  `NONE`), and
* every external row `y` the code `extCode I E y`,

i.e. for each row both key counts and the first external record of its key.
That is what the `BTreeMap` reference engine (`payment_reconcile::reference`:
a map from key to the list of external row indices, and one from key to the
internal count) computes; `Reconcile.lean` proves what those codes mean.

The table invariant (`TInv t P`, `P` the rows inserted so far): each full
slot holds a distinct key with its row count and lowest row index in `P`;
every row of `P` has a slot; every slot on the probe path from a key's start
to its slot is full (`Chains`, so a probe that meets an empty slot may stop);
and at most `|P|` slots are full while `|P|` is less than the capacity (so
there is always an empty slot and every probe ends).

Nothing here depends on where probing starts (`startN`): any start below the
capacity gives the same results.
-/
open Aeneas Aeneas.Std Result payment_reconcile payment_reconcile.rules payment_reconcile.Reconcile

namespace payment_reconcile.Join

/-! ## Probe paths, without `%` -/

/-- Slot `d` steps after `a` in a table of `c` slots (`a, d < c`). -/
def at' (c a d : Nat) : Nat := if a + d < c then a + d else a + d - c

/-- How many steps from `a` to `t` (`a, t < c`). -/
def dist (c a t : Nat) : Nat := if a ≤ t then t - a else t + c - a

theorem at'_lt {c a d : Nat} (ha : a < c) (hd : d < c) : at' c a d < c := by unfold at'; split <;> omega

theorem dist_lt {c a t : Nat} (ha : a < c) (ht : t < c) : dist c a t < c := by unfold dist; split <;> omega

theorem at'_dist {c a t : Nat} (ha : a < c) (ht : t < c) : at' c a (dist c a t) = t := by
  unfold at' dist; split <;> split <;> omega

theorem dist_at' {c a d : Nat} (ha : a < c) (hd : d < c) : dist c a (at' c a d) = d := by
  unfold at' dist; split <;> split <;> omega

theorem at'_zero {c a : Nat} (ha : a < c) : at' c a 0 = a := by unfold at'; split <;> omega

theorem at'_succ_wrap {c a n : Nat} (_ha : a < c) (_hn : n < c) (h : at' c a n + 1 = c) : at' c a (n + 1) = 0 := by
  unfold at' at *; split at h <;> split <;> omega

theorem at'_succ {c a n : Nat} (_ha : a < c) (_hn : n < c) (h : at' c a n + 1 ≠ c) :
    at' c a (n + 1) = at' c a n + 1 := by
  unfold at' at *; split at h <;> split <;> omega

/-! ## Where a probe starts -/

theorem usize_lt (x : Usize) : x.val < 2^64 := by
  have h1 := x.hBounds
  rcases System.Platform.numBits_eq with h2 | h2 <;> simp [UScalarTy.numBits, h2] at h1 <;> omega

theorem hash_ok (k : U64) : ∃ h, rules.hash k = ok h := by
  have : rules.hash k ⦃ _ => True ⦄ := by unfold rules.hash; step*
  obtain ⟨h, hh, -⟩ := (WP.spec_equiv_exists _ _).1 this
  exact ⟨h, hh⟩

/-- `rules::hash k` (it never fails: wrapping multiplies, xors, shifts). -/
noncomputable def hashOf (k : U64) : U64 := Classical.choose (hash_ok k)

theorem hash_eq (k : U64) : rules.hash k = ok (hashOf k) := Classical.choose_spec (hash_ok k)

/-- The slot probing for `k` starts at in a table of `c` slots: the hash
masked to the capacity, as in `Table`, `find_slot` and the GPU tables. -/
noncomputable def startN (k : U64) (c : Nat) : Nat := (hashOf k).val &&& (c - 1)

theorem startN_lt (k : U64) {c : Nat} (h : 0 < c) : startN k c < c := by
  have := Nat.and_le_right (n := (hashOf k).val) (m := c - 1)
  unfold startN; omega

theorem start_spec (k : U64) (cap : Usize) (h : 0 < cap.val) :
    join.start k cap ⦃ s => s.val = startN k cap.val ∧ s.val < cap.val ⦄ := by
  have hc := usize_lt cap
  have hc1 : (UScalar.cast UScalarTy.U64 cap).val = cap.val := by
    rw [UScalar.cast_val_eq]; exact Nat.mod_eq_of_lt (by simpa using hc)
  unfold join.start
  rw [hash_eq]
  step*
  have hand : i3.val = startN k cap.val := by
    rw [i3_post, UScalar.val_and, i2_post, i1_post, hc1]; rfl
  have hlt : i3.val < cap.val := hand ▸ startN_lt k h
  have hcu : (UScalar.cast UScalarTy.Usize i3).val = i3.val := by
    rw [UScalar.cast_val_eq]; apply Nat.mod_eq_of_lt; have := cap.hBounds; omega
  rw [hcu]; exact ⟨hand, hlt⟩

/-! ## Probing -/

/-- The first `n` slots of the path from `a` are full. -/
def PathFull (kl : List U64) (a n : Nat) : Prop := ∀ d < n, kl[at' kl.length a d]! ≠ EMPTY

/-- The probe path invariant: a full slot is reached from its key's start
through full slots only. -/
def Chains (kl : List U64) : Prop :=
  ∀ t < kl.length, kl[t]! ≠ EMPTY →
    PathFull kl (startN kl[t]! kl.length) (dist kl.length (startN kl[t]! kl.length) t)

def HasEmpty (kl : List U64) : Prop := ∃ e < kl.length, kl[e]! = EMPTY

/-- What a probe for `k` returns: `k`'s slot, or an empty slot reached
through full slots when no slot holds `k`. -/
def Probed (kl : List U64) (k : U64) (s : Nat) : Prop :=
  s < kl.length ∧ (kl[s]! = k ∨
    (kl[s]! = EMPTY ∧ (∀ t < kl.length, kl[t]! ≠ k) ∧
      PathFull kl (startN k kl.length) (dist kl.length (startN k kl.length) s)))

theorem get_eq {α} [Inhabited α] {l : List α} {i : Nat} (h : i < l.length) : l[i] = l[i]! := by
  simp [getElem!_pos, h]

theorem probe_spec (keys : alloc.vec.Vec U64) (k : U64) (hk : k ≠ EMPTY)
    (hch : Chains keys.val) (he : HasEmpty keys.val) :
    join.probe keys k ⦃ s => Probed keys.val k s.val ⦄ := by
  obtain ⟨e, hel, hee⟩ := he
  have hlen : (alloc.vec.Vec.len keys).val = keys.val.length := by simp
  have hpos : 0 < (alloc.vec.Vec.len keys).val := by omega
  unfold join.probe
  apply WP.spec_bind (start_spec k _ hpos)
  intro a ⟨ha, hac⟩
  rw [hlen] at ha hac
  unfold join.probe_loop
  apply loop.spec_decr_nat (measure := fun (x : Usize × Usize) => keys.val.length - x.2.val)
    (inv := fun x => x.2.val ≤ keys.val.length ∧ x.1.val = at' keys.val.length a.val x.2.val ∧
      ∀ d < x.2.val, keys.val[at' keys.val.length a.val d]! ≠ k ∧
        keys.val[at' keys.val.length a.val d]! ≠ EMPTY)
  · rintro ⟨s, n⟩ ⟨hn, hs, hpath⟩
    simp only at hn hs hpath
    unfold join.probe_loop.body
    by_cases hlt : n.val < keys.val.length
    · have hsl : s.val < keys.val.length := by rw [hs]; exact at'_lt hac hlt
      have hget := get_eq hsl
      have hds : dist keys.val.length a.val s.val = n.val := by rw [hs]; exact dist_at' hac hlt
      step*
      · exact ⟨hsl, Or.inl (by rw [← hget, ← i_post]; exact ‹i = k›)⟩
      · have hemp : keys.val[s.val]! = EMPTY := by rw [← hget, ← i_post]; exact ‹i = EMPTY›
        refine ⟨hsl, Or.inr ⟨hemp, ?_, ?_⟩⟩
        · intro t ht hkt
          have hc := hch t ht (by rw [hkt]; exact hk)
          rw [hkt, ← ha] at hc
          have htd := at'_dist hac ht
          rcases Nat.lt_trichotomy (dist keys.val.length a.val t) n.val with hm | hm | hm
          · exact (hpath _ hm).1 (by rw [htd]; exact hkt)
          · rw [hm, ← hs] at htd; rw [htd] at hemp; exact hk (hkt ▸ hemp)
          · exact hc n.val hm (by rw [← hs]; exact hemp)
        · intro d hd; rw [← ha] at *; rw [hds] at hd; exact (hpath d hd).2
      · have hne1 : keys.val[s.val]! ≠ k := by rw [← hget, ← i_post]; exact ‹¬i = k›
        have hne2 : keys.val[s.val]! ≠ EMPTY := by rw [← hget, ← i_post]; exact ‹¬i = EMPTY›
        have hpath' : ∀ d < n.val + 1, keys.val[at' keys.val.length a.val d]! ≠ k ∧
            keys.val[at' keys.val.length a.val d]! ≠ EMPTY := by
          intro d hd
          rcases Nat.lt_or_ge d n.val with h | h
          · exact hpath d h
          · have : d = n.val := by omega
            rw [this, ← hs]; exact ⟨hne1, hne2⟩
        split
        · step*
          have hw : at' keys.val.length a.val n.val + 1 = keys.val.length := by
            have := congrArg UScalar.val ‹i1 = keys.len›; simp at this; omega
          refine ⟨by omega, ?_, by rw [n1_post]; exact hpath', by omega⟩
          rw [n1_post, at'_succ_wrap hac hlt hw]; rfl
        · step*
          have hw : at' keys.val.length a.val n.val + 1 ≠ keys.val.length := by
            intro h; apply ‹¬i1 = keys.len›; apply UScalar.eq_of_val_eq; simp; omega
          refine ⟨by omega, ?_, by rw [n1_post]; exact hpath', by omega⟩
          rw [n1_post, at'_succ hac hlt hw, i1_post, hs]
    · exfalso
      have hd := dist_lt hac hel
      exact (hpath (dist keys.val.length a.val e) (by omega)).2 (by rw [at'_dist hac hel]; exact hee)
  · refine ⟨by simp, ?_, fun d hd => absurd hd (by simp)⟩
    simp [at'_zero hac]

/-! ## The table invariant -/

def keyP (k : U64) : Record → Bool := fun y => decide (y.key = k)

theorem withKey_nil_iff {P : List Record} {k : U64} : withKey P k = [] ↔ ∀ x ∈ P, x.key ≠ k := by
  unfold withKey; simp [List.filter_eq_nil_iff]

theorem withKey_snoc (P : List Record) (r : Record) (k : U64) :
    withKey (P ++ [r]) k = withKey P k ++ (if r.key = k then [r] else []) := by
  unfold withKey; by_cases h : r.key = k <;> simp [List.filter_append, h]

theorem findIdx_lt_of_withKey {P : List Record} {k : U64} (h : withKey P k ≠ []) :
    P.findIdx (keyP k) < P.length := by
  rw [List.findIdx_lt_length]
  obtain ⟨x, hx⟩ := List.exists_mem_of_ne_nil _ h
  unfold withKey at hx
  obtain ⟨hm, hp⟩ := List.mem_filter.1 hx
  exact ⟨x, hm, by simpa [keyP] using hp⟩

theorem findIdx_snoc_present {P : List Record} {k : U64} (r : Record) (h : withKey P k ≠ []) :
    (P ++ [r]).findIdx (keyP k) = P.findIdx (keyP k) := by
  rw [List.findIdx_append]; simp [findIdx_lt_of_withKey h]

theorem findIdx_snoc_absent {P : List Record} {r : Record} (h : withKey P r.key = []) :
    (P ++ [r]).findIdx (keyP r.key) = P.length := by
  rw [List.findIdx_append]
  have : ¬ P.findIdx (keyP r.key) < P.length := by
    rw [List.findIdx_lt_length]
    rintro ⟨x, hm, hp⟩
    exact (withKey_nil_iff.1 h) x hm (by simpa [keyP] using hp)
  simp [this, keyP]

theorem set_get {α} [Inhabited α] (l : List α) (s j : Nat) (v : α) (hs : s < l.length) :
    (l.set s v)[j]! = if j = s then v else l[j]! := by
  split
  · subst_vars; simp [hs]
  · rename_i h; simp [Ne.symm h]

/-- The table `t` after inserting the rows `P` (see the header). -/
structure TInv (t : join.JoinTable) (P : List Record) : Prop where
  lc : t.count.val.length = t.keys.val.length
  lf : t.first.val.length = t.keys.val.length
  room : P.length < t.keys.val.length
  full : t.keys.val.countP (fun x => x != EMPTY) ≤ P.length
  chains : Chains t.keys.val
  distinct : ∀ s < t.keys.val.length, ∀ u < t.keys.val.length,
    t.keys.val[s]! ≠ EMPTY → t.keys.val[s]! = t.keys.val[u]! → s = u
  slot : ∀ s < t.keys.val.length, t.keys.val[s]! ≠ EMPTY →
    t.count.val[s]!.val = (withKey P t.keys.val[s]!).length ∧
    t.first.val[s]!.val = P.findIdx (keyP t.keys.val[s]!) ∧ withKey P t.keys.val[s]! ≠ []
  keys_ok : ∀ x ∈ P, x.key ≠ EMPTY
  present : ∀ x ∈ P, ∃ s < t.keys.val.length, t.keys.val[s]! = x.key

theorem TInv.hasEmpty {t : join.JoinTable} {P : List Record} (h : TInv t P) : HasEmpty t.keys.val := by
  by_contra hne
  have hall : ∀ a ∈ t.keys.val, (a != EMPTY) = true := by
    intro a ha
    obtain ⟨i, hi, rfl⟩ := List.getElem_of_mem ha
    have : ¬ t.keys.val[i]! = EMPTY := fun he => hne ⟨i, hi, he⟩
    rw [← get_eq hi] at this; simpa using this
  have := List.countP_eq_length.2 hall
  have := h.full; have := h.room; omega

/-- Turning empty slots full keeps every probe path full. -/
theorem chains_mono {kl kl' : List U64} (hl : kl'.length = kl.length)
    (h : ∀ j < kl.length, kl[j]! ≠ EMPTY → kl'[j]! ≠ EMPTY) {k : U64} {t : Nat} (ht : t < kl.length)
    (hp : PathFull kl (startN k kl.length) (dist kl.length (startN k kl.length) t)) :
    PathFull kl' (startN k kl'.length) (dist kl'.length (startN k kl'.length) t) := by
  rw [hl]
  intro d hd
  have ha := startN_lt k (c := kl.length) (by omega)
  rw [hl]
  exact h _ (at'_lt ha (by have := dist_lt ha ht; omega)) (hp d hd)

theorem insert_spec (t : join.JoinTable) (P : List Record) (r : Record) (i : U32)
    (hinv : TInv t P) (hk : r.key ≠ EMPTY) (hi : i.val = P.length)
    (hroom : P.length + 1 < t.keys.val.length) (hmax : P.length < U32.max) :
    join.insert t r.key i ⦃ t' => TInv t' (P ++ [r]) ∧ t'.keys.val.length = t.keys.val.length ⦄ := by
  unfold join.insert
  apply WP.spec_bind (probe_spec t.keys r.key hk hinv.chains hinv.hasEmpty)
  intro s ⟨hsl, hpr⟩
  have hget := get_eq hsl
  have hsc : s.val < t.count.val.length := by rw [hinv.lc]; exact hsl
  have hsf : s.val < t.first.val.length := by rw [hinv.lf]; exact hsl
  step*
  · -- `r.key` is new: it takes the empty slot `s`
    have hemp : t.keys.val[s.val]! = EMPTY := by rw [← hget, ← i1_post]; exact ‹i1 = EMPTY›
    have hemp' : t.keys.val[s.val] = EMPTY := hget ▸ hemp
    obtain ⟨-, hnot, hpath⟩ := hpr.resolve_left (fun h => hk (h.symm.trans hemp))
    have hPnone : withKey P r.key = [] := withKey_nil_iff.2 (fun x hx hx' => by
      obtain ⟨u, hu, hku⟩ := hinv.present x hx; exact hnot u hu (hku.trans hx'))
    subst index_mut_back_post index_mut_back1_post index_mut_back2_post
    have hmono : ∀ j < t.keys.val.length, t.keys.val[j]! ≠ EMPTY → (t.keys.val.set s.val r.key)[j]! ≠ EMPTY := by
      intro j _ h; rw [set_get _ _ _ _ hsl]; split
      · exact hk
      · exact h
    refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, by simp⟩ <;> (try dsimp only) <;> (try simp only [alloc.vec.Vec.set_val_eq, List.length_set])
    · exact hinv.lc
    · exact hinv.lf
    · simp; omega
    · rw [List.countP_set hsl]; simp [hemp', hk]; have := hinv.full; omega
    · intro u hu hne
      rw [List.length_set] at hu
      rw [set_get _ _ _ _ hsl] at hne ⊢
      by_cases hus : u = s.val
      · rw [if_pos hus]; subst hus
        exact chains_mono (by simp) hmono hsl hpath
      · rw [if_neg hus] at hne ⊢
        exact chains_mono (by simp) hmono hu (hinv.chains u hu hne)
    · intro a ha b hb hne heq
      rw [set_get _ _ _ _ hsl] at hne heq; rw [set_get _ _ _ _ hsl] at heq
      split_ifs at hne heq with h1 h2 h2
      · omega
      · exact absurd heq.symm (hnot b hb)
      · exact absurd heq (hnot a ha)
      · exact hinv.distinct a ha b hb hne heq
    · intro u hu hne
      rw [set_get _ _ _ _ hsl] at hne ⊢
      rw [set_get _ _ _ _ hsc, set_get _ _ _ _ hsf]
      by_cases hus : u = s.val
      · simp only [if_pos hus, withKey_snoc, hPnone, findIdx_snoc_absent hPnone, if_true]
        simp [hi]
      · simp only [if_neg hus] at hne ⊢
        have hne' : r.key ≠ t.keys.val[u]! := fun h => hnot u hu h.symm
        obtain ⟨h1, h2, h3⟩ := hinv.slot u hu hne
        simp only [withKey_snoc, if_neg hne', List.append_nil, findIdx_snoc_present r h3]
        exact ⟨h1, h2, h3⟩
    · intro x hx
      rcases List.mem_append.1 hx with hx | hx
      · exact hinv.keys_ok x hx
      · rw [List.mem_singleton.1 hx]; exact hk
    · intro x hx
      rcases List.mem_append.1 hx with hx | hx
      · obtain ⟨u, hu, hku⟩ := hinv.present x hx
        refine ⟨u, hu, ?_⟩
        rw [set_get _ _ _ _ hsl]
        have : u ≠ s.val := fun h => hinv.keys_ok x hx (by rw [← hku, h, hemp])
        rw [if_neg this, hku]
      · rw [List.mem_singleton.1 hx]
        exact ⟨s.val, hsl, by rw [set_get _ _ _ _ hsl, if_pos rfl]⟩
  case hmax =>
    have hfull : t.keys.val[s.val]! ≠ EMPTY := by rw [← hget, ← i1_post]; exact ‹¬i1 = EMPTY›
    have h1 := (hinv.slot s hsl hfull).1
    rw [← get_eq hsc, ← i2_post] at h1
    have := List.length_filter_le (fun x : Record => decide (x.key = t.keys.val[s.val]!)) P
    unfold withKey at h1
    scalar_tac
  · -- `r.key` is already in slot `s`: one more row
    have hfull : t.keys.val[s.val]! ≠ EMPTY := by rw [← hget, ← i1_post]; exact ‹¬i1 = EMPTY›
    have hks : t.keys.val[s.val]! = r.key := by
      rcases hpr with h | h
      · exact h
      · exact absurd h.1 hfull
    obtain ⟨hc1, hc2, hc3⟩ := hinv.slot s hsl hfull
    rw [hks] at hc1 hc2 hc3
    rw [← get_eq hsc, ← i2_post] at hc1
    subst index_mut_back_post
    refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;> (try dsimp only) <;> (try simp only [alloc.vec.Vec.set_val_eq, List.length_set])
    · exact hinv.lc
    · exact hinv.lf
    · simp; omega
    · have := hinv.full; simp; omega
    · exact hinv.chains
    · exact hinv.distinct
    · intro u hu hne
      rw [set_get _ _ _ _ hsc]
      by_cases hus : u = s.val
      · subst hus
        simp only [if_true, hks, withKey_snoc, List.length_append, findIdx_snoc_present r hc3]
        refine ⟨by simp [i3_post, hc1], hc2, by simp⟩
      · rw [if_neg hus]
        have hne' : r.key ≠ t.keys.val[u]! := fun h =>
          hus (hinv.distinct u hu s hsl hne (by rw [hks, h]))
        obtain ⟨h1, h2, h3⟩ := hinv.slot u hu hne
        simp only [withKey_snoc, if_neg hne', List.append_nil, findIdx_snoc_present r h3]
        exact ⟨h1, h2, h3⟩
    · intro x hx
      rcases List.mem_append.1 hx with hx | hx
      · exact hinv.keys_ok x hx
      · rw [List.mem_singleton.1 hx]; exact hk
    · intro x hx
      rcases List.mem_append.1 hx with hx | hx
      · exact hinv.present x hx
      · rw [List.mem_singleton.1 hx]; exact ⟨s.val, hsl, hks⟩

/-! ## Building a table -/

theorem table_cap_spec (n : Usize) (h : 4 * n.val + 32 ≤ Usize.max) :
    join.table_cap n ⦃ c => 2 * n.val ≤ c.val ∧ 16 ≤ c.val ⦄ := by
  unfold join.table_cap join.table_cap_loop
  apply loop.spec_decr_nat (measure := fun (c : Usize) => 4 * n.val + 32 - c.val)
    (inv := fun c => 16 ≤ c.val ∧ (c.val = 16 ∨ c.val < 4 * n.val))
  · intro c ⟨h16, hc⟩
    unfold join.table_cap_loop.body
    step*
    have : c.val < i.val := by scalar_tac
    omega
  · simp

theorem empty_table_spec (cap : Usize) :
    join.empty_table cap ⦃ t => t.keys.val = List.replicate cap.val EMPTY ∧
      t.count.val = List.replicate cap.val 0#u32 ∧ t.first.val = List.replicate cap.val NONE ⦄ := by
  unfold join.empty_table
  step*

theorem empty_inv (t : join.JoinTable) (c : Nat) (hc : 0 < c) (hk : t.keys.val = List.replicate c EMPTY)
    (hcount : t.count.val = List.replicate c 0#u32) (hf : t.first.val = List.replicate c NONE) : TInv t [] := by
  have hz : ∀ j < t.keys.val.length, t.keys.val[j]! = EMPTY := by
    intro j hj; rw [← get_eq hj]; simp [hk]
  refine ⟨by simp [hk, hcount], by simp [hk, hf], by simp [hk, hc], by simp [hk], ?_, ?_, ?_, by simp, by simp⟩
  · intro j hj hne; exact absurd (hz j hj) hne
  · intro j hj _ _ hne; exact absurd (hz j hj) hne
  · intro j hj hne; exact absurd (hz j hj) hne

theorem take_succ_eq {α} (l : List α) (i : Nat) (h : i < l.length) : l.take (i + 1) = l.take i ++ [l[i]] := by
  exact (List.take_concat_get' l i h).symm

theorem build_spec (rows : alloc.vec.Vec Record) (hk : ∀ x ∈ rows.val, x.key ≠ EMPTY)
    (hn : rows.val.length < U32.max) (hc : 4 * rows.val.length + 32 ≤ Usize.max) :
    join.build rows ⦃ t => TInv t rows.val ⦄ := by
  unfold join.build
  apply WP.spec_bind (table_cap_spec _ (by simpa using hc))
  intro cap ⟨hc2, hc16⟩
  simp only [alloc.vec.Vec.len_val, alloc.vec.Vec.length] at hc2
  apply WP.spec_bind (empty_table_spec cap)
  intro t0 ⟨hk0, hc0, hf0⟩
  have hinv0 := empty_inv t0 cap.val (by omega) hk0 hc0 hf0
  have hlen0 : t0.keys.val.length = cap.val := by simp [hk0]
  unfold join.build_loop
  apply loop.spec_decr_nat (measure := fun (x : join.JoinTable × Usize) => rows.val.length - x.2.val)
    (inv := fun x => x.2.val ≤ rows.val.length ∧ TInv x.1 (rows.val.take x.2.val) ∧
      rows.val.length < x.1.keys.val.length)
  · rintro ⟨t, i⟩ ⟨hi, hinv, hroom⟩
    simp only at hi hinv hroom
    unfold join.build_loop.body
    by_cases hlt : i.val < rows.val.length
    · have hlt' : i < alloc.vec.Vec.len rows := by scalar_tac
      simp only [hlt', if_true]
      step*
      have hi2 : i2.val = i.val := by
        rw [i2_post, UScalar.cast_val_eq]; apply Nat.mod_eq_of_lt
        have : U32.max = 2^32 - 1 := by simp [U32.max, U32.numBits]
        simp only [UScalarTy.numBits]; omega
      have hlenT : (rows.val.take i.val).length = i.val := by simp; omega
      have hkr : r.key ≠ EMPTY := hk r (by rw [r_post]; exact List.getElem_mem _)
      apply WP.spec_bind (insert_spec t _ r i2 hinv hkr (by rw [hi2, hlenT]) (by rw [hlenT]; omega)
        (by rw [hlenT]; omega))
      rintro t1 ⟨hinv1, hl1⟩
      step*
      refine ⟨by omega, ?_, by omega, by omega⟩
      rw [i3_post, take_succ_eq _ _ hlt, ← r_post]; exact hinv1
    · have hlt' : ¬ i < alloc.vec.Vec.len rows := by scalar_tac
      simp only [hlt', if_false]
      simp only [WP.spec_ok]
      rw [List.take_of_length_le (by omega)] at hinv
      exact hinv
  · exact ⟨by simp, by simpa using hinv0, by simp only; omega⟩

/-! ## Looking a key up -/

/-- The matched index the reference reports for key `k`: the index of the
first row of `E` with it, or `NONE`. -/
def matchIdx (E : List Record) (k : U64) : Nat :=
  if withKey E k = [] then NONE.val else E.findIdx (keyP k)

theorem lookup_spec (t : join.JoinTable) (R : List Record) (k : U64) (hinv : TInv t R) (hk : k ≠ EMPTY) :
    join.lookup t k ⦃ p => p.1.val = (withKey R k).length ∧ p.2.val = matchIdx R k ⦄ := by
  unfold join.lookup
  apply WP.spec_bind (probe_spec t.keys k hk hinv.chains hinv.hasEmpty)
  intro s ⟨hsl, hpr⟩
  have hget := get_eq hsl
  have hsc : s.val < t.count.val.length := by rw [hinv.lc]; exact hsl
  have hsf : s.val < t.first.val.length := by rw [hinv.lf]; exact hsl
  step*
  · have hks : t.keys.val[s.val]! = k := by rw [← hget, ← i_post]; exact ‹i = k›
    obtain ⟨h1, h2, h3⟩ := hinv.slot s hsl (by rw [hks]; exact hk)
    rw [hks, ← get_eq hsc, ← i1_post] at h1
    rw [hks, ← get_eq hsf, ← i2_post] at h2
    rw [hks] at h3
    exact ⟨h1, by rw [h2]; simp [matchIdx, h3]⟩
  · have hne : t.keys.val[s.val]! ≠ k := by rw [← hget, ← i_post]; exact ‹¬i = k›
    obtain ⟨-, hnot, -⟩ := hpr.resolve_left hne
    have hnil : withKey R k = [] := withKey_nil_iff.2 (fun x hx hx' => by
      obtain ⟨u, hu, hku⟩ := hinv.present x hx; exact hnot u hu (hku.trans hx'))
    simp [matchIdx, hnil]

theorem matchIdx_lt {E : List Record} {k : U64} (h : withKey E k ≠ []) : matchIdx E k < E.length := by
  simp only [matchIdx, h, if_false]; exact findIdx_lt_of_withKey h

theorem find_some {E : List Record} {k : U64} (h : withKey E k ≠ []) :
    E.find? (fun y => decide (y.key = k)) = some (E[matchIdx E k]'(matchIdx_lt h)) := by
  have hl := findIdx_lt_of_withKey h
  simp only [matchIdx, h, if_false]
  rw [List.find?_eq_getElem?_findIdx]
  exact List.getElem?_eq_getElem hl

theorem internal_code_spec (x : Record) (I : List Record) (external : alloc.vec.Vec Record)
    (int ext : join.JoinTable) (hint : TInv int I) (hext : TInv ext external.val) (hk : x.key ≠ EMPTY) :
    join.internal_code x external int ext ⦃ p =>
      p.1.val = rowCode I external.val x ∧ p.2.val = matchIdx external.val x.key ⦄ := by
  unfold join.internal_code
  apply WP.spec_bind (lookup_spec int I x.key hint hk)
  rintro ⟨ic, ifst⟩ ⟨hic, -⟩
  apply WP.spec_bind (lookup_spec ext external.val x.key hext hk)
  rintro ⟨ec, ef⟩ ⟨hec, hef⟩
  simp only at hic hec hef
  by_cases h0 : ec = 0#u32
  · simp only [h0]
    have hnil : withKey external.val x.key = [] := by
      rw [h0] at hec; simp at hec; exact hec
    apply WP.spec_bind (classify_internal_spec x ic ec none)
    intro c hc
    refine (WP.spec_ok _).2 ⟨?_, hef⟩
    rw [hc, rowCode, find_eq_head, hnil, hic, hec, hnil]; rfl
  · have hne : withKey external.val x.key ≠ [] := by
      intro h; apply h0; apply UScalar.eq_of_val_eq; rw [hec, h]; rfl
    have hlt := matchIdx_lt hne
    have hcast : (UScalar.cast UScalarTy.Usize ef).val = ef.val := by
      rw [UScalar.cast_val_eq]; apply Nat.mod_eq_of_lt
      have := ef.hBounds
      rcases System.Platform.numBits_eq with h2 | h2 <;> simp [UScalarTy.numBits, h2] at this ⊢ <;> omega
    step*
    apply WP.spec_bind (classify_internal_spec x ic ec _)
    intro c hc
    refine (WP.spec_ok _).2 ⟨?_, hef⟩
    rw [hc, rowCode, find_some hne, hic, hec]
    congr 3
    simp only [r_post]
    congr 1; rw [i_post, hcast, hef]

/-! ## The join -/

theorem map_take_succ {α β} (f : α → β) (l : List α) (i : Nat) (h : i < l.length) :
    (l.take (i + 1)).map f = (l.take i).map f ++ [f l[i]] := by
  rw [take_succ_eq l i h, List.map_append, List.map_singleton]

theorem join_loop0_spec (internal external : alloc.vec.Vec Record) (ext int : join.JoinTable)
    (hint : TInv int internal.val) (hext : TInv ext external.val) (hk : ∀ x ∈ internal.val, x.key ≠ EMPTY) :
    join.join_loop0 internal external ext int (alloc.vec.Vec.new U32) (alloc.vec.Vec.new U32) 0#usize ⦃ r =>
      r.1.val.map (·.val) = internal.val.map (rowCode internal.val external.val) ∧
      r.2.val.map (·.val) = internal.val.map (fun x => matchIdx external.val x.key) ⦄ := by
  unfold join.join_loop0
  apply loop.spec_decr_nat
    (measure := fun (x : alloc.vec.Vec U32 × alloc.vec.Vec U32 × Usize) => internal.val.length - x.2.2.val)
    (inv := fun x => x.2.2.val ≤ internal.val.length ∧
      x.1.val.map (·.val) = (internal.val.take x.2.2.val).map (rowCode internal.val external.val) ∧
      x.2.1.val.map (·.val) = (internal.val.take x.2.2.val).map (fun x => matchIdx external.val x.key))
  · rintro ⟨codes, matched, i⟩ ⟨hi, hc, hm⟩
    simp only at hi hc hm
    have hcl : codes.val.length = i.val := by
      have := congrArg List.length hc; simp at this; omega
    have hml : matched.val.length = i.val := by
      have := congrArg List.length hm; simp at this; omega
    have hmax := internal.property
    unfold join.join_loop0.body
    by_cases hlt : i.val < internal.val.length
    · have hlt' : i < alloc.vec.Vec.len internal := by scalar_tac
      simp only [hlt', if_true]
      step
      have hkr : r.key ≠ EMPTY := hk r (by rw [r_post]; exact List.getElem_mem _)
      apply WP.spec_bind (internal_code_spec r internal.val external int ext hint hext hkr)
      rintro ⟨c, m⟩ ⟨hcc, hmm⟩
      step*
      refine ⟨by omega, ?_, ?_, by omega⟩
      · rw [i2_post, codes1_post, map_take_succ _ _ _ hlt, ← r_post, ← hc]; simp [hcc]
      · rw [i2_post, matched1_post, map_take_succ _ _ _ hlt, ← r_post, ← hm]; simp [hmm]
    · have hlt' : ¬ i < alloc.vec.Vec.len internal := by scalar_tac
      simp only [hlt', if_false]
      refine (WP.spec_ok _).2 ?_
      rw [List.take_of_length_le (by omega)] at hc hm
      exact ⟨hc, hm⟩
  · simp

theorem join_loop1_spec (internal external : alloc.vec.Vec Record) (ext int : join.JoinTable)
    (hint : TInv int internal.val) (hext : TInv ext external.val) (hk : ∀ x ∈ external.val, x.key ≠ EMPTY) :
    join.join_loop1 external ext int (alloc.vec.Vec.new U32) 0#usize ⦃ r =>
      r.val.map (·.val) = external.val.map (extCode internal.val external.val) ⦄ := by
  unfold join.join_loop1
  apply loop.spec_decr_nat
    (measure := fun (x : alloc.vec.Vec U32 × Usize) => external.val.length - x.2.val)
    (inv := fun x => x.2.val ≤ external.val.length ∧
      x.1.val.map (·.val) = (external.val.take x.2.val).map (extCode internal.val external.val))
  · rintro ⟨codes, j⟩ ⟨hj, hc⟩
    simp only at hj hc
    have hcl : codes.val.length = j.val := by
      have := congrArg List.length hc; simp at this; omega
    have hmax := external.property
    unfold join.join_loop1.body
    by_cases hlt : j.val < external.val.length
    · have hlt' : j < alloc.vec.Vec.len external := by scalar_tac
      simp only [hlt', if_true]
      step
      have hkr : r.key ≠ EMPTY := hk r (by rw [r_post]; exact List.getElem_mem _)
      apply WP.spec_bind (lookup_spec int internal.val r.key hint hkr)
      rintro ⟨ic, _⟩ ⟨hic, -⟩
      apply WP.spec_bind (lookup_spec ext external.val r.key hext hkr)
      rintro ⟨ec, _⟩ ⟨hec, -⟩
      simp only at hic hec
      apply WP.spec_bind (classify_external_spec ic ec)
      intro e he
      step*
      refine ⟨by omega, ?_, by omega⟩
      rw [j1_post, ecodes1_post, map_take_succ _ _ _ hlt, ← r_post, ← hc]
      simp [he, extCode, hic, hec]
    · have hlt' : ¬ j < alloc.vec.Vec.len external := by scalar_tac
      simp only [hlt', if_false]
      refine (WP.spec_ok _).2 ?_
      rw [List.take_of_length_le (by omega)] at hc
      exact hc
  · simp

/-- **The single-thread hash join computes the reference's answer**, row for
row: every internal row gets `rowCode` (its key's counts on both sides and
the first external record with it) and the index of that first external
record (`NONE` if none), every external row gets `extCode`. Preconditions
are what `validate` checks (no key is `EMPTY`, fewer than `u32::MAX` rows a
side) plus that `4n + 32` fits in a `usize` (always, on 64-bit targets). -/
theorem join_eq_reference (I E : alloc.vec.Vec Record)
    (hkI : ∀ x ∈ I.val, x.key ≠ EMPTY) (hkE : ∀ y ∈ E.val, y.key ≠ EMPTY)
    (hnI : I.val.length < U32.max) (hnE : E.val.length < U32.max)
    (hcI : 4 * I.val.length + 32 ≤ Usize.max) (hcE : 4 * E.val.length + 32 ≤ Usize.max) :
    join.join I E ⦃ out =>
      out.internal.val.map (·.val) = I.val.map (rowCode I.val E.val) ∧
      out.matched.val.map (·.val) = I.val.map (fun x => matchIdx E.val x.key) ∧
      out.external.val.map (·.val) = E.val.map (extCode I.val E.val) ⦄ := by
  unfold join.join
  simp only [alloc.vec.Vec.with_capacity]
  apply WP.spec_bind (build_spec E hkE hnE hcE)
  intro ext hext
  apply WP.spec_bind (build_spec I hkI hnI hcI)
  intro int hint
  apply WP.spec_bind (join_loop0_spec I E ext int hint hext hkI)
  rintro ⟨codes, matched⟩ ⟨h1, h2⟩
  apply WP.spec_bind (join_loop1_spec I E ext int hint hext hkE)
  intro ecodes h3
  exact (WP.spec_ok _).2 ⟨h1, h2, h3⟩

/-- The same, per row: internal row `i` and external row `j`. -/
theorem join_rows (I E : alloc.vec.Vec Record)
    (hkI : ∀ x ∈ I.val, x.key ≠ EMPTY) (hkE : ∀ y ∈ E.val, y.key ≠ EMPTY)
    (hnI : I.val.length < U32.max) (hnE : E.val.length < U32.max)
    (hcI : 4 * I.val.length + 32 ≤ Usize.max) (hcE : 4 * E.val.length + 32 ≤ Usize.max) :
    join.join I E ⦃ out =>
      out.internal.val.length = I.val.length ∧ out.matched.val.length = I.val.length ∧
      out.external.val.length = E.val.length ∧
      (∀ i (h : i < I.val.length), ∀ h1 h2,
        (out.internal.val[i]'h1).val = rowCode I.val E.val (I.val[i]'h) ∧
        (out.matched.val[i]'h2).val = matchIdx E.val (I.val[i]'h).key) ∧
      (∀ j (h : j < E.val.length), ∀ h1, (out.external.val[j]'h1).val = extCode I.val E.val (E.val[j]'h)) ⦄ := by
  apply WP.spec_mono (join_eq_reference I E hkI hkE hnI hnE hcI hcE)
  rintro out ⟨h1, h2, h3⟩
  have l1 := congrArg List.length h1; have l2 := congrArg List.length h2; have l3 := congrArg List.length h3
  simp only [List.length_map] at l1 l2 l3
  refine ⟨l1, l2, l3, fun i h hi1 hi2 => ⟨?_, ?_⟩, fun j h hj1 => ?_⟩
  · have := congrArg (fun l => l[i]?) h1; simpa [List.getElem?_eq_getElem, hi1, h] using this
  · have := congrArg (fun l => l[i]?) h2; simpa [List.getElem?_eq_getElem, hi2, h] using this
  · have := congrArg (fun l => l[j]?) h3; simpa [List.getElem?_eq_getElem, hj1, h] using this

end payment_reconcile.Join
