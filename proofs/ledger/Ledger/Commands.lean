import Ledger.Spec
/-!
# What each ledger command does

For each command, what a successful run writes and the facts that made it
succeed, read off the extracted code.
-/
open Aeneas Aeneas.Std Result payment_ledger payment_ledger.Spec H5iAppLib

namespace payment_ledger.Commands

attribute [simp] u64_val_eq

/-! ## Helpers -/

@[step] theorem two_spec (a b : Write) : two a b ⦃ v => v.val = [a, b] ⦄ := by
  unfold two; step*

@[step] theorem three_spec (a b c : Write) : three a b c ⦃ v => v.val = [a, b, c] ⦄ := by
  unfold three; step*

@[step] theorem find_account_spec (v : alloc.vec.Vec Account) (id : U64) :
    find_account v id ⦃ o => o = v.val.find? (fun a => a.id.val = id.val) ⦄ := by
  unfold find_account find_account_loop
  apply WP.spec_mono (loop_search v.val (fun a => decide (a.id = id)) (fun o : Option Account => o)
    (fun _ a => some a) none _ ?_ 0#usize (by simp))
  · intro r hr; rw [search_find _ _ _ hr]; simp
  · intro j hj; unfold find_account_loop.body; h5i_step

@[step] theorem find_transfer_spec (v : alloc.vec.Vec Transfer) (id : U64) :
    find_transfer v id ⦃ o => o = v.val.find? (fun t => t.id.val = id.val) ⦄ := by
  unfold find_transfer find_transfer_loop
  apply WP.spec_mono (loop_search v.val (fun t => decide (t.id = id)) (fun o : Option Transfer => o)
    (fun _ t => some t) none _ ?_ 0#usize (by simp))
  · intro r hr; rw [search_find _ _ _ hr]; simp
  · intro j hj; unfold find_transfer_loop.body; h5i_step

@[step] theorem may_debit_spec (actor : Principal) (a : Account) :
    may_debit actor a ⦃ b => b = true ↔ (actor.operator = true ∨ a.owner = actor.user) ⦄ := by
  unfold may_debit
  cases h : actor.operator <;> simp

@[step] theorem same_transfer_spec (old : Transfer) (t : TransferReq) :
    same_transfer old t ⦃ b => b = true ↔
      (old.debit = t.debit ∧ old.credit = t.credit ∧ old.amount = t.amount ∧ old.code = t.code) ⦄ := by
  unfold same_transfer
  by_cases h1 : old.debit = t.debit <;> by_cases h2 : old.credit = t.credit <;>
    by_cases h3 : old.amount = t.amount <;> simp [h1, h2, h3]

theorem findAcc_eq (s : Snapshot) (id : U64) :
    s.accounts.val.find? (fun a => a.id.val = id.val) = findAcc (Snapshot.toSt s) id.val := rfl

theorem findTr_eq (s : Snapshot) (id : U64) :
    s.transfers.val.find? (fun t => t.id.val = id.val) = findTr (Snapshot.toSt s) id.val := rfl

/-- The journal row a posting writes. -/
def journalRow (t : TransferReq) (cur : U32) : Transfer :=
  { id := t.id, debit := t.debit, credit := t.credit, amount := t.amount, currency := cur, code := t.code }

/-- The arithmetic of a posting: same currency, the debit account's debits
and the credit account's credits both grow by the amount, within limits. -/
@[step] theorem post_spec (s : Snapshot) (t : TransferReq) (a b : Account) :
    post s t a b ⦃ r => ∀ ws rep, r = .Ok (ws, rep) →
      a.currency = b.currency ∧ rep = .Posted t.id ∧
      ∃ d c : U64, d.val = a.debits.val + t.amount.val ∧ c.val = b.credits.val + t.amount.val ∧
        (a.no_overdraft = true → d.val ≤ a.credits.val) ∧
        (b.no_overcredit = true → c.val ≤ b.debits.val) ∧
        ws.val = [.PutAccount { a with debits := d }, .PutAccount { b with credits := c },
          .PutTransfer (journalRow t a.currency)] ⦄ := by
  unfold post
  step*
  all_goals try (simp only [core.num.U64.MAX, U64.rMax] at *; scalar_tac)
  all_goals (intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h)
  all_goals (refine ⟨UScalar.eq_of_val_eq (by simp_all), rfl, debits, credits, debits_post, credits_post, ?_⟩; simp_all [journalRow]; try scalar_tac)

/-! ## What a successful command writes -/

/-- An account for `owner` (the caller, unless an operator opens it), with
the next id and nothing posted. -/
theorem open_spec (actor : Principal) (s : Snapshot) (owner : U64) (cur code : U32) (nod noc : Bool) :
    «open» actor s owner cur code nod noc ⦃ r => ∀ ws rep, r = .Ok (ws, rep) →
      (actor.operator = true ∨ owner = actor.user) ∧ ¬(nod = true ∧ noc = true) ∧
      ∃ n : U64, n.val = s.ledger.next_account.val + 1 ∧
        ws.val = [.PutAccount ⟨s.ledger.next_account, owner, cur, code, nod, noc, 0#u64, 0#u64⟩,
          .SetLedger ⟨n⟩] ⦄ := by
  unfold «open»
  cases hop : actor.operator <;> cases nod <;> cases noc <;> simp only [Bool.false_eq_true, ↓reduceIte] <;>
    step* <;> simp_all [core.num.U64.MAX, U64.rMax] <;> scalar_tac

/-- A transfer either recognises its replay (same id, same payload: no
writes) or posts a balanced pair between two different, existing accounts
of one currency, the debit side authorised. -/
theorem transfer_spec (actor : Principal) (s : Snapshot) (t : TransferReq) :
    transfer actor s t ⦃ r => ∀ ws rep, r = .Ok (ws, rep) →
      (∃ old, findTr (Snapshot.toSt s) t.id.val = some old ∧ old.debit = t.debit ∧ old.credit = t.credit ∧
        old.amount = t.amount ∧ old.code = t.code ∧ ws.val = [] ∧ rep = .Exists t.id) ∨
      (t.id.val ≠ 0 ∧ t.amount.val ≠ 0 ∧ findTr (Snapshot.toSt s) t.id.val = none ∧ t.debit ≠ t.credit ∧
        ∃ a b, findAcc (Snapshot.toSt s) t.debit.val = some a ∧ (actor.operator = true ∨ a.owner = actor.user) ∧
          findAcc (Snapshot.toSt s) t.credit.val = some b ∧ a.currency = b.currency ∧ rep = .Posted t.id ∧
          ∃ d c : U64, d.val = a.debits.val + t.amount.val ∧ c.val = b.credits.val + t.amount.val ∧
            (a.no_overdraft = true → d.val ≤ a.credits.val) ∧
            (b.no_overcredit = true → c.val ≤ b.debits.val) ∧
            ws.val = [.PutAccount { a with debits := d }, .PutAccount { b with credits := c },
              .PutTransfer (journalRow t a.currency)]) ⦄ := by
  unfold transfer
  step*
  · right
    obtain ⟨hcur, hrep, d, c, hd, hc, hld, hlc, hws⟩ := r_post _ _ r_post1
    refine ⟨by scalar_tac, by scalar_tac, by rw [← findTr_eq, ← o_post]; assumption, by assumption,
      a, b1, by rw [← findAcc_eq, ← o1_post]; assumption, by simp_all,
      by rw [← findAcc_eq, ← o2_post]; assumption, hcur, hrep, d, c, hd, hc, hld, hlc, hws⟩
  · intro ws rep h
    obtain ⟨rfl, rfl⟩ := ok_inj h
    left
    exact ⟨old, by rw [← findTr_eq, ← o_post]; assumption, by simp_all⟩

end payment_ledger.Commands
