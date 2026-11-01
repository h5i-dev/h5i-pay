import Payments.Plan
/-!
# Frame theorem: the payment kernel's read plans load enough

`frame`: for every command (`readsA a c = some plan`, which the extracted
`reads` computes by `Plan.reads_abs`), `transition` on `Snapshot.load plan s`
(what `PaymentsStore::load_for` decodes, `decode_load`) gives exactly the
result it gives on the whole tenant `s`, for every `s`.

Every command looks rows up by key, except the listings: each plan keeps,
of every table it reads, all rows a lookup can return
(`H5iAppLib.Query.find?_slice`), and for `List` and `Refunds` every row the
listing's filter keeps (`filter_slice`). `GetRefund` loads the payment of
the requested refund with `IN (SELECT payment FROM refunds WHERE id = ...)`
(`holds_in_1`).
-/
open Aeneas Aeneas.Std Result payment_domain payment_domain.Commands payment_domain.Schema
  payment_domain.Plan H5iAppLib H5iAppLib.Query

namespace payment_domain.Frame

open Classical

/-! ## Lookups as equations -/

theorem find_payment_eq (v : alloc.vec.Vec Payment) (id : U64) :
    find_payment v id = ok (v.val.find? (fun p => p.id.val = id.val)) :=
  eq_ok_of_spec (find_payment_spec v id)

theorem find_refund_eq (v : alloc.vec.Vec Refund) (id : U64) :
    find_refund v id = ok (v.val.find? (fun r => r.id.val = id.val)) :=
  eq_ok_of_spec (find_refund_spec v id)

theorem find_event_eq (v : alloc.vec.Vec Event) (id : U64) :
    find_event v id = ok (v.val.find? (fun e => e.id.val = id.val)) :=
  eq_ok_of_spec (find_event_spec v id)

