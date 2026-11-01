import Batch.Serializable
/-!
# The ledger's theorems

Every state the ledger can reach, by any sequence of successful commands
from any callers (batches included), satisfies `Spec.Inv`. From it:

* `conservation`: in every currency, total debits = total credits;
* `journal_balanced`: and both equal the journal's total, so every balance
  is explained by committed transfers;
* `no_overdraft` / `no_overcredit`: constrained accounts stay within limits;
* `posting_balanced`: each committed transfer posts one debit and one credit
  of the same amount in the same currency, to two different accounts;
* `replay_is_noop`: re-sending a committed transfer changes nothing;
* `replay_needs_debit_right`: only those who may debit get `Exists` (SEC-03);
* `only_owner_or_operator_debits`: only an account's owner or the tenant's
  operator can increase its debits (move money out of it).
-/
open Aeneas Aeneas.Std Result payment_ledger payment_ledger.Spec payment_ledger.Commands
  payment_ledger.Theorems payment_ledger.ApplyProofs payment_ledger.Batch payment_ledger.Schema H5iAppLib

namespace payment_ledger.Theorems

/-! ## Reads write nothing -/

theorem read_account_spec (a : Principal) (s : Snapshot) (id : U64) :
    read_account a s id ⦃ r => ∀ ws rep, r = .Ok (ws, rep) → ws.val = [] ⦄ := by
  unfold read_account
  step*

theorem reads_nothing (a : Principal) (s : Snapshot) (c : Command) ws r
    (h : transition a s c = ok (.Ok (ws, r)))
    (hc : c = .Accounts ∨ c = .Transfers ∨ ∃ id, c = .Account id) : ws.val = [] := by
  rcases hc with rfl | rfl | ⟨id, rfl⟩
  · simp only [transition] at h
    obtain ⟨v, -, hv⟩ := bind_tc_eq_ok.1 h
    simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at hv
    rw [← hv.1]; rfl
  · simp only [transition] at h
    split at h
    · obtain ⟨v, -, hv⟩ := bind_tc_eq_ok.1 h
      simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at hv
      rw [← hv.1]; rfl
    · simp at h
  · exact post_of_ok (read_account_spec a s id) h ws r rfl

/-! ## The invariant -/

