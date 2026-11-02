import Ledger.Plan
import Batch.Serializable
/-!
# Frame theorem: the ledger's read plans load enough

`frame`: for every command with a read plan (`readsA a c = some plan`, which
the extracted `reads` computes by `Plan.reads_abs`), `transition` on
`Snapshot.load plan s` (what `LedgerStore::load_for` decodes, `decode_load`)
gives exactly the result it gives on the whole tenant `s`.

Each plan keeps, of every table it reads, all rows a lookup of the kernel
can return (`H5iAppLib.Query.find?_slice`, `filter_slice`):

* `Open` reads only the counter row;
* `Transfer` reads its transfer id (replay detection) and its two accounts;
* a linked `Batch` of at most 64 transfers reads every transfer id and
  account it names. Each transfer runs on the state the earlier ones left;
  those states keep the same accounts and ids (`keeps_applyAll`: a write of a
  kept row keeps the slice a slice), so every transfer sees what it would
  have seen on the whole tenant (`batch_loop_congr`). The only assumption is
  `HasRoom s` (no table within 5 · 8192 rows of `usize::MAX`), under which
  `apply` cannot overflow on the whole tenant either;
* `Account` reads its account; `Accounts` every account for an operator,
  else the caller's own; `Transfers` the journal for an operator, else
  nothing (the command is refused without reading).
-/
open Aeneas Aeneas.Std Result payment_ledger payment_ledger.Spec payment_ledger.Commands
  payment_ledger.Schema payment_ledger.Plan payment_ledger.ApplyProofs H5iAppLib H5iAppLib.Query

namespace payment_ledger.Frame

open Classical

/-! ## Lookups as equations -/

theorem find_account_eq (v : alloc.vec.Vec Account) (id : U64) :
    find_account v id = ok (v.val.find? (fun a => a.id.val = id.val)) :=
  eq_ok_of_spec (find_account_spec v id)

theorem find_transfer_eq (v : alloc.vec.Vec Transfer) (id : U64) :
    find_transfer v id = ok (v.val.find? (fun t => t.id.val = id.val)) :=
  eq_ok_of_spec (find_transfer_spec v id)

theorem may_debit_eq (actor : Principal) (a : Account) :
    may_debit actor a = ok (actor.operator || decide (a.owner = actor.user)) := by
  unfold may_debit; simp only [is_operator_eq, user_of_eq, bind_tc_ok, bind_ok]; cases actor.operator <;> simp