@[step] theorem find_ref_spec (v : alloc.vec.Vec PspRef) (id : U64) :
    find_ref v id ⦃ o => o = v.val.find? (fun r => r.payment.val = id.val) ⦄ := by
  unfold find_ref find_ref_loop
  apply WP.spec_mono (loop_search v.val (fun r => decide (r.payment = id)) (fun o : Option PspRef => o)
    (fun _ r => some r) none _ ?_ 0#usize (by simp))
  · intro r hr; rw [search_find _ _ _ hr]; simp
  · intro j hj; unfold find_ref_loop.body; h5i_step

theorem find_ref_eq (v : alloc.vec.Vec PspRef) (id : U64) :
    find_ref v id = ok (v.val.find? (fun r => r.payment.val = id.val)) :=
  eq_ok_of_spec (find_ref_spec v id)

theorem may_manage_eq (actor : Principal) (p : Payment) :
    may_manage actor p = ok (actor.operator || decide (p.merchant = actor.user)) := by
  unfold may_manage; cases actor.operator <;> simp

theorem vec_index_eq {α : Type} (v : alloc.vec.Vec α) (i : Usize) (h : i.val < v.val.length) :
    alloc.vec.Vec.index (core.slice.index.SliceIndexUsizeSlice α) v i = ok v.val[i.val] := by
  rw [alloc.vec.Vec.index_slice_index]; unfold alloc.vec.Vec.index_usize
  simp [h]

/-! ## Each command reads its keys -/

/-- `s'` agrees with `s` on the payment with id `id`. -/
def AgreeP (s s' : Snapshot) (id : U64) : Prop :=
  s'.payments.val.find? (fun p => p.id.val = id.val) = s.payments.val.find? (fun p => p.id.val = id.val)

def AgreeR (s s' : Snapshot) (id : U64) : Prop :=
  s'.refunds.val.find? (fun r => r.id.val = id.val) = s.refunds.val.find? (fun r => r.id.val = id.val)

def AgreeE (s s' : Snapshot) (id : U64) : Prop :=
  s'.events.val.find? (fun e => e.id.val = id.val) = s.events.val.find? (fun e => e.id.val = id.val)

def AgreeF (s s' : Snapshot) (id : U64) : Prop :=
  s'.refs.val.find? (fun r => r.payment.val = id.val) = s.refs.val.find? (fun r => r.payment.val = id.val)

theorem create_congr (a : Principal) (s s' : Snapshot) (h : s'.counter = s.counter) (amt : U64) (cur : U32)
    (m : Bool) : create a s' amt cur m = create a s amt cur m := by
  unfold create; rw [h]

theorem begin_authorize_congr (a : Principal) (s s' : Snapshot) (id : U64) (h : AgreeP s s' id) :
    begin_authorize a s' id = begin_authorize a s id := by
  try simp only [AgreeP, AgreeR, AgreeE, AgreeF] at *
  unfold begin_authorize; simp only [find_payment_eq, h]

theorem cancel_congr (a : Principal) (s s' : Snapshot) (id : U64) (h : AgreeP s s' id) :
    cancel a s' id = cancel a s id := by
  try simp only [AgreeP, AgreeR, AgreeE, AgreeF] at *
  unfold cancel; simp only [find_payment_eq, h]

theorem request_refund_congr (a : Principal) (s s' : Snapshot) (id amt : U64) (h : AgreeP s s' id)
    (hc : s'.counter = s.counter) : request_refund a s' id amt = request_refund a s id amt := by
  try simp only [AgreeP, AgreeR, AgreeE, AgreeF] at *
  unfold request_refund; simp only [find_payment_eq, h, hc]

/-- A refund result reads its refund; the others read nothing but the payment. -/
def AgreeRes (s s' : Snapshot) : PspResult → Prop
  | .RefundSucceeded r => AgreeR s s' r
  | .RefundFailed r => AgreeR s s' r
  | _ => True

theorem record_refund_congr (s s' : Snapshot) (p : Payment) (r : U64) (b : Bool) (h : AgreeR s s' r) :
    record_refund s' p r b = record_refund s p r b := by
  try simp only [AgreeP, AgreeR, AgreeE, AgreeF] at *
  unfold record_refund; simp only [find_refund_eq, h]

theorem record_congr (a : Principal) (s s' : Snapshot) (id : U64) (res : PspResult) (h : AgreeP s s' id)
    (hr : AgreeRes s s' res) : record a s' id res = record a s id res := by
  simp only [AgreeP] at h
  unfold record
  simp only [find_payment_eq, h, bind_ok]
  cases hf : s.payments.val.find? (fun p => decide (p.id.val = id.val)) with
  | none => rfl
  | some p =>
    simp only []
    cases res <;> simp only [AgreeRes] at hr <;> first | rfl | simp only [record_refund_congr s s' _ _ _ hr]

theorem event_congr (a : Principal) (s s' : Snapshot) (ev id : U64) (res : PspResult) (he : AgreeE s s' ev)
    (h : AgreeP s s' id) (hr : AgreeRes s s' res) : event a s' ev id res = event a s ev id res := by
  have he' := he
  simp only [AgreeE] at he'
  unfold event
  simp only [find_event_eq, he', he, record_congr a s s' id res h hr]

theorem attach_ref_congr (a : Principal) (s s' : Snapshot) (id : U64) (r : alloc.vec.Vec U8) (h : AgreeP s s' id)
    (hf : AgreeF s s' id) : attach_ref a s' id r = attach_ref a s id r := by
  try simp only [AgreeP, AgreeR, AgreeE, AgreeF] at *
  unfold attach_ref; simp only [find_payment_eq, find_ref_eq, h, hf]

theorem get_congr (a : Principal) (s s' : Snapshot) (id : U64) (h : AgreeP s s' id) : get a s' id = get a s id := by
  try simp only [AgreeP, AgreeR, AgreeE, AgreeF] at *
  unfold get; simp only [find_payment_eq, h]

theorem get_ref_congr (a : Principal) (s s' : Snapshot) (id : U64) (h : AgreeP s s' id) (hf : AgreeF s s' id) :
    get_ref a s' id = get_ref a s id := by
  try simp only [AgreeP, AgreeR, AgreeE, AgreeF] at *
  unfold get_ref; simp only [find_payment_eq, find_ref_eq, h, hf]

theorem get_refund_congr (a : Principal) (s s' : Snapshot) (id : U64) (hR : AgreeR s s' id)
    (hP : ∀ r, s.refunds.val.find? (fun r => r.id.val = id.val) = some r → AgreeP s s' r.payment) :
    get_refund a s' id = get_refund a s id := by
  simp only [AgreeR] at hR
  unfold get_refund
  simp only [find_refund_eq, hR]
  simp only [bind_ok]
  cases hf : s.refunds.val.find? (fun r => r.id.val = id.val) with
  | none => rfl
  | some r =>
    have := hP r hf
    simp only [AgreeP] at this
    simp only [bind_ok, find_payment_eq, this]

/-! ## The listings are filters -/

def refundsStep (id : U64) (acc : List Refund) (x : Refund) : List Refund :=
  if x.payment = id then acc ++ [x] else acc

theorem foldl_refundsStep (id : U64) (l acc : List Refund) :
    l.foldl (refundsStep id) acc = acc ++ l.filter (fun x => decide (x.payment = id)) := by
  induction l generalizing acc with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, refundsStep, List.filter_cons]
    split <;> simp_all

theorem refunds_loop_spec (v : alloc.vec.Vec Refund) (id : U64) :
    refunds_of_loop v id (alloc.vec.Vec.new Refund) 0#usize ⦃ out => out.val = v.val.filter (fun x => decide (x.payment = id)) ⦄ := by
  unfold refunds_of_loop
  have h := loop_fold v.val (fun w : alloc.vec.Vec Refund => w.val) (refundsStep id)
    (fun w k => w.length + (v.length - k) ≤ Usize.max)
    (fun x => refunds_of_loop.body v id x.1 x.2) ?_ (alloc.vec.Vec.new Refund) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_refundsStep]; simp)
  · intro w j hj hw
    unfold refunds_of_loop.body
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

theorem refunds_of_congr (a : Principal) (s s' : Snapshot) (id : U64) (h : AgreeP s s' id)
    (hr : s'.refunds.val.filter (fun x => decide (x.payment = id)) = s.refunds.val.filter (fun x => decide (x.payment = id))) :
    refunds_of a s' id = refunds_of a s id := by
  simp only [AgreeP] at h
  unfold refunds_of
  simp only [find_payment_eq, h]
  congr 1; funext o
  split
  · rfl
  · simp only [may_manage_eq, bind_ok]
    split
    · obtain ⟨v, hv⟩ := ok_of (refunds_loop_spec s.refunds id)
      obtain ⟨v', hv'⟩ := ok_of (refunds_loop_spec s'.refunds id)
      have e := post_of_ok (refunds_loop_spec s.refunds id) hv
      have e' := post_of_ok (refunds_loop_spec s'.refunds id) hv'
      have : v' = v := alloc.vec.Vec.ext _ _ (by rw [e, e', hr])
      rw [hv, hv', this]
    · rfl

def visibleP (actor : Principal) (x : Payment) : Bool := actor.operator || decide (x.merchant = actor.user)

def visStep (actor : Principal) (acc : List Payment) (x : Payment) : List Payment :=
  if visibleP actor x then acc ++ [x] else acc

theorem foldl_visStep (actor : Principal) (l acc : List Payment) :
    l.foldl (visStep actor) acc = acc ++ l.filter (visibleP actor) := by
  induction l generalizing acc with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, visStep, List.filter_cons]
    split <;> simp

theorem visible_spec (actor : Principal) (s : Snapshot) :
    visible actor s ⦃ v => v.val = s.payments.val.filter (visibleP actor) ⦄ := by
  unfold visible visible_loop
  have h := loop_fold s.payments.val (fun v : alloc.vec.Vec Payment => v.val) (visStep actor)
    (fun v k => v.length + (s.payments.length - k) ≤ Usize.max)
    (fun x => visible_loop.body actor s x.1 x.2) ?_ (alloc.vec.Vec.new Payment) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_visStep]; simp)
  · intro v j hj hv
    unfold visible_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < s.payments.val.length := by scalar_tac
      rw [vec_index_eq s.payments j hlt']
      simp only [bind_ok, may_manage_eq]
      by_cases hb : visibleP actor s.payments.val[j.val]
      · have hb' : (actor.operator || decide (s.payments.val[j.val].merchant = actor.user)) = true := hb
        simp only [hb', if_true]
        step*
        simp only [FoldStep]
        refine ⟨hlt', ?_, by scalar_tac, ?_⟩
        · simp_all [visStep]
        · simp only [alloc.vec.Vec.length] at *; scalar_tac
      · have hb' : (actor.operator || decide (s.payments.val[j.val].merchant = actor.user)) = false :=
          Bool.eq_false_iff.mpr hb
        simp only [hb', Bool.false_eq_true, if_false, bind_ok]
        step*
        simp only [FoldStep]
        refine ⟨hlt', ?_, by scalar_tac, ?_⟩
        · show v.val = visStep actor v.val s.payments.val[j.val]
          simp only [visStep, if_neg hb]
        · simp only [alloc.vec.Vec.length] at *; scalar_tac
    · rename_i hge
      simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem visible_eq (actor : Principal) (s : Snapshot) :
    visible actor s = ok (alloc.vec.Vec.from (s.payments.val.filter (visibleP actor))
      (le_trans (List.length_filter_le _ _) s.payments.len_ineq)) := by
  obtain ⟨v, hv⟩ := ok_of (visible_spec actor s)
  have := post_of_ok (visible_spec actor s) hv
  rw [hv]; congr 1; apply alloc.vec.Vec.ext; simp [this]

/-! ## What a plan loads -/

/-- A read of table `t` by `q` (no window) keeps every row a lookup by `P` can return. -/
theorem pick_find {α : Type} (plan : List (Read Val)) (tabs : H5iAppLib.Sql.Tables Val) (t : Nat)
    (row : α → List Val) (l : List α) (q : Read Val)
    (hq : plan.find? (fun q => decide (q.table = t)) = some q) (hw : q.window = {}) (P : α → Bool)
    (h : ∀ x ∈ l, P x = true → q.pred.holds tabs (row x) = true) :
    (pickRows plan tabs t row l).find? P = l.find? P := by
  rw [pickRows_filter _ _ _ _ _ q hq hw]; exact find?_slice _ _ _ h

theorem holds_eq_id (tabs : H5iAppLib.Sql.Tables Val) (id : U64) (x : Payment) :
    (eqU 0 id).holds tabs (Payment.row x) = decide (x.id = id) := by
  simp [eqU, Pred.holds, Payment.row]

/-- Loading by `plan` keeps the payment `id`, when the plan's read of payments is `id = …`. -/
theorem agreeP_load (plan : List (Read Val)) (s : Snapshot) (id : U64)
    (hq : plan.find? (fun q => decide (q.table = 0)) = some ⟨0, eqU 0 id, {}⟩) :
    AgreeP s (Snapshot.load plan s) id := by
  unfold AgreeP; rw [Snapshot.load_payments]
  apply pick_find _ _ _ _ _ _ hq rfl
  intro x _ hx; simp_all [eqU, Pred.holds, Payment.row]

theorem agreeR_load (plan : List (Read Val)) (s : Snapshot) (id : U64)
    (hq : plan.find? (fun q => decide (q.table = 1)) = some ⟨1, eqU 0 id, {}⟩) :
    AgreeR s (Snapshot.load plan s) id := by
  unfold AgreeR; rw [Snapshot.load_refunds]
  apply pick_find _ _ _ _ _ _ hq rfl
  intro x _ hx; simp_all [eqU, Pred.holds, Refund.row]

theorem agreeE_load (plan : List (Read Val)) (s : Snapshot) (id : U64)
    (hq : plan.find? (fun q => decide (q.table = 2)) = some ⟨2, eqU 0 id, {}⟩) :
    AgreeE s (Snapshot.load plan s) id := by
  unfold AgreeE; rw [Snapshot.load_events]
  apply pick_find _ _ _ _ _ _ hq rfl
  intro x _ hx; simp_all [eqU, Pred.holds, Event.row]

theorem agreeF_load (plan : List (Read Val)) (s : Snapshot) (id : U64)
    (hq : plan.find? (fun q => decide (q.table = 3)) = some ⟨3, eqU 0 id, {}⟩) :
    AgreeF s (Snapshot.load plan s) id := by
  unfold AgreeF; rw [Snapshot.load_refs]
  apply pick_find _ _ _ _ _ _ hq rfl
  intro x _ hx; simp_all [eqU, Pred.holds, PspRef.row]

theorem agreeRes_load (plan : List (Read Val)) (s : Snapshot) (res : PspResult)
    (hq : ∀ r, resultA res = [⟨1, eqU 0 r, {}⟩] → plan.find? (fun q => decide (q.table = 1)) = some ⟨1, eqU 0 r, {}⟩) :
    AgreeRes s (Snapshot.load plan s) res := by
  cases res <;> simp only [AgreeRes] <;> first | trivial | exact agreeR_load _ _ _ (hq _ rfl)

theorem counter_load (s : Snapshot) (plan : List (Read Val))
    (hq : plan.find? (fun q => decide (q.table = 4)) = some ⟨4, .all, {}⟩) :
    (Snapshot.load plan s).counter = s.counter := by
  simp [Snapshot.load, pickRows, hq, Pred.holds]

theorem resultA_tables (res : PspResult) : ∀ q ∈ resultA res, q.table = 1 := by
  cases res <;> simp [resultA]

theorem find_append_res {t : Nat} (ht : t ≠ 1) (pre : List (Read Val)) (res : PspResult) :
    (pre ++ resultA res).find? (fun q => decide (q.table = t)) = pre.find? (fun q => decide (q.table = t)) := by
  rw [List.find?_append]
  cases pre.find? (fun q => decide (q.table = t)) with
  | some _ => rfl
  | none =>
    simp only [Option.none_or]
    rw [List.find?_eq_none]
    intro q hq; simp [resultA_tables res q hq, Ne.symm ht]

/-! ## The frame theorem -/

/-- **Frame.** On what a command's plan loads, `transition` gives what it
gives on the whole tenant. -/
theorem frame (a : Principal) (c : Command) (plan : List (Read Val)) (s : Snapshot)
    (hp : readsA a c = some plan) : transition a (Snapshot.load plan s) c = transition a s c := by
  cases c with
  | Create amt cur m =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact create_congr a s _ (counter_load s _ (by simp)) amt cur m
  | BeginAuthorize id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact begin_authorize_congr a s _ id (agreeP_load _ s id (by simp))
  | Cancel id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact cancel_congr a s _ id (agreeP_load _ s id (by simp))
  | RequestRefund id amt =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact request_refund_congr a s _ id amt (agreeP_load _ s id (by simp)) (counter_load s _ (by simp))
  | Record id res =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    apply record_congr a s _ id res
    · apply agreeP_load; simp
    · apply agreeRes_load; intro r hr; simp [hr]
  | Event ev id res =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    apply event_congr a s _ ev id res
    · apply agreeE_load
      have := find_append_res (t := 2) (by omega) [⟨2, eqU 0 ev, {}⟩, ⟨0, eqU 0 id, {}⟩] res
      simpa using this
    · apply agreeP_load
      have := find_append_res (t := 0) (by omega) [⟨2, eqU 0 ev, {}⟩, ⟨0, eqU 0 id, {}⟩] res
      simpa using this
    · apply agreeRes_load; intro r hr; simp [hr]
  | AttachRef id r =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact attach_ref_congr a s _ id r (agreeP_load _ s id (by simp)) (agreeF_load _ s id (by simp))
  | Get id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact get_congr a s _ id (agreeP_load _ s id (by simp))
  | List =>
    simp only [transition, visible_eq]
    cases ho : a.operator
    · simp only [readsA, ho, Bool.false_eq_true, if_false, Option.some.injEq] at hp; subst hp
      have : (Snapshot.load [⟨0, eqU 1 a.user, {}⟩] s).payments.val.filter (visibleP a) =
          s.payments.val.filter (visibleP a) := by
        rw [Snapshot.load_payments, pickRows_filter _ _ _ _ _ ⟨0, eqU 1 a.user, {}⟩ (by simp) rfl]
        apply filter_slice
        intro x _ hx
        simp_all [visibleP, eqU, Pred.holds, Payment.row]
      simp only [this]
    · simp only [readsA, ho, if_true, Option.some.injEq] at hp; subst hp
      have : (Snapshot.load [⟨0, .all, {}⟩] s).payments.val = s.payments.val := by
        simp [Snapshot.load_payments, pickRows, Pred.holds]
      simp only [this]
  | GetRefund id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    simp only [transition]
    apply get_refund_congr a s _ id (agreeR_load _ s id (by simp))
    intro r hr
    have hmem := List.mem_of_find?_eq_some hr
    have hid : r.id.val = id.val := by simpa using List.find?_some hr
    unfold AgreeP; rw [Snapshot.load_payments]
    apply pick_find _ _ _ _ _ ⟨0, .isIn 0 1 (eqU 0 id) 1 {}, {}⟩ (by simp) rfl
    intro x _ hx
    rw [holds_in_1]
    simp only [Payment.row, List.getElem?_cons_zero, Window.apply_default, List.any_eq_true, List.mem_filter]
    refine ⟨r, ⟨hmem, ?_⟩, ?_⟩
    · simp [eqU, Pred.holds, Refund.row, UScalar.eq_of_val_eq hid]
    · simp only [decide_eq_true_eq] at hx
      simp [Refund.row, UScalar.eq_of_val_eq hx]
  | Refunds id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    simp only [transition]
    apply refunds_of_congr a s _ id (agreeP_load _ s id (by simp))
    rw [Snapshot.load_refunds, pickRows_filter _ _ _ _ _ ⟨1, eqU 1 id, {}⟩ (by simp) rfl]
    apply filter_slice
    intro x _ hx
    simp_all [eqU, Pred.holds, Refund.row]
  | GetRef id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact get_ref_congr a s _ id (agreeP_load _ s id (by simp)) (agreeF_load _ s id (by simp))

/-- **The plan the server runs.** For every command, `transition` on what
loading the extracted `reads` plan decodes to equals `transition` on the
whole tenant. -/
theorem load_for_sound (a : Principal) (c : Command) (s : Snapshot) (v : alloc.vec.Vec h5i_app_sql.Read)
    (hr : reads a c = ok (some v)) :
    transition a (Snapshot.load (v.val.map Read.abs) s) c = transition a s c := by
  obtain ⟨o, ho, hab⟩ := reads_abs a c
  rw [hr, Result.ok.injEq] at ho
  subst ho
  exact frame a c _ s (by simpa [planOf] using hab.symm)

end payment_domain.Frame
