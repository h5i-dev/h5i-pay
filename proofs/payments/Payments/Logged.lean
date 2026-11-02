import Payments.Commands
/-!
# The history entry `transition` adds (G1)

`transition` runs `decide_command` and, when its write set is not empty,
`logged`: the write set without its payment and history writes
(`othersL`), then the payment row the command wrote (or, if it wrote none,
the row as it was) with `changes` bumped and the caller as its operator,
then one history entry, number `n` = the old `changes` (0 for a new
payment), naming the caller, the transaction's clock (`a.at`), what the
command was, and the status and amounts before and after.

Here: `logged_spec` (what `logged` writes, read off the extracted code),
`transition_ok` (a successful `transition` is a successful
`decide_command`, unchanged when it writes nothing, else `logged`), the
list facts that relate the two write sets (`applyAll_logged`), and
`decide_shape` (every command decides at most three writes, no history
row and at most one payment row).
-/
open Aeneas Aeneas.Std Result payment_domain payment_domain.Spec payment_domain.Commands H5iAppLib

namespace payment_domain.Logged

/-! ## The write sets as lists -/

def isPayW : Write → Bool
  | .PutPayment _ => true
  | _ => false

def isHistW : Write → Bool
  | .PutHistory _ => true
  | _ => false

/-- A write set without its payment and history rows. -/
def othersL (ws : List Write) : List Write := ws.filter (fun w => !(isPayW w || isHistW w))

def lastPayStep (o : Option Payment) : Write → Option Payment
  | .PutPayment p => some p
  | _ => o

/-- The last payment row a write set writes. -/
def lastPay (ws : List Write) : Option Payment := ws.foldl lastPayStep none

/-- The id the history records for the caller (`actor_id`). -/
def actorId (a : Principal) : U64 :=
  match a.actor with
  | .User u => u
  | .Operator o => o

@[simp] theorem actor_id_eq (a : Principal) : actor_id a = ok (actorId a) := by
  unfold actor_id actorId; cases a.actor <;> rfl

def isMark : Command → Bool
  | .MarkPosted _ _ _ => true
  | _ => false

@[simp] theorem keeps_mark_eq (c : Command) : keeps_mark c = ok (isMark c) := by
  unfold keeps_mark; cases c <;> rfl

/-- The payment a command about an existing payment names (`subject`). -/
def subj : Command → Option U64
  | .BeginAuthorize id => some id
  | .Cancel id => some id
  | .RequestRefund p _ => some p
  | .Record p _ => some p
  | .Event _ p _ => some p
  | .AttachRef p _ => some p
  | .MarkPosted p _ _ => some p
  | _ => none

@[simp] theorem subject_eq (c : Command) : subject c = ok (subj c) := by
  unfold subject; cases c <;> rfl