theorem inv_serial {a : Principal} {s s' : Snapshot} {l : List TransferReq} {ws : List Write}
    (h : Serial a s l ws s') (hinv : Inv (Snapshot.toSt s)) : Inv (applyAll (Snapshot.toSt s) ws) := by
  induction h with
  | nil s => exact hinv
  | cons ht _ hst _ ih =>
    have h1 := inv_transfer _ _ _ _ _ hinv ht
    rw [← hst] at h1
    have := ih h1
    rw [hst] at this
    simpa [applyAll, List.foldl_append] using this

/-- Successful commands keep the invariant. -/
theorem inv_preserved (a : Principal) (s : Snapshot) (c : Command) ws r
    (hinv : Inv (Snapshot.toSt s)) (hfit : HasRoom s) (h : transition a s c = .ok (.Ok (ws, r))) :
    Inv (applyAll (Snapshot.toSt s) ws.val) := by
  cases c with
  | Open owner cur code nod noc =>
    obtain ⟨_, _, n, hn, hws⟩ := post_of_ok (open_spec a s owner cur code nod noc) h ws r rfl
    rw [hws]
    exact inv_open _ _ n hinv rfl hn rfl rfl
  | Transfer t => exact inv_transfer a s t ws r hinv h
  | Batch ts =>
    have hsz : Snapshot.size s + 3 * 8192 < Usize.max := by
      simp only [HasRoom, Snapshot.size, alloc.vec.Vec.length] at *; omega
    obtain ⟨s', hser⟩ := batch_serial a s ts ws r hsz h
    exact inv_serial hser hinv
  | Account id => rw [reads_nothing a s _ ws r h (.inr (.inr ⟨id, rfl⟩))]; exact hinv
  | Accounts => rw [reads_nothing a s _ ws r h (.inl rfl)]; exact hinv
  | Transfers => rw [reads_nothing a s _ ws r h (.inr (.inl rfl))]; exact hinv

theorem init_inv : Inv init := by
  refine ⟨by simp [init], fun c => ?_, by simp [init]⟩
  simp [init, debits, credits, journal]

/-- The invariant holds in every state the ledger can reach. -/
theorem reachable_inv {s : St} (h : Reachable s) : Inv s := by
  induction h with
  | init => exact init_inv
  | step _ hfit ht ih => exact inv_preserved _ _ _ _ _ ih hfit ht

/-! ## The theorems -/

/-- **Conservation of value.** In every reachable state and every currency,
the debits posted to all accounts equal the credits posted to all accounts. -/
theorem conservation {s : St} (h : Reachable s) (c : Nat) : debits c s = credits c s := by
  obtain ⟨h1, h2⟩ := (reachable_inv h).balanced c
  rw [h1, h2]

/-- **Balanced journal.** Both sides also equal the total of the committed
transfers in that currency: balances are exactly what the journal explains. -/
theorem journal_balanced {s : St} (h : Reachable s) (c : Nat) :
    debits c s = journal c s ∧ credits c s = journal c s :=
  (reachable_inv h).balanced c

/-- **Non-negative available balance.** A `no_overdraft` account never has
more debits than credits. -/
theorem no_overdraft {s : St} (h : Reachable s) {q : Account} (hq : q ∈ s.accounts)
    (hf : q.no_overdraft = true) : q.debits.val ≤ q.credits.val :=
  ((reachable_inv h).limits q hq).1 hf

/-- A `no_overcredit` account never has more credits than debits. -/
theorem no_overcredit {s : St} (h : Reachable s) {q : Account} (hq : q ∈ s.accounts)
    (hf : q.no_overcredit = true) : q.credits.val ≤ q.debits.val :=
  ((reachable_inv h).limits q hq).2 hf

/-- **Balanced postings.** A committed transfer debits `amount` from one
account and credits `amount` to a different account of the same currency,
and records exactly that in the journal; or it is a replay and writes
nothing. -/
theorem posting_balanced (a : Principal) (s : Snapshot) (t : TransferReq) ws r
    (h : transition a s (.Transfer t) = ok (.Ok (ws, r))) :
    ws.val = [] ∨
      ∃ qa qb d k, findAcc (Snapshot.toSt s) t.debit.val = some qa ∧ findAcc (Snapshot.toSt s) t.credit.val = some qb ∧
        t.debit ≠ t.credit ∧ qa.currency = qb.currency ∧
        d.val = qa.debits.val + t.amount.val ∧ k.val = qb.credits.val + t.amount.val ∧
        ws.val = [.PutAccount { qa with debits := d }, .PutAccount { qb with credits := k },
          .PutTransfer ⟨t.id, t.debit, t.credit, t.amount, qa.currency, t.code⟩] := by
  rcases post_of_ok (transfer_spec a s t) h ws r rfl with
    ⟨_, _, _, _, _, _, hws, _⟩ | ⟨_, _, _, hne, qa, qb, ha, _, hb, hcur, _, d, k, hd, hk, _, _, hws⟩
  · exact .inl hws
  · exact .inr ⟨qa, qb, d, k, ha, hb, hne, hcur, hd, hk, hws⟩

/-! ## Idempotency -/

/-- A transfer whose id is in the journal with the same payload is a
replay, for a caller who may debit its debit account. -/
theorem replay_ok (a : Principal) (s : Snapshot) (t : TransferReq) (old : Transfer) (acc : Account)
    (hid : t.id.val ≠ 0) (hamt : t.amount.val ≠ 0) (hne : t.debit ≠ t.credit)
    (hacc : findAcc (Snapshot.toSt s) t.debit.val = some acc) (hauth : a.operator = true ∨ acc.owner = a.user)
    (hf : findTr (Snapshot.toSt s) t.id.val = some old)
    (h1 : old.debit = t.debit) (h2 : old.credit = t.credit) (h3 : old.amount = t.amount) (h4 : old.code = t.code) :
    transition a s (.Transfer t) = ok (.Ok (alloc.vec.Vec.new Write, .Exists t.id)) := by
  have hid' : t.id ≠ 0#u64 := fun e => hid (by simp [e])
  have hamt' : t.amount ≠ 0#u64 := fun e => hamt (by simp [e])
  have ho : find_transfer s.transfers t.id = ok (some old) := by
    rw [eq_ok_of_spec (find_transfer_spec _ _), findTr_eq, hf]
  have ha : find_account s.accounts t.debit = ok (some acc) := by
    rw [eq_ok_of_spec (find_account_spec _ _), findAcc_eq, hacc]
  have hm : may_debit a acc = ok true := by
    unfold may_debit; rcases hauth with h | h <;> simp [h]
  have hs : same_transfer old t = ok true := by
    unfold same_transfer; simp [h1, h2, h3, h4]
  simp only [transition]
  unfold transfer
  simp [hid', hamt', hne, ho, ha, hm, hs]

/-- **Idempotency.** Re-sending a committed transfer (same id, same payload)
to the state it produced, by a caller who may debit its debit account (its
owner or an operator), is accepted, writes nothing and reports `Exists`.
Since SEC-03 a caller without that right gets `Forbidden`, as for an
unused id, so the replay path reveals nothing to it. -/
theorem replay_is_noop (a a' : Principal) (s s' : Snapshot) (t : TransferReq) ws r
    (h : transition a s (.Transfer t) = ok (.Ok (ws, r)))
    (hs' : Snapshot.toSt s' = applyAll (Snapshot.toSt s) ws.val)
    (hauth : a'.operator = true ∨ ∀ acc, findAcc (Snapshot.toSt s) t.debit.val = some acc → acc.owner = a'.user) :
    transition a' s' (.Transfer t) = ok (.Ok (alloc.vec.Vec.new Write, .Exists t.id)) := by
  have hT : transfer a s t = ok (.Ok (ws, r)) := h
  have hid : t.id.val ≠ 0 ∧ t.amount.val ≠ 0 := by
    unfold transfer at hT
    by_cases e1 : t.id = 0#u64
    · simp [e1] at hT
    · by_cases e2 : t.amount = 0#u64
      · simp [e1, e2] at hT
      · exact ⟨fun e => e1 (by scalar_tac), fun e => e2 (by scalar_tac)⟩
  rcases post_of_ok (transfer_spec a s t) h ws r rfl with
    ⟨old, hf, h1, h2, h3, h4, hws, _, hne, acc, hacc, _⟩ |
    ⟨_, _, htr, hne, qa, qb, ha, _, hb, hcur, _, d, k, hd, hk, _, _, hws⟩
  · rw [hws] at hs'
    have hf' : findTr (Snapshot.toSt s') t.id.val = some old := by rw [hs']; exact hf
    have hacc' : findAcc (Snapshot.toSt s') t.debit.val = some acc := by rw [hs']; exact hacc
    exact replay_ok a' s' t old acc hid.1 hid.2 hne hacc' (hauth.imp id (fun h => h acc hacc)) hf' h1 h2 h3 h4
  · have hf' : findTr (Snapshot.toSt s') t.id.val = some (Commands.journalRow t qa.currency) := by
      rw [hs', hws]
      simp only [applyAll, List.foldl_cons, List.foldl_nil, findTr_put, findTr_putAcc, Commands.journalRow]
      simp
    have hqa : qa.id.val = t.debit.val := by simpa using (List.find?_some ha)
    have hqb : qb.id.val = t.credit.val := by simpa using (List.find?_some hb)
    have hacc' : findAcc (Snapshot.toSt s') t.debit.val = some { qa with debits := d } := by
      rw [hs', hws]
      simp only [applyAll, List.foldl_cons, List.foldl_nil, findAcc_putTr, findAcc_put]
      have : qb.id.val ≠ t.debit.val := by
        intro e; apply hne; apply UScalar.eq_of_val_eq; rw [← hqb, e]
      simp [this, hqa]
    exact replay_ok a' s' t _ { qa with debits := d } hid.1 hid.2 hne hacc' (hauth.imp id (fun h => h qa ha)) hf'
      rfl rfl rfl rfl

/-- **Replays answer only to those who may debit (SEC-03).** A transfer
command that reports `Exists` was sent by the debit account's owner or an
operator; anyone else gets the refusal an unused id gets. -/
theorem replay_needs_debit_right (a : Principal) (s : Snapshot) (t : TransferReq) ws
    (h : transition a s (.Transfer t) = ok (.Ok (ws, .Exists t.id))) :
    ∃ acc, findAcc (Snapshot.toSt s) t.debit.val = some acc ∧ (a.operator = true ∨ acc.owner = a.user) := by
  rcases post_of_ok (transfer_spec a s t) h ws _ rfl with
    ⟨_, _, _, _, _, _, _, _, _, acc, hacc, hm⟩ | ⟨_, _, _, _, _, _, _, _, _, _, hrep, _⟩
  · exact ⟨acc, hacc, hm⟩
  · cases hrep

/-! ## Only owners and operators move money out -/

/-- What one command does to an existing account: it is still there, with
the same owner, and its debits did not decrease; if they increased, the
caller owns it or is the operator. -/
def Guarded (a : Principal) (st st' : St) : Prop :=
  ∀ n q, findAcc st n = some q → ∃ p, findAcc st' n = some p ∧ p.owner = q.owner ∧
    q.debits.val ≤ p.debits.val ∧ (q.debits.val < p.debits.val → a.operator = true ∨ q.owner = a.user)

theorem Guarded.refl (a : Principal) (st : St) : Guarded a st st :=
  fun _ q h => ⟨q, h, rfl, le_refl _, fun h => absurd h (lt_irrefl _)⟩

theorem Guarded.trans {a : Principal} {s1 s2 s3 : St} (h1 : Guarded a s1 s2) (h2 : Guarded a s2 s3) :
    Guarded a s1 s3 := by
  intro n q hq
  obtain ⟨p, hp, ho, hle, himp⟩ := h1 n q hq
  obtain ⟨p', hp', ho', hle', himp'⟩ := h2 n p hp
  refine ⟨p', hp', ho'.trans ho, le_trans hle hle', fun hlt => ?_⟩
  by_cases e : q.debits.val < p.debits.val
  · exact himp e
  · rw [← ho]; exact himp' (by omega)

theorem guarded_transfer (a : Principal) (s : Snapshot) (t : TransferReq) ws r
    (h : transfer a s t = ok (.Ok (ws, r))) :
    Guarded a (Snapshot.toSt s) (applyAll (Snapshot.toSt s) ws.val) := by
  rcases post_of_ok (transfer_spec a s t) h ws r rfl with
    ⟨_, _, _, _, _, _, hws, _⟩ | ⟨_, _, _, hne, qa, qb, ha, hauth, hb, _, _, d, k, hd, hk, _, _, hws⟩
  · rw [hws]; exact Guarded.refl _ _
  · rw [hws]
    intro n q hq
    have hai := (findAcc_mem ha).2
    have hbi := (findAcc_mem hb).2
    have hne' : t.debit.val ≠ t.credit.val := fun e => hne ((H5iAppLib.u64_val_eq _ _).1 e)
    simp only [applyAll, List.foldl_cons, List.foldl_nil, findAcc_putTr, findAcc_put]
    by_cases hnb : qb.id.val = n
    · -- the credit account: debits unchanged
      have : q = qb := by rw [← hnb, hbi] at hq; rw [hb] at hq; exact (Option.some.inj hq).symm
      subst this
      simp only [hnb, if_true]
      exact ⟨_, rfl, rfl, le_refl _, fun h => absurd h (lt_irrefl _)⟩
    · simp only [hnb, if_false]
      by_cases hna : qa.id.val = n
      · have : q = qa := by rw [← hna, hai] at hq; rw [ha] at hq; exact (Option.some.inj hq).symm
        subst this
        simp only [hna, if_true]
        exact ⟨_, rfl, rfl, by simp only at *; omega, fun _ => hauth⟩
      · simp only [hna, if_false]
        exact ⟨q, hq, rfl, le_refl _, fun h => absurd h (lt_irrefl _)⟩

theorem guarded_serial {a : Principal} {s s' : Snapshot} {l : List TransferReq} {ws : List Write}
    (h : Serial a s l ws s') : Guarded a (Snapshot.toSt s) (applyAll (Snapshot.toSt s) ws) := by
  induction h with
  | nil s => exact Guarded.refl _ _
  | cons ht _ hst _ ih =>
    have g1 := guarded_transfer _ _ _ _ _ ht
    rw [← hst] at g1
    have := Guarded.trans g1 ih
    rw [hst] at this
    simpa [applyAll, List.foldl_append] using this

theorem guarded (a : Principal) (s : Snapshot) (c : Command) ws r
    (hinv : Inv (Snapshot.toSt s)) (hfit : HasRoom s) (h : transition a s c = .ok (.Ok (ws, r))) :
    Guarded a (Snapshot.toSt s) (applyAll (Snapshot.toSt s) ws.val) := by
  cases c with
  | Open owner cur code nod noc =>
    obtain ⟨_, _, n, hn, hws⟩ := post_of_ok (open_spec a s owner cur code nod noc) h ws r rfl
    rw [hws]
    intro m q hq
    have hfresh : (Snapshot.toSt s).next ≠ m := by
      intro e; rw [← e, findAcc_fresh hinv] at hq; simp at hq
    simp only [applyAll, List.foldl_cons, List.foldl_nil, findAcc_set, findAcc_put]
    simp only [Snapshot.toSt] at hfresh
    rw [if_neg hfresh]
    exact ⟨q, hq, rfl, le_refl _, fun h => absurd h (lt_irrefl _)⟩
  | Transfer t => exact guarded_transfer a s t ws r h
  | Batch ts =>
    have hsz : Snapshot.size s + 3 * 8192 < Usize.max := by
      simp only [HasRoom, Snapshot.size, alloc.vec.Vec.length] at *; omega
    obtain ⟨s', hser⟩ := batch_serial a s ts ws r hsz h
    exact guarded_serial hser
  | Account id => rw [reads_nothing a s _ ws r h (.inr (.inr ⟨id, rfl⟩))]; exact Guarded.refl _ _
  | Accounts => rw [reads_nothing a s _ ws r h (.inl rfl)]; exact Guarded.refl _ _
  | Transfers => rw [reads_nothing a s _ ws r h (.inr (.inl rfl))]; exact Guarded.refl _ _

/-- **Authorization of debits.** In every reachable state, if a command
(a batch included) increases the debits of an account, the caller is the
account's owner or the tenant's operator. -/
theorem only_owner_or_operator_debits (a : Principal) (s : Snapshot) (c : Command) ws r
    (hr : Reachable (Snapshot.toSt s)) (hfit : HasRoom s) (h : transition a s c = .ok (.Ok (ws, r)))
    {n : Nat} {q p : Account} (hq : findAcc (Snapshot.toSt s) n = some q)
    (hp : findAcc (applyAll (Snapshot.toSt s) ws.val) n = some p)
    (hlt : q.debits.val < p.debits.val) : a.operator = true ∨ q.owner = a.user := by
  obtain ⟨p', hp', _, _, himp⟩ := guarded a s c ws r (reachable_inv hr) hfit h n q hq
  rw [hp] at hp'
  cases hp'
  exact himp hlt

end payment_ledger.Theorems
