import Ledger.Invariants
/-!
# The accounts' history (G1)

Every account row a command writes is followed, in the same write set, by
exactly one history entry for it: entry `n` of that account, where `n` is
the number of entries the account had, leaving it with `n + 1`; the entry
names the acting principal (user or operator, and its id), the
transaction's clock (`a.at`), and the account's debits and credits after
the change (`Logged`, `every_change_logged`). No history row is written
any other way.

In every reachable state the history is only appended to: a command's
entries are added after the existing ones, under keys `(account, n)` no
entry has, so every earlier entry stays, unchanged and in order, and keys
stay unique (`history_appended`). Batches are covered transfer by
transfer (`Batch.Serial`).
-/
open Aeneas Aeneas.Std Result payment_ledger payment_ledger.Spec payment_ledger.Commands
  payment_ledger.Theorems payment_ledger.Batch payment_ledger.Schema H5iAppLib

namespace payment_ledger.History

/-! ## Every change is logged -/

/-- Entry `e` records the change that left account row `x`, by `a`. -/
def EntryOk (a : Principal) (x : Account) (e : HistoryEntry) : Prop :=
  e.account = x.id ∧ e.n.val + 1 = x.changes.val ∧
  ((e.by_operator = false ∧ a.actor = .User e.actor) ∨ (e.by_operator = true ∧ a.actor = .Operator e.actor)) ∧
  e.at = a.at ∧ e.debits_after = x.debits ∧ e.credits_after = x.credits

/-- Every account write is immediately followed by its entry, and there is
no other history write. -/
inductive Logged (a : Principal) : List Write → Prop
  | nil : Logged a []
  | acc {x : Account} {e : HistoryEntry} {rest : List Write} :
      EntryOk a x e → Logged a rest → Logged a (.PutAccount x :: .PutHistory e :: rest)
  | other {w : Write} {rest : List Write} :
      (∀ x, w ≠ .PutAccount x) → (∀ e, w ≠ .PutHistory e) → Logged a rest → Logged a (w :: rest)

theorem Logged.append {a : Principal} {l1 l2 : List Write} (h1 : Logged a l1) (h2 : Logged a l2) :
    Logged a (l1 ++ l2) := by
  induction h1 with
  | nil => exact h2
  | acc he _ ih => exact .acc he ih
  | other hx he _ ih => exact .other hx he ih

def isAccW : Write → Bool
  | .PutAccount _ => true
  | _ => false

def histW : Write → Option HistoryEntry
  | .PutHistory e => some e
  | _ => none

@[simp] theorem histW_acc (x : Account) : histW (.PutAccount x) = none := rfl
@[simp] theorem histW_hist (e : HistoryEntry) : histW (.PutHistory e) = some e := rfl
@[simp] theorem histW_tr (t : Transfer) : histW (.PutTransfer t) = none := rfl
@[simp] theorem histW_set (l : Ledger) : histW (.SetLedger l) = none := rfl
@[simp] theorem histW_own (c : OwnerCounter) : histW (.PutOwner c) = none := rfl
@[simp] theorem isAccW_acc (x : Account) : isAccW (.PutAccount x) = true := rfl
@[simp] theorem isAccW_hist (e : HistoryEntry) : isAccW (.PutHistory e) = false := rfl

/-- As many entries as account writes. -/
theorem Logged.count {a : Principal} {l : List Write} (h : Logged a l) :
    (l.filterMap histW).length = (l.filter isAccW).length := by
  induction h with
  | nil => rfl
  | acc _ _ ih =>
    simp only [List.filterMap_cons, List.filter_cons, histW_acc, histW_hist, isAccW_acc, isAccW_hist, if_true,
      Bool.false_eq_true, if_false, List.length_cons, ih]
  | @other w rest hx he _ ih =>
    have h1 : histW w = none := by cases w <;> first | rfl | exact absurd rfl (he _)
    have h2 : isAccW w = false := by cases w <;> first | rfl | exact absurd rfl (hx _)
    rw [List.filterMap_cons_none (h := h1), List.filter_cons_of_neg (by simp [h2])]
    exact ih

