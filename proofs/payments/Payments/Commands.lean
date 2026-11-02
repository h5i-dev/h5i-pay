import Payments.Spec
import PaymentsSchema
/-!
# What each payment command writes

Read off the extracted code with `step*`: per command, the shape of the
write set and the facts that made the command succeed.
-/
open Aeneas Aeneas.Std Result payment_domain payment_domain.Spec payment_domain.Schema H5iAppLib

namespace payment_domain.Commands

attribute [local simp] H5iAppLib.u64_val_eq

/-- `u64_val_eq` for the `u32` columns (statuses, currency); h5i_app_lib has
only the `U64` version. -/
theorem u32_val_eq (x y : U32) : x.val = y.val ↔ x = y :=
  ⟨fun h => by scalar_tac, fun h => h ▸ rfl⟩

attribute [local simp] u32_val_eq

@[step] theorem one_spec (a : Write) : one a ⦃ v => v.val = [a] ⦄ := by unfold one; step*
@[step] theorem two_spec (a b : Write) : two a b ⦃ v => v.val = [a, b] ⦄ := by unfold two; step*
@[step] theorem three_spec (a b c : Write) : three a b c ⦃ v => v.val = [a, b, c] ⦄ := by unfold three; step*

@[step] theorem with_status_spec (p : Payment) (st : U32) (a c op : U64) :
    with_status p st a c op ⦃ q => q = { p with status := st, authorized := a, captured := c, operator := op, ledger_done := false } ⦄ := by
  unfold with_status; step*

@[step] theorem with_refunds_spec (p : Payment) (r q op : U64) :
    with_refunds p r q op ⦃ x => x = { p with refunded := r, refund_pending := q, operator := op, ledger_done := false } ⦄ := by
  unfold with_refunds; step*

