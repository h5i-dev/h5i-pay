import Ledger.Commands
import Ledger.Apply
/-!
# Single-command facts about the ledger

How a write changes lookups and per-currency sums, and that one transfer
and one account opening keep the invariant. The batch command and the
theorems about every reachable state are in `Batch/Serializable.lean` and
`Ledger/Invariants.lean`.
-/
open Aeneas Aeneas.Std Result payment_ledger payment_ledger.Spec payment_ledger.Commands H5iAppLib

namespace payment_ledger.Theorems

/-! ## Lookups after a write -/

theorem findAcc_put (s : St) (a : Account) (n : Nat) :
    findAcc (applyWrite s (.PutAccount a)) n = if a.id.val = n then some a else findAcc s n := by
  simp only [findAcc, applyWrite]
  induction s.accounts with
  | nil => simp [upsert]
  | cons y ys ih =>
    by_cases h : y.id = a.id
    · by_cases ha : a.id.val = n <;> simp [upsert, h, ha]
    · simp only [upsert, h, if_false, List.find?_cons, ih]
      by_cases hy : y.id.val = n <;> by_cases ha : a.id.val = n <;> simp_all

theorem findTr_put (s : St) (t : Transfer) (n : Nat) :
    findTr (applyWrite s (.PutTransfer t)) n = if t.id.val = n then some t else findTr s n := by
  simp only [findTr, applyWrite]
  induction s.transfers with
  | nil => simp [upsert]
  | cons y ys ih =>
    by_cases h : y.id = t.id
    · by_cases ha : t.id.val = n <;> simp [upsert, h, ha]
    · simp only [upsert, h, if_false, List.find?_cons, ih]
      by_cases hy : y.id.val = n <;> by_cases ha : t.id.val = n <;> simp_all

theorem findAcc_putTr (s : St) (t : Transfer) (n : Nat) : findAcc (applyWrite s (.PutTransfer t)) n = findAcc s n := rfl
theorem findTr_putAcc (s : St) (a : Account) (n : Nat) : findTr (applyWrite s (.PutAccount a)) n = findTr s n := rfl
theorem findAcc_set (s : St) (l : Ledger) (n : Nat) : findAcc (applyWrite s (.SetLedger l)) n = findAcc s n := rfl
theorem findTr_set (s : St) (l : Ledger) (n : Nat) : findTr (applyWrite s (.SetLedger l)) n = findTr s n := rfl
theorem findAcc_owner (s : St) (c : OwnerCounter) (n : Nat) : findAcc (applyWrite s (.PutOwner c)) n = findAcc s n := rfl

theorem findAcc_mem {s : St} {n : Nat} {q : Account} (h : findAcc s n = some q) :
    q ∈ s.accounts ∧ q.id.val = n :=
  ⟨List.mem_of_find?_eq_some h, by simpa using List.find?_some h⟩

theorem findTr_mem {s : St} {n : Nat} {q : Transfer} (h : findTr s n = some q) :
    q ∈ s.transfers ∧ q.id.val = n :=
  ⟨List.mem_of_find?_eq_some h, by simpa using List.find?_some h⟩

/-! ## Sums after a write

Writing a row changes a sum over its table by the new row minus the row it
replaces (`H5iAppLib.sum_upsert`). -/

theorem accounts_put (s : St) (a : Account) (f : Account → Nat) :
    ((applyWrite s (.PutAccount a)).accounts.map f).sum + ((findAcc s a.id.val).map f).getD 0 =
      (s.accounts.map f).sum + f a := by
  have := sum_upsert (·.id) f a s.accounts
  simp only [findAcc, applyWrite]
  simpa using this

theorem transfers_put (s : St) (t : Transfer) (f : Transfer → Nat) :
    ((applyWrite s (.PutTransfer t)).transfers.map f).sum + ((findTr s t.id.val).map f).getD 0 =
      (s.transfers.map f).sum + f t := by
  have := sum_upsert (·.id) f t s.transfers
  simp only [findTr, applyWrite]
  simpa using this

