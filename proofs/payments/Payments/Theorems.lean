import Payments.Commands
/-!
# Theorems about payments

* `status_steps_allowed` — every committed command moves each payment's
  status only along `Spec.Allowed` (or leaves it), and new payments start
  `CREATED`; so canceled and failed payments are final (`terminal_final`)
  and a captured payment stays captured (`captured_final`).
* `refund_safety` — in every reachable state, refunded plus in-flight
  refunds never exceed the captured amount, which never exceeds the
  authorized amount, which never exceeds the payment's amount.
* `event_dedup` — a webhook whose event id was processed before changes
  nothing.
-/
open Aeneas Aeneas.Std Result payment_domain payment_domain.Spec payment_domain.Commands H5iAppLib

namespace payment_domain.Theorems

/-! ## Lookups after a write -/

theorem findPay_put (s : St) (p : Payment) (n : Nat) :
    findPay (applyWrite s (.PutPayment p)) n = if p.id.val = n then some p else findPay s n := by
  simp only [findPay, applyWrite]
  induction s.payments with
  | nil => simp [upsert]
  | cons y ys ih =>
    by_cases h : y.id = p.id
    · by_cases ha : p.id.val = n <;> simp [upsert, h, ha]
    · simp only [upsert, h, if_false, List.find?_cons, ih]
      by_cases hy : y.id.val = n <;> by_cases ha : p.id.val = n <;> simp_all

theorem findPay_mem {s : St} {n : Nat} {q : Payment} (h : findPay s n = some q) : q ∈ s.payments ∧ q.id.val = n :=
  ⟨List.mem_of_find?_eq_some h, by simpa using List.find?_some h⟩

theorem findRefund_mem {s : St} {n : Nat} {q : Refund} (h : findRefund s n = some q) : q ∈ s.refunds ∧ q.id.val = n :=
  ⟨List.mem_of_find?_eq_some h, by simpa using List.find?_some h⟩

/-- Writes that touch neither payments, refunds nor the counter. -/
def Quiet : Write → Prop
  | .PutEvent _ => True
  | .PutRef _ => True
  | _ => False

theorem applyAll_append (s : St) (a b : List Write) : applyAll s (a ++ b) = applyAll (applyAll s a) b := by
  simp [applyAll, List.foldl_append]

theorem applyAll_quiet (s : St) (ws : List Write) (h : ∀ w ∈ ws, Quiet w) :
    (applyAll s ws).payments = s.payments ∧ (applyAll s ws).refunds = s.refunds ∧ (applyAll s ws).next = s.next := by
  induction ws generalizing s with
  | nil => simp [applyAll]
  | cons w ws ih =>
    have hw := h w (by simp)
    obtain ⟨h1, h2, h3⟩ := ih (applyWrite s w) (fun x hx => h x (List.mem_cons_of_mem _ hx))
    simp only [applyAll, List.foldl_cons] at *
    cases w <;> simp_all [Quiet, applyWrite]

/-! ## What a command does to statuses -/