@[step] theorem find_payment_spec (v : alloc.vec.Vec Payment) (id : U64) :
    find_payment v id ⦃ o => o = v.val.find? (fun p => p.id.val = id.val) ⦄ := by
  unfold find_payment find_payment_loop
  apply WP.spec_mono (loop_search v.val (fun p => decide (p.id = id)) (fun o : Option Payment => o)
    (fun _ p => some p) none _ ?_ 0#usize (by simp))
  · intro r hr; rw [search_find _ _ _ hr]; simp
  · intro j hj; unfold find_payment_loop.body; h5i_step

@[step] theorem find_refund_spec (v : alloc.vec.Vec Refund) (id : U64) :
    find_refund v id ⦃ o => o = v.val.find? (fun r => r.id.val = id.val) ⦄ := by
  unfold find_refund find_refund_loop
  apply WP.spec_mono (loop_search v.val (fun r => decide (r.id = id)) (fun o : Option Refund => o)
    (fun _ r => some r) none _ ?_ 0#usize (by simp))
  · intro r hr; rw [search_find _ _ _ hr]; simp
  · intro j hj; unfold find_refund_loop.body; h5i_step

@[step] theorem find_event_spec (v : alloc.vec.Vec Event) (id : U64) :
    find_event v id ⦃ o => o = v.val.find? (fun e => e.id.val = id.val) ⦄ := by
  unfold find_event find_event_loop
  apply WP.spec_mono (loop_search v.val (fun e => decide (e.id = id)) (fun o : Option Event => o)
    (fun _ e => some e) none _ ?_ 0#usize (by simp))
  · intro r hr; rw [search_find _ _ _ hr]; simp
  · intro j hj; unfold find_event_loop.body; h5i_step

@[step] theorem may_manage_spec (actor : Principal) (p : Payment) :
    may_manage actor p ⦃ b => b = true ↔ (actor.operator = true ∨ p.merchant = actor.user) ⦄ := by
  unfold may_manage
  simp only [is_operator_eq, user_of_eq, bind_tc_ok, bind_ok]
  cases h : actor.operator <;> simp

theorem findPay_eq (s : Snapshot) (id : U64) :
    s.payments.val.find? (fun p => p.id.val = id.val) = findPay (Snapshot.toSt s) id.val := rfl
theorem findRefund_eq (s : Snapshot) (id : U64) :
    s.refunds.val.find? (fun r => r.id.val = id.val) = findRefund (Snapshot.toSt s) id.val := rfl
theorem findEvent_eq (s : Snapshot) (id : U64) :
    s.events.val.find? (fun e => e.id.val = id.val) = findEvent (Snapshot.toSt s) id.val := rfl

theorem findPay_of {s : Snapshot} {id : U64} {o : Option Payment} {p : Payment}
    (h : o = s.payments.val.find? (fun p => decide (p.id = id)) ∨ o = s.payments.val.find? (fun p => decide (p.id.val = id.val)))
    (h2 : o = some p) : findPay (Snapshot.toSt s) id.val = some p := by
  rcases h with h | h <;> rw [← h2, h] <;> simp [findPay, Snapshot.toSt]
theorem findRefund_of {s : Snapshot} {id : U64} {o : Option Refund} {r : Refund}
    (h : o = s.refunds.val.find? (fun r => decide (r.id = id)) ∨ o = s.refunds.val.find? (fun r => decide (r.id.val = id.val)))
    (h2 : o = some r) : findRefund (Snapshot.toSt s) id.val = some r := by
  rcases h with h | h <;> rw [← h2, h] <;> simp [findRefund, Snapshot.toSt]
theorem findEvent_of {s : Snapshot} {id : U64} {o : Option Event}
    (h : o = s.events.val.find? (fun r => decide (r.id = id)) ∨ o = s.events.val.find? (fun r => decide (r.id.val = id.val))) :
    findEvent (Snapshot.toSt s) id.val = o := by
  rcases h with h | h <;> rw [h] <;> simp [findEvent, Snapshot.toSt]

/-- Writes that change one existing payment by a `Step`, recording
operator `op` on it and clearing its ledger marker. -/
def StepWrites (op : U64) (p : Payment) (ws : List Write) : Prop :=
  ws = [] ∨ ∃ p', ws = [.PutPayment p'] ∧ Step p p' ∧ p'.operator = op ∧ p'.ledger_done = false

/-! ## Recording PSP results -/

@[step] theorem record_authorized_spec (p : Payment) (amt op : U64) :
    record_authorized p amt op ⦃ r => ∀ ws rep, r = .Ok (ws, rep) → StepWrites op p ws.val ⦄ := by
  unfold record_authorized
  step*
  all_goals (intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h)
  all_goals first
    | (left; rfl)
    | (right; refine ⟨p1, by simp_all, ?_, by simp_all, by simp_all⟩
       simp only [PROCESSING, AUTHORIZED] at *
       subst_vars
       refine ⟨rfl, rfl, rfl, rfl, .inr (by simp_all [Allowed]), ?_⟩
       simp only [PayInv]; intro h; simp_all <;> scalar_tac)

@[step] theorem record_declined_spec (p : Payment) (op : U64) :
    record_declined p op ⦃ r => ∀ ws rep, r = .Ok (ws, rep) → StepWrites op p ws.val ⦄ := by
  unfold record_declined
  step*
  all_goals (intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h)
  all_goals first
    | (left; rfl)
    | (right; refine ⟨p1, by simp_all, ?_, by simp_all, by simp_all⟩
       simp only [PROCESSING, FAILED] at *
       subst_vars
       refine ⟨rfl, rfl, rfl, rfl, .inr (by simp_all [Allowed]), ?_⟩
       simp only [PayInv]; intro h; simp_all <;> scalar_tac)

@[step] theorem record_captured_spec (p : Payment) (amt op : U64) :
    record_captured p amt op ⦃ r => ∀ ws rep, r = .Ok (ws, rep) → StepWrites op p ws.val ⦄ := by
  unfold record_captured
  step*
  all_goals (intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h)
  all_goals first
    | (left; rfl)
    | (right; refine ⟨p1, by simp_all, ?_, by simp_all, by simp_all⟩
       simp only [PROCESSING, AUTHORIZED, CAPTURED] at *
       subst_vars
       refine ⟨rfl, rfl, rfl, rfl, .inr (by simp_all [Allowed]), ?_⟩
       simp only [PayInv]; intro h; simp_all <;> scalar_tac)

@[step] theorem record_canceled_spec (p : Payment) (op : U64) :
    record_canceled p op ⦃ r => ∀ ws rep, r = .Ok (ws, rep) → StepWrites op p ws.val ⦄ := by
  unfold record_canceled
  step*
  all_goals (intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h)
  all_goals first
    | (left; rfl)
    | (right; refine ⟨p1, by simp_all, ?_, by simp_all, by simp_all⟩
       simp only [CREATED, PROCESSING, AUTHORIZED, CANCELED] at *
       subst_vars
       refine ⟨rfl, rfl, rfl, rfl, .inr (by simp_all [Allowed]), ?_⟩
       simp only [PayInv]; intro h; simp_all <;> scalar_tac)

/-- Writes that settle a refund row of payment `p` and change `p` by a
`Step`, recording operator `op` on both. -/
def RefundWrites (s : St) (op : U64) (p : Payment) (refund : Nat) (ws : List Write) : Prop :=
  ws = [] ∨ ∃ rf rf' p', findRefund s refund = some rf ∧ rf'.id = rf.id ∧
    ws = [.PutRefund rf', .PutPayment p'] ∧ Step p p' ∧ rf'.operator = op ∧ p'.operator = op ∧
    rf'.payment = p'.id ∧ p'.ledger_done = false

@[step] theorem record_refund_spec (s : Snapshot) (p : Payment) (refund : U64) (b : Bool) (op : U64) :
    record_refund s p refund b op ⦃ r => ∀ ws rep, r = .Ok (ws, rep) →
      RefundWrites (Snapshot.toSt s) op p refund.val ws.val ⦄ := by
  unfold record_refund
  step*
  all_goals try (simp only [core.num.U64.MAX, U64.rMax] at *; scalar_tac)
  all_goals (intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h)
  all_goals first
    | exact Or.inl rfl
    | (right
       refine ⟨r, _, p2, findRefund_of (Or.inr o_post) ‹_›, ?_, v_post, ?_⟩
       · rfl
       simp only [REFUND_PENDING] at *
       subst_vars
       refine ⟨⟨rfl, rfl, rfl, rfl, .inl rfl, ?_⟩, by simp_all, by simp_all, by simp_all, by simp_all⟩
       simp only [PayInv]; intro h; simp_all <;> scalar_tac)

/-- What recording a PSP result writes: nothing, or one `Step` of the
payment (and, for a refund result, the refund row). -/
def RecordWrites (s : St) (op : U64) (id : Nat) (ws : List Write) : Prop :=
  ws = [] ∨ ∃ p, findPay s id = some p ∧ (StepWrites op p ws ∨ ∃ refund, RefundWrites s op p refund ws)

@[step] theorem record_spec (a : Principal) (s : Snapshot) (id : U64) (res : PspResult) :
    record a s id res ⦃ r => ∀ ws rep, r = .Ok (ws, rep) → RecordWrites (Snapshot.toSt s) a.opId id.val ws.val ⦄ := by
  unfold record
  simp only [is_operator_eq, operator_of_eq, bind_ok]
  step*
  all_goals first
    | exact Or.inr ⟨p, findPay_of (Or.inr o_post) ‹_›, Or.inl (r_post _ _ r_post1)⟩
    | exact Or.inr ⟨p, findPay_of (Or.inr o_post) ‹_›, Or.inr ⟨_, r_post _ _ r_post1⟩⟩
    | (intro ws rep h; simp_all)

theorem write_clone (w : Write) : Write.Insts.CoreCloneClone.clone w = ok w := by
  cases w <;> simp [Write.Insts.CoreCloneClone.clone, Payment.clone_eq, Refund.clone_eq, Event.clone_eq,
    PspRef.clone_eq, Counter.clone_eq, MerchantCounter.clone_eq, HistoryEntry.clone_eq]

@[step] theorem append_spec (ws more : alloc.vec.Vec Write) (h : ws.length + more.length ≤ Usize.max) :
    append ws more ⦃ v => v.val = ws.val ++ more.val ⦄ := by
  unfold append append_loop
  apply WP.spec_mono (loop_fold more.val (fun v : alloc.vec.Vec Write => v.val) (fun acc x => acc ++ [x])
    (fun v k => v.length + (more.length - k) ≤ Usize.max)
    (fun x => append_loop.body more x.1 x.2) ?_ ws 0#usize (by simp) (by simpa using h))
  · intro r hr; rw [hr, foldl_snoc]; simp
  · intro v j hj hv
    unfold append_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    dsimp only
    split
    · rename_i hlt
      step
      rw [write_clone]
      simp only [bind_ok]
      step*
      simp only [FoldStep]
      have : j.val < more.val.length := by scalar_tac
      refine ⟨this, ?_, by scalar_tac, ?_⟩
      · simp_all
      · simp only [alloc.vec.Vec.length] at *; scalar_tac
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem record_len {s : St} {op : U64} {id : Nat} {ws : List Write} (h : RecordWrites s op id ws) : ws.length ≤ 2 := by
  rcases h with rfl | ⟨p, _, (rfl | ⟨_, rfl, _⟩) | ⟨_, rfl | ⟨_, _, _, _, _, rfl, _⟩⟩⟩ <;> simp

/-- A webhook: a seen event writes nothing; otherwise the record's writes
and the event row, or only the event row if the result is stale. -/
theorem event_spec (a : Principal) (s : Snapshot) (ev id : U64) (res : PspResult) :
    event a s ev id res ⦃ r => ∀ ws rep, r = .Ok (ws, rep) →
      (findEvent (Snapshot.toSt s) ev.val ≠ none ∧ ws.val = [] ∧ rep = .Duplicate) ∨
      (findEvent (Snapshot.toSt s) ev.val = none ∧ ∃ w0, RecordWrites (Snapshot.toSt s) a.opId id.val w0 ∧
        ws.val = w0 ++ [.PutEvent ⟨ev, id, a.opId⟩]) ⦄ := by
  unfold event
  simp only [is_operator_eq, operator_of_eq, bind_ok]
  step*
  · rename_i hr
    obtain ⟨w0, rep0⟩ := p
    have hw := r_post w0 rep0 hr
    have hlen := record_len hw
    show WP.spec ((do let out ← append (alloc.vec.Vec.new Write) w0; let out1 ← out.push (Write.PutEvent { id := ev, payment := id, operator := a.opId }); ok (core.result.Result.Ok (out1, rep0))) : Result _) _
    step*
    intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h
    right
    exact ⟨by rw [findEvent_of (Or.inr o_post)]; assumption, w0.val, hw, by simp_all⟩
  · intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h
    right; exact ⟨by rw [findEvent_of (Or.inr o_post)]; assumption, [], Or.inl rfl, by simp_all⟩
  · intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h
    left; exact ⟨by rw [findEvent_of (Or.inr o_post), ‹o = some _›]; simp, rfl, rfl⟩

/-! ## Merchant commands -/

theorem searchFrom_getD {α β : Type} (l : List α) (P : α → Bool) (f : α → β) (d : β) (k : Nat) :
    searchFrom l P (fun _ x => f x) d k = (((l.drop k).find? P).map f).getD d := by
  induction h : l.length - k generalizing k with
  | zero => rw [searchFrom_end (by omega), List.drop_eq_nil_of_le (by omega)]; rfl
  | succ n ih =>
    have hk : k < l.length := by omega
    rw [List.drop_eq_getElem_cons hk, List.find?_cons]
    by_cases hp : P l[k]
    · rw [searchFrom_found hk hp]; simp [hp]
    · rw [searchFrom_skip hk hp, ih (k + 1) (by omega)]; simp [hp]

/-- The merchant's payment count (0 if it has no counter). -/
def merchantNext (l : List MerchantCounter) (m : U64) : U64 :=
  ((l.find? (fun c => decide (c.merchant = m))).map (·.next)).getD 0#u64

@[step] theorem merchant_next_spec (v : alloc.vec.Vec MerchantCounter) (m : U64) :
    merchant_next v m ⦃ n => n = merchantNext v.val m ⦄ := by
  unfold merchant_next merchant_next_loop
  apply WP.spec_mono (loop_search v.val (fun c => decide (c.merchant = m)) (fun n : U64 => n)
    (fun _ c => c.next) 0#u64 _ ?_ 0#usize (by simp))
  · intro r hr; rw [hr, searchFrom_getD]; simp [merchantNext]
  · intro j hj; unfold merchant_next_loop.body; h5i_step

theorem create_spec (a : Principal) (s : Snapshot) (amt : U64) (cur : U32) (mc : Bool) :
    create a s amt cur mc ⦃ r => ∀ ws rep, r = .Ok (ws, rep) → ∃ n : U64, n.val = s.counter.next_id.val + 1 ∧
      ∃ q m : U64, q = merchantNext s.merchants.val a.user ∧ m.val = q.val + 1 ∧
      ws.val = [.PutPayment ⟨s.counter.next_id, a.user, amt, cur, CREATED, mc, 0#u64, 0#u64, 0#u64, 0#u64, 0#u64, a.opId,
        false, q, 0#u64⟩, .SetCounter ⟨n⟩, .PutMerchant ⟨a.user, m⟩] ⦄ := by
  unfold create
  simp only [user_of_eq, operator_of_eq, bind_tc_ok, bind_ok]
  step*
  all_goals try (simp only [core.num.U64.MAX, U64.rMax] at *; scalar_tac)
  all_goals (intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h; exact ⟨_, by assumption, _, _, by assumption, by assumption, v_post⟩)

theorem begin_authorize_spec (a : Principal) (s : Snapshot) (id : U64) :
    begin_authorize a s id ⦃ r => ∀ ws rep, r = .Ok (ws, rep) →
      ∃ p, findPay (Snapshot.toSt s) id.val = some p ∧ StepWrites a.opId p ws.val ⦄ := by
  unfold begin_authorize
  simp only [operator_of_eq, bind_tc_ok, bind_ok]
  step*
  all_goals try (simp only [core.num.U64.MAX, U64.rMax] at *; scalar_tac)
  intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h
  refine ⟨p, findPay_of (Or.inr o_post) ‹_›, Or.inr ⟨_, v_post, ⟨rfl, rfl, rfl, rfl, .inr ?_, ?_⟩, rfl, rfl⟩⟩
  · simp only [CREATED, PROCESSING] at *; simp_all [Allowed]
  · simp only [PayInv, CREATED, PROCESSING] at *; intro h; simp_all

theorem cancel_spec (a : Principal) (s : Snapshot) (id : U64) :
    cancel a s id ⦃ r => ∀ ws rep, r = .Ok (ws, rep) →
      ∃ p, findPay (Snapshot.toSt s) id.val = some p ∧ StepWrites a.opId p ws.val ⦄ := by
  unfold cancel
  simp only [operator_of_eq, bind_tc_ok, bind_ok]
  step*
  all_goals (intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h)
  · exact ⟨p, findPay_of (Or.inr o_post) ‹_›, Or.inl rfl⟩
  · refine ⟨p, findPay_of (Or.inr o_post) ‹_›, Or.inr ⟨p2, v_post, ?_, by simp_all, by simp_all⟩⟩
    simp only [CREATED, CANCELED] at *
    subst_vars
    refine ⟨rfl, rfl, rfl, rfl, .inr (by simp_all [Allowed]), ?_⟩
    simp only [PayInv]; intro h; simp_all

theorem request_refund_spec (a : Principal) (s : Snapshot) (id amt : U64) :
    request_refund a s id amt ⦃ r => ∀ ws rep, r = .Ok (ws, rep) →
      ∃ p p' n, findPay (Snapshot.toSt s) id.val = some p ∧ Step p p' ∧ p'.operator = a.opId ∧
        p'.ledger_done = false ∧
        n.val = s.counter.next_id.val + 1 ∧
        ws.val = [.PutRefund ⟨s.counter.next_id, p.id, amt, REFUND_PENDING, a.opId⟩, .PutPayment p', .SetCounter ⟨n⟩] ⦄ := by
  unfold request_refund
  simp only [operator_of_eq, bind_tc_ok, bind_ok]
  step*
  all_goals try (simp only [core.num.U64.MAX, U64.rMax] at *; scalar_tac)
  intro ws rep h; obtain ⟨rfl, rfl⟩ := ok_inj h
  refine ⟨p, p2, i4, findPay_of (Or.inr o_post) ‹_›, ?_, by simp_all, by simp_all, i4_post, v_post⟩
  simp only [CAPTURED] at *
  subst_vars
  refine ⟨rfl, rfl, rfl, rfl, .inl rfl, ?_⟩
  simp only [PayInv]; intro h; simp_all

/-! ## Commands that write no payment -/

theorem attach_ref_writes (a : Principal) (s : Snapshot) (pay : U64) (ref : alloc.vec.Vec U8) ws r
    (h : attach_ref a s pay ref = ok (.Ok (ws, r))) :
    a.operator = true ∧ (ws.val = [] ∨ ∃ x, ws.val = [.PutRef x] ∧ x.operator = a.opId) := by
  unfold attach_ref at h
  simp only [is_operator_eq, operator_of_eq, bind_ok] at h
  split at h
  · rename_i hop
    refine ⟨hop, ?_⟩
    obtain ⟨o, -, h⟩ := bind_tc_eq_ok.1 h
    split at h
    · simp at h
    · obtain ⟨o1, -, h⟩ := bind_tc_eq_ok.1 h
      split at h
      · obtain ⟨v, -, h⟩ := bind_tc_eq_ok.1 h
        obtain ⟨v1, hv1, h⟩ := bind_tc_eq_ok.1 h
        simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h
        rw [← h.1]
        have hv :=
          post_of_ok (one_spec (Write.PutRef { payment := pay, reference := v, operator := a.opId })) hv1
        exact Or.inr ⟨_, hv, rfl⟩
      · obtain ⟨b, -, h⟩ := bind_tc_eq_ok.1 h
        split at h
        · simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h
          rw [← h.1]; exact Or.inl rfl
        · simp at h
  · simp at h

theorem get_writes (a : Principal) (s : Snapshot) (id : U64) ws r
    (h : get a s id = ok (.Ok (ws, r))) : ws.val = [] := by
  unfold get at h
  obtain ⟨o, -, h⟩ := bind_tc_eq_ok.1 h
  split at h
  · simp at h
  · obtain ⟨b, -, h⟩ := bind_tc_eq_ok.1 h
    split at h
    · simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h; rw [← h.1]; rfl
    · simp at h

theorem get_refund_writes (a : Principal) (s : Snapshot) (id : U64) ws r
    (h : get_refund a s id = ok (.Ok (ws, r))) : ws.val = [] := by
  unfold get_refund at h
  obtain ⟨o, -, h⟩ := bind_tc_eq_ok.1 h
  split at h
  · simp at h
  · obtain ⟨o1, -, h⟩ := bind_tc_eq_ok.1 h
    split at h
    · simp at h
    · obtain ⟨b, -, h⟩ := bind_tc_eq_ok.1 h
      split at h
      · simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h; rw [← h.1]; rfl
      · simp at h

theorem get_ref_writes (a : Principal) (s : Snapshot) (id : U64) ws r
    (h : get_ref a s id = ok (.Ok (ws, r))) : ws.val = [] := by
  unfold get_ref at h
  obtain ⟨o, -, h⟩ := bind_tc_eq_ok.1 h
  split at h
  · simp at h
  · obtain ⟨b, -, h⟩ := bind_tc_eq_ok.1 h
    split at h
    · obtain ⟨o1, -, h⟩ := bind_tc_eq_ok.1 h
      split at h
      · simp at h
      · simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h; rw [← h.1]; rfl
    · simp at h

theorem refunds_of_writes (a : Principal) (s : Snapshot) (id : U64) ws r
    (h : refunds_of a s id = ok (.Ok (ws, r))) : ws.val = [] := by
  unfold refunds_of at h
  obtain ⟨o, -, h⟩ := bind_tc_eq_ok.1 h
  split at h
  · simp at h
  · obtain ⟨b, -, h⟩ := bind_tc_eq_ok.1 h
    split at h
    · obtain ⟨out, -, h⟩ := bind_tc_eq_ok.1 h
      simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h; rw [← h.1]; rfl
    · simp at h

theorem view_writes (a : Principal) (s : Snapshot) (id : U64) ws r
    (h : view a s id = ok (.Ok (ws, r))) : ws.val = [] := by
  unfold view at h
  obtain ⟨o, -, h⟩ := bind_tc_eq_ok.1 h
  split at h
  · simp at h
  · obtain ⟨b, -, h⟩ := bind_tc_eq_ok.1 h
    split at h
    · obtain ⟨o1, -, h⟩ := bind_tc_eq_ok.1 h
      obtain ⟨rf, -, h⟩ := bind_tc_eq_ok.1 h
      obtain ⟨out, -, h⟩ := bind_tc_eq_ok.1 h
      simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h; rw [← h.1]; rfl
    · simp at h

/-! ## The ledger marker (F2) -/

theorem vec_index_eq {α : Type} (v : alloc.vec.Vec α) (i : Usize) (h : i.val < v.val.length) :
    alloc.vec.Vec.index (core.slice.index.SliceIndexUsizeSlice α) v i = ok v.val[i.val] := by
  rw [alloc.vec.Vec.index_slice_index]; unfold alloc.vec.Vec.index_usize
  simp [h]

def refundsStep (id : U64) (acc : List Refund) (x : Refund) : List Refund :=
  if x.payment = id then acc ++ [x] else acc

theorem foldl_refundsStep (id : U64) (l acc : List Refund) :
    l.foldl (refundsStep id) acc = acc ++ l.filter (fun x => decide (x.payment = id)) := by
  induction l generalizing acc with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, refundsStep, List.filter_cons]
    split <;> simp_all

@[step] theorem refunds_loop_spec (v : alloc.vec.Vec Refund) (id : U64) :
    refunds_for v id ⦃ out => out.val = v.val.filter (fun x => decide (x.payment = id)) ⦄ := by
  unfold refunds_for refunds_for_loop
  have h := loop_fold v.val (fun w : alloc.vec.Vec Refund => w.val) (refundsStep id)
    (fun w k => w.length + (v.length - k) ≤ Usize.max)
    (fun x => refunds_for_loop.body v id x.1 x.2) ?_ (alloc.vec.Vec.new Refund) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_refundsStep]; simp)
  · intro w j hj hw
    unfold refunds_for_loop.body
    simp only [alloc.vec.Vec.length] at hj hw
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < v.val.length := by scalar_tac
      rw [vec_index_eq v j hlt']
      simp only [bind_ok]
      by_cases hb : v.val[j.val].payment = id
      · simp only [hb, if_true]
        step*
        simp only [FoldStep]
        refine ⟨hlt', ?_, by scalar_tac, ?_⟩
        · simp_all [refundsStep]
        · simp only [alloc.vec.Vec.length] at *; scalar_tac
      · simp only [hb, if_false, bind_ok]
        step*
        simp only [FoldStep]
        refine ⟨hlt', ?_, by scalar_tac, ?_⟩
        · show w.val = refundsStep id w.val v.val[j.val]
          simp only [refundsStep, if_neg hb]
        · simp only [alloc.vec.Vec.length] at *; scalar_tac
    · rename_i hge
      simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem same_refund_eq (x y : Refund) : same_refund x y = ok (decide (SameRefund x y)) := by
  unfold same_refund SameRefund
  by_cases h1 : x.id = y.id <;> by_cases h2 : x.payment = y.payment <;> by_cases h3 : x.amount = y.amount <;>
    simp [h1, h2, h3]

/-- `has_same` as a function on lists. -/
def hasSameB (ys : List Refund) (x : Refund) : Bool :=
  match ys.find? (fun r => r.id.val = x.id.val) with
  | none => false
  | some y => decide (SameRefund x y)

theorem has_same_eq (ys : alloc.vec.Vec Refund) (x : Refund) : has_same ys x = ok (hasSameB ys.val x) := by
  unfold has_same
  rw [eq_ok_of_spec (find_refund_spec ys x.id)]
  simp only [bind_ok, hasSameB]
  cases ys.val.find? (fun r => r.id.val = x.id.val) <;> simp [same_refund_eq]

theorem foldl_and (f : Refund → Bool) (l : List Refund) (b : Bool) :
    l.foldl (fun acc x => acc && f x) b = (b && l.all f) := by
  induction l generalizing b with
  | nil => simp
  | cons x xs ih => simp only [List.foldl_cons, ih, List.all_cons, Bool.and_assoc]

@[step] theorem covers_spec (xs ys : alloc.vec.Vec Refund) :
    covers xs ys ⦃ b => b = xs.val.all (hasSameB ys.val) ⦄ := by
  unfold covers covers_loop
  have h := loop_fold xs.val (fun b : Bool => b) (fun acc x => acc && hasSameB ys.val x)
    (fun _ _ => True) (fun x => covers_loop.body xs ys x.1 x.2) ?_ true 0#usize (by simp) trivial
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_and]; simp)
  · intro b j hj _
    unfold covers_loop.body
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < xs.val.length := by scalar_tac
      rw [vec_index_eq xs j hlt']
      simp only [bind_ok, has_same_eq]
      by_cases hb : hasSameB ys.val xs.val[j.val] = true
      · simp only [hb, if_true, bind_ok]
        step*
        simp only [FoldStep]
        exact ⟨hlt', by simp [hb], by scalar_tac, trivial⟩
      · simp only [hb, Bool.false_eq_true, if_false, bind_ok]
        step*
        simp only [FoldStep]
        exact ⟨hlt', by simp_all, by scalar_tac, trivial⟩
    · rename_i hge
      simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem covers_iff (xs ys : List Refund) : xs.all (hasSameB ys) = true ↔ RefundsCover xs ys := by
  simp only [List.all_eq_true, RefundsCover]
  refine forall_congr' fun x => imp_congr_right fun _ => ?_
  unfold hasSameB
  cases ys.find? (fun r => r.id.val = x.id.val) <;> simp

theorem refundsOf_eq (s : Snapshot) (id : U64) :
    s.refunds.val.filter (fun x => decide (x.payment = id)) = refundsOf (Snapshot.toSt s) id.val := by
  simp [refundsOf, Snapshot.toSt]

/-- `MarkPosted`: only an operator; it writes nothing, or the payment marked
(and nothing else about it changed) when the payment is settled (captured,
failed or canceled) for the captured amount the orchestrator saw and its
refund rows are exactly the ones the orchestrator saw. -/
theorem mark_posted_spec (a : Principal) (s : Snapshot) (id cap : U64) (seen : alloc.vec.Vec Refund) :
    mark_posted a s id cap seen ⦃ r => ∀ ws rep, r = .Ok (ws, rep) → a.operator = true ∧
      (ws.val = [] ∨ ∃ p, findPay (Snapshot.toSt s) id.val = some p ∧
        (p.status = CAPTURED ∨ p.status = FAILED ∨ p.status = CANCELED) ∧ p.captured = cap ∧
        RefundsCover (refundsOf (Snapshot.toSt s) id.val) seen.val ∧ RefundsCover seen.val (refundsOf (Snapshot.toSt s) id.val) ∧
        ws.val = [.PutPayment { p with operator := a.opId, ledger_done := true }]) ⦄ := by
  unfold mark_posted with_ledger_done
  simp only [is_operator_eq, operator_of_eq, bind_ok]
  cases ho : a.operator
  · simp
  · simp only [if_true]
    step*
    all_goals (intro ws rep h; first | (obtain ⟨rfl, rfl⟩ := ok_inj h) | simp at h)
    all_goals try (refine ⟨rfl, ?_⟩)
    all_goals first
      | exact Or.inl rfl
      | (refine Or.inr ⟨p, findPay_of (Or.inr o_post) ‹_›, by simp_all, by simp_all, ?_, ?_, by simp_all⟩
         · rw [← covers_iff, ← refundsOf_eq]; simp_all
         · rw [← covers_iff, ← refundsOf_eq]; simp_all)

theorem list_writes (a : Principal) (s : Snapshot) (m f : U64) (l : U32) ws r
    (h : list a s m f l = ok (.Ok (ws, r))) : ws.val = [] := by
  unfold list at h
  simp only [is_operator_eq, user_of_eq, bind_ok] at h
  split at h
  · simp at h
  split at h
  · simp at h
  have fin : ∀ (k : Result (alloc.vec.Vec Payment)) (g : alloc.vec.Vec Payment → Reply),
      (do let v ← k; ok (core.result.Result.Ok (E := Error) (alloc.vec.Vec.new Write, g v))) = ok (.Ok (ws, r)) →
        ws.val = [] := by
    intro k g hk
    obtain ⟨v, -, hv⟩ := bind_tc_eq_ok.1 hk
    simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at hv
    rw [← hv.1]; rfl
  split at h
  · obtain ⟨e, -, h⟩ := bind_tc_eq_ok.1 h
    obtain ⟨i, -, h⟩ := bind_tc_eq_ok.1 h
    obtain ⟨nx, -, h⟩ := bind_tc_eq_ok.1 h
    exact fin _ _ h
  · split at h
    · simp at h
    · obtain ⟨e, -, h⟩ := bind_tc_eq_ok.1 h
      obtain ⟨i, -, h⟩ := bind_tc_eq_ok.1 h
      obtain ⟨nx, -, h⟩ := bind_tc_eq_ok.1 h
      exact fin _ _ h

theorem history_page_writes (a : Principal) (s : Snapshot) (p f : U64) (l : U32) ws r
    (h : history_page a s p f l = ok (.Ok (ws, r))) : ws.val = [] := by
  unfold history_page at h
  simp only [is_operator_eq, bind_ok] at h
  split at h
  · simp at h
  split at h
  · simp at h
  split at h
  · obtain ⟨o, -, h⟩ := bind_tc_eq_ok.1 h
    split at h
    · simp at h
    · obtain ⟨e, -, h⟩ := bind_tc_eq_ok.1 h
      obtain ⟨nx, -, h⟩ := bind_tc_eq_ok.1 h
      obtain ⟨v, -, h⟩ := bind_tc_eq_ok.1 h
      simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h; rw [← h.1]; rfl
  · simp at h

theorem range_page_writes (a : Principal) (s : Snapshot) (f : U64) (l : U32) ws r
    (h : range_page a s f l = ok (.Ok (ws, r))) : ws.val = [] := by
  unfold range_page at h
  simp only [is_operator_eq, bind_ok] at h
  split at h
  · simp at h
  split at h
  · simp at h
  split at h
  · obtain ⟨e, -, h⟩ := bind_tc_eq_ok.1 h
    obtain ⟨nx, -, h⟩ := bind_tc_eq_ok.1 h
    obtain ⟨v, -, h⟩ := bind_tc_eq_ok.1 h
    simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h; rw [← h.1]; rfl
  · simp at h

theorem refund_range_page_writes (a : Principal) (s : Snapshot) (f : U64) (l : U32) ws r
    (h : refund_range_page a s f l = ok (.Ok (ws, r))) : ws.val = [] := by
  unfold refund_range_page at h
  simp only [is_operator_eq, bind_ok] at h
  split at h
  · simp at h
  split at h
  · simp at h
  split at h
  · obtain ⟨e, -, h⟩ := bind_tc_eq_ok.1 h
    obtain ⟨nx, -, h⟩ := bind_tc_eq_ok.1 h
    obtain ⟨rs, -, h⟩ := bind_tc_eq_ok.1 h
    obtain ⟨ps, -, h⟩ := bind_tc_eq_ok.1 h
    simp only [Result.ok.injEq, core.result.Result.Ok.injEq, Prod.mk.injEq] at h; rw [← h.1]; rfl
  · simp at h

end payment_domain.Commands