def resultCode : PspResult → U32 × U64
  | .Authorized _ => (RES_AUTHORIZED, 0#u64)
  | .Declined => (RES_DECLINED, 0#u64)
  | .Captured _ => (RES_CAPTURED, 0#u64)
  | .Canceled => (RES_CANCELED, 0#u64)
  | .RefundSucceeded r => (RES_REFUND_SUCCEEDED, r)
  | .RefundFailed r => (RES_REFUND_FAILED, r)

/-- What a command is, for its entry (`what_of`): command, result, event, refund. -/
def whatOf (s : Snapshot) : Command → U32 × U32 × U64 × U64
  | .Create .. => (CMD_CREATE, RES_NONE, 0#u64, 0#u64)
  | .BeginAuthorize _ => (CMD_BEGIN_AUTHORIZE, RES_NONE, 0#u64, 0#u64)
  | .Cancel _ => (CMD_CANCEL, RES_NONE, 0#u64, 0#u64)
  | .RequestRefund _ _ => (CMD_REQUEST_REFUND, RES_NONE, 0#u64, s.counter.next_id)
  | .Record _ r => (CMD_RECORD, (resultCode r).1, 0#u64, (resultCode r).2)
  | .Event ev _ r => (CMD_EVENT, (resultCode r).1, ev, (resultCode r).2)
  | .AttachRef _ _ => (CMD_ATTACH_REF, RES_NONE, 0#u64, 0#u64)
  | .MarkPosted _ _ _ => (CMD_MARK_POSTED, RES_NONE, 0#u64, 0#u64)
  | _ => (0#u32, RES_NONE, 0#u64, 0#u64)

@[simp] theorem what_of_eq (s : Snapshot) (c : Command) : what_of s c = ok (whatOf s c) := by
  unfold what_of
  cases c with
  | Record _ r => cases r <;> simp [result_code, whatOf, resultCode]
  | Event _ _ r => cases r <;> simp [result_code, whatOf, resultCode]
  | _ => rfl

/-- The row a change leaves: written by the caller, marked only by
`MarkPosted`, with `m` history entries. -/
def afterOf (a : Principal) (c : Command) (base : Payment) (m : U64) : Payment :=
  { base with operator := a.opId, ledger_done := isMark c && base.ledger_done, changes := m }

/-- The entry `n` a change records, `old` the row before (`none`: a new payment). -/
def entryOf (a : Principal) (s : Snapshot) (c : Command) (n : U64) (old : Option Payment) (after : Payment) :
    HistoryEntry :=
  { payment := after.id, n := n, by_operator := a.operator, actor := actorId a, «at» := a.at,
    command := (whatOf s c).1, result := (whatOf s c).2.1, event := (whatOf s c).2.2.1,
    refund := (whatOf s c).2.2.2,
    status_before := (old.map (·.status)).getD NO_STATUS, status_after := after.status,
    captured_before := (old.getD after).captured, captured_after := after.captured,
    refunded_before := (old.getD after).refunded, refunded_after := after.refunded,
    pending_before := (old.getD after).refund_pending, pending_after := after.refund_pending }

/-! ## The extracted helpers -/

theorem lastPay_foldl (o : Option Payment) (ws : List Write) :
    ws.foldl lastPayStep o = (ws.foldl lastPayStep none).or o := by
  induction ws generalizing o with
  | nil => simp
  | cons w ws ih =>
    cases w <;> simp only [List.foldl_cons, lastPayStep] <;> rw [ih] <;> try rw [ih (some _)]
    all_goals cases ws.foldl lastPayStep none <;> simp

@[step] theorem last_payment_spec (ws : alloc.vec.Vec Write) :
    last_payment ws ⦃ o => o = lastPay ws.val ⦄ := by
  unfold last_payment last_payment_loop
  have h := loop_fold ws.val (fun o : Option Payment => o) lastPayStep (fun _ _ => True)
    (fun x => last_payment_loop.body ws x.1 x.2) ?_ none 0#usize (by simp) trivial
  · exact WP.spec_mono h (fun r hr => by rw [hr]; simp [lastPay])
  · intro o j hj _
    unfold last_payment_loop.body
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < ws.val.length := by scalar_tac
      rw [vec_index_eq ws j hlt']
      simp only [bind_ok]
      cases hw : ws.val[j.val] <;> simp only [bind_ok] <;> step* <;> simp only [FoldStep] <;>
        exact ⟨hlt', by simp [lastPayStep, hw], by scalar_tac, trivial⟩
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

def othersStep (acc : List Write) (w : Write) : List Write :=
  if !(isPayW w || isHistW w) then acc ++ [w] else acc

theorem foldl_othersStep (l acc : List Write) : l.foldl othersStep acc = acc ++ othersL l := by
  induction l generalizing acc with
  | nil => simp [othersL]
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, othersStep, othersL, List.filter_cons]
    split <;> simp_all

theorem write_clone' (w : Write) : Write.Insts.CoreCloneClone.clone w = ok w := write_clone w

@[step] theorem others_spec (ws : alloc.vec.Vec Write) :
    others ws ⦃ v => v.val = othersL ws.val ⦄ := by
  unfold others others_loop
  have h := loop_fold ws.val (fun v : alloc.vec.Vec Write => v.val) othersStep
    (fun v k => v.length + (ws.length - k) ≤ Usize.max)
    (fun x => others_loop.body ws x.1 x.2) ?_ (alloc.vec.Vec.new Write) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_othersStep]; simp)
  · intro v j hj hv
    unfold others_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < ws.val.length := by scalar_tac
      rw [vec_index_eq ws j hlt']
      simp only [bind_ok]
      cases hw : ws.val[j.val] <;> simp only [bind_ok, write_clone'] <;> step* <;> simp only [FoldStep] <;>
        refine ⟨hlt', ?_, by scalar_tac, ?_⟩ <;>
        simp_all [othersStep, isPayW, isHistW, alloc.vec.Vec.length] <;> scalar_tac
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

@[simp] theorem changed_eq (p : Payment) (op : U64) (d : Bool) (m : U64) :
    changed p op d m = ok { p with operator := op, ledger_done := d, changes := m } := rfl

theorem othersL_length (ws : List Write) : (othersL ws).length ≤ ws.length := List.length_filter_le _ _

/-! ## What `logged` writes -/

/-- `logged`: the reply is kept, and the writes are the others, the
changed payment row and its entry. For a new payment (`subj c = none`) the
row is the one the command wrote, its id the counter, and `n = 0`; for an
existing payment `id`, the row as written (or as it was) with id `id`, and
`n` its old number of entries. -/
theorem logged_spec (a : Principal) (s : Snapshot) (c : Command) (ws : alloc.vec.Vec Write) (rep : Reply)
    (hl : ws.length + 2 ≤ Usize.max) :
    logged a s c ws rep ⦃ r => ∀ ws' rep', r = .Ok (ws', rep') → rep' = rep ∧
      ∃ (base : Payment) (n m : U64) (old : Option Payment),
        ((subj c = none ∧ lastPay ws.val = some base ∧ base.id = s.counter.next_id ∧ old = none ∧ n = 0#u64) ∨
         (∃ id o, subj c = some id ∧ findPay (Snapshot.toSt s) id.val = some o ∧ old = some o ∧
            base = (lastPay ws.val).getD o ∧ base.id = id ∧ n = o.changes)) ∧
        m.val = n.val + 1 ∧
        ws'.val = othersL ws.val ++ [.PutPayment (afterOf a c base m),
          .PutHistory (entryOf a s c n old (afterOf a c base m))] ⦄ := by
  have hol := othersL_length ws.val
  simp only [alloc.vec.Vec.length] at hl
  unfold logged
  simp only [what_of_eq, subject_eq, keeps_mark_eq, is_operator_eq, operator_of_eq, actor_id_eq, bind_ok]
  rw [eq_ok_of_spec (last_payment_spec ws)]
  simp only [bind_ok]
  rcases hw : whatOf s c with ⟨w1, w2, w3, w4⟩
  try dsimp only
  cases hs : subj c with
  | none =>
    cases hp : lastPay ws.val with
    | none => simp
    | some p =>
      by_cases hid : p.id = s.counter.next_id
      · have h0 : ¬ (0#u64 = core.num.U64.MAX) := by
          intro e; have := congrArg UScalar.val e; simp [core.num.U64.MAX, U64.rMax] at this
        simp only [hid, bne_self_eq_false, Bool.false_eq_true, if_false, h0]
        cases hm : isMark c <;> simp only [if_true, if_false, Bool.false_eq_true, bind_ok, changed_eq] <;> step*
        all_goals try (simp_all; scalar_tac)
        all_goals (intro ws' rep' h; obtain ⟨rfl, rfl⟩ := ok_inj h
                   refine ⟨rfl, p, 0#u64, i1, none, .inl (by first | exact ⟨hp, hid, rfl, rfl⟩ | exact ⟨hs, hp, hid, rfl, rfl⟩ | simp_all), by scalar_tac, ?_⟩
                   simp_all [afterOf, entryOf])
      · simp [hid]
  | some id =>
    try dsimp only
    rw [eq_ok_of_spec (find_payment_spec s.payments id)]
    simp only [bind_ok]
    cases ho : s.payments.val.find? (fun p => decide (p.id.val = id.val)) with
    | none => simp
    | some o =>
      try dsimp only
      have hfo : findPay (Snapshot.toSt s) id.val = some o := findPay_of (o := some o) (Or.inr ho.symm) rfl
      cases hlp : lastPay ws.val with
      | none =>
        try dsimp only
        by_cases hid : o.id = id
        · simp only [hid, bne_self_eq_false, Bool.false_eq_true, if_false, bind_ok]
          by_cases hmax : o.changes = core.num.U64.MAX
          · simp [hmax]
          simp only [hmax, if_false]
          have : o.changes.val + 1 ≤ U64.max := by
            have : o.changes.val ≠ U64.max := fun e => hmax (by simp only [core.num.U64.MAX, U64.rMax]; scalar_tac)
            scalar_tac
          cases hm : isMark c <;> simp only [if_true, if_false, Bool.false_eq_true, bind_ok, changed_eq] <;> step*
          all_goals try (simp_all; scalar_tac)
          all_goals (intro ws' rep' h; obtain ⟨rfl, rfl⟩ := ok_inj h
                     refine ⟨rfl, o, o.changes, i1, some o, .inr ⟨id, o, rfl, hfo, rfl, by simp, hid, rfl⟩, ?_, ?_⟩
                     · scalar_tac
                     · simp_all [afterOf, entryOf])
        · simp [hid]
      | some q =>
        try dsimp only
        by_cases hid : q.id = id
        · simp only [hid, bne_self_eq_false, Bool.false_eq_true, if_false, bind_ok]
          by_cases hmax : o.changes = core.num.U64.MAX
          · simp [hmax]
          simp only [hmax, if_false]
          have : o.changes.val + 1 ≤ U64.max := by
            have : o.changes.val ≠ U64.max := fun e => hmax (by simp only [core.num.U64.MAX, U64.rMax]; scalar_tac)
            scalar_tac
          cases hm : isMark c <;> simp only [if_true, if_false, Bool.false_eq_true, bind_ok, changed_eq] <;> step*
          all_goals try (simp_all; scalar_tac)
          all_goals (intro ws' rep' h; obtain ⟨rfl, rfl⟩ := ok_inj h
                     refine ⟨rfl, q, o.changes, i1, some o, .inr ⟨id, o, rfl, hfo, rfl, by simp, hid, rfl⟩, ?_, ?_⟩
                     · scalar_tac
                     · simp_all [afterOf, entryOf])
        · simp [hid]

/-- A successful `transition` is a successful `decide_command`: the same
when it writes nothing, else with `logged`'s writes. -/
theorem transition_ok (a : Principal) (s : Snapshot) (c : Command) ws r
    (h : transition a s c = ok (.Ok (ws, r))) :
    ∃ ws0 r0, decide_command a s c = ok (.Ok (ws0, r0)) ∧
      ((ws0.val = [] ∧ ws = ws0 ∧ r = r0) ∨ (ws0.val ≠ [] ∧ logged a s c ws0 r0 = ok (.Ok (ws, r)))) := by
  unfold transition at h
  obtain ⟨r0, hr0, h⟩ := bind_tc_eq_ok.1 h
  rcases r0 with ⟨ws0, rep0⟩ | e
  · by_cases hz : ws0.len = 0#usize
    · change (if ws0.len = 0#usize then ok (core.result.Result.Ok (ws0, rep0)) else logged a s c ws0 rep0) = _ at h
      simp only [hz, if_true, Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h
      refine ⟨ws0, rep0, hr0, .inl ⟨?_, h.1.symm, h.2.symm⟩⟩
      have := congrArg UScalar.val hz
      simpa [alloc.vec.Vec.len] using this
    · change (if ws0.len = 0#usize then ok (core.result.Result.Ok (ws0, rep0)) else logged a s c ws0 rep0) = _ at h
      simp only [hz, if_false] at h
      have hne : ws0.val ≠ [] := by
        intro e; apply hz; apply UScalar.eq_of_val_eq; simp [alloc.vec.Vec.len, e]
      exact ⟨ws0, rep0, hr0, .inr ⟨hne, h⟩⟩
  · simp at h

/-! ## The write sets `decide_command` makes -/

theorem mem_othersL {w : Write} {ws : List Write} :
    w ∈ othersL ws ↔ w ∈ ws ∧ isPayW w = false ∧ isHistW w = false := by
  simp only [othersL, List.mem_filter, Bool.not_eq_true', Bool.or_eq_false_iff]

theorem lastPay_cons_pay (p : Payment) (ws : List Write) :
    lastPay (.PutPayment p :: ws) = (lastPay ws).or (some p) := by
  simp only [lastPay, List.foldl_cons, lastPayStep]; exact lastPay_foldl _ _

theorem lastPay_mem {ws : List Write} {p : Payment} (h : lastPay ws = some p) : Write.PutPayment p ∈ ws := by
  induction ws with
  | nil => simp [lastPay] at h
  | cons w ws ih =>
    cases w with
    | PutPayment q =>
      rw [lastPay_cons_pay] at h
      cases hl : lastPay ws with
      | none => rw [hl] at h; simp at h; simp [h]
      | some r => rw [hl] at h; simp at h; subst h; exact List.mem_cons_of_mem _ (ih hl)
    | _ =>
      simp only [lastPay, List.foldl_cons, lastPayStep] at h
      exact List.mem_cons_of_mem _ (ih h)

theorem lastPay_none {ws : List Write} (h : lastPay ws = none) : ∀ p, Write.PutPayment p ∉ ws := by
  induction ws with
  | nil => simp
  | cons w ws ih =>
    cases w with
    | PutPayment q => rw [lastPay_cons_pay] at h; cases lastPay ws <;> simp at h
    | _ =>
      simp only [lastPay, List.foldl_cons, lastPayStep] at h
      intro p hp
      simp only [List.mem_cons, reduceCtorEq, false_or] at hp
      exact ih h p hp

/-- At most three writes, no history row, and at most one payment row. -/
def Shape (ws : List Write) : Prop :=
  ws.length ≤ 3 ∧ (∀ w ∈ ws, isHistW w = false) ∧
    (∀ p q, Write.PutPayment p ∈ ws → Write.PutPayment q ∈ ws → p = q)

theorem shape_nil : Shape [] := by simp [Shape]

theorem shape_step {op : U64} {p : Payment} {ws : List Write} (h : StepWrites op p ws) : Shape ws := by
  rcases h with rfl | ⟨p', rfl, _⟩
  · exact shape_nil
  · refine ⟨by simp, by simp [isHistW], ?_⟩; intro x y hx hy; simp at hx hy; rw [hx, hy]

theorem shape_record {s : St} {op : U64} {id : Nat} {ws : List Write} (h : RecordWrites s op id ws) : Shape ws := by
  rcases h with rfl | ⟨p, _, hs | ⟨refund, rfl | ⟨rf, rf', p', _, _, rfl, _⟩⟩⟩
  · exact shape_nil
  · exact shape_step hs
  · exact shape_nil
  · refine ⟨by simp, by simp [isHistW], ?_⟩; intro x y hx hy; simp at hx hy; rw [hx, hy]

theorem decide_shape (a : Principal) (s : Snapshot) (c : Command) ws r
    (h : decide_command a s c = ok (.Ok (ws, r))) : Shape ws.val := by
  cases c with
  | Create amt cur mc =>
    obtain ⟨n, hn, q0, m0, _, _, hws⟩ := post_of_ok (create_spec a s amt cur mc) h ws r rfl
    rw [hws]; refine ⟨by simp, by simp [isHistW], ?_⟩; intro x y hx hy; simp at hx hy; rw [hx, hy]
  | BeginAuthorize id =>
    obtain ⟨p, _, hw⟩ := post_of_ok (begin_authorize_spec a s id) h ws r rfl; exact shape_step hw
  | Cancel id =>
    obtain ⟨p, _, hw⟩ := post_of_ok (cancel_spec a s id) h ws r rfl; exact shape_step hw
  | RequestRefund id amt =>
    obtain ⟨p, p', n, _, _, _, _, _, hws⟩ := post_of_ok (request_refund_spec a s id amt) h ws r rfl
    rw [hws]; refine ⟨by simp, by simp [isHistW], ?_⟩; intro x y hx hy; simp at hx hy; rw [hx, hy]
  | Record id res => exact shape_record (post_of_ok (record_spec a s id res) h ws r rfl)
  | Event ev id res =>
    rcases post_of_ok (event_spec a s ev id res) h ws r rfl with ⟨_, hws, _⟩ | ⟨_, w0, hw0, hws⟩
    · rw [hws]; exact shape_nil
    · rw [hws]
      obtain ⟨h1, h2, h3⟩ := shape_record hw0
      have := record_len hw0
      refine ⟨by simp; omega, ?_, ?_⟩
      · intro w hw; rcases List.mem_append.1 hw with hw | hw
        · exact h2 w hw
        · simp at hw; subst hw; rfl
      · intro x y hx hy
        rcases List.mem_append.1 hx with hx | hx <;> rcases List.mem_append.1 hy with hy | hy
        · exact h3 x y hx hy
        all_goals simp at hx hy
  | AttachRef pay ref =>
    rcases (attach_ref_writes a s pay ref ws r h).2 with hws | ⟨x, hws, _⟩
    · rw [hws]; exact shape_nil
    · rw [hws]; refine ⟨by simp, by simp [isHistW], ?_⟩; intro x y hx hy; simp at hx hy
  | Get id => rw [get_writes a s id ws r h]; exact shape_nil
  | List m f l => rw [list_writes a s m f l ws r h]; exact shape_nil
  | GetRefund id => rw [get_refund_writes a s id ws r h]; exact shape_nil
  | Refunds id => rw [refunds_of_writes a s id ws r h]; exact shape_nil
  | GetRef id => rw [get_ref_writes a s id ws r h]; exact shape_nil
  | View id => rw [view_writes a s id ws r h]; exact shape_nil
  | MarkPosted id cap seen =>
    rcases (post_of_ok (mark_posted_spec a s id cap seen) h ws r rfl).2 with hws | ⟨p, _, _, _, _, _, hws⟩
    · rw [hws]; exact shape_nil
    · rw [hws]; refine ⟨by simp, by simp [isHistW], ?_⟩; intro x y hx hy; simp at hx hy; rw [hx, hy]
  | History p f l => rw [history_page_writes a s p f l ws r h]; exact shape_nil
  | Range f l => rw [range_page_writes a s f l ws r h]; exact shape_nil
  | RefundRange f l => rw [refund_range_page_writes a s f l ws r h]; exact shape_nil

/-- **What a successful `transition` writes.** Nothing (the command wrote
nothing), or what `decide_command` wrote without its payment row, then the
changed payment row and its history entry. -/
theorem transition_writes (a : Principal) (s : Snapshot) (c : Command) ws r
    (h : transition a s c = ok (.Ok (ws, r))) :
    ∃ ws0, decide_command a s c = ok (.Ok (ws0, r)) ∧ Shape ws0.val ∧
      ((ws0.val = [] ∧ ws = ws0) ∨
       (ws0.val ≠ [] ∧ ∃ (base : Payment) (n m : U64) (old : Option Payment),
        ((subj c = none ∧ lastPay ws0.val = some base ∧ base.id = s.counter.next_id ∧ old = none ∧ n = 0#u64) ∨
         (∃ id o, subj c = some id ∧ findPay (Snapshot.toSt s) id.val = some o ∧ old = some o ∧
            base = (lastPay ws0.val).getD o ∧ base.id = id ∧ n = o.changes)) ∧
        m.val = n.val + 1 ∧
        ws.val = othersL ws0.val ++ [.PutPayment (afterOf a c base m),
          .PutHistory (entryOf a s c n old (afterOf a c base m))])) := by
  obtain ⟨ws0, r0, hd, hc⟩ := transition_ok a s c ws r h
  have hsh := decide_shape a s c ws0 r0 hd
  rcases hc with ⟨he, rfl, rfl⟩ | ⟨hne, hl⟩
  · exact ⟨ws, hd, hsh, .inl ⟨he, rfl⟩⟩
  · have hlen : ws0.length + 2 ≤ Usize.max := by
      have := hsh.1; simp only [alloc.vec.Vec.length]; scalar_tac
    obtain ⟨hrep, rest⟩ := post_of_ok (logged_spec a s c ws0 r0 hlen) hl ws r rfl
    subst hrep
    exact ⟨ws0, hd, hsh, .inr ⟨hne, rest⟩⟩

end payment_domain.Logged