theorem post_irrel (actor : Principal) (s s' : Snapshot) (t : TransferReq) (a b : Account) (op : U64) :
    post actor s' t a b op = post actor s t a b op := by
  unfold post; rfl

/-- A transfer reads its id in the journal and its two accounts, nothing else. -/
theorem transfer_congr (actor : Principal) (s s' : Snapshot) (t : TransferReq)
    (hT : s'.transfers.val.find? (fun x => x.id.val = t.id.val) = s.transfers.val.find? (fun x => x.id.val = t.id.val))
    (hD : s'.accounts.val.find? (fun x => x.id.val = t.debit.val) = s.accounts.val.find? (fun x => x.id.val = t.debit.val))
    (hC : s'.accounts.val.find? (fun x => x.id.val = t.credit.val) = s.accounts.val.find? (fun x => x.id.val = t.credit.val)) :
    transfer actor s' t = transfer actor s t := by
  unfold transfer
  simp only [find_transfer_eq, find_account_eq, hT, hD, hC, bind_ok, bind_tc_ok, post_irrel actor s s']

theorem owner_next_eq (v : alloc.vec.Vec OwnerCounter) (o : U64) : owner_next v o = ok (ownerNext v.val o) :=
  eq_ok_of_spec (owner_next_spec v o)

theorem open_congr (actor : Principal) (s s' : Snapshot) (h : s'.ledger = s.ledger) (owner : U64) (cur code : U32)
    (nod noc : Bool)
    (ho : s'.owners.val.find? (fun c => decide (c.owner = owner)) = s.owners.val.find? (fun c => decide (c.owner = owner))) :
    «open» actor s' owner cur code nod noc = «open» actor s owner cur code nod noc := by
  unfold «open»; rw [h]; simp only [owner_next_eq, ownerNext, ho]

theorem read_account_congr (actor : Principal) (s s' : Snapshot) (id : U64)
    (h : s'.accounts.val.find? (fun x => x.id.val = id.val) = s.accounts.val.find? (fun x => x.id.val = id.val)) :
    read_account actor s' id = read_account actor s id := by
  unfold read_account
  simp only [find_account_eq, h]

/-! ## An `Accounts` page is a filter -/

def rangeP (o f e : U64) (x : Account) : Bool := decide (x.owner = o) && decide (f ≤ x.seq) && decide (x.seq < e)

def rangeStepL (o f e : U64) (acc : List Account) (x : Account) : List Account :=
  if rangeP o f e x then acc ++ [x] else acc

theorem foldl_rangeStepL (o f e : U64) (l acc : List Account) :
    l.foldl (rangeStepL o f e) acc = acc ++ l.filter (rangeP o f e) := by
  induction l generalizing acc with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, rangeStepL, List.filter_cons]
    split <;> simp

theorem owner_range_spec (s : Snapshot) (o f e : U64) :
    owner_range s o f e ⦃ v => v.val = s.accounts.val.filter (rangeP o f e) ⦄ := by
  unfold owner_range owner_range_loop
  have h := loop_fold s.accounts.val (fun v : alloc.vec.Vec Account => v.val) (rangeStepL o f e)
    (fun v k => v.length + (s.accounts.length - k) ≤ Usize.max)
    (fun x => owner_range_loop.body s o f e x.1 x.2) ?_ (alloc.vec.Vec.new Account) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_rangeStepL]; simp)
  · intro v j hj hv
    unfold owner_range_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < s.accounts.val.length := by scalar_tac
      rw [vec_index_eq s.accounts j hlt']
      simp only [bind_ok]
      have hj1 : j.val + 1 ≤ Usize.max := by scalar_tac
      by_cases h1 : s.accounts.val[j.val].owner = o <;> by_cases h2 : f ≤ s.accounts.val[j.val].seq <;>
        by_cases h3 : s.accounts.val[j.val].seq < e <;>
        simp only [h1, h2, h3, if_true, if_false, bind_ok] <;> step* <;> simp only [FoldStep] <;>
        refine ⟨hlt', ?_, by scalar_tac, by simp only [alloc.vec.Vec.length] at *; scalar_tac⟩ <;>
        simp_all [rangeStepL, rangeP]
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem owner_range_congr (s s' : Snapshot) (o f e : U64)
    (h : s'.accounts.val.filter (rangeP o f e) = s.accounts.val.filter (rangeP o f e)) :
    owner_range s' o f e = owner_range s o f e := by
  obtain ⟨v, hv⟩ := ok_of (owner_range_spec s o f e)
  obtain ⟨v', hv'⟩ := ok_of (owner_range_spec s' o f e)
  have e1 := post_of_ok (owner_range_spec s o f e) hv
  have e2 := post_of_ok (owner_range_spec s' o f e) hv'
  have : v' = v := alloc.vec.Vec.ext _ _ (by rw [e1, e2, h])
  rw [hv, hv', this]

theorem accounts_page_congr (a : Principal) (s s' : Snapshot) (o f : U64) (l : U32) (e : U64)
    (he : page_end f l = ok e)
    (h1 : s'.accounts.val.filter (rangeP o f e) = s.accounts.val.filter (rangeP o f e))
    (h2 : s'.owners.val.find? (fun c => decide (c.owner = o)) = s.owners.val.find? (fun c => decide (c.owner = o))) :
    accounts_page a s' o f l = accounts_page a s o f l := by
  unfold accounts_page
  simp only [he, owner_next_eq, ownerNext, h2, owner_range_congr s s' o f e h1, bind_ok]

/-! ## A `History` page is a filter -/

def hrangeP (acc f e : U64) (x : HistoryEntry) : Bool := decide (x.account = acc) && decide (f ≤ x.n) && decide (x.n < e)

def hrangeStepL (acc f e : U64) (l : List HistoryEntry) (x : HistoryEntry) : List HistoryEntry :=
  if hrangeP acc f e x then l ++ [x] else l

theorem foldl_hrangeStepL (acc f e : U64) (l init : List HistoryEntry) :
    l.foldl (hrangeStepL acc f e) init = init ++ l.filter (hrangeP acc f e) := by
  induction l generalizing init with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, hrangeStepL, List.filter_cons]
    split <;> simp

theorem history_range_spec (s : Snapshot) (acc f e : U64) :
    history_range s acc f e ⦃ v => v.val = s.history.val.filter (hrangeP acc f e) ⦄ := by
  unfold history_range history_range_loop
  have h := loop_fold s.history.val (fun v : alloc.vec.Vec HistoryEntry => v.val) (hrangeStepL acc f e)
    (fun v k => v.length + (s.history.length - k) ≤ Usize.max)
    (fun x => history_range_loop.body s acc f e x.1 x.2) ?_ (alloc.vec.Vec.new HistoryEntry) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_hrangeStepL]; simp)
  · intro v j hj hv
    unfold history_range_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < s.history.val.length := by scalar_tac
      rw [vec_index_eq s.history j hlt']
      simp only [bind_ok]
      have hj1 : j.val + 1 ≤ Usize.max := by scalar_tac
      by_cases h1 : s.history.val[j.val].account = acc <;> by_cases h2 : f ≤ s.history.val[j.val].n <;>
        by_cases h3 : s.history.val[j.val].n < e <;>
        simp only [h1, h2, h3, if_true, if_false, bind_ok] <;> step* <;> simp only [FoldStep] <;>
        refine ⟨hlt', ?_, by scalar_tac, by simp only [alloc.vec.Vec.length] at *; scalar_tac⟩ <;>
        simp_all [hrangeStepL, hrangeP]
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem history_range_congr (s s' : Snapshot) (acc f e : U64)
    (h : s'.history.val.filter (hrangeP acc f e) = s.history.val.filter (hrangeP acc f e)) :
    history_range s' acc f e = history_range s acc f e := by
  obtain ⟨v, hv⟩ := ok_of (history_range_spec s acc f e)
  obtain ⟨v', hv'⟩ := ok_of (history_range_spec s' acc f e)
  have e1 := post_of_ok (history_range_spec s acc f e) hv
  have e2 := post_of_ok (history_range_spec s' acc f e) hv'
  have : v' = v := alloc.vec.Vec.ext _ _ (by rw [e1, e2, h])
  rw [hv, hv', this]

theorem history_page_congr (a : Principal) (s s' : Snapshot) (acc f : U64) (l : U32) (e : U64)
    (he : page_end f l = ok e)
    (h0 : s'.accounts.val.find? (fun x => x.id.val = acc.val) = s.accounts.val.find? (fun x => x.id.val = acc.val))
    (h1 : s'.history.val.filter (hrangeP acc f e) = s.history.val.filter (hrangeP acc f e)) :
    history_page a s' acc f l = history_page a s acc f l := by
  unfold history_page
  simp only [find_account_eq, h0, he, history_range_congr s s' acc f e h1, bind_ok]

/-! ## A journal page (G4) -/

def toStep (s : Snapshot) (acc : List Transfer) (e : HistoryEntry) : List Transfer :=
  match s.transfers.val.find? (fun t => t.id.val = e.transfer.val) with
  | none => acc
  | some t => acc ++ [t]

theorem toStep_length (s : Snapshot) (acc : List Transfer) (e : HistoryEntry) :
    (toStep s acc e).length ≤ acc.length + 1 := by
  unfold toStep; split <;> simp

theorem foldl_toStep_length (s : Snapshot) (l : List HistoryEntry) (acc : List Transfer) :
    (l.foldl (toStep s) acc).length ≤ acc.length + l.length := by
  induction l generalizing acc with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, List.length_cons]
    have := ih (toStep s acc x); have := toStep_length s acc x; omega

theorem transfers_of_spec (s : Snapshot) (es : alloc.vec.Vec HistoryEntry) :
    transfers_of s es ⦃ v => v.val = es.val.foldl (toStep s) [] ⦄ := by
  unfold transfers_of transfers_of_loop
  have h := loop_fold es.val (fun v : alloc.vec.Vec Transfer => v.val) (toStep s)
    (fun v k => v.length ≤ k)
    (fun x => transfers_of_loop.body s es x.1 x.2) ?_ (alloc.vec.Vec.new Transfer) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr]; simp)
  · intro v j hj hv
    unfold transfers_of_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < es.val.length := by scalar_tac
      rw [vec_index_eq es j hlt']
      simp only [bind_ok]
      rw [find_transfer_eq]
      simp only [bind_ok]
      have hj1 : j.val + 1 ≤ Usize.max := by scalar_tac
      cases hf : s.transfers.val.find? (fun t => decide (t.id.val = es.val[j.val].transfer.val)) with
      | none =>
        simp only [bind_ok]
        step*
        simp only [FoldStep]
        refine ⟨hlt', ?_, by scalar_tac, by scalar_tac⟩
        show v.val = toStep s v.val es.val[j.val]
        unfold toStep; rw [hf]
      | some t =>
        simp only [bind_ok]
        have : v.val.length < Usize.max := by scalar_tac
        step*
        simp only [FoldStep]
        refine ⟨hlt', by simp_all [toStep], by scalar_tac, ?_⟩
        simp_all [alloc.vec.Vec.length]
        try scalar_tac
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem foldl_toStep_congr (s s' : Snapshot) (l : List HistoryEntry)
    (h : ∀ e ∈ l, s'.transfers.val.find? (fun t => t.id.val = e.transfer.val) = s.transfers.val.find? (fun t => t.id.val = e.transfer.val)) :
    ∀ acc, l.foldl (toStep s') acc = l.foldl (toStep s) acc := by
  induction l with
  | nil => intro acc; rfl
  | cons x xs ih =>
    intro acc
    simp only [List.foldl_cons]
    have e1 : toStep s' acc x = toStep s acc x := by simp only [toStep, h x (by simp)]
    rw [e1]; exact ih (fun e he => h e (List.mem_cons_of_mem _ he)) _

theorem transfers_of_congr (s s' : Snapshot) (es : alloc.vec.Vec HistoryEntry)
    (h : ∀ e ∈ es.val, s'.transfers.val.find? (fun t => t.id.val = e.transfer.val) = s.transfers.val.find? (fun t => t.id.val = e.transfer.val)) :
    transfers_of s' es = transfers_of s es := by
  obtain ⟨v, hv⟩ := ok_of (transfers_of_spec s es)
  obtain ⟨v', hv'⟩ := ok_of (transfers_of_spec s' es)
  have e1 := post_of_ok (transfers_of_spec s es) hv
  have e2 := post_of_ok (transfers_of_spec s' es) hv'
  have : v' = v := alloc.vec.Vec.ext _ _ (by rw [e1, e2, foldl_toStep_congr s s' _ h])
  rw [hv, hv', this]

theorem journal_page_congr (a : Principal) (s s' : Snapshot) (acc f : U64) (l : U32) (e : U64)
    (he : page_end f l = ok e)
    (h0 : s'.accounts.val.find? (fun x => x.id.val = acc.val) = s.accounts.val.find? (fun x => x.id.val = acc.val))
    (h1 : s'.history.val.filter (hrangeP acc f e) = s.history.val.filter (hrangeP acc f e))
    (h2 : ∀ x ∈ s.history.val.filter (hrangeP acc f e),
      s'.transfers.val.find? (fun t => t.id.val = x.transfer.val) = s.transfers.val.find? (fun t => t.id.val = x.transfer.val)) :
    journal_page a s' acc f l = journal_page a s acc f l := by
  obtain ⟨v, hv⟩ := ok_of (history_range_spec s acc f e)
  have hvv := post_of_ok (history_range_spec s acc f e) hv
  have hc := transfers_of_congr s s' v (fun x hx => h2 x (by rw [← hvv]; exact hx))
  unfold journal_page
  simp only [find_account_eq, h0, he, history_range_congr s s' acc f e h1, hv, hc, bind_ok]

/-! ## Accounts by id (G4) -/

def idP (f e : U64) (x : Account) : Bool := decide (f ≤ x.id) && decide (x.id < e)

def idStepL (f e : U64) (l : List Account) (x : Account) : List Account :=
  if idP f e x then l ++ [x] else l

theorem foldl_idStepL (f e : U64) (l init : List Account) :
    l.foldl (idStepL f e) init = init ++ l.filter (idP f e) := by
  induction l generalizing init with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, idStepL, List.filter_cons]
    split <;> simp

theorem id_range_spec (s : Snapshot) (f e : U64) :
    id_range s f e ⦃ v => v.val = s.accounts.val.filter (idP f e) ⦄ := by
  unfold id_range id_range_loop
  have h := loop_fold s.accounts.val (fun v : alloc.vec.Vec Account => v.val) (idStepL f e)
    (fun v k => v.length + (s.accounts.length - k) ≤ Usize.max)
    (fun x => id_range_loop.body s f e x.1 x.2) ?_ (alloc.vec.Vec.new Account) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_idStepL]; simp)
  · intro v j hj hv
    unfold id_range_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < s.accounts.val.length := by scalar_tac
      rw [vec_index_eq s.accounts j hlt']
      simp only [bind_ok]
      have hj1 : j.val + 1 ≤ Usize.max := by scalar_tac
      by_cases h2 : f ≤ s.accounts.val[j.val].id <;> by_cases h3 : s.accounts.val[j.val].id < e <;>
        simp only [h2, h3, if_true, if_false, bind_ok] <;> step* <;> simp only [FoldStep] <;>
        refine ⟨hlt', ?_, by scalar_tac, by simp only [alloc.vec.Vec.length] at *; scalar_tac⟩ <;>
        simp_all [idStepL, idP]
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem id_range_congr (s s' : Snapshot) (f e : U64)
    (h : s'.accounts.val.filter (idP f e) = s.accounts.val.filter (idP f e)) :
    id_range s' f e = id_range s f e := by
  obtain ⟨v, hv⟩ := ok_of (id_range_spec s f e)
  obtain ⟨v', hv'⟩ := ok_of (id_range_spec s' f e)
  have e1 := post_of_ok (id_range_spec s f e) hv
  have e2 := post_of_ok (id_range_spec s' f e) hv'
  have : v' = v := alloc.vec.Vec.ext _ _ (by rw [e1, e2, h])
  rw [hv, hv', this]

theorem account_range_congr (a : Principal) (s s' : Snapshot) (f : U64) (l : U32) (e : U64)
    (he : page_end f l = ok e) (hl : s'.ledger = s.ledger)
    (h1 : s'.accounts.val.filter (idP f e) = s.accounts.val.filter (idP f e)) :
    account_range a s' f l = account_range a s f l := by
  unfold account_range
  simp only [he, hl, id_range_congr s s' f e h1, bind_ok]

/-! ## Transfers by id (G6) -/

def tidP (f e : U64) (x : Transfer) : Bool := decide (f ≤ x.id) && decide (x.id < e)

def tidStepL (f e : U64) (l : List Transfer) (x : Transfer) : List Transfer :=
  if tidP f e x then l ++ [x] else l

theorem foldl_tidStepL (f e : U64) (l init : List Transfer) :
    l.foldl (tidStepL f e) init = init ++ l.filter (tidP f e) := by
  induction l generalizing init with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, tidStepL, List.filter_cons]
    split <;> simp

theorem transfer_ids_spec (s : Snapshot) (f e : U64) :
    transfer_ids s f e ⦃ v => v.val = s.transfers.val.filter (tidP f e) ⦄ := by
  unfold transfer_ids transfer_ids_loop
  have h := loop_fold s.transfers.val (fun v : alloc.vec.Vec Transfer => v.val) (tidStepL f e)
    (fun v k => v.length + (s.transfers.length - k) ≤ Usize.max)
    (fun x => transfer_ids_loop.body s f e x.1 x.2) ?_ (alloc.vec.Vec.new Transfer) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_tidStepL]; simp)
  · intro v j hj hv
    unfold transfer_ids_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < s.transfers.val.length := by scalar_tac
      rw [vec_index_eq s.transfers j hlt']
      simp only [bind_ok]
      have hj1 : j.val + 1 ≤ Usize.max := by scalar_tac
      by_cases h2 : f ≤ s.transfers.val[j.val].id <;> by_cases h3 : s.transfers.val[j.val].id < e <;>
        simp only [h2, h3, if_true, if_false, bind_ok] <;> step* <;> simp only [FoldStep] <;>
        refine ⟨hlt', ?_, by scalar_tac, by simp only [alloc.vec.Vec.length] at *; scalar_tac⟩ <;>
        simp_all [tidStepL, tidP]
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem transfer_range_congr (a : Principal) (s s' : Snapshot) (f : U64) (l : U32) (e : U64)
    (he : page_end f l = ok e)
    (h1 : s'.transfers.val.filter (tidP f e) = s.transfers.val.filter (tidP f e)) :
    transfer_range a s' f l = transfer_range a s f l := by
  obtain ⟨v, hv⟩ := ok_of (transfer_ids_spec s f e)
  obtain ⟨v', hv'⟩ := ok_of (transfer_ids_spec s' f e)
  have e1 := post_of_ok (transfer_ids_spec s f e) hv
  have e2 := post_of_ok (transfer_ids_spec s' f e) hv'
  have hvv : v' = v := alloc.vec.Vec.ext _ _ (by rw [e1, e2, h1])
  unfold transfer_range
  simp only [he, hv, hv', hvv, bind_ok]

/-! ## Slices kept through a batch -/

/-- The accounts a batch names. -/
def kA (ts : List TransferReq) (x : Account) : Bool := ts.any (fun t => decide (x.id = t.debit) || decide (x.id = t.credit))
/-- The transfer ids a batch names. -/
def kT (ts : List TransferReq) (x : Transfer) : Bool := ts.any (fun t => decide (x.id = t.id))

/-- Upserting a kept row commutes with keeping, when keeping depends only on the key. -/
theorem upsert_filter {α κ : Type} [DecidableEq κ] (key : α → κ) (k : α → Bool) (x : α)
    (hk : ∀ y, key y = key x → k y = k x) (hx : k x = true) (l : List α) :
    (upsert key x l).filter k = upsert key x (l.filter k) := by
  induction l with
  | nil => simp [upsert, hx]
  | cons y ys ih =>
    by_cases hy : key y = key x
    · have hky : k y = true := (hk y hy).trans hx
      simp [upsert, hy, hx, hky, List.filter_cons]
    · simp only [upsert, hy, if_false, List.filter_cons]
      split <;> simp [upsert, hy, ih]

/-- A write that only touches rows a slice keeps. -/
def Kept (ka : Account → Bool) (kt : Transfer → Bool) : Write → Prop
  | .PutAccount a => ka a = true
  | .PutTransfer t => kt t = true
  | .SetLedger _ => True
  | .PutOwner _ => False
  | .PutHistory _ => True

/-- `s'` keeps of `s` the accounts `ka` and the transfers `kt` keep. -/
def Keeps (ka : Account → Bool) (kt : Transfer → Bool) (s s' : St) : Prop :=
  s'.accounts = s.accounts.filter ka ∧ s'.transfers = s.transfers.filter kt ∧ s'.owners.length ≤ s.owners.length

theorem keeps_applyAll (ts : List TransferReq) (s s' : St) (ws : List Write)
    (hw : ∀ w ∈ ws, Kept (kA ts) (kT ts) w) (h : Keeps (kA ts) (kT ts) s s') :
    Keeps (kA ts) (kT ts) (applyAll s ws) (applyAll s' ws) := by
  induction ws generalizing s s' with
  | nil => simpa [applyAll] using h
  | cons w ws ih =>
    simp only [applyAll, List.foldl_cons] at *
    apply ih _ _ (fun w' hw' => hw w' (List.mem_cons_of_mem _ hw'))
    have hw0 := hw w (by simp)
    obtain ⟨ha, ht, ho⟩ := h
    cases w with
    | PutAccount a =>
      refine ⟨?_, ht, ho⟩
      simp only [applyWrite, ha]
      exact (upsert_filter _ _ a (fun y hy => by simp [kA, hy]) hw0 _).symm
    | PutTransfer t =>
      refine ⟨ha, ?_, ho⟩
      simp only [applyWrite, ht]
      exact (upsert_filter _ _ t (fun y hy => by simp [kT, hy]) hw0 _).symm
    | SetLedger l => exact ⟨ha, ht, ho⟩
    | PutOwner c => exact absurd hw0 id
    | PutHistory x => exact ⟨ha, ht, ho⟩

theorem find_filter_kept {α : Type} (l : List α) (keep P : α → Bool) (h : ∀ x, P x = true → keep x = true) :
    (l.filter keep).find? P = l.find? P :=
  find?_slice l P keep (fun x _ hp => h x hp)

/-- What a transfer in a batch writes is kept, and it sees on a slice what it sees on the whole. -/
theorem transfer_slice (actor : Principal) (ts : List TransferReq) (t : TransferReq) (ht : t ∈ ts)
    (c c' : Snapshot) (hk : Keeps (kA ts) (kT ts) (Snapshot.toSt c) (Snapshot.toSt c')) :
    transfer actor c' t = transfer actor c t := by
  obtain ⟨ha, htr, -⟩ := hk
  simp only [Snapshot.toSt] at ha htr
  apply transfer_congr
  · rw [htr]; apply find_filter_kept
    intro x hx; simp only [kT, List.any_eq_true]; exact ⟨t, ht, by simpa using hx⟩
  · rw [ha]; apply find_filter_kept
    intro x hx; simp only [kA, List.any_eq_true]; exact ⟨t, ht, by simp_all⟩
  · rw [ha]; apply find_filter_kept
    intro x hx; simp only [kA, List.any_eq_true]; exact ⟨t, ht, by simp_all⟩

theorem transfer_kept (actor : Principal) (ts : List TransferReq) (t : TransferReq) (ht : t ∈ ts)
    (c : Snapshot) ws r (h : transfer actor c t = ok (.Ok (ws, r))) : ∀ w ∈ ws.val, Kept (kA ts) (kT ts) w := by
  rcases post_of_ok (transfer_spec actor c t) h ws r rfl with
    ⟨_, _, _, _, _, _, hws, _⟩ | ⟨_, _, _, _, a, b, ha, _, hb, _, _, d, cr, _, _, _, _, _, _, _, _, hws⟩
  · simp [hws]
  · have hai := (List.find?_some ha)
    have hbi := (List.find?_some hb)
    simp only [decide_eq_true_eq] at hai hbi
    intro w hw
    simp only [hws, postWrites, List.mem_cons, List.mem_nil_iff, or_false] at hw
    rcases hw with rfl | rfl | rfl | rfl | rfl
    · simp only [Kept, kA, List.any_eq_true]; exact ⟨t, ht, by simp [UScalar.eq_of_val_eq hai]⟩
    · trivial
    · simp only [Kept, kA, List.any_eq_true]; exact ⟨t, ht, by simp [UScalar.eq_of_val_eq hbi]⟩
    · trivial
    · simp only [Kept, kT, List.any_eq_true]; exact ⟨t, ht, by simp [journalRow]⟩

/-- Binds that agree on every value the first computation returns. -/
theorem bind_congr_ok {α β : Type} (m : Result α) (f g : α → Result β) (h : ∀ x, m = ok x → f x = g x) :
    (do let x ← m; f x) = (do let x ← m; g x) := by
  change Std.bind m f = Std.bind m g
  cases m using Result.cases with
  | ret a => exact (bind_ok a f).trans ((h a rfl).trans (bind_ok a g).symm)
  | vis e k =>
    cases e with
    | fail err =>
      rw [bind_vis, bind_vis]
      congr 1; funext y; exact y.elim
  | div => simp

theorem size_le_of_keeps (ka : Account → Bool) (kt : Transfer → Bool) (c c' : Snapshot)
    (h : Keeps ka kt (Snapshot.toSt c) (Snapshot.toSt c')) (hh : c'.history.length ≤ c.history.length) :
    Snapshot.size c' ≤ Snapshot.size c := by
  obtain ⟨ha, ht, ho⟩ := h
  simp only [Snapshot.toSt] at ha ht ho
  simp only [alloc.vec.Vec.length] at hh
  simp only [Snapshot.size, alloc.vec.Vec.length, ha, ht]
  have := List.length_filter_le ka c.accounts.val
  have := List.length_filter_le kt c.transfers.val
  omega

theorem body_ok (actor : Principal) (ts : alloc.vec.Vec TransferReq) (c : Snapshot) (ws : alloc.vec.Vec Write)
    (i : Usize) (w : alloc.vec.Vec Write) (r : Reply) (hlt : i.val < ts.val.length)
    (hr : transfer actor c ts.val[i.val] = ok (.Ok (w, r))) :
    batch_loop.body actor ts c ws i =
      (do let cur1 ← apply c w; let ws1 ← append ws w; let i2 ← i + 1#usize; ok (ControlFlow.cont (cur1, ws1, i2))) := by
  have : i < ts.len := by simp only [alloc.vec.Vec.len]; scalar_tac
  unfold batch_loop.body
  simp only [this, if_true, vec_index_eq ts i hlt, bind_ok, hr]
  rfl

theorem body_err (actor : Principal) (ts : alloc.vec.Vec TransferReq) (c : Snapshot) (ws : alloc.vec.Vec Write)
    (i : Usize) (e : payment_ledger.Error) (hlt : i.val < ts.val.length)
    (hr : transfer actor c ts.val[i.val] = ok (.Err e)) :
    batch_loop.body actor ts c ws i = ok (ControlFlow.done (.Err e)) := by
  have : i < ts.len := by simp only [alloc.vec.Vec.len]; scalar_tac
  unfold batch_loop.body
  simp only [this, if_true, vec_index_eq ts i hlt, bind_ok, hr]

theorem body_end (actor : Principal) (ts : alloc.vec.Vec TransferReq) (c c' : Snapshot) (ws : alloc.vec.Vec Write)
    (i : Usize) (hge : ¬ i.val < ts.val.length) :
    batch_loop.body actor ts c' ws i = batch_loop.body actor ts c ws i := by
  have : ¬ i < ts.len := by simp only [alloc.vec.Vec.len]; scalar_tac
  unfold batch_loop.body
  simp only [this, if_false]

/-- The batch loop on a slice that keeps the batch's accounts and ids runs as on the whole. -/
theorem batch_loop_congr (actor : Principal) (ts : alloc.vec.Vec TransferReq) :
    ∀ n (c c' : Snapshot) (ws : alloc.vec.Vec Write) (i : Usize), ts.length - i.val = n → i.val ≤ ts.length →
      Keeps (kA ts.val) (kT ts.val) (Snapshot.toSt c) (Snapshot.toSt c') →
      Snapshot.size c + 5 * (ts.length - i.val) < Usize.max →
      Snapshot.size c' + 5 * (ts.length - i.val) < Usize.max →
      batch_loop actor ts c' ws i = batch_loop actor ts c ws i := by
  intro n
  induction n using Nat.strong_induction_on with
  | _ n ih =>
    intro c c' ws i hn hi hk hsz hsz'
    unfold batch_loop
    conv_lhs => rw [loop]
    conv_rhs => rw [loop]
    dsimp only
    by_cases hlt' : i.val < ts.val.length
    · have hmem : ts.val[i.val] ∈ ts.val := List.getElem_mem hlt'
      have heq := transfer_slice actor ts.val _ hmem c c' hk
      obtain ⟨r0, hr0⟩ := ok_of (transfer_spec actor c ts.val[i.val])
      rcases r0 with ⟨w1, rep⟩ | e
      · rw [body_ok actor ts c' ws i w1 rep hlt' (heq ▸ hr0), body_ok actor ts c ws i w1 rep hlt' hr0]
        have h3 := Batch.transfer_writes_le actor c _ w1 rep hr0
        have hfit : Snapshot.size c + w1.length < Usize.max := by simp only [alloc.vec.Vec.length] at *; omega
        have hfit' : Snapshot.size c' + w1.length < Usize.max := by
          simp only [alloc.vec.Vec.length] at *; omega
        obtain ⟨c1, hap⟩ := ok_of (apply_spec c w1 hfit)
        have hst := post_of_ok (apply_spec c w1 hfit) hap
        obtain ⟨c1', hap'⟩ := ok_of (apply_spec c' w1 hfit')
        have hst' := post_of_ok (apply_spec c' w1 hfit') hap'
        simp only [hap, hap', bind_ok, bind_tc_ok, bind_assoc_eq, Std.bind_assoc]
        apply bind_congr_ok
        intro ws1 _
        apply bind_congr_ok
        intro i2 hi2
        have hi2v : i2.val = i.val + 1 := by
          have := post_of_ok (Usize.add_spec (x := i) (y := 1#usize) (by scalar_tac)) hi2; scalar_tac
        have hk1 : Keeps (kA ts.val) (kT ts.val) (Snapshot.toSt c1) (Snapshot.toSt c1') := by
          rw [hst, hst']
          exact keeps_applyAll ts.val _ _ w1.val (transfer_kept actor ts.val _ hmem c w1 rep hr0) hk
        have hsz1 : Snapshot.size c1 ≤ Snapshot.size c + w1.length := by
          rw [← rows_toSt, ← rows_toSt, hst]; exact rows_applyAll _ _
        have hsz1' : Snapshot.size c1' ≤ Snapshot.size c' + w1.length := by
          rw [← rows_toSt, ← rows_toSt, hst']; exact rows_applyAll _ _
        have := ih (ts.length - i2.val) (by simp only [alloc.vec.Vec.length] at *; omega) c1 c1' ws1 i2 rfl
          (by simp only [alloc.vec.Vec.length] at *; omega) hk1 (by simp only [alloc.vec.Vec.length] at *; omega)
          (by simp only [alloc.vec.Vec.length] at *; omega)
        unfold batch_loop at this
        exact this
      · rw [body_err actor ts c' ws i e hlt' (heq ▸ hr0), body_err actor ts c ws i e hlt' hr0]
    · rw [body_end actor ts c c' ws i hlt']

/-! ## What a plan loads -/

theorem foldl_rangeStep_holds (c : Nat) (tabs : H5iAppLib.Sql.Tables Val) (row : List Val) (l : List Nat) (p : Pred Val) :
    (l.foldl (rangeStep c) p).holds tabs row = (p.holds tabs row || l.any (fun z => decide (row[c]? = some (int z)))) := by
  induction l generalizing p with
  | nil => simp
  | cons z zs ih => simp [List.foldl_cons, ih, rangeStep, Pred.holds, Bool.or_assoc]

/-- A row whose column `c` holds a number of the range satisfies the range. -/
theorem rangeA_holds_of (c f e y : Nat) (hfy : f ≤ y) (hye : y < e) {tabs : H5iAppLib.Sql.Tables Val} {row : List Val}
    (hrow : row[c]? = some (int y)) : (rangeA c f e).holds tabs row = true := by
  unfold rangeA
  rw [foldl_rangeStep_holds]
  rcases Nat.eq_or_lt_of_le hfy with rfl | hlt
  · simp [Pred.holds, hrow]
  · simp only [Bool.or_eq_true, List.any_eq_true]
    right
    exact ⟨y, List.mem_range'_1.2 ⟨by omega, by omega⟩, by simp [hrow]⟩


theorem holds_accsA (tabs : H5iAppLib.Sql.Tables Val) (t : TransferReq) (x : Account) :
    (accsA t).holds tabs (Account.row x) = (decide (x.id = t.debit) || decide (x.id = t.credit)) := by
  simp [accsA, eqU, Pred.holds, Account.row]

theorem holds_eqU0 (tabs : H5iAppLib.Sql.Tables Val) (u : U64) (x : Transfer) :
    (eqU 0 u).holds tabs (Transfer.row x) = decide (x.id = u) := by
  simp [eqU, Pred.holds, Transfer.row]

theorem holds_accsFold (tabs : H5iAppLib.Sql.Tables Val) (l : List TransferReq) (p : Pred Val) (x : Account) :
    (l.foldl accsStep p).holds tabs (Account.row x) =
      (p.holds tabs (Account.row x) || l.any (fun t => decide (x.id = t.debit) || decide (x.id = t.credit))) := by
  induction l generalizing p with
  | nil => simp
  | cons t l ih => simp [ih, accsStep, Pred.holds, holds_accsA, Bool.or_assoc]

theorem holds_idsFold (tabs : H5iAppLib.Sql.Tables Val) (l : List TransferReq) (p : Pred Val) (x : Transfer) :
    (l.foldl idsStep p).holds tabs (Transfer.row x) =
      (p.holds tabs (Transfer.row x) || l.any (fun t => decide (x.id = t.id))) := by
  induction l generalizing p with
  | nil => simp
  | cons t l ih => simp [ih, idsStep, Pred.holds, holds_eqU0, Bool.or_assoc]

theorem load_batch (s : Snapshot) (t0 : TransferReq) (rest : List TransferReq) :
    Keeps (kA (t0 :: rest)) (kT (t0 :: rest)) (Snapshot.toSt s) (Snapshot.toSt (Snapshot.load (batchA t0 rest) s)) := by
  refine ⟨?_, ?_, by simp [Snapshot.toSt, Snapshot.load_owners, pickRows, batchA]⟩
  · simp only [Snapshot.toSt, Snapshot.load_accounts, pickRows, batchA]
    simp only [List.find?, decide_true, Window.apply_default]
    apply List.filter_congr
    intro x _
    rw [holds_accsFold, holds_accsA]; simp [kA]
  · simp only [Snapshot.toSt, Snapshot.load_transfers, pickRows, batchA]
    simp only [List.find?, decide_true, decide_false, Window.apply_default]
    apply List.filter_congr
    intro x _
    rw [holds_idsFold, holds_eqU0]; simp [kT]

/-! ## The frame theorem -/

theorem vec_eq {α : Type} {v w : alloc.vec.Vec α} (h : v.val = w.val) : v = w := alloc.vec.Vec.ext _ _ h

/-- **Frame.** On what a command's plan loads, `transition` gives what it
gives on the whole tenant. `HasRoom s` matters only for batches. -/
theorem frame (a : Principal) (c : Command) (plan : List (Read Val)) (s : Snapshot)
    (hp : readsA a c = some plan) (hroom : HasRoom s) :
    transition a (Snapshot.load plan s) c = transition a s c := by
  cases c with
  | Open owner cur code nod noc =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    simp only [transition]
    apply open_congr
    · simp [Snapshot.load, pickRows, Pred.holds]
    · simp only [Snapshot.load_owners, pickRows, List.find?, decide_true, decide_false, Window.apply_default]
      apply find_filter_kept; intro x hx; simp_all [eqU, Pred.holds, OwnerCounter.row]
  | Transfer t =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    simp only [transition]
    apply transfer_congr
    · simp only [Snapshot.load_transfers, pickRows, List.find?, decide_true, decide_false, Window.apply_default]
      apply find_filter_kept; intro x hx; simp_all [holds_eqU0]
    · simp only [Snapshot.load_accounts, pickRows, List.find?, decide_true, Window.apply_default]
      apply find_filter_kept; intro x hx; simp_all [holds_accsA]
    · simp only [Snapshot.load_accounts, pickRows, List.find?, decide_true, Window.apply_default]
      apply find_filter_kept; intro x hx; simp_all [holds_accsA]
  | Batch ts =>
    simp only [readsA] at hp
    simp only [transition, batch]
    cases hv : ts.val with
    | nil =>
      have h0 : alloc.vec.Vec.len ts = 0#usize := by
        apply UScalar.eq_of_val_eq; simp [alloc.vec.Vec.len, hv]
      simp [h0]
    | cons t0 rest =>
      simp only [hv] at hp
      have hne : ¬ (alloc.vec.Vec.len ts = 0#usize) := by
        intro h0; have := congrArg UScalar.val h0; simp [alloc.vec.Vec.len, hv] at this
      split at hp
      · rename_i h64
        have hmax : alloc.vec.Vec.len ts > MAX_BATCH := by
          simp only [MAX_BATCH, alloc.vec.Vec.len, gt_iff_lt]
          have : ts.val.length > 64 := by rw [hv]; exact h64
          scalar_tac
        simp [hne, hmax]
      · rename_i h64
        simp only [Option.some.injEq] at hp; subst hp
        have hmax : ¬ (alloc.vec.Vec.len ts > MAX_BATCH) := by
          simp only [MAX_BATCH, alloc.vec.Vec.len, gt_iff_lt]; intro hc
          have : ts.val.length > 64 := by scalar_tac
          rw [hv] at this; exact h64 this
        simp only [hne, hmax, if_false, Snapshot.clone_eq, bind_ok, bind_tc_ok]
        have hk := load_batch s t0 rest
        rw [← hv] at hk
        have hle := size_le_of_keeps _ _ s _ hk (by simp [Snapshot.load_history, pickRows, batchA])
        apply batch_loop_congr a ts _ s _ _ 0#usize rfl (by simp) hk
        all_goals
          have : ts.length ≤ 64 := by simp only [alloc.vec.Vec.length, hv]; omega
          unfold HasRoom at hroom
          simp only [Snapshot.size, alloc.vec.Vec.length] at *
          omega
  | Account id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    simp only [transition]
    apply read_account_congr
    simp only [Snapshot.load_accounts, pickRows, List.find?, decide_true, Window.apply_default]
    apply find_filter_kept; intro x hx; simp_all [eqU, Pred.holds, Account.row]
  | Accounts o f l =>
    simp only [transition]
    simp only [readsA] at hp
    split at hp
    · rename_i hc
      obtain ⟨hl0, hl64, hauth⟩ := hc
      simp only [Option.some.injEq] at hp; subst hp
      obtain ⟨e, he, hev⟩ := ok_of' (page_end_spec f l)
      apply accounts_page_congr a s _ o f l e he
      · unfold pageA
        rw [← hev]
        by_cases hfe : f.val < e.val
        · simp only [hfe, if_true]
          rw [Snapshot.load_accounts, pickRows_filter _ _ _ _ _ ⟨0, .and (eqU 1 o) (rangeA 8 f.val e.val), {}⟩ (by simp) rfl]
          apply filter_slice
          intro x _ hx
          simp only [rangeP, Bool.and_eq_true, decide_eq_true_eq] at hx
          obtain ⟨⟨h1, h2⟩, h3⟩ := hx
          simp only [Pred.holds, Bool.and_eq_true]
          refine ⟨by simp [eqU, Pred.holds, Account.row, h1], ?_⟩
          apply rangeA_holds_of 8 f.val e.val x.seq.val (by scalar_tac) (by scalar_tac)
          simp [Account.row]
        · simp only [hfe, if_false]
          have : s.accounts.val.filter (rangeP o f e) = [] := by
            rw [List.filter_eq_nil_iff]; intro x _; simp only [rangeP]; intro h
            simp only [Bool.and_eq_true, decide_eq_true_eq] at h; scalar_tac
          rw [this]
          simp [Snapshot.load_accounts, pickRows]
      · unfold pageA
        split <;>
        · simp only [Snapshot.load_owners, pickRows, List.find?, decide_true, decide_false, Window.apply_default]
          apply find_filter_kept; intro x hx; simp_all [eqU, Pred.holds, OwnerCounter.row]
    · rename_i hc
      unfold accounts_page
      by_cases h0 : l = 0#u32
      · simp [h0]
      have h0' : l.val ≠ 0 := fun h => h0 (by scalar_tac)
      by_cases hm : l > MAX_PAGE
      · simp [h0, hm]
      have hm' : l.val ≤ 64 := by simp only [MAX_PAGE] at hm; scalar_tac
      have hna : ¬ (a.operator = true ∨ o = a.user) := fun h => hc ⟨h0', hm', h⟩
      simp only [not_or] at hna
      simp [h0, hm, hna.1, hna.2, is_operator_eq, user_of_eq]
  | History acc f l =>
    simp only [transition]
    simp only [readsA] at hp
    split at hp
    · rename_i hc
      simp only [Option.some.injEq] at hp; subst hp
      obtain ⟨e, he, hev⟩ := ok_of' (page_end_spec f l)
      apply history_page_congr a s _ acc f l e he
      · have hq : (histA acc f.val (pageEndN f.val l.val)).find? (fun q => decide (q.table = 0)) =
            some ⟨0, eqU 0 acc, {}⟩ := by unfold histA; split <;> simp
        rw [Snapshot.load_accounts, pickRows_filter _ _ _ _ _ ⟨0, eqU 0 acc, {}⟩ hq rfl]
        apply find_filter_kept; intro x hx; simp_all [eqU, Pred.holds, Account.row]
      · unfold histA
        rw [← hev]
        by_cases hfe : f.val < e.val
        · simp only [hfe, if_true]
          rw [Snapshot.load_history, pickRows_filter _ _ _ _ _ ⟨4, entryA acc f.val e.val, {}⟩ (by simp) rfl]
          apply filter_slice
          intro x _ hx
          simp only [hrangeP, Bool.and_eq_true, decide_eq_true_eq] at hx
          obtain ⟨⟨h1, h2⟩, h3⟩ := hx
          simp only [entryA, Pred.holds, Bool.and_eq_true]
          refine ⟨by simp [eqU, Pred.holds, HistoryEntry.row, h1], ?_⟩
          apply rangeA_holds_of 1 f.val e.val x.n.val (by scalar_tac) (by scalar_tac)
          simp [HistoryEntry.row]
        · simp only [hfe, if_false]
          have : s.history.val.filter (hrangeP acc f e) = [] := by
            rw [List.filter_eq_nil_iff]; intro x _; simp only [hrangeP]; intro h
            simp only [Bool.and_eq_true, decide_eq_true_eq] at h; scalar_tac
          rw [this]
          simp [Snapshot.load_history, pickRows]
    · rename_i hc
      unfold history_page
      by_cases h0 : l = 0#u32
      · simp [h0]
      have h0' : l.val ≠ 0 := fun h => h0 (by scalar_tac)
      by_cases hm : l > MAX_PAGE
      · simp [h0, hm]
      have hm' : l.val ≤ 64 := by simp only [MAX_PAGE] at hm; scalar_tac
      have hna : ¬ a.operator = true := fun h => hc ⟨h0', hm', h⟩
      simp [h0, hm, hna, is_operator_eq]
  | Transfers acc f l =>
    simp only [transition]
    simp only [readsA] at hp
    split at hp
    · rename_i hc
      simp only [Option.some.injEq] at hp; subst hp
      obtain ⟨e, he, hev⟩ := ok_of' (page_end_spec f l)
      have hentry : ∀ x : HistoryEntry, hrangeP acc f e x = true →
          (entryA acc f.val e.val).holds (Snapshot.enc s) (HistoryEntry.row x) = true := by
        intro x hx
        simp only [hrangeP, Bool.and_eq_true, decide_eq_true_eq] at hx
        obtain ⟨⟨h1, h2⟩, h3⟩ := hx
        simp only [entryA, Pred.holds, Bool.and_eq_true]
        refine ⟨by simp [eqU, Pred.holds, HistoryEntry.row, h1], ?_⟩
        apply rangeA_holds_of 1 f.val e.val x.n.val (by scalar_tac) (by scalar_tac)
        simp [HistoryEntry.row]
      apply journal_page_congr a s _ acc f l e he
      · have hq : (journalA acc f.val (pageEndN f.val l.val)).find? (fun q => decide (q.table = 0)) =
            some ⟨0, eqU 0 acc, {}⟩ := by unfold journalA; split <;> simp
        rw [Snapshot.load_accounts, pickRows_filter _ _ _ _ _ ⟨0, eqU 0 acc, {}⟩ hq rfl]
        apply find_filter_kept; intro x hx; simp_all [eqU, Pred.holds, Account.row]
      · unfold journalA
        rw [← hev]
        by_cases hfe : f.val < e.val
        · simp only [hfe, if_true]
          rw [Snapshot.load_history, pickRows_filter _ _ _ _ _ ⟨4, entryA acc f.val e.val, {}⟩ (by simp) rfl]
          exact filter_slice _ _ _ (fun x _ hx => hentry x hx)
        · simp only [hfe, if_false]
          have : s.history.val.filter (hrangeP acc f e) = [] := by
            rw [List.filter_eq_nil_iff]; intro x _; simp only [hrangeP]; intro h
            simp only [Bool.and_eq_true, decide_eq_true_eq] at h; scalar_tac
          rw [this]
          simp [Snapshot.load_history, pickRows]
      · intro x hx
        have hxr := (List.mem_filter.1 hx).2
        have hfe : f.val < e.val := by
          simp only [hrangeP, Bool.and_eq_true, decide_eq_true_eq] at hxr; scalar_tac
        unfold journalA
        rw [← hev]
        simp only [hfe, if_true]
        rw [Snapshot.load_transfers, pickRows_filter _ _ _ _ _ ⟨1, .isIn 0 4 (entryA acc f.val e.val) 6 {}, {}⟩ (by simp) rfl]
        apply find_filter_kept
        intro t ht
        simp only [decide_eq_true_eq] at ht
        rw [holds_in_4]
        simp only [Transfer.row, List.getElem?_cons_zero, Window.apply_default, List.any_eq_true, List.mem_filter]
        exact ⟨x, ⟨(List.mem_filter.1 hx).1, hentry x hxr⟩, by simp [HistoryEntry.row, ht]⟩
    · rename_i hc
      unfold journal_page
      by_cases h0 : l = 0#u32
      · simp [h0]
      have h0' : l.val ≠ 0 := fun h => h0 (by scalar_tac)
      by_cases hm : l > MAX_PAGE
      · simp [h0, hm]
      have hm' : l.val ≤ 64 := by simp only [MAX_PAGE] at hm; scalar_tac
      have hna : ¬ a.operator = true := fun h => hc ⟨h0', hm', h⟩
      simp [h0, hm, hna, is_operator_eq]
  | AccountRange f l =>
    simp only [transition]
    simp only [readsA] at hp
    split at hp
    · rename_i hc
      simp only [Option.some.injEq] at hp; subst hp
      obtain ⟨e, he, hev⟩ := ok_of' (page_end_spec f l)
      apply account_range_congr a s _ f l e he
      · unfold idsA; split <;> simp [Snapshot.load, pickRows, Pred.holds]
      · unfold idsA
        rw [← hev]
        by_cases hfe : f.val < e.val
        · simp only [hfe, if_true]
          rw [Snapshot.load_accounts, pickRows_filter _ _ _ _ _ ⟨0, rangeA 0 f.val e.val, {}⟩ (by simp) rfl]
          apply filter_slice
          intro x _ hx
          simp only [idP, Bool.and_eq_true, decide_eq_true_eq] at hx
          apply rangeA_holds_of 0 f.val e.val x.id.val (by scalar_tac) (by scalar_tac)
          simp [Account.row]
        · simp only [hfe, if_false]
          have : s.accounts.val.filter (idP f e) = [] := by
            rw [List.filter_eq_nil_iff]; intro x _; simp only [idP]; intro h
            simp only [Bool.and_eq_true, decide_eq_true_eq] at h; scalar_tac
          rw [this]
          simp [Snapshot.load_accounts, pickRows]
    · rename_i hc
      unfold account_range
      by_cases h0 : l = 0#u32
      · simp [h0]
      have h0' : l.val ≠ 0 := fun h => h0 (by scalar_tac)
      by_cases hm : l > MAX_PAGE
      · simp [h0, hm]
      have hm' : l.val ≤ 64 := by simp only [MAX_PAGE] at hm; scalar_tac
      have hna : ¬ a.operator = true := fun h => hc ⟨h0', hm', h⟩
      simp [h0, hm, hna, is_operator_eq]
  | TransferRange f l =>
    simp only [transition]
    simp only [readsA] at hp
    split at hp
    · rename_i hc
      simp only [Option.some.injEq] at hp; subst hp
      obtain ⟨e, he, hev⟩ := ok_of' (page_end_spec f l)
      apply transfer_range_congr a s _ f l e he
      unfold tidsA
      rw [← hev]
      by_cases hfe : f.val < e.val
      · simp only [hfe, if_true]
        rw [Snapshot.load_transfers, pickRows_filter _ _ _ _ _ ⟨1, rangeA 0 f.val e.val, {}⟩ (by simp) rfl]
        apply filter_slice
        intro x _ hx
        simp only [tidP, Bool.and_eq_true, decide_eq_true_eq] at hx
        apply rangeA_holds_of 0 f.val e.val x.id.val (by scalar_tac) (by scalar_tac)
        simp [Transfer.row]
      · simp only [hfe, if_false]
        have : s.transfers.val.filter (tidP f e) = [] := by
          rw [List.filter_eq_nil_iff]; intro x _; simp only [tidP]; intro h
          simp only [Bool.and_eq_true, decide_eq_true_eq] at h; scalar_tac
        rw [this]
        simp [Snapshot.load_transfers, pickRows]
    · rename_i hc
      unfold transfer_range
      by_cases h0 : l = 0#u32
      · simp [h0]
      have h0' : l.val ≠ 0 := fun h => h0 (by scalar_tac)
      by_cases hm : l > MAX_PAGE
      · simp [h0, hm]
      have hm' : l.val ≤ 64 := by simp only [MAX_PAGE] at hm; scalar_tac
      have hna : ¬ a.operator = true := fun h => hc ⟨h0', hm', h⟩
      simp [h0, hm, hna, is_operator_eq]

/-- **The plan the server runs.** For every command for which the extracted
`reads` names a plan, `transition` on what loading that plan decodes to
equals `transition` on the whole tenant. -/
theorem load_for_sound (a : Principal) (c : Command) (s : Snapshot) (v : alloc.vec.Vec h5i_app_sql.Read)
    (hr : reads a c = ok (some v)) (hroom : HasRoom s) :
    transition a (Snapshot.load (v.val.map Read.abs) s) c = transition a s c := by
  obtain ⟨o, ho, hab⟩ := reads_abs a c
  rw [hr, Result.ok.injEq] at ho
  subst ho
  exact frame a c _ s (by simpa [planOf] using hab.symm) hroom

end payment_ledger.Frame