/-! ## Opening an account -/

theorem findAcc_fresh {s : St} (hi : Inv s) : findAcc s s.next = none :=
  List.find?_eq_none.2 fun b hb => by have := hi.fresh b hb; simp; omega

theorem fresh_upsert {l : List Account} {N : Nat} (hl : ∀ b ∈ l, b.id.val < N) {a : Account} (ha : a.id.val < N) :
    ∀ b ∈ upsert (·.id) a l, b.id.val < N := by
  intro b hb
  rcases mem_upsert_of hb with rfl | hb
  · exact ha
  · exact hl b hb

theorem limited_upsert {l : List Account} (hl : ∀ b ∈ l, Limited b) {a : Account} (ha : Limited a) :
    ∀ b ∈ upsert (·.id) a l, Limited b := by
  intro b hb
  rcases mem_upsert_of hb with rfl | hb
  · exact ha
  · exact hl b hb

/-- Opening an account keeps the invariant: the new id is fresh, so the
empty account is appended and no sum changes. -/
theorem inv_open (s : St) (a : Account) (n : U64) (hinv : Inv s) (hid : a.id.val = s.next)
    (hn : n.val = s.next + 1) (hd : a.debits.val = 0) (hc : a.credits.val = 0) :
    Inv (applyAll s [.PutAccount a, .SetLedger ⟨n⟩]) := by
  simp only [applyAll, List.foldl_cons, List.foldl_nil]
  have hf : findAcc s a.id.val = none := by rw [hid]; exact findAcc_fresh hinv
  refine ⟨?_, fun c => ?_, ?_⟩
  · simp only [applyWrite]
    exact fresh_upsert (fun b hb => by have := hinv.fresh b hb; omega) (by omega)
  · have h1 := accounts_put s a (debitIn c)
    have h2 := accounts_put s a (creditIn c)
    simp only [hf, Option.map_none, Option.getD_none, Nat.add_zero] at h1 h2
    obtain ⟨b1, b2⟩ := hinv.balanced c
    simp only [debits, credits, journal, applyWrite, debitIn, creditIn, hd, hc, ite_self] at *
    omega
  · simp only [applyWrite]
    exact limited_upsert hinv.limits ⟨fun _ => by omega, fun _ => by omega⟩

/-! ## Posting a transfer -/