/-- Every payment after the command either existed before with a status
`Allowed` to step to its new one (or the same), or is new and `CREATED`. -/
def Stepped (s s' : St) : Prop :=
  ∀ n p', findPay s' n = some p' →
    (∃ p, findPay s n = some p ∧ (p'.status = p.status ∨ Allowed p.status.val p'.status.val)) ∨
    (findPay s n = none ∧ p'.status.val = 0)

theorem Stepped.refl (s : St) : Stepped s s :=
  fun n p' h => .inl ⟨p', h, .inl rfl⟩

theorem Stepped.same {s s1 s2 : St} (h : Stepped s s1) (e : s2.payments = s1.payments) : Stepped s s2 := by
  intro n p' hp; apply h n p'; simpa [findPay, e] using hp

theorem stepped_put (s : St) (p p' : Payment) (hp : findPay s p'.id.val = some p) (hs : Step p p') :
    Stepped s (applyWrite s (.PutPayment p')) := by
  intro n q hq
  rw [findPay_put] at hq
  split at hq
  · rename_i e
    cases hq
    exact .inl ⟨p, by rw [← e]; exact hp, hs.2.2.2.2.1⟩
  · exact .inl ⟨q, hq, .inl rfl⟩

/-! ## The invariant, write by write -/

theorem payInv_upsert {l : List Payment} (hl : ∀ q ∈ l, PayInv q) {p : Payment} (hp : PayInv p) :
    ∀ q ∈ upsert (·.id) p l, PayInv q := by
  intro q hq
  rcases mem_upsert_of hq with rfl | hq
  · exact hp
  · exact hl q hq

theorem fresh_upsert {α : Type} {key : α → U64} {l : List α} {N : Nat} (hl : ∀ b ∈ l, (key b).val < N) {a : α}
    (ha : (key a).val < N) : ∀ b ∈ upsert key a l, (key b).val < N := by
  intro b hb
  rcases mem_upsert_of hb with rfl | hb
  · exact ha
  · exact hl b hb

theorem inv_put (s : St) (p p' : Payment) (hinv : Inv s) (hp : findPay s p'.id.val = some p) (hs : Step p p') :
    Inv (applyWrite s (.PutPayment p')) := by
  have m := findPay_mem hp
  refine ⟨?_, hinv.fresh_refund, ?_⟩
  · simp only [applyWrite]
    exact fresh_upsert (key := Payment.id) hinv.fresh_pay (by rw [hs.1]; exact hinv.fresh_pay p m.1)
  · simp only [applyWrite]
    exact payInv_upsert hinv.amounts (hs.2.2.2.2.2 (hinv.amounts p m.1))

theorem inv_refund (s : St) (rf rf' : Refund) (hinv : Inv s) (hr : findRefund s rf.id.val = some rf) (he : rf'.id = rf.id) :
    Inv (applyWrite s (.PutRefund rf')) := by
  have m := findRefund_mem hr
  refine ⟨hinv.fresh_pay, ?_, hinv.amounts⟩
  simp only [applyWrite]
  exact fresh_upsert (key := Refund.id) hinv.fresh_refund (by rw [he]; exact hinv.fresh_refund rf m.1)

/-- Recording a PSP result keeps the invariant and steps statuses. -/
theorem record_effect (s : St) (op : U64) (id : Nat) (ws : List Write) (hinv : Inv s) (h : RecordWrites s op id ws) :
    Inv (applyAll s ws) ∧ Stepped s (applyAll s ws) := by
  rcases h with rfl | ⟨p, hp, (rfl | ⟨p', rfl, hs, _, _⟩) | ⟨refund, rfl | ⟨rf, rf', p', hrf, he, rfl, hs, _, _, _, _⟩⟩⟩
  · exact ⟨hinv, Stepped.refl _⟩
  · exact ⟨hinv, Stepped.refl _⟩
  · have hp' : findPay s p'.id.val = some p := by rw [hs.1, (findPay_mem hp).2]; exact hp
    exact ⟨inv_put s p p' hinv hp' hs, stepped_put s p p' hp' hs⟩
  · exact ⟨hinv, Stepped.refl _⟩
  · have hp' : findPay s p'.id.val = some p := by rw [hs.1, (findPay_mem hp).2]; exact hp
    have hrf' : findRefund s rf.id.val = some rf := by rw [(findRefund_mem hrf).2]; exact hrf
    simp only [applyAll, List.foldl_cons, List.foldl_nil]
    have hi1 := inv_refund s rf rf' hinv hrf' he
    have hp1 : findPay (applyWrite s (.PutRefund rf')) p'.id.val = some p := hp'
    refine ⟨inv_put _ p p' hi1 hp1 hs, ?_⟩
    have := stepped_put _ p p' hp1 hs
    intro n q hq
    rcases this n q hq with ⟨p0, h0, h1⟩ | ⟨h0, h1⟩
    · exact .inl ⟨p0, h0, h1⟩
    · exact .inr ⟨h0, h1⟩

theorem quiet_effect (s : St) (w0 q : List Write) (hq : ∀ w ∈ q, Quiet w)
    (h : Inv (applyAll s w0) ∧ Stepped s (applyAll s w0)) :
    Inv (applyAll s (w0 ++ q)) ∧ Stepped s (applyAll s (w0 ++ q)) := by
  rw [applyAll_append]
  obtain ⟨e1, e2, e3⟩ := applyAll_quiet (applyAll s w0) q hq
  obtain ⟨⟨f1, f2, f3⟩, hs⟩ := h
  refine ⟨⟨?_, ?_, ?_⟩, hs.same e1⟩ <;> simp only [e1, e2, e3] <;> assumption

/-- Every successful command keeps the invariant and steps statuses only
along `Allowed`. -/
theorem effect (a : Principal) (s : Snapshot) (c : Command) ws r
    (hinv : Inv (Snapshot.toSt s)) (h : transition a s c = .ok (.Ok (ws, r))) :
    Inv (applyAll (Snapshot.toSt s) ws.val) ∧ Stepped (Snapshot.toSt s) (applyAll (Snapshot.toSt s) ws.val) := by
  have nothing : ws.val = [] → Inv (applyAll (Snapshot.toSt s) ws.val) ∧
      Stepped (Snapshot.toSt s) (applyAll (Snapshot.toSt s) ws.val) := fun e => by
    rw [e]; exact ⟨hinv, Stepped.refl _⟩
  cases c with
  | Create amt cur mc =>
    obtain ⟨n, hn, q0, m0, _, _, hws⟩ := post_of_ok (create_spec a s amt cur mc) h ws r rfl
    rw [hws]
    simp only [applyAll, List.foldl_cons, List.foldl_nil, applyWrite]
    have hfresh : findPay (Snapshot.toSt s) s.counter.next_id.val = none :=
      List.find?_eq_none.2 fun b hb => by have := hinv.fresh_pay b hb; simp [Snapshot.toSt] at *; omega
    refine ⟨⟨?_, ?_, ?_⟩, ?_⟩
    · exact fresh_upsert (key := Payment.id) (fun b hb => by have := hinv.fresh_pay b hb; simp [Snapshot.toSt] at *; omega)
        (by simp; omega)
    · exact fun b hb => by have := hinv.fresh_refund b hb; simp [Snapshot.toSt] at *; omega
    · exact payInv_upsert hinv.amounts (by simp [PayInv, CREATED])
    · intro m q hq
      have := findPay_put (Snapshot.toSt s) ⟨s.counter.next_id, a.user, amt, cur, CREATED, mc, 0#u64, 0#u64, 0#u64, 0#u64, 0#u64, a.opId, false, q0⟩ m
      simp only [applyWrite] at this
      simp only [findPay] at hq this
      rw [this] at hq
      split at hq
      · rename_i e
        cases hq
        refine .inr ⟨?_, by simp [CREATED]⟩
        rw [← e]; exact hfresh
      · exact .inl ⟨q, hq, .inl rfl⟩
  | BeginAuthorize id =>
    obtain ⟨p, hp, hw⟩ := post_of_ok (begin_authorize_spec a s id) h ws r rfl
    exact record_effect _ _ id.val _ hinv (Or.inr ⟨p, hp, Or.inl hw⟩)
  | Cancel id =>
    obtain ⟨p, hp, hw⟩ := post_of_ok (cancel_spec a s id) h ws r rfl
    exact record_effect _ _ id.val _ hinv (Or.inr ⟨p, hp, Or.inl hw⟩)
  | RequestRefund id amt =>
    obtain ⟨p, p', n, hp, hs, _, _, hn, hws⟩ := post_of_ok (request_refund_spec a s id amt) h ws r rfl
    rw [hws]
    have m := findPay_mem hp
    have hp' : findPay (Snapshot.toSt s) p'.id.val = some p := by rw [hs.1, m.2]; exact hp
    let rf : Refund := ⟨s.counter.next_id, p.id, amt, REFUND_PENDING, a.opId⟩
    let s1 := applyWrite (Snapshot.toSt s) (.PutRefund rf)
    have hp1 : findPay s1 p'.id.val = some p := hp'
    have hs1 := stepped_put s1 p p' hp1 hs
    simp only [applyAll, List.foldl_cons, List.foldl_nil]
    refine ⟨⟨?_, ?_, ?_⟩, ?_⟩
    · simp only [applyWrite]
      exact fresh_upsert (key := Payment.id) (fun b hb => by have := hinv.fresh_pay b hb; simp [Snapshot.toSt] at *; omega)
        (by have := hinv.fresh_pay p m.1; rw [hs.1]; simp [Snapshot.toSt] at *; omega)
    · simp only [applyWrite]
      exact fresh_upsert (key := Refund.id) (fun b hb => by have := hinv.fresh_refund b hb; simp [Snapshot.toSt] at *; omega)
        (by simp [rf]; omega)
    · simp only [applyWrite]
      exact payInv_upsert hinv.amounts (hs.2.2.2.2.2 (hinv.amounts p m.1))
    · intro k q hq
      exact hs1 k q hq
  | Record id res =>
    exact record_effect _ _ id.val _ hinv (post_of_ok (record_spec a s id res) h ws r rfl)
  | Event ev id res =>
    rcases post_of_ok (event_spec a s ev id res) h ws r rfl with ⟨_, hws, _⟩ | ⟨_, w0, hw0, hws⟩
    · exact nothing hws
    · rw [hws]
      exact quiet_effect _ w0 _ (by simp [Quiet]) (record_effect _ _ id.val w0 hinv hw0)
  | AttachRef pay ref =>
    rcases (attach_ref_writes a s pay ref ws r h).2 with hws | ⟨x, hws, _⟩
    · exact nothing hws
    · rw [hws, show [Write.PutRef x] = [] ++ [Write.PutRef x] from rfl]
      exact quiet_effect _ [] _ (by simp [Quiet]) ⟨hinv, Stepped.refl _⟩
  | Get id => exact nothing (get_writes a s id ws r h)
  | List m f l => exact nothing (list_writes a s m f l ws r h)
  | GetRefund id => exact nothing (get_refund_writes a s id ws r h)
  | Refunds id => exact nothing (refunds_of_writes a s id ws r h)
  | GetRef id => exact nothing (get_ref_writes a s id ws r h)
  | View id => exact nothing (view_writes a s id ws r h)
  | MarkPosted id cap seen =>
    rcases (post_of_ok (mark_posted_spec a s id cap seen) h ws r rfl).2 with hws | ⟨p, hp, _, _, _, _, hws⟩
    · exact nothing hws
    · rw [hws]
      have hs : Step p { p with operator := a.opId, ledger_done := true } :=
        ⟨rfl, rfl, rfl, rfl, .inl rfl, fun h => h⟩
      have hp' : findPay (Snapshot.toSt s) ({ p with operator := a.opId, ledger_done := true } : Payment).id.val = some p := by
        rw [(findPay_mem hp).2]; exact hp
      exact ⟨inv_put _ p _ hinv hp' hs, stepped_put _ p _ hp' hs⟩

theorem init_inv : Inv init := ⟨by simp [init], by simp [init], by simp [init]⟩

/-- The invariant holds in every reachable state. -/
theorem reachable_inv {s : St} (h : Reachable s) : Inv s := by
  induction h with
  | init => exact init_inv
  | step _ ht ih => exact (effect _ _ _ _ _ ih ht).1

/-! ## The theorems -/

/-- **Valid state transitions.** In every reachable state, a committed
command moves each payment's status only along `Allowed`, or leaves it;
a payment it creates starts `CREATED`. -/
theorem status_steps_allowed (a : Principal) (s : Snapshot) (c : Command) ws r
    (hr : Reachable (Snapshot.toSt s)) (h : transition a s c = .ok (.Ok (ws, r)))
    {n : Nat} {p' : Payment} (hp' : findPay (applyAll (Snapshot.toSt s) ws.val) n = some p') :
    (∃ p, findPay (Snapshot.toSt s) n = some p ∧ (p'.status = p.status ∨ Allowed p.status.val p'.status.val)) ∨
    (findPay (Snapshot.toSt s) n = none ∧ p'.status.val = 0) :=
  (effect a s c ws r (reachable_inv hr) h).2 n p' hp'

/-- Canceled (4) and failed (5) payments never change status again. -/
theorem terminal_final (a : Principal) (s : Snapshot) (c : Command) ws r
    (hr : Reachable (Snapshot.toSt s)) (h : transition a s c = .ok (.Ok (ws, r)))
    {n : Nat} {p p' : Payment} (hp : findPay (Snapshot.toSt s) n = some p) (ht : p.status.val = 4 ∨ p.status.val = 5)
    (hp' : findPay (applyAll (Snapshot.toSt s) ws.val) n = some p') : p'.status = p.status := by
  rcases status_steps_allowed a s c ws r hr h hp' with ⟨q, hq, hst⟩ | ⟨hn, _⟩
  · rw [hp] at hq; cases hq
    rcases hst with e | e
    · exact e
    · simp only [Allowed] at e; omega
  · rw [hp] at hn; cases hn

/-- A captured payment stays captured. -/
theorem captured_final (a : Principal) (s : Snapshot) (c : Command) ws r
    (hr : Reachable (Snapshot.toSt s)) (h : transition a s c = .ok (.Ok (ws, r)))
    {n : Nat} {p p' : Payment} (hp : findPay (Snapshot.toSt s) n = some p) (ht : p.status.val = 3)
    (hp' : findPay (applyAll (Snapshot.toSt s) ws.val) n = some p') : p'.status = p.status := by
  rcases status_steps_allowed a s c ws r hr h hp' with ⟨q, hq, hst⟩ | ⟨hn, _⟩
  · rw [hp] at hq; cases hq
    rcases hst with e | e
    · exact e
    · simp only [Allowed] at e; omega
  · rw [hp] at hn; cases hn

/-- **Refund safety.** In every reachable state, for every payment: the
refunded amount plus the refunds in flight never exceed the captured
amount, which never exceeds the authorized amount, which never exceeds the
payment's amount; and nothing is refunded or reserved unless the payment
is captured. -/
theorem refund_safety {s : St} (h : Reachable s) {p : Payment} (hp : p ∈ s.payments) :
    p.refunded.val + p.refund_pending.val ≤ p.captured.val ∧ p.captured.val ≤ p.authorized.val ∧
    p.authorized.val ≤ p.amount.val ∧ (p.status.val ≠ 3 → p.refunded.val = 0 ∧ p.refund_pending.val = 0) :=
  (reachable_inv h).amounts p hp

/-- Corollary: the money returned to the customer never exceeds what was
taken from them. -/
theorem refunded_le_captured {s : St} (h : Reachable s) {p : Payment} (hp : p ∈ s.payments) :
    p.refunded.val ≤ p.captured.val := by
  have := (refund_safety h hp).1; omega

/-- **Webhook deduplication.** A PSP event whose id was processed before is
acknowledged and changes nothing. -/
theorem event_dedup (a : Principal) (s : Snapshot) (ev id : U64) (res : PspResult) ws r
    (hseen : findEvent (Snapshot.toSt s) ev.val ≠ none)
    (h : transition a s (.Event ev id res) = .ok (.Ok (ws, r))) : ws.val = [] ∧ r = .Duplicate := by
  rcases post_of_ok (event_spec a s ev id res) h ws r rfl with ⟨_, hws, hr⟩ | ⟨hn, _⟩
  · exact ⟨hws, hr⟩
  · exact absurd hn hseen

/-! ## Operators (SEC-02) -/

theorem record_operator (a : Principal) (s : Snapshot) (id : U64) (res : PspResult) x
    (h : record a s id res = ok (.Ok x)) : a.operator = true := by
  cases ho : a.operator
  · simp [record, ho] at h
  · rfl

theorem event_operator (a : Principal) (s : Snapshot) (ev id : U64) (res : PspResult) x
    (h : event a s ev id res = ok (.Ok x)) : a.operator = true := by
  cases ho : a.operator
  · simp [event, ho] at h
  · rfl

/-- **Only operators record what the PSP said.** `Record`, `Event`,
`AttachRef` and `MarkPosted` succeed only when an operator (`Actor.Operator`) sends them;
a user's command is refused (`Forbidden`) and writes nothing. -/
theorem operator_only (a : Principal) (s : Snapshot) (c : Command) ws r
    (hc : (∃ id res, c = .Record id res) ∨ (∃ ev id res, c = .Event ev id res) ∨ (∃ id ref, c = .AttachRef id ref) ∨
      (∃ id cap seen, c = .MarkPosted id cap seen))
    (h : transition a s c = .ok (.Ok (ws, r))) : ∃ o, a.actor = .Operator o := by
  apply (operator_iff a).1
  rcases hc with ⟨id, res, rfl⟩ | ⟨ev, id, res, rfl⟩ | ⟨id, ref, rfl⟩ | ⟨id, cap, seen, rfl⟩
  · exact record_operator a s id res _ h
  · exact event_operator a s ev id res _ h
  · exact (attach_ref_writes a s id ref ws r h).1
  · exact (post_of_ok (mark_posted_spec a s id cap seen) h ws r rfl).1

/-- Writes that record operator `op` on every row they write. -/
def Records (op : U64) (ws : List Write) : Prop := ∀ w ∈ ws, ∀ o, w.recorded = some o → o = op

theorem records_nil (op : U64) : Records op [] := by simp [Records]

theorem records_step {op : U64} {p : Payment} {ws : List Write} (h : StepWrites op p ws) : Records op ws := by
  rcases h with rfl | ⟨p', rfl, _, hp, _⟩
  · exact records_nil op
  · intro w hw o ho; simp at hw; subst hw; simp [Write.recorded] at ho; rw [← ho, hp]

theorem records_record {s : St} {op : U64} {id : Nat} {ws : List Write} (h : RecordWrites s op id ws) :
    Records op ws := by
  rcases h with rfl | ⟨p, _, hs | ⟨refund, rfl | ⟨rf, rf', p', _, _, rfl, _, h1, h2, _, _⟩⟩⟩
  · exact records_nil op
  · exact records_step hs
  · exact records_nil op
  · intro w hw o ho
    simp at hw
    rcases hw with rfl | rfl <;> simp [Write.recorded] at ho <;> rw [← ho] <;> assumption

/-- **Every row records who wrote it.** Each row a successful command
writes (payment, refund, event, PSP reference) carries the caller's
operator id: the operator's own id for `Actor.Operator o`, 0 for a user.
Operators are told apart in the stored state, and nothing a user does is
attributed to an operator. -/
theorem writes_record_operator (a : Principal) (s : Snapshot) (c : Command) ws r
    (h : transition a s c = .ok (.Ok (ws, r))) : Records a.opId ws.val := by
  cases c with
  | Create amt cur mc =>
    obtain ⟨n, hn, _, _, _, _, hws⟩ := post_of_ok (create_spec a s amt cur mc) h ws r rfl
    rw [hws]; intro w hw o ho; simp at hw
    rcases hw with rfl | rfl | rfl <;> simp [Write.recorded] at ho <;> rw [← ho]
  | BeginAuthorize id =>
    obtain ⟨p, _, hw⟩ := post_of_ok (begin_authorize_spec a s id) h ws r rfl
    exact records_step hw
  | Cancel id =>
    obtain ⟨p, _, hw⟩ := post_of_ok (cancel_spec a s id) h ws r rfl
    exact records_step hw
  | RequestRefund id amt =>
    obtain ⟨p, p', n, _, _, hop, _, _, hws⟩ := post_of_ok (request_refund_spec a s id amt) h ws r rfl
    rw [hws]; intro w hw o ho; simp at hw
    rcases hw with rfl | rfl | rfl <;> simp [Write.recorded] at ho <;> rw [← ho]; exact hop
  | Record id res => exact records_record (post_of_ok (record_spec a s id res) h ws r rfl)
  | Event ev id res =>
    rcases post_of_ok (event_spec a s ev id res) h ws r rfl with ⟨_, hws, _⟩ | ⟨_, w0, hw0, hws⟩
    · rw [hws]; exact records_nil _
    · rw [hws]; intro w hw o ho
      rcases List.mem_append.1 hw with hw | hw
      · exact records_record hw0 w hw o ho
      · simp at hw; subst hw; simp [Write.recorded] at ho; rw [← ho]
  | AttachRef pay ref =>
    rcases (attach_ref_writes a s pay ref ws r h).2 with hws | ⟨x, hws, hx⟩
    · rw [hws]; exact records_nil _
    · rw [hws]; intro w hw o ho; simp at hw; subst hw; simp [Write.recorded] at ho; rw [← ho, hx]
  | Get id => rw [get_writes a s id ws r h]; exact records_nil _
  | List m f l => rw [list_writes a s m f l ws r h]; exact records_nil _
  | GetRefund id => rw [get_refund_writes a s id ws r h]; exact records_nil _
  | Refunds id => rw [refunds_of_writes a s id ws r h]; exact records_nil _
  | GetRef id => rw [get_ref_writes a s id ws r h]; exact records_nil _
  | View id => rw [view_writes a s id ws r h]; exact records_nil _
  | MarkPosted id cap seen =>
    rcases (post_of_ok (mark_posted_spec a s id cap seen) h ws r rfl).2 with hws | ⟨p, _, _, _, _, _, hws⟩
    · rw [hws]; exact records_nil _
    · rw [hws]; intro w hw o ho; simp at hw; subst hw; simp [Write.recorded] at ho; rw [← ho]

/-! ## The ledger marker (F2)

`ledger_done` on a payment says the ledger holds every posting the
payment needs as of its current state. The two theorems below are what
the kernel guarantees about it; that the orchestrator sends `MarkPosted`
only after the ledger accepted every posting is the shell's part
(`Orchestrator::sync_ledger`). Together: a marked payment's captured
amount and refund rows are the ones the orchestrator synced, because
nothing has written them since. -/

/-- **The marker is set only on the state it names.** `MarkPosted` is an
operator's, and the payment row it writes is the payment, settled
(captured, failed or canceled) with exactly the captured amount the
orchestrator saw, whose refund rows are exactly the ones it saw (each found
by id, the same), with only `operator` and `ledger_done` changed. -/
theorem marked_only_as_seen (a : Principal) (s : Snapshot) (id cap : U64) (seen : alloc.vec.Vec Refund) ws r
    (h : transition a s (.MarkPosted id cap seen) = .ok (.Ok (ws, r))) :
    (∃ o, a.actor = .Operator o) ∧ ∀ p', Write.PutPayment p' ∈ ws.val →
      ∃ p, findPay (Snapshot.toSt s) id.val = some p ∧
        (p.status = CAPTURED ∨ p.status = FAILED ∨ p.status = CANCELED) ∧ p.captured = cap ∧
        RefundsCover (refundsOf (Snapshot.toSt s) id.val) seen.val ∧
        RefundsCover seen.val (refundsOf (Snapshot.toSt s) id.val) ∧
        p' = { p with operator := a.opId, ledger_done := true } := by
  obtain ⟨hop, hw⟩ := post_of_ok (mark_posted_spec a s id cap seen) h ws r rfl
  refine ⟨(operator_iff a).1 hop, fun p' hp' => ?_⟩
  rcases hw with hws | ⟨p, hp, h1, h2, h3, h4, hws⟩
  · rw [hws] at hp'; simp at hp'
  · rw [hws] at hp'; simp at hp'; exact ⟨p, hp, h1, h2, h3, h4, hp'⟩

/-- Writes that leave every payment row they write unmarked, and write a
refund row only with its payment's row. -/
def ClearsMark (ws : List Write) : Prop :=
  (∀ p, Write.PutPayment p ∈ ws → p.ledger_done = false) ∧
  (∀ rf, Write.PutRefund rf ∈ ws → ∃ p, Write.PutPayment p ∈ ws ∧ p.id = rf.payment)

theorem clears_nil : ClearsMark [] := by simp [ClearsMark]

theorem clears_step {op : U64} {p : Payment} {ws : List Write} (h : StepWrites op p ws) : ClearsMark ws := by
  rcases h with rfl | ⟨p', rfl, _, _, hd⟩
  · exact clears_nil
  · exact ⟨fun q hq => by simp at hq; subst hq; exact hd, fun rf hrf => by simp at hrf⟩

theorem clears_record {s : St} {op : U64} {id : Nat} {ws : List Write} (h : RecordWrites s op id ws) : ClearsMark ws := by
  rcases h with rfl | ⟨p, _, hs | ⟨refund, rfl | ⟨rf, rf', p', _, _, rfl, _, _, _, hpay, hd⟩⟩⟩
  · exact clears_nil
  · exact clears_step hs
  · exact clears_nil
  · refine ⟨fun q hq => ?_, fun x hx => ?_⟩
    · simp at hq; subst hq; exact hd
    · simp at hx; subst hx; exact ⟨p', by simp, hpay.symm⟩

theorem clears_quiet {w0 q : List Write} (h : ClearsMark w0) (hq : ∀ w ∈ q, Quiet w) : ClearsMark (w0 ++ q) := by
  refine ⟨fun p hp => ?_, fun rf hrf => ?_⟩
  · rcases List.mem_append.1 hp with hp | hp
    · exact h.1 p hp
    · exact absurd (hq _ hp) (by simp [Quiet])
  · rcases List.mem_append.1 hrf with hrf | hrf
    · obtain ⟨p, hp, he⟩ := h.2 rf hrf; exact ⟨p, List.mem_append_left _ hp, he⟩
    · exact absurd (hq _ hrf) (by simp [Quiet])

/-- **Every other change clears the marker.** A successful command other
than `MarkPosted` writes each payment row it writes with
`ledger_done = false`, and writes a refund row only together with that
refund's payment row. So a command that changes what a payment needs from
the ledger (its capture, a refund request, a refund's outcome) leaves it
unmarked, and recovery (`status = CAPTURED AND NOT ledger_done`) finds it. -/
theorem changes_clear_ledger_done (a : Principal) (s : Snapshot) (c : Command) ws r
    (hc : ∀ id cap seen, c ≠ .MarkPosted id cap seen) (h : transition a s c = .ok (.Ok (ws, r))) :
    ClearsMark ws.val := by
  cases c with
  | Create amt cur mc =>
    obtain ⟨n, hn, _, _, _, _, hws⟩ := post_of_ok (create_spec a s amt cur mc) h ws r rfl
    rw [hws]; exact ⟨fun p hp => by simp at hp; subst hp; rfl, fun rf hrf => by simp at hrf⟩
  | BeginAuthorize id =>
    obtain ⟨p, _, hw⟩ := post_of_ok (begin_authorize_spec a s id) h ws r rfl
    exact clears_step hw
  | Cancel id =>
    obtain ⟨p, _, hw⟩ := post_of_ok (cancel_spec a s id) h ws r rfl
    exact clears_step hw
  | RequestRefund id amt =>
    obtain ⟨p, p', n, _, hs, _, hd, _, hws⟩ := post_of_ok (request_refund_spec a s id amt) h ws r rfl
    rw [hws]
    refine ⟨fun q hq => ?_, fun rf hrf => ?_⟩
    · simp at hq; subst hq; exact hd
    · simp at hrf; subst hrf; exact ⟨p', by simp, hs.1⟩
  | Record id res => exact clears_record (post_of_ok (record_spec a s id res) h ws r rfl)
  | Event ev id res =>
    rcases post_of_ok (event_spec a s ev id res) h ws r rfl with ⟨_, hws, _⟩ | ⟨_, w0, hw0, hws⟩
    · rw [hws]; exact clears_nil
    · rw [hws]; exact clears_quiet (clears_record hw0) (by simp [Quiet])
  | AttachRef pay ref =>
    rcases (attach_ref_writes a s pay ref ws r h).2 with hws | ⟨x, hws, _⟩
    · rw [hws]; exact clears_nil
    · rw [hws]; exact ⟨fun p hp => by simp at hp, fun rf hrf => by simp at hrf⟩
  | Get id => rw [get_writes a s id ws r h]; exact clears_nil
  | List m f l => rw [list_writes a s m f l ws r h]; exact clears_nil
  | GetRefund id => rw [get_refund_writes a s id ws r h]; exact clears_nil
  | Refunds id => rw [refunds_of_writes a s id ws r h]; exact clears_nil
  | GetRef id => rw [get_ref_writes a s id ws r h]; exact clears_nil
  | View id => rw [view_writes a s id ws r h]; exact clears_nil
  | MarkPosted id cap seen => exact absurd rfl (hc id cap seen)

end payment_domain.Theorems