theorem actor_cases (a : Principal) :
    (a.operator = false ∧ a.actor = .User (actorId a)) ∨ (a.operator = true ∧ a.actor = .Operator (actorId a)) := by
  rcases a with ⟨org, act, t⟩
  cases act <;> simp [actorId, Principal.operator]

theorem logged_post (actor : Principal) (t : TransferReq) (a b : Account) (d c ca cb : U64) (cur : U32) (op : U64)
    (hca : ca.val = a.changes.val + 1) (hcb : cb.val = b.changes.val + 1) :
    Logged actor (postWrites { a with debits := d, changes := ca } { b with credits := c, changes := cb } actor t a b cur op) := by
  have hact := actor_cases actor
  refine .acc ⟨rfl, by simp [entryOf, hca], by simpa [entryOf] using hact, rfl, rfl, rfl⟩ <|
    .acc ⟨rfl, by simp [entryOf, hcb], by simpa [entryOf] using hact, rfl, rfl, rfl⟩ <|
    .other (by simp) (by simp) .nil

theorem logged_transfer (a : Principal) (s : Snapshot) (t : TransferReq) ws r
    (h : transfer a s t = ok (.Ok (ws, r))) : Logged a ws.val := by
  rcases post_of_ok (transfer_spec a s t) h ws r rfl with
    ⟨_, _, _, _, _, _, hws, _⟩ | ⟨_, _, _, _, qa, qb, _, _, _, _, _, d, k, ca, cb, _, _, hca, hcb, _, _, hws⟩
  · rw [hws]; exact .nil
  · rw [hws]; exact logged_post a t qa qb d k ca cb _ _ hca hcb