/-- What a posting does to the sums of currency `c`: all three grow by the
amount if the transfer is in `c`, and stay the same otherwise. -/
theorem post_sums (s : St) (t : TransferReq) (a b : Account) (d k op : U64) (c : Nat)
    (ha : findAcc s t.debit.val = some a) (hb : findAcc s t.credit.val = some b) (hne : t.debit ≠ t.credit)
    (htr : findTr s t.id.val = none) (hcur : a.currency = b.currency)
    (hd : d.val = a.debits.val + t.amount.val) (hk : k.val = b.credits.val + t.amount.val) :
    let s' := applyAll s [.PutAccount { a with debits := d }, .PutAccount { b with credits := k },
      .PutTransfer (journalRow t a.currency op)]
    let x := if a.currency.val = c then t.amount.val else 0
    debits c s' = debits c s + x ∧ credits c s' = credits c s + x ∧ journal c s' = journal c s + x := by
  intro s' x
  have hai := (findAcc_mem ha).2
  have hbi := (findAcc_mem hb).2
  have hne' : a.id.val ≠ b.id.val := by
    rw [hai, hbi]; intro e; exact hne ((u64_val_eq _ _).1 e)
  let a' : Account := { a with debits := d }
  let b' : Account := { b with credits := k }
  let s1 := applyWrite s (.PutAccount a')
  let s2 := applyWrite s1 (.PutAccount b')
  have fa : findAcc s a'.id.val = some a := by simpa [a', hai] using ha
  have fb : findAcc s1 b'.id.val = some b := by
    simp only [s1, findAcc_put]; simp only [a', b']; rw [if_neg hne', hbi]; exact hb
  have ft : findTr s2 (journalRow t a.currency op).id.val = none := by
    simp only [s2, s1, findTr_putAcc]; simpa [journalRow] using htr
  have d1 := accounts_put s a' (debitIn c)
  have d2 := accounts_put s1 b' (debitIn c)
  have c1 := accounts_put s a' (creditIn c)
  have c2 := accounts_put s1 b' (creditIn c)
  have j3 := transfers_put s2 (journalRow t a.currency op) (amountIn c)
  rw [fa] at d1 c1
  rw [fb] at d2 c2
  rw [ft] at j3
  simp only [Option.map_some, Option.map_none, Option.getD_some, Option.getD_none] at d1 d2 c1 c2 j3
  simp only [s', s2, s1, applyAll, List.foldl_cons, List.foldl_nil, debits, credits, journal, applyWrite] at d1 d2 c1 c2 j3 ⊢
  simp only [debitIn, creditIn, amountIn, a', b', journalRow, ← hcur] at d1 d2 c1 c2 j3 ⊢
  by_cases hc : a.currency.val = c <;> simp only [hc, if_true, if_false, x] at d1 d2 c1 c2 j3 ⊢ <;> omega

/-- Posting a transfer keeps the invariant. -/
theorem inv_post (s : St) (t : TransferReq) (a b : Account) (d k op : U64) (hinv : Inv s)
    (ha : findAcc s t.debit.val = some a) (hb : findAcc s t.credit.val = some b) (hne : t.debit ≠ t.credit)
    (htr : findTr s t.id.val = none) (hcur : a.currency = b.currency)
    (hd : d.val = a.debits.val + t.amount.val) (hk : k.val = b.credits.val + t.amount.val)
    (hld : a.no_overdraft = true → d.val ≤ a.credits.val) (hlc : b.no_overcredit = true → k.val ≤ b.debits.val) :
    Inv (applyAll s [.PutAccount { a with debits := d }, .PutAccount { b with credits := k },
      .PutTransfer (journalRow t a.currency op)]) := by
  have ma := findAcc_mem ha
  have mb := findAcc_mem hb
  refine ⟨?_, fun c => ?_, ?_⟩
  · simp only [applyAll, List.foldl_cons, List.foldl_nil, applyWrite]
    exact fresh_upsert (a := { b with credits := k }) (fresh_upsert (a := { a with debits := d }) hinv.fresh
      (hinv.fresh a ma.1)) (hinv.fresh b mb.1)
  · obtain ⟨e1, e2, e3⟩ := post_sums s t a b d k op c ha hb hne htr hcur hd hk
    obtain ⟨b1, b2⟩ := hinv.balanced c
    exact ⟨by rw [e1, e3, b1], by rw [e2, e3, b2]⟩
  · simp only [applyAll, List.foldl_cons, List.foldl_nil, applyWrite]
    have la := hinv.limits a ma.1
    have lb := hinv.limits b mb.1
    refine limited_upsert (a := { b with credits := k })
      (limited_upsert (a := { a with debits := d }) hinv.limits ⟨fun h => hld h, fun h => ?_⟩) ⟨fun h => ?_, fun h => hlc h⟩
    · have := la.2 h; simp only at *; omega
    · have := lb.1 h; simp only at *; omega

/-- One transfer command keeps the invariant. -/
theorem inv_transfer (actor : Principal) (s : Snapshot) (t : TransferReq) ws r
    (hinv : Inv (Snapshot.toSt s)) (h : transfer actor s t = ok (.Ok (ws, r))) :
    Inv (applyAll (Snapshot.toSt s) ws.val) := by
  rcases post_of_ok (transfer_spec actor s t) h ws r rfl with
    ⟨old, _, _, _, _, _, hws, _⟩ | ⟨_, _, htr, hne, a, b, ha, _, hb, hcur, _, d, k, hd, hk, hld, hlc, hws⟩
  · rw [hws]; exact hinv
  · rw [hws]; exact inv_post _ t a b d k _ hinv ha hb hne htr hcur hd hk hld hlc

end payment_ledger.Theorems
