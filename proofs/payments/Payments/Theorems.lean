import Payments.Logged
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
open Aeneas Aeneas.Std Result payment_domain payment_domain.Spec payment_domain.Commands payment_domain.Logged H5iAppLib

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
  | .PutHistory _ => True
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

/-- Every successful command decision keeps the invariant and steps
statuses only along `Allowed` (before the history entry is added). -/
theorem effect0 (a : Principal) (s : Snapshot) (c : Command) ws r
    (hinv : Inv (Snapshot.toSt s)) (h : decide_command a s c = .ok (.Ok (ws, r))) :
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
      have := findPay_put (Snapshot.toSt s) ⟨s.counter.next_id, a.user, amt, cur, CREATED, mc, 0#u64, 0#u64, 0#u64, 0#u64, 0#u64, a.opId, false, q0, 0#u64⟩ m
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
  | History p f l => exact nothing (history_page_writes a s p f l ws r h)

/-! ## The history entry, as a state change (G1) -/

theorem applyAll_setPay (P : List Payment) : ∀ (l : List Write) (st : St), (∀ w ∈ l, isPayW w = false) →
    applyAll { st with payments := P } l = { applyAll st l with payments := P } := by
  intro l
  induction l with
  | nil => intro st _; rfl
  | cons w ws ih =>
    intro st hl
    have hw := hl w (by simp)
    simp only [applyAll, List.foldl_cons] at *
    have e : applyWrite { st with payments := P } w = { applyWrite st w with payments := P } := by
      cases w <;> simp_all [isPayW, applyWrite]
    rw [e]; exact ih _ (fun x hx => hl x (List.mem_cons_of_mem _ hx))

theorem applyAll_noPay : ∀ (l : List Write) (st : St), (∀ w ∈ l, isPayW w = false) →
    (applyAll st l).payments = st.payments := by
  intro l
  induction l with
  | nil => intro st _; rfl
  | cons w ws ih =>
    intro st hl
    have hw := hl w (by simp)
    simp only [applyAll, List.foldl_cons] at *
    rw [ih _ (fun x hx => hl x (List.mem_cons_of_mem _ hx))]
    cases w <;> simp_all [isPayW, applyWrite]

theorem applyAll_noHist : ∀ (l : List Write) (st : St), (∀ w ∈ l, isHistW w = false) →
    (applyAll st l).history = st.history := by
  intro l
  induction l with
  | nil => intro st _; rfl
  | cons w ws ih =>
    intro st hl
    have hw := hl w (by simp)
    simp only [applyAll, List.foldl_cons] at *
    rw [ih _ (fun x hx => hl x (List.mem_cons_of_mem _ hx))]
    cases w <;> simp_all [isHistW, applyWrite]

