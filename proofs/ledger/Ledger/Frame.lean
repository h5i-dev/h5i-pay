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
  `HasRoom s` (no table within 3 · 8192 rows of `usize::MAX`), under which
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

theorem post_irrel (s s' : Snapshot) (t : TransferReq) (a b : Account) (op : U64) : post s' t a b op = post s t a b op := by
  unfold post; rfl

/-- A transfer reads its id in the journal and its two accounts, nothing else. -/
theorem transfer_congr (actor : Principal) (s s' : Snapshot) (t : TransferReq)
    (hT : s'.transfers.val.find? (fun x => x.id.val = t.id.val) = s.transfers.val.find? (fun x => x.id.val = t.id.val))
    (hD : s'.accounts.val.find? (fun x => x.id.val = t.debit.val) = s.accounts.val.find? (fun x => x.id.val = t.debit.val))
    (hC : s'.accounts.val.find? (fun x => x.id.val = t.credit.val) = s.accounts.val.find? (fun x => x.id.val = t.credit.val)) :
    transfer actor s' t = transfer actor s t := by
  unfold transfer
  simp only [find_transfer_eq, find_account_eq, hT, hD, hC, bind_ok, bind_tc_ok, post_irrel s s']

theorem open_congr (actor : Principal) (s s' : Snapshot) (h : s'.ledger = s.ledger) (owner : U64) (cur code : U32)
    (nod noc : Bool) : «open» actor s' owner cur code nod noc = «open» actor s owner cur code nod noc := by
  unfold «open»; rw [h]

theorem read_account_congr (actor : Principal) (s s' : Snapshot) (id : U64)
    (h : s'.accounts.val.find? (fun x => x.id.val = id.val) = s.accounts.val.find? (fun x => x.id.val = id.val)) :
    read_account actor s' id = read_account actor s id := by
  unfold read_account
  simp only [find_account_eq, h]

/-! ## `visible_accounts` is a filter -/

def visibleP (actor : Principal) (x : Account) : Bool := actor.operator || decide (x.owner = actor.user)

def visStep (actor : Principal) (acc : List Account) (x : Account) : List Account :=
  if visibleP actor x then acc ++ [x] else acc

theorem foldl_visStep (actor : Principal) (l acc : List Account) :
    l.foldl (visStep actor) acc = acc ++ l.filter (visibleP actor) := by
  induction l generalizing acc with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, visStep, List.filter_cons]
    split <;> simp

theorem visible_spec (actor : Principal) (s : Snapshot) :
    visible_accounts actor s ⦃ v => v.val = s.accounts.val.filter (visibleP actor) ⦄ := by
  unfold visible_accounts visible_accounts_loop
  have h := loop_fold s.accounts.val (fun v : alloc.vec.Vec Account => v.val) (visStep actor)
    (fun v k => v.length + (s.accounts.length - k) ≤ Usize.max)
    (fun x => visible_accounts_loop.body actor s x.1 x.2) ?_ (alloc.vec.Vec.new Account) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_visStep]; simp)
  · intro v j hj hv
    unfold visible_accounts_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < s.accounts.val.length := by scalar_tac
      rw [vec_index_eq s.accounts j hlt']
      simp only [bind_ok, bind_tc_ok, may_debit_eq]
      by_cases hb : visibleP actor s.accounts.val[j.val]
      · have hb' : (actor.operator || decide (s.accounts.val[j.val].owner = actor.user)) = true := hb
        simp only [hb', if_true]
        step*
        simp only [FoldStep]
        refine ⟨hlt', ?_, by scalar_tac, ?_⟩
        · simp_all [visStep]
        · simp only [alloc.vec.Vec.length] at *; scalar_tac
      · have hb' : (actor.operator || decide (s.accounts.val[j.val].owner = actor.user)) = false :=
          Bool.eq_false_iff.mpr hb
        simp only [hb', Bool.false_eq_true, if_false, bind_ok, bind_tc_ok]
        step*
        simp only [FoldStep]
        refine ⟨hlt', ?_, by scalar_tac, ?_⟩
        · show v.val = visStep actor v.val s.accounts.val[j.val]
          simp only [visStep, if_neg hb]
        · simp only [alloc.vec.Vec.length] at *; scalar_tac
    · rename_i hge
      simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem visible_eq (actor : Principal) (s : Snapshot) :
    visible_accounts actor s = ok (alloc.vec.Vec.from (s.accounts.val.filter (visibleP actor))
      (le_trans (List.length_filter_le _ _) s.accounts.len_ineq)) := by
  obtain ⟨v, hv⟩ := ok_of (visible_spec actor s)
  have := post_of_ok (visible_spec actor s) hv
  rw [hv]; congr 1; apply alloc.vec.Vec.ext; simp [this]

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

/-- `s'` keeps of `s` the accounts `ka` and the transfers `kt` keep. -/
def Keeps (ka : Account → Bool) (kt : Transfer → Bool) (s s' : St) : Prop :=
  s'.accounts = s.accounts.filter ka ∧ s'.transfers = s.transfers.filter kt

theorem keeps_applyAll (ts : List TransferReq) (s s' : St) (ws : List Write)
    (hw : ∀ w ∈ ws, Kept (kA ts) (kT ts) w) (h : Keeps (kA ts) (kT ts) s s') :
    Keeps (kA ts) (kT ts) (applyAll s ws) (applyAll s' ws) := by
  induction ws generalizing s s' with
  | nil => simpa [applyAll] using h
  | cons w ws ih =>
    simp only [applyAll, List.foldl_cons] at *
    apply ih _ _ (fun w' hw' => hw w' (List.mem_cons_of_mem _ hw'))
    have hw0 := hw w (by simp)
    obtain ⟨ha, ht⟩ := h
    cases w with
    | PutAccount a =>
      refine ⟨?_, ht⟩
      simp only [applyWrite, ha]
      exact (upsert_filter _ _ a (fun y hy => by simp [kA, hy]) hw0 _).symm
    | PutTransfer t =>
      refine ⟨ha, ?_⟩
      simp only [applyWrite, ht]
      exact (upsert_filter _ _ t (fun y hy => by simp [kT, hy]) hw0 _).symm
    | SetLedger l => exact ⟨ha, ht⟩

theorem find_filter_kept {α : Type} (l : List α) (keep P : α → Bool) (h : ∀ x, P x = true → keep x = true) :
    (l.filter keep).find? P = l.find? P :=
  find?_slice l P keep (fun x _ hp => h x hp)

/-- What a transfer in a batch writes is kept, and it sees on a slice what it sees on the whole. -/
theorem transfer_slice (actor : Principal) (ts : List TransferReq) (t : TransferReq) (ht : t ∈ ts)
    (c c' : Snapshot) (hk : Keeps (kA ts) (kT ts) (Snapshot.toSt c) (Snapshot.toSt c')) :
    transfer actor c' t = transfer actor c t := by
  obtain ⟨ha, htr⟩ := hk
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
    ⟨_, _, _, _, _, _, hws, _⟩ | ⟨_, _, _, _, a, b, ha, _, hb, _, _, d, cr, _, _, _, _, hws⟩
  · simp [hws]
  · have hai := (List.find?_some ha)
    have hbi := (List.find?_some hb)
    simp only [decide_eq_true_eq] at hai hbi
    intro w hw
    simp only [hws, List.mem_cons, List.mem_nil_iff, or_false] at hw
    rcases hw with rfl | rfl | rfl
    · simp only [Kept, kA, List.any_eq_true]; exact ⟨t, ht, by simp [UScalar.eq_of_val_eq hai]⟩
    · simp only [Kept, kA, List.any_eq_true]; exact ⟨t, ht, by simp [UScalar.eq_of_val_eq hbi]⟩
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
    (h : Keeps ka kt (Snapshot.toSt c) (Snapshot.toSt c')) : Snapshot.size c' ≤ Snapshot.size c := by
  obtain ⟨ha, ht⟩ := h
  simp only [Snapshot.toSt] at ha ht
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
      Snapshot.size c + 3 * (ts.length - i.val) < Usize.max →
      batch_loop actor ts c' ws i = batch_loop actor ts c ws i := by
  intro n
  induction n using Nat.strong_induction_on with
  | _ n ih =>
    intro c c' ws i hn hi hk hsz
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
          have := size_le_of_keeps _ _ c c' hk; omega
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
        have := ih (ts.length - i2.val) (by simp only [alloc.vec.Vec.length] at *; omega) c1 c1' ws1 i2 rfl
          (by simp only [alloc.vec.Vec.length] at *; omega) hk1 (by simp only [alloc.vec.Vec.length] at *; omega)
        unfold batch_loop at this
        exact this
      · rw [body_err actor ts c' ws i e hlt' (heq ▸ hr0), body_err actor ts c ws i e hlt' hr0]
    · rw [body_end actor ts c c' ws i hlt']

/-! ## What a plan loads -/

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
  constructor
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
    simp [Snapshot.load, pickRows, Pred.holds]
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
      split at hp
      · cases hp
      · rename_i h64
        simp only [Option.some.injEq] at hp; subst hp
        have hne : ¬ (alloc.vec.Vec.len ts = 0#usize) := by
          intro h0; have := congrArg UScalar.val h0; simp [alloc.vec.Vec.len, hv] at this
        have hmax : ¬ (alloc.vec.Vec.len ts > MAX_BATCH) := by
          simp only [MAX_BATCH, alloc.vec.Vec.len, gt_iff_lt]; intro hc
          have : ts.val.length > 8192 := by scalar_tac
          rw [hv] at this; omega
        simp only [hne, hmax, if_false, Snapshot.clone_eq, bind_ok, bind_tc_ok]
        have hk := load_batch s t0 rest
        rw [← hv] at hk
        apply batch_loop_congr a ts _ s _ _ 0#usize rfl (by simp) hk
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
  | Accounts =>
    simp only [transition, visible_eq]
    cases ho : a.operator
    · simp only [readsA, ho, Bool.false_eq_true, if_false, Option.some.injEq] at hp; subst hp
      have : (Snapshot.load [⟨0, eqU 1 a.user, {}⟩] s).accounts.val.filter (visibleP a) =
          s.accounts.val.filter (visibleP a) := by
        simp only [Snapshot.load_accounts, pickRows, List.find?, decide_true, Window.apply_default]
        apply filter_slice
        intro x _ hx
        simp_all [visibleP, eqU, Pred.holds, Account.row]
      simp only [this]
    · simp only [readsA, ho, if_true, Option.some.injEq] at hp; subst hp
      have : (Snapshot.load [⟨0, .all, {}⟩] s).accounts.val = s.accounts.val := by
        simp [Snapshot.load_accounts, pickRows, Pred.holds]
      simp only [this]
  | Transfers =>
    simp only [transition, is_operator_eq, bind_tc_ok, bind_ok]
    cases ho : a.operator
    · simp
    · simp only [readsA, ho, if_true, Option.some.injEq] at hp; subst hp
      have : (Snapshot.load [⟨1, .all, {}⟩] s).transfers = s.transfers := by
        apply vec_eq; simp [Snapshot.load_transfers, pickRows, Pred.holds]
      simp only [this]

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