theorem logged_serial {a : Principal} {s s' : Snapshot} {l : List TransferReq} {ws : List Write}
    (h : Serial a s l ws s') : Logged a ws := by
  induction h with
  | nil => exact .nil
  | cons ht _ _ _ ih => exact (logged_transfer _ _ _ _ _ ht).append ih

/-- **Every account change is logged once, naming the caller.** In the
write set of every successful command (a batch included), each account
row is immediately followed by exactly its history entry: entry `n` of
that account, which leaves it with `n + 1` entries, naming the acting
principal and the transaction's clock, with the account's debits and
credits after the change; no other history row is written. So there are
as many entries as account writes. -/
theorem every_change_logged (a : Principal) (s : Snapshot) (c : Command) ws r
    (hfit : HasRoom s) (h : transition a s c = .ok (.Ok (ws, r))) :
    Logged a ws.val ∧ (ws.val.filterMap histW).length = (ws.val.filter isAccW).length := by
  suffices hl : Logged a ws.val from ⟨hl, hl.count⟩
  cases c with
  | Open owner cur code nod noc =>
    obtain ⟨_, _, n, hn, q, m, _, _, hws⟩ := post_of_ok (open_spec a s owner cur code nod noc) h ws r rfl
    rw [hws]
    have hact := actor_cases a
    exact .acc ⟨rfl, by simp [entryOf], by simpa [entryOf] using hact, rfl, rfl, rfl⟩ <|
      .other (by simp) (by simp) <| .other (by simp) (by simp) .nil
  | Transfer t => exact logged_transfer a s t ws r h
  | Batch ts =>
    have hsz : Snapshot.size s + 5 * 8192 < Usize.max := by
      simp only [HasRoom, Snapshot.size, alloc.vec.Vec.length] at *; omega
    obtain ⟨s', hser⟩ := batch_serial a s ts ws r hsz h
    exact logged_serial hser
  | Account id => rw [reads_nothing a s _ ws r h trivial]; exact .nil
  | Accounts o f l => rw [reads_nothing a s _ ws r h trivial]; exact .nil
  | Transfers acc f l => rw [reads_nothing a s _ ws r h trivial]; exact .nil
  | AccountRange f l => rw [reads_nothing a s _ ws r h trivial]; exact .nil
  | TransferRange f l => rw [reads_nothing a s _ ws r h trivial]; exact .nil
  | History p f l => rw [reads_nothing a s _ ws r h trivial]; exact .nil

/-! ## The history is only appended to -/

def hkey (e : HistoryEntry) : U64 × U64 := (e.account, e.n)

/-- History keys are unique, and every entry belongs to an account and is
numbered below its count of entries. -/
def HInv (st : St) : Prop :=
  (st.history.map hkey).Nodup ∧ ∀ e ∈ st.history, ∃ q, findAcc st e.account.val = some q ∧ e.n.val < q.changes.val

/-- How many entries account `id` has (0 for an account not opened). -/
def prevChanges (st : St) (id : Nat) : Nat :=
  match findAcc st id with
  | some q => q.changes.val
  | none => 0

/-- An account write followed by its entry: the entry is appended under a
fresh key, and the invariant is kept. -/
theorem hinv_pair (st : St) (x : Account) (e : HistoryEntry) (hh : HInv st)
    (hacc : e.account = x.id) (hn : e.n.val = prevChanges st x.id.val) (hc : x.changes.val = e.n.val + 1) :
    HInv (applyAll st [.PutAccount x, .PutHistory e]) ∧
    (applyAll st [.PutAccount x, .PutHistory e]).history = st.history ++ [e] := by
  obtain ⟨hnd, hall⟩ := hh
  have hfresh : ∀ y ∈ st.history, (y.account, y.n) ≠ (e.account, e.n) := by
    intro y hy hk
    simp only [Prod.mk.injEq] at hk
    obtain ⟨q, hq, hlt⟩ := hall y hy
    rw [hk.1, hacc] at hq
    have : prevChanges st x.id.val = q.changes.val := by simp [prevChanges, hq]
    rw [hk.2] at hlt; omega
  have hhist : (applyAll st [.PutAccount x, .PutHistory e]).history = st.history ++ [e] := by
    simp only [applyAll, List.foldl_cons, List.foldl_nil, applyWrite]
    exact upsert_fresh _ _ _ hfresh
  refine ⟨⟨?_, ?_⟩, hhist⟩
  · rw [hhist, List.map_append, List.nodup_append]
    refine ⟨hnd, by simp, ?_⟩
    intro k hk k' hk'
    simp only [List.mem_map, List.mem_singleton] at hk hk'
    obtain ⟨y, hy, rfl⟩ := hk
    obtain ⟨z, rfl, rfl⟩ := hk'
    exact hfresh y hy
  · rw [hhist]
    intro y hy
    simp only [applyAll, List.foldl_cons, List.foldl_nil, findAcc_hist, findAcc_put]
    rcases List.mem_append.1 hy with hy | hy
    · obtain ⟨q, hq, hlt⟩ := hall y hy
      by_cases hk : x.id.val = y.account.val
      · rw [if_pos hk]
        refine ⟨x, rfl, ?_⟩
        have hq' : findAcc st x.id.val = some q := by rw [hk]; exact hq
        have : prevChanges st x.id.val = q.changes.val := by simp [prevChanges, hq']
        omega
      · rw [if_neg hk]; exact ⟨q, hq, hlt⟩
    · simp at hy; subst hy
      rw [if_pos (by rw [hacc])]; exact ⟨x, rfl, by omega⟩

theorem hinv_core {st st' : St} (h : HInv st) (hh : st'.history = st.history) (ha : st'.accounts = st.accounts) :
    HInv st' := by
  refine ⟨by rw [hh]; exact h.1, fun e he => ?_⟩
  rw [hh] at he
  obtain ⟨q, hq, hlt⟩ := h.2 e he
  exact ⟨q, by simp only [findAcc, ha] at hq ⊢; exact hq, hlt⟩

/-- A posting appends its two entries and keeps the invariant. -/
theorem hinv_post (st : St) (actor : Principal) (t : TransferReq) (a b : Account) (d c ca cb : U64) (op : U64)
    (hh : HInv st) (ha : findAcc st t.debit.val = some a) (hb : findAcc st t.credit.val = some b)
    (hne : t.debit ≠ t.credit) (hca : ca.val = a.changes.val + 1) (hcb : cb.val = b.changes.val + 1) :
    let ws := postWrites { a with debits := d, changes := ca } { b with credits := c, changes := cb } actor t a b a.currency op
    HInv (applyAll st ws) ∧ (applyAll st ws).history = st.history ++ ws.filterMap histW := by
  intro ws
  have hai := (findAcc_mem ha).2
  have hbi := (findAcc_mem hb).2
  have hne' : a.id.val ≠ b.id.val := by
    rw [hai, hbi]; intro e; exact hne ((H5iAppLib.u64_val_eq _ _).1 e)
  let a' : Account := { a with debits := d, changes := ca }
  let b' : Account := { b with credits := c, changes := cb }
  let ea := entryOf actor CMD_POST t.id t.code a.changes a a'
  let eb := entryOf actor CMD_POST t.id t.code b.changes b b'
  have h1 := hinv_pair st a' ea hh rfl (by simp only [ea, entryOf, a', prevChanges]; rw [hai, ha])
    (by simp [a', ea, entryOf, hca])
  let st1 := applyAll st [.PutAccount a', .PutHistory ea]
  have hb1 : findAcc st1 b.id.val = some b := by
    simp only [st1, applyAll, List.foldl_cons, List.foldl_nil, findAcc_hist, findAcc_put]
    rw [if_neg (by simpa [a'] using hne'), hbi]; exact hb
  have h2 := hinv_pair st1 b' eb h1.1 rfl (by simp only [eb, entryOf, b', prevChanges]; rw [hb1])
    (by simp [b', eb, entryOf, hcb])
  have e : ws = [.PutAccount a', .PutHistory ea] ++ [.PutAccount b', .PutHistory eb] ++ [.PutTransfer (journalRow t a.currency op)] := rfl
  have happ : ∀ (x : St) (l1 l2 : List Write), applyAll x (l1 ++ l2) = applyAll (applyAll x l1) l2 :=
    fun _ _ _ => by simp [applyAll, List.foldl_append]
  rw [e, happ, happ]
  refine ⟨hinv_core h2.1 rfl rfl, ?_⟩
  show (applyWrite (applyAll st1 [.PutAccount b', .PutHistory eb]) (.PutTransfer _)).history = _
  simp only [applyWrite]
  rw [h2.2]
  simp only [st1]
  rw [h1.2]
  simp

theorem hinv_transfer (a : Principal) (s : Snapshot) (t : TransferReq) ws r
    (hh : HInv (Snapshot.toSt s)) (h : transfer a s t = ok (.Ok (ws, r))) :
    HInv (applyAll (Snapshot.toSt s) ws.val) ∧
      (applyAll (Snapshot.toSt s) ws.val).history = (Snapshot.toSt s).history ++ ws.val.filterMap histW := by
  rcases post_of_ok (transfer_spec a s t) h ws r rfl with
    ⟨_, _, _, _, _, _, hws, _⟩ | ⟨_, _, _, hne, qa, qb, ha, _, hb, _, _, d, k, ca, cb, _, _, hca, hcb, _, _, hws⟩
  · rw [hws]; exact ⟨hh, by simp [applyAll]⟩
  · rw [hws]; exact hinv_post _ a t qa qb d k ca cb _ hh ha hb hne hca hcb

theorem hinv_serial {a : Principal} {s s' : Snapshot} {l : List TransferReq} {ws : List Write}
    (h : Serial a s l ws s') (hh : HInv (Snapshot.toSt s)) :
    HInv (applyAll (Snapshot.toSt s) ws) ∧
      (applyAll (Snapshot.toSt s) ws).history = (Snapshot.toSt s).history ++ ws.filterMap histW := by
  induction h with
  | nil s => exact ⟨hh, by simp [applyAll]⟩
  | cons ht _ hst _ ih =>
    obtain ⟨h1, e1⟩ := hinv_transfer _ _ _ _ _ hh ht
    have h1' := h1
    rw [← hst] at h1'
    obtain ⟨h2, e2⟩ := ih h1'
    rw [hst] at h2 e2
    have happ : ∀ (x : St) (l1 l2 : List Write), applyAll x (l1 ++ l2) = applyAll (applyAll x l1) l2 :=
      fun _ _ _ => by simp [applyAll, List.foldl_append]
    rw [happ]
    refine ⟨h2, ?_⟩
    rw [e2, e1, List.filterMap_append, List.append_assoc]

/-- Every successful command keeps the history invariant and appends its
entries, in order, after the existing ones. -/
theorem hinv_step (a : Principal) (s : Snapshot) (c : Command) ws r
    (hinv : Inv (Snapshot.toSt s)) (hh : HInv (Snapshot.toSt s)) (hfit : HasRoom s)
    (h : transition a s c = .ok (.Ok (ws, r))) :
    HInv (applyAll (Snapshot.toSt s) ws.val) ∧
      (applyAll (Snapshot.toSt s) ws.val).history = (Snapshot.toSt s).history ++ ws.val.filterMap histW := by
  cases c with
  | Open owner cur code nod noc =>
    obtain ⟨_, _, n, hn, q, m, _, _, hws⟩ := post_of_ok (open_spec a s owner cur code nod noc) h ws r rfl
    rw [hws]
    let A : Account := ⟨s.ledger.next_account, owner, cur, code, nod, noc, 0#u64, 0#u64, q, 1#u64⟩
    have hf : findAcc (Snapshot.toSt s) A.id.val = none := findAcc_fresh hinv
    have h1 := hinv_pair (Snapshot.toSt s) A (entryOf a CMD_OPEN 0#u64 code 0#u64 A A) hh rfl
      (by simp [prevChanges, hf, entryOf]) (by simp [A, entryOf])
    have e : ∀ x y z w : Write, [x, y, z, w] = [x, y] ++ [z, w] := fun _ _ _ _ => rfl
    have happ : ∀ (x : St) (l1 l2 : List Write), applyAll x (l1 ++ l2) = applyAll (applyAll x l1) l2 :=
      fun _ _ _ => by simp [applyAll, List.foldl_append]
    rw [e, happ]
    refine ⟨hinv_core (hinv_core h1.1 rfl rfl) rfl rfl, ?_⟩
    have hq : ∀ x : St, (applyAll x [Write.SetLedger ⟨n⟩, Write.PutOwner ⟨owner, m⟩]).history = x.history :=
      fun _ => rfl
    rw [hq, h1.2]
    simp [A]
  | Transfer t => exact hinv_transfer a s t ws r hh h
  | Batch ts =>
    have hsz : Snapshot.size s + 5 * 8192 < Usize.max := by
      simp only [HasRoom, Snapshot.size, alloc.vec.Vec.length] at *; omega
    obtain ⟨s', hser⟩ := batch_serial a s ts ws r hsz h
    exact hinv_serial hser hh
  | Account id => rw [reads_nothing a s _ ws r h trivial]; exact ⟨hh, by simp [applyAll]⟩
  | Accounts o f l => rw [reads_nothing a s _ ws r h trivial]; exact ⟨hh, by simp [applyAll]⟩
  | Transfers acc f l => rw [reads_nothing a s _ ws r h trivial]; exact ⟨hh, by simp [applyAll]⟩
  | AccountRange f l => rw [reads_nothing a s _ ws r h trivial]; exact ⟨hh, by simp [applyAll]⟩
  | TransferRange f l => rw [reads_nothing a s _ ws r h trivial]; exact ⟨hh, by simp [applyAll]⟩
  | History p f l =>
    rw [reads_nothing a s _ ws r h trivial]; exact ⟨hh, by simp [applyAll]⟩

theorem init_hinv : HInv init := by simp [HInv, init]

/-- The history invariant holds in every reachable state. -/
theorem reachable_hinv {s : St} (h : Reachable s) : HInv s := by
  induction h with
  | init => exact init_hinv
  | step hr hfit ht ih => exact (hinv_step _ _ _ _ _ (reachable_inv hr) ih hfit ht).1

/-- **The history is only appended to.** In every reachable state, a
successful command (a batch included) leaves the history as the old
history followed by the entries it wrote, in order: every earlier entry
stays, unchanged and in place, and the keys `(account, n)` of all entries
stay unique, so no entry is ever overwritten. -/
theorem history_appended (a : Principal) (s : Snapshot) (c : Command) ws r
    (hr : Reachable (Snapshot.toSt s)) (hfit : HasRoom s) (h : transition a s c = .ok (.Ok (ws, r))) :
    (applyAll (Snapshot.toSt s) ws.val).history = (Snapshot.toSt s).history ++ ws.val.filterMap histW ∧
      ((applyAll (Snapshot.toSt s) ws.val).history.map hkey).Nodup := by
  obtain ⟨h1, h2⟩ := hinv_step a s c ws r (reachable_inv hr) (reachable_hinv hr) hfit h
  exact ⟨h2, h1.1⟩

end payment_ledger.History