theorem upsert_upsert_same {α κ : Type} [DecidableEq κ] (k : α → κ) (p q : α) (h : k p = k q) :
    ∀ l : List α, upsert k q (upsert k p l) = upsert k q l := by
  intro l
  induction l with
  | nil => simp [upsert, h]
  | cons y ys ih =>
    by_cases hy : k y = k p
    · simp [upsert, hy, h]
    · have hy' : k y ≠ k q := by rw [← h]; exact hy
      simp [upsert, hy, hy', ih]

theorem applyAll_putPay_comm (p : Payment) : ∀ (l : List Write) (st : St), (∀ w ∈ l, isPayW w = false) →
    applyAll (applyWrite st (.PutPayment p)) l = applyWrite (applyAll st l) (.PutPayment p) := by
  intro l
  induction l with
  | nil => intro st _; rfl
  | cons w ws ih =>
    intro st hl
    have hw := hl w (by simp)
    simp only [applyAll, List.foldl_cons] at *
    have e : applyWrite (applyWrite st (.PutPayment p)) w = applyWrite (applyWrite st w) (.PutPayment p) := by
      cases w <;> simp_all [isPayW, applyWrite]
    rw [e]; exact ih _ (fun x hx => hl x (List.mem_cons_of_mem _ hx))

theorem othersL_noPay (ws : List Write) : ∀ w ∈ othersL ws, isPayW w = false :=
  fun _ hw => (mem_othersL.1 hw).2.1

theorem othersL_noHist (ws : List Write) : ∀ w ∈ othersL ws, isHistW w = false :=
  fun _ hw => (mem_othersL.1 hw).2.2

theorem othersL_cons_other (w : Write) (ws : List Write) (h1 : isPayW w = false) (h2 : isHistW w = false) :
    othersL (w :: ws) = w :: othersL ws := by
  simp [othersL, List.filter_cons, h1, h2]

/-- Putting payment `q` after a write set whose payment rows all have `q`'s
id is putting it after the same writes without those rows. -/
theorem reput_others (q : Payment) : ∀ (ws : List Write) (st : St), (∀ w ∈ ws, isHistW w = false) →
    (∀ p, Write.PutPayment p ∈ ws → p.id = q.id) →
    applyWrite (applyAll st (othersL ws)) (.PutPayment q) = applyWrite (applyAll st ws) (.PutPayment q) := by
  intro ws
  induction ws with
  | nil => intro st _ _; rfl
  | cons w ws ih =>
    intro st hh hp
    have hh' : ∀ x ∈ ws, isHistW x = false := fun x hx => hh x (List.mem_cons_of_mem _ hx)
    have hp' : ∀ p, Write.PutPayment p ∈ ws → p.id = q.id := fun p hx => hp p (List.mem_cons_of_mem _ hx)
    cases w with
    | PutPayment p =>
      have hpq := hp p (by simp)
      have e1 : othersL (Write.PutPayment p :: ws) = othersL ws := by simp [othersL, isPayW]
      rw [e1]
      show _ = applyWrite (applyAll (applyWrite st (.PutPayment p)) ws) _
      rw [← ih (applyWrite st (.PutPayment p)) hh' hp', applyAll_putPay_comm _ _ _ (othersL_noPay ws)]
      simp only [applyWrite]
      rw [upsert_upsert_same _ p q hpq]
    | PutHistory x => exact absurd (hh (.PutHistory x) (by simp)) (by simp [isHistW])
    | _ =>
      rw [othersL_cons_other _ _ rfl rfl]
      simp only [applyAll, List.foldl_cons]
      exact ih _ hh' hp'

theorem findPay_applyAll (k : Nat) : ∀ (ws : List Write) (st : St), (∀ p, Write.PutPayment p ∈ ws → p.id.val = k) →
    findPay (applyAll st ws) k = ws.foldl lastPayStep (findPay st k) := by
  intro ws
  induction ws with
  | nil => intro st _; rfl
  | cons w ws ih =>
    intro st hp
    simp only [applyAll, List.foldl_cons] at *
    rw [ih _ (fun p hx => hp p (List.mem_cons_of_mem _ hx))]
    congr 1
    cases w with
    | PutPayment p => rw [findPay_put, if_pos (hp p (by simp))]; rfl
    | _ => rfl

/-- The payments' history invariant: every entry belongs to a payment and
is numbered below its count of entries. -/
def HInv (s : St) : Prop :=
  ∀ e ∈ s.history, ∃ p, findPay s e.payment.val = some p ∧ e.n.val < p.changes.val

/-- What a successful `transition` that writes leaves: the decision's
state with the payment row re-put (only operator, mark and count of
entries changed) and the entry appended; in terms of the old state, the
payment upserted and the entry upserted. -/
theorem logged_state (a : Principal) (s : Snapshot) (c : Command) (ws0 : alloc.vec.Vec Write)
    (base : Payment) (n m : U64) (old : Option Payment) (ws : List Write)
    (hsh : Shape ws0.val)
    (hcase : (subj c = none ∧ lastPay ws0.val = some base ∧ base.id = s.counter.next_id ∧ old = none ∧ n = 0#u64) ∨
         (∃ id o, subj c = some id ∧ findPay (Snapshot.toSt s) id.val = some o ∧ old = some o ∧
            base = (lastPay ws0.val).getD o ∧ base.id = id ∧ n = o.changes))
    (hws : ws = othersL ws0.val ++ [.PutPayment (afterOf a c base m), .PutHistory (entryOf a s c n old (afterOf a c base m))]) :
    let st := Snapshot.toSt s
    let after := afterOf a c base m
    findPay (applyAll st ws0.val) base.id.val = some base ∧
    applyAll st ws = applyWrite (applyWrite (applyAll st ws0.val) (.PutPayment after))
      (.PutHistory (entryOf a s c n old after)) ∧
    (applyAll st ws).payments = upsert (·.id) after st.payments ∧
    (applyAll st ws).history = upsert (fun y => (y.payment, y.n)) (entryOf a s c n old after) st.history := by
  intro st after
  obtain ⟨hlen, hnh, huniq⟩ := hsh
  have hall : ∀ p, Write.PutPayment p ∈ ws0.val → p = base := by
    intro p hp
    rcases hcase with ⟨_, hl, _⟩ | ⟨id, o, _, _, _, hb, _⟩
    · exact huniq p base hp (lastPay_mem hl)
    · cases hl : lastPay ws0.val with
      | none => exact absurd hp (lastPay_none hl p)
      | some b => rw [hl] at hb; simp at hb; subst hb; exact huniq p _ hp (lastPay_mem hl)
  have hfind : findPay (applyAll st ws0.val) base.id.val = some base := by
    rw [findPay_applyAll _ _ _ (fun p hp => by rw [hall p hp]), lastPay_foldl]
    rcases hcase with ⟨_, hl, _⟩ | ⟨id, o, _, ho, _, hb, hid, _⟩
    · simp [lastPay] at hl; simp [hl]
    · cases hl : lastPay ws0.val with
      | none =>
        rw [hl] at hb; simp at hb; subst hb
        simp only [lastPay] at hl; simp only [hl, Option.none_or]; rw [hid]; exact ho
      | some b => rw [hl] at hb; simp at hb; subst hb; simp only [lastPay] at hl; simp [hl]
  refine ⟨hfind, ?_, ?_, ?_⟩
  · rw [hws, show ∀ x y : Write, othersL ws0.val ++ [x, y] = (othersL ws0.val ++ [x]) ++ [y] from fun _ _ => by simp]
    simp only [applyAll, List.foldl_append, List.foldl_cons, List.foldl_nil]
    congr 1
    exact reput_others after ws0.val st hnh (fun p hp => by rw [hall p hp]; rfl)
  · rw [hws]
    simp only [applyAll, List.foldl_append, List.foldl_cons, List.foldl_nil, applyWrite]
    exact congrArg _ (applyAll_noPay _ _ (othersL_noPay _))
  · rw [hws]
    simp only [applyAll, List.foldl_append, List.foldl_cons, List.foldl_nil, applyWrite]
    exact congrArg _ (applyAll_noHist _ _ (othersL_noHist _))

/-- Re-putting an existing payment with the same status keeps statuses stepped. -/
theorem stepped_reput {s s1 : St} {base q : Payment} (h : Stepped s s1) (hb : findPay s1 base.id.val = some base)
    (hid : q.id = base.id) (hst : q.status = base.status) : Stepped s (applyWrite s1 (.PutPayment q)) := by
  intro n p' hp'
  rw [findPay_put] at hp'
  split at hp'
  · rename_i e
    cases hp'
    have : findPay s1 n = some base := by rw [← e, hid]; exact hb
    rcases h n base this with ⟨p, hp, hst'⟩ | ⟨hn, hz⟩
    · exact .inl ⟨p, hp, by rw [hst]; exact hst'⟩
    · exact .inr ⟨hn, by rw [hst]; exact hz⟩
  · exact h n p' hp'

/-- Every successful command keeps the invariant and steps statuses only
along `Allowed`. -/
theorem effect (a : Principal) (s : Snapshot) (c : Command) ws r
    (hinv : Inv (Snapshot.toSt s)) (h : transition a s c = .ok (.Ok (ws, r))) :
    Inv (applyAll (Snapshot.toSt s) ws.val) ∧ Stepped (Snapshot.toSt s) (applyAll (Snapshot.toSt s) ws.val) := by
  obtain ⟨ws0, hd, hsh, ⟨_, rfl⟩ | ⟨_, base, n, m, old, hcase, _, hws⟩⟩ := transition_writes a s c ws r h
  · exact effect0 a s c ws r hinv hd
  obtain ⟨hi1, hs1⟩ := effect0 a s c ws0 r hinv hd
  obtain ⟨hfind, hst, -, -⟩ := logged_state a s c ws0 base n m old ws.val hsh hcase hws
  rw [hst]
  have hstep : Step base (afterOf a c base m) := ⟨rfl, rfl, rfl, rfl, .inl rfl, fun h => h⟩
  have hp' : findPay (applyAll (Snapshot.toSt s) ws0.val) (afterOf a c base m).id.val = some base := hfind
  have hi2 := inv_put _ base _ hi1 hp' hstep
  have hs2 := stepped_reput (q := afterOf a c base m) hs1 hfind rfl rfl
  exact ⟨⟨hi2.fresh_pay, hi2.fresh_refund, hi2.amounts⟩, fun k q hq => hs2 k q hq⟩

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
  obtain ⟨ws0, hd, _, ⟨_, rfl⟩ | ⟨hne, _⟩⟩ := transition_writes a s _ ws r h
  · rcases post_of_ok (event_spec a s ev id res) hd ws r rfl with ⟨_, hws, hr⟩ | ⟨hn, _⟩
    · exact ⟨hws, hr⟩
    · exact absurd hn hseen
  · rcases post_of_ok (event_spec a s ev id res) hd ws0 r rfl with ⟨_, hws, _⟩ | ⟨hn, _⟩
    · exact absurd hws hne
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
  obtain ⟨ws, r, h, -⟩ := transition_ok a s c ws r h
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
theorem records0 (a : Principal) (s : Snapshot) (c : Command) ws r
    (h : decide_command a s c = .ok (.Ok (ws, r))) : Records a.opId ws.val := by
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
  | History p f l => rw [history_page_writes a s p f l ws r h]; exact records_nil _

/-- The payment row a logged change writes is its base row: the only one
the decision wrote, or the row as it was. -/
theorem base_of_mem {s : Snapshot} {c : Command} {ws0 : List Write} {base p : Payment} {n : U64} {old : Option Payment}
    (hsh : Shape ws0)
    (hcase : (subj c = none ∧ lastPay ws0 = some base ∧ base.id = s.counter.next_id ∧ old = none ∧ n = 0#u64) ∨
         (∃ id o, subj c = some id ∧ findPay (Snapshot.toSt s) id.val = some o ∧ old = some o ∧
            base = (lastPay ws0).getD o ∧ base.id = id ∧ n = o.changes))
    (hp : Write.PutPayment p ∈ ws0) : base = p := by
  rcases hcase with ⟨_, hl, _⟩ | ⟨id, o, _, _, _, hb, _⟩
  · exact hsh.2.2 _ _ (lastPay_mem hl) hp
  · cases hl : lastPay ws0 with
    | none => exact absurd hp (lastPay_none hl p)
    | some b => rw [hl] at hb; simp at hb; subst hb; exact hsh.2.2 _ _ (lastPay_mem hl) hp

/-- **Every row records who wrote it.** Each row a successful command
writes (payment, refund, event, PSP reference) carries the caller's
operator id: the operator's own id for `Actor.Operator o`, 0 for a user.
Operators are told apart in the stored state, and nothing a user does is
attributed to an operator. -/
theorem writes_record_operator (a : Principal) (s : Snapshot) (c : Command) ws r
    (h : transition a s c = .ok (.Ok (ws, r))) : Records a.opId ws.val := by
  obtain ⟨ws0, hd, _, ⟨_, rfl⟩ | ⟨_, base, n, m, old, _, _, hws⟩⟩ := transition_writes a s c ws r h
  · exact records0 a s c ws r hd
  · have h0 := records0 a s c ws0 r hd
    rw [hws]; intro w hw o ho
    rcases List.mem_append.1 hw with hw | hw
    · exact h0 w (mem_othersL.1 hw).1 o ho
    · simp at hw; rcases hw with rfl | rfl <;> simp [Write.recorded, afterOf] at ho; exact ho.symm

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
        p' = { p with operator := a.opId, ledger_done := true, changes := p'.changes } ∧
        p'.changes.val = p.changes.val + 1 := by
  obtain ⟨ws0, hd, _, ⟨he, rfl⟩ | ⟨hne, base, n, m, old, hcase, hm, hws⟩⟩ := transition_writes a s _ ws r h
  · obtain ⟨hop, _⟩ := post_of_ok (mark_posted_spec a s id cap seen) hd ws r rfl
    exact ⟨(operator_iff a).1 hop, fun p' hp' => by rw [he] at hp'; simp at hp'⟩
  obtain ⟨hop, hw⟩ := post_of_ok (mark_posted_spec a s id cap seen) hd ws0 r rfl
  refine ⟨(operator_iff a).1 hop, fun p' hp' => ?_⟩
  rcases hw with hws0 | ⟨p, hp, h1, h2, h3, h4, hws0⟩
  · exact absurd hws0 hne
  rw [hws] at hp'
  rcases List.mem_append.1 hp' with hp' | hp'
  · exact absurd (mem_othersL.1 hp').2.1 (by simp [isPayW])
  simp only [List.mem_cons, Write.PutPayment.injEq, reduceCtorEq, List.mem_nil_iff, or_false] at hp'
  subst hp'
  rcases hcase with ⟨hs, _⟩ | ⟨id', o, hs, ho, _, hb, _, hn⟩
  · simp [subj] at hs
  simp only [subj, Option.some.injEq] at hs; subst hs
  rw [hp] at ho; cases ho
  rw [hws0] at hb; simp [lastPay, lastPayStep] at hb; subst hb
  refine ⟨p, hp, h1, h2, h3, h4, by simp [afterOf, isMark], by simp [afterOf, hm, hn]⟩

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
theorem clears0 (a : Principal) (s : Snapshot) (c : Command) ws r
    (hc : ∀ id cap seen, c ≠ .MarkPosted id cap seen) (h : decide_command a s c = .ok (.Ok (ws, r))) :
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
  | History p f l => rw [history_page_writes a s p f l ws r h]; exact clears_nil

theorem changes_clear_ledger_done (a : Principal) (s : Snapshot) (c : Command) ws r
    (hc : ∀ id cap seen, c ≠ .MarkPosted id cap seen) (h : transition a s c = .ok (.Ok (ws, r))) :
    ClearsMark ws.val := by
  obtain ⟨ws0, hd, hsh, ⟨_, rfl⟩ | ⟨_, base, n, m, old, hcase, _, hws⟩⟩ := transition_writes a s c ws r h
  · exact clears0 a s c ws r hc hd
  have h0 := clears0 a s c ws0 r hc hd
  have hmk : isMark c = false := by
    cases c <;> simp only [isMark]
    exact absurd rfl (hc _ _ _)
  rw [hws]
  refine ⟨fun p hp => ?_, fun rf hrf => ?_⟩
  · rcases List.mem_append.1 hp with hp | hp
    · exact absurd (mem_othersL.1 hp).2.1 (by simp [isPayW])
    · simp at hp; subst hp; simp [afterOf, hmk]
  · rcases List.mem_append.1 hrf with hrf | hrf
    · obtain ⟨p, hp, hpe⟩ := h0.2 rf (mem_othersL.1 hrf).1
      have := base_of_mem hsh hcase hp
      exact ⟨afterOf a c base m, by simp, by rw [← hpe, this]; rfl⟩
    · simp at hrf

/-! ## The history (G1)

Every successful command that changes anything writes exactly one history
entry, for the payment it changes, naming the caller and the transaction's
clock (`every_change_logged`); in every reachable state, that entry is
appended under a key no earlier entry has, so every earlier entry stays,
unchanged and in order (`history_appended`): the history is only ever
appended to. -/

theorem filter_others_pay (ws : List Write) : (othersL ws).filter isPayW = [] :=
  List.filter_eq_nil_iff.2 (fun w hw => by simp [(mem_othersL.1 hw).2.1])

theorem filter_others_hist (ws : List Write) : (othersL ws).filter isHistW = [] :=
  List.filter_eq_nil_iff.2 (fun w hw => by simp [(mem_othersL.1 hw).2.2])

/-- **Every change is logged once, naming the caller.** A successful
command writes nothing, or exactly one payment row and exactly one history
entry: entry `n` of that payment, which leaves it with `n + 1` entries;
it names the acting principal (user or operator, and its id) and the
transaction's clock, and records the row's status and amounts after the
change. -/
theorem every_change_logged (a : Principal) (s : Snapshot) (c : Command) ws r
    (h : transition a s c = .ok (.Ok (ws, r))) :
    ws.val = [] ∨ ∃ p e, ws.val.filter isPayW = [.PutPayment p] ∧ ws.val.filter isHistW = [.PutHistory e] ∧
      e.payment = p.id ∧ e.n.val + 1 = p.changes.val ∧
      ((e.by_operator = false ∧ a.actor = .User e.actor) ∨ (e.by_operator = true ∧ a.actor = .Operator e.actor)) ∧
      e.at = a.at ∧ e.status_after = p.status ∧ e.captured_after = p.captured ∧
      e.refunded_after = p.refunded ∧ e.pending_after = p.refund_pending := by
  obtain ⟨ws0, hd, _, ⟨he, rfl⟩ | ⟨_, base, n, m, old, _, hm, hws⟩⟩ := transition_writes a s c ws r h
  · exact .inl he
  refine .inr ⟨afterOf a c base m, entryOf a s c n old (afterOf a c base m), ?_, ?_, rfl, ?_, ?_, rfl, rfl, rfl, rfl, rfl⟩
  · rw [hws, List.filter_append, filter_others_pay]; simp [isPayW]
  · rw [hws, List.filter_append, filter_others_hist]; simp [isHistW]
  · simp [entryOf, afterOf, hm]
  · rcases a with ⟨org, act, t⟩
    cases act <;> simp [entryOf, actorId, Principal.operator]

theorem findPay_of_payments {s s' : St} {q : Payment} (h : s'.payments = upsert (·.id) q s.payments) (k : Nat) :
    findPay s' k = if q.id.val = k then some q else findPay s k := by
  have := findPay_put s q k
  simp only [findPay, applyWrite] at this ⊢
  rw [h]; exact this

/-- The history after a logged change: the entry appended under a fresh
key, and the invariant kept. -/
theorem hinv_step (a : Principal) (s : Snapshot) (c : Command) ws r
    (hinv : Inv (Snapshot.toSt s)) (hh : HInv (Snapshot.toSt s)) (h : transition a s c = .ok (.Ok (ws, r))) :
    HInv (applyAll (Snapshot.toSt s) ws.val) ∧
    ((applyAll (Snapshot.toSt s) ws.val).history = (Snapshot.toSt s).history ∨
     ∃ e, Write.PutHistory e ∈ ws.val ∧
       (∀ x ∈ (Snapshot.toSt s).history, (x.payment, x.n) ≠ (e.payment, e.n)) ∧
       (applyAll (Snapshot.toSt s) ws.val).history = (Snapshot.toSt s).history ++ [e] ∧
       (match findPay (Snapshot.toSt s) e.payment.val with
        | none => e.n = 0#u64 ∧ e.status_before = NO_STATUS
        | some p => e.n = p.changes ∧ e.status_before = p.status ∧ e.captured_before = p.captured ∧
            e.refunded_before = p.refunded ∧ e.pending_before = p.refund_pending)) := by
  obtain ⟨ws0, hd, hsh, ⟨_, rfl⟩ | ⟨_, base, n, m, old, hcase, hm, hws⟩⟩ := transition_writes a s c ws r h
  · have e0 := (decide_shape a s c ws r hd)
    rename_i he
    rw [he]; exact ⟨hh, .inl rfl⟩
  obtain ⟨-, -, hpay, hhist⟩ := logged_state a s c ws0 base n m old ws.val hsh hcase hws
  set st := Snapshot.toSt s
  set after := afterOf a c base m
  set e := entryOf a s c n old after
  have hep : e.payment = base.id := rfl
  have hen : e.n = n := rfl
  -- No earlier entry has the new key.
  have hfresh : ∀ x ∈ st.history, (x.payment, x.n) ≠ (e.payment, e.n) := by
    intro x hx hk
    simp only [Prod.mk.injEq] at hk
    obtain ⟨p, hp, hlt⟩ := hh x hx
    rcases hcase with ⟨_, _, hid, _, hn0⟩ | ⟨id, o, _, ho, _, _, hid, hno⟩
    · rw [hk.1, hep, hid] at hp
      have := (findPay_mem hp)
      have := hinv.fresh_pay p this.1
      simp only [st, Snapshot.toSt] at *; omega
    · rw [hk.1, hep, hid, ho] at hp; cases hp
      rw [hk.2, hen, hno] at hlt; exact lt_irrefl _ hlt
  have happ : (applyAll st ws.val).history = st.history ++ [e] := by
    rw [hhist]; exact upsert_fresh _ _ _ hfresh
  have hfind := findPay_of_payments hpay
  refine ⟨?_, .inr ⟨e, by rw [hws]; simp, hfresh, happ, ?_⟩⟩
  · intro x hx
    rw [happ] at hx
    rw [hfind]
    rcases List.mem_append.1 hx with hx | hx
    · obtain ⟨p, hp, hlt⟩ := hh x hx
      by_cases hk : after.id.val = x.payment.val
      · rw [if_pos hk]
        refine ⟨after, rfl, ?_⟩
        rcases hcase with ⟨_, _, hid, _, _⟩ | ⟨id, o, _, ho, _, _, hid, hno⟩
        · have : base.id.val = x.payment.val := hk
          rw [← this, hid] at hp
          have := hinv.fresh_pay p (findPay_mem hp).1
          have := (findPay_mem hp).2
          simp only [st, Snapshot.toSt] at *; omega
        · have : base.id.val = x.payment.val := hk
          rw [← this, hid, ho] at hp; cases hp
          show x.n.val < m.val; rw [hm, hno]; omega
      · rw [if_neg hk]; exact ⟨p, hp, hlt⟩
    · simp at hx; subst hx
      have he : after.id.val = e.payment.val := rfl
      simp only [he, if_true]; exact ⟨after, rfl, by show n.val < m.val; omega⟩
  · rcases hcase with ⟨_, _, hid, hold, hn0⟩ | ⟨id, o, _, ho, hold, _, hid, hno⟩
    · have : findPay st e.payment.val = none := by
        rw [hep, hid]
        exact List.find?_eq_none.2 fun b hb => by
          have := hinv.fresh_pay b hb; simp [st, Snapshot.toSt] at *; omega
      rw [this]; exact ⟨hn0, by simp [e, entryOf, hold]⟩
    · rw [hep, hid, ho]; simp [e, entryOf, hold, hno]

theorem init_hinv : HInv init := by simp [HInv, init]

/-- The history invariant holds in every reachable state. -/
theorem reachable_hinv {s : St} (h : Reachable s) : HInv s := by
  induction h with
  | init => exact init_hinv
  | step hr ht ih => exact (hinv_step _ _ _ _ _ (reachable_inv hr) ih ht).1

/-- **The history is only appended to.** In every reachable state, a
successful command leaves the history as it was (it wrote nothing), or
appends one entry, written by the command, under a key (payment, n) no
earlier entry has: every earlier entry stays, unchanged and in order. The
entry's "before" fields are the payment row as it was (number of entries,
status, amounts), or, for a new payment, entry 0 with no status before. -/
theorem history_appended (a : Principal) (s : Snapshot) (c : Command) ws r
    (hr : Reachable (Snapshot.toSt s)) (h : transition a s c = .ok (.Ok (ws, r))) :
    (applyAll (Snapshot.toSt s) ws.val).history = (Snapshot.toSt s).history ∨
     ∃ e, Write.PutHistory e ∈ ws.val ∧
       (∀ x ∈ (Snapshot.toSt s).history, (x.payment, x.n) ≠ (e.payment, e.n)) ∧
       (applyAll (Snapshot.toSt s) ws.val).history = (Snapshot.toSt s).history ++ [e] ∧
       (match findPay (Snapshot.toSt s) e.payment.val with
        | none => e.n = 0#u64 ∧ e.status_before = NO_STATUS
        | some p => e.n = p.changes ∧ e.status_before = p.status ∧ e.captured_before = p.captured ∧
            e.refunded_before = p.refunded ∧ e.pending_before = p.refund_pending) :=
  (hinv_step a s c ws r (reachable_inv hr) (reachable_hinv hr) h).2

end payment_domain.Theorems
