import Payments.Plan
import Payments.Logged
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
  payment_domain.Plan payment_domain.Logged H5iAppLib H5iAppLib.Query

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
  unfold may_manage; simp only [is_operator_eq, user_of_eq, bind_tc_ok, bind_ok]; cases actor.operator <;> simp

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

theorem merchant_next_eq (v : alloc.vec.Vec MerchantCounter) (m : U64) :
    merchant_next v m = ok (merchantNext v.val m) :=
  eq_ok_of_spec (merchant_next_spec v m)

theorem create_congr (a : Principal) (s s' : Snapshot) (h : s'.counter = s.counter) (amt : U64) (cur : U32)
    (m : Bool)
    (hm : s'.merchants.val.find? (fun c => decide (c.merchant = a.user)) =
      s.merchants.val.find? (fun c => decide (c.merchant = a.user))) :
    create a s' amt cur m = create a s amt cur m := by
  unfold create; rw [h]; simp only [user_of_eq, merchant_next_eq, merchantNext, hm, bind_ok]

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

theorem refunds_for_congr (v v' : alloc.vec.Vec Refund) (id : U64)
    (hr : v'.val.filter (fun x => decide (x.payment = id)) = v.val.filter (fun x => decide (x.payment = id))) :
    refunds_for v' id = refunds_for v id := by
  obtain ⟨o, ho⟩ := ok_of (refunds_loop_spec v id)
  obtain ⟨o', ho'⟩ := ok_of (refunds_loop_spec v' id)
  have e := post_of_ok (refunds_loop_spec v id) ho
  have e' := post_of_ok (refunds_loop_spec v' id) ho'
  have : o' = o := alloc.vec.Vec.ext _ _ (by rw [e, e', hr])
  rw [ho, ho', this]

theorem refunds_of_congr (a : Principal) (s s' : Snapshot) (id : U64) (h : AgreeP s s' id)
    (hr : s'.refunds.val.filter (fun x => decide (x.payment = id)) = s.refunds.val.filter (fun x => decide (x.payment = id))) :
    refunds_of a s' id = refunds_of a s id := by
  simp only [AgreeP] at h
  unfold refunds_of
  simp only [find_payment_eq, h, refunds_for_congr s.refunds s'.refunds id hr]

theorem mark_posted_congr (a : Principal) (s s' : Snapshot) (id cap : U64) (seen : alloc.vec.Vec Refund) (h : AgreeP s s' id)
    (hr : s'.refunds.val.filter (fun x => decide (x.payment = id)) = s.refunds.val.filter (fun x => decide (x.payment = id))) :
    mark_posted a s' id cap seen = mark_posted a s id cap seen := by
  simp only [AgreeP] at h
  unfold mark_posted
  simp only [find_payment_eq, h, refunds_for_congr s.refunds s'.refunds id hr]

theorem view_congr (a : Principal) (s s' : Snapshot) (id : U64) (h : AgreeP s s' id) (hf : AgreeF s s' id)
    (hr : s'.refunds.val.filter (fun x => decide (x.payment = id)) = s.refunds.val.filter (fun x => decide (x.payment = id))) :
    view a s' id = view a s id := by
  simp only [AgreeP, AgreeF] at h hf
  unfold view
  simp only [find_payment_eq, find_ref_eq, h, hf, refunds_for_congr s.refunds s'.refunds id hr]

def rangeP (m f e : U64) (x : Payment) : Bool := decide (x.merchant = m) && decide (f ≤ x.seq) && decide (x.seq < e)

def rangeStepL (m f e : U64) (acc : List Payment) (x : Payment) : List Payment :=
  if rangeP m f e x then acc ++ [x] else acc

theorem foldl_rangeStepL (m f e : U64) (l acc : List Payment) :
    l.foldl (rangeStepL m f e) acc = acc ++ l.filter (rangeP m f e) := by
  induction l generalizing acc with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, rangeStepL, List.filter_cons]
    split <;> simp

theorem merchant_range_spec (s : Snapshot) (m f e : U64) :
    merchant_range s m f e ⦃ v => v.val = s.payments.val.filter (rangeP m f e) ⦄ := by
  unfold merchant_range merchant_range_loop
  have h := loop_fold s.payments.val (fun v : alloc.vec.Vec Payment => v.val) (rangeStepL m f e)
    (fun v k => v.length + (s.payments.length - k) ≤ Usize.max)
    (fun x => merchant_range_loop.body s m f e x.1 x.2) ?_ (alloc.vec.Vec.new Payment) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_rangeStepL]; simp)
  · intro v j hj hv
    unfold merchant_range_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < s.payments.val.length := by scalar_tac
      rw [vec_index_eq s.payments j hlt']
      simp only [bind_ok]
      have hj1 : j.val + 1 ≤ Usize.max := by scalar_tac
      by_cases h1 : s.payments.val[j.val].merchant = m <;> by_cases h2 : f ≤ s.payments.val[j.val].seq <;>
        by_cases h3 : s.payments.val[j.val].seq < e <;>
        simp only [h1, h2, h3, if_true, if_false, bind_ok] <;> step* <;> simp only [FoldStep] <;>
        refine ⟨hlt', ?_, by scalar_tac, by simp only [alloc.vec.Vec.length] at *; scalar_tac⟩ <;>
        simp_all [rangeStepL, rangeP]
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem merchant_range_congr (s s' : Snapshot) (m f e : U64)
    (h : s'.payments.val.filter (rangeP m f e) = s.payments.val.filter (rangeP m f e)) :
    merchant_range s' m f e = merchant_range s m f e := by
  obtain ⟨v, hv⟩ := ok_of (merchant_range_spec s m f e)
  obtain ⟨v', hv'⟩ := ok_of (merchant_range_spec s' m f e)
  have e1 := post_of_ok (merchant_range_spec s m f e) hv
  have e2 := post_of_ok (merchant_range_spec s' m f e) hv'
  have : v' = v := alloc.vec.Vec.ext _ _ (by rw [e1, e2, h])
  rw [hv, hv', this]

theorem list_congr (a : Principal) (s s' : Snapshot) (m f : U64) (l : U32) (e : U64)
    (he : page_end f l = ok e)
    (h1 : s'.payments.val.filter (rangeP m f e) = s.payments.val.filter (rangeP m f e))
    (h2 : s'.merchants.val.find? (fun c => decide (c.merchant = m)) = s.merchants.val.find? (fun c => decide (c.merchant = m))) :
    list a s' m f l = list a s m f l := by
  unfold list
  simp only [he, merchant_next_eq, merchantNext, h2, merchant_range_congr s s' m f e h1, bind_ok]

def hrangeP (p f e : U64) (x : HistoryEntry) : Bool := decide (x.payment = p) && decide (f ≤ x.n) && decide (x.n < e)

def hrangeStepL (p f e : U64) (acc : List HistoryEntry) (x : HistoryEntry) : List HistoryEntry :=
  if hrangeP p f e x then acc ++ [x] else acc

theorem foldl_hrangeStepL (p f e : U64) (l acc : List HistoryEntry) :
    l.foldl (hrangeStepL p f e) acc = acc ++ l.filter (hrangeP p f e) := by
  induction l generalizing acc with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, hrangeStepL, List.filter_cons]
    split <;> simp

theorem history_range_spec (s : Snapshot) (p f e : U64) :
    history_range s p f e ⦃ v => v.val = s.history.val.filter (hrangeP p f e) ⦄ := by
  unfold history_range history_range_loop
  have h := loop_fold s.history.val (fun v : alloc.vec.Vec HistoryEntry => v.val) (hrangeStepL p f e)
    (fun v k => v.length + (s.history.length - k) ≤ Usize.max)
    (fun x => history_range_loop.body s p f e x.1 x.2) ?_ (alloc.vec.Vec.new HistoryEntry) 0#usize (by simp)
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
      by_cases h1 : s.history.val[j.val].payment = p <;> by_cases h2 : f ≤ s.history.val[j.val].n <;>
        by_cases h3 : s.history.val[j.val].n < e <;>
        simp only [h1, h2, h3, if_true, if_false, bind_ok] <;> step* <;> simp only [FoldStep] <;>
        refine ⟨hlt', ?_, by scalar_tac, by simp only [alloc.vec.Vec.length] at *; scalar_tac⟩ <;>
        simp_all [hrangeStepL, hrangeP]
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem history_range_congr (s s' : Snapshot) (p f e : U64)
    (h : s'.history.val.filter (hrangeP p f e) = s.history.val.filter (hrangeP p f e)) :
    history_range s' p f e = history_range s p f e := by
  obtain ⟨v, hv⟩ := ok_of (history_range_spec s p f e)
  obtain ⟨v', hv'⟩ := ok_of (history_range_spec s' p f e)
  have e1 := post_of_ok (history_range_spec s p f e) hv
  have e2 := post_of_ok (history_range_spec s' p f e) hv'
  have : v' = v := alloc.vec.Vec.ext _ _ (by rw [e1, e2, h])
  rw [hv, hv', this]

theorem history_page_congr (a : Principal) (s s' : Snapshot) (p f : U64) (l : U32) (e : U64)
    (he : page_end f l = ok e) (h0 : AgreeP s s' p)
    (h1 : s'.history.val.filter (hrangeP p f e) = s.history.val.filter (hrangeP p f e)) :
    history_page a s' p f l = history_page a s p f l := by
  simp only [AgreeP] at h0
  unfold history_page
  simp only [find_payment_eq, h0, he, history_range_congr s s' p f e h1, bind_ok]

/-! ## A page by id (G6) -/

def pidP (f e : U64) (x : Payment) : Bool := decide (f ≤ x.id) && decide (x.id < e)

def pidStepL (f e : U64) (l : List Payment) (x : Payment) : List Payment :=
  if pidP f e x then l ++ [x] else l

theorem foldl_pidStepL (f e : U64) (l init : List Payment) :
    l.foldl (pidStepL f e) init = init ++ l.filter (pidP f e) := by
  induction l generalizing init with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, pidStepL, List.filter_cons]
    split <;> simp

theorem payment_ids_spec (s : Snapshot) (f e : U64) :
    payment_ids s f e ⦃ v => v.val = s.payments.val.filter (pidP f e) ⦄ := by
  unfold payment_ids payment_ids_loop
  have h := loop_fold s.payments.val (fun v : alloc.vec.Vec Payment => v.val) (pidStepL f e)
    (fun v k => v.length + (s.payments.length - k) ≤ Usize.max)
    (fun x => payment_ids_loop.body s f e x.1 x.2) ?_ (alloc.vec.Vec.new Payment) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_pidStepL]; simp)
  · intro v j hj hv
    unfold payment_ids_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < s.payments.val.length := by scalar_tac
      rw [vec_index_eq s.payments j hlt']
      simp only [bind_ok]
      have hj1 : j.val + 1 ≤ Usize.max := by scalar_tac
      by_cases h2 : f ≤ s.payments.val[j.val].id <;> by_cases h3 : s.payments.val[j.val].id < e <;>
        simp only [h2, h3, if_true, if_false, bind_ok] <;> step* <;> simp only [FoldStep] <;>
        refine ⟨hlt', ?_, by scalar_tac, by simp only [alloc.vec.Vec.length] at *; scalar_tac⟩ <;>
        simp_all [pidStepL, pidP]
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

def ridP (f e : U64) (x : Refund) : Bool := decide (f ≤ x.id) && decide (x.id < e)

def ridStepL (f e : U64) (l : List Refund) (x : Refund) : List Refund :=
  if ridP f e x then l ++ [x] else l

theorem foldl_ridStepL (f e : U64) (l init : List Refund) :
    l.foldl (ridStepL f e) init = init ++ l.filter (ridP f e) := by
  induction l generalizing init with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, ridStepL, List.filter_cons]
    split <;> simp

theorem refund_ids_spec (s : Snapshot) (f e : U64) :
    refund_ids s f e ⦃ v => v.val = s.refunds.val.filter (ridP f e) ⦄ := by
  unfold refund_ids refund_ids_loop
  have h := loop_fold s.refunds.val (fun v : alloc.vec.Vec Refund => v.val) (ridStepL f e)
    (fun v k => v.length + (s.refunds.length - k) ≤ Usize.max)
    (fun x => refund_ids_loop.body s f e x.1 x.2) ?_ (alloc.vec.Vec.new Refund) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr, foldl_ridStepL]; simp)
  · intro v j hj hv
    unfold refund_ids_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < s.refunds.val.length := by scalar_tac
      rw [vec_index_eq s.refunds j hlt']
      simp only [bind_ok]
      have hj1 : j.val + 1 ≤ Usize.max := by scalar_tac
      by_cases h2 : f ≤ s.refunds.val[j.val].id <;> by_cases h3 : s.refunds.val[j.val].id < e <;>
        simp only [h2, h3, if_true, if_false, bind_ok] <;> step* <;> simp only [FoldStep] <;>
        refine ⟨hlt', ?_, by scalar_tac, by simp only [alloc.vec.Vec.length] at *; scalar_tac⟩ <;>
        simp_all [ridStepL, ridP]
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

def parStep (s : Snapshot) (acc : List Payment) (r : Refund) : List Payment :=
  match s.payments.val.find? (fun p => p.id.val = r.payment.val) with
  | none => acc
  | some p => acc ++ [p]

theorem parStep_length (s : Snapshot) (acc : List Payment) (r : Refund) :
    (parStep s acc r).length ≤ acc.length + 1 := by
  unfold parStep; split <;> simp

theorem parents_of_spec (s : Snapshot) (rs : alloc.vec.Vec Refund) :
    parents_of s rs ⦃ v => v.val = rs.val.foldl (parStep s) [] ⦄ := by
  unfold parents_of parents_of_loop
  have h := loop_fold rs.val (fun v : alloc.vec.Vec Payment => v.val) (parStep s)
    (fun v k => v.length ≤ k)
    (fun x => parents_of_loop.body s rs x.1 x.2) ?_ (alloc.vec.Vec.new Payment) 0#usize (by simp)
    (by simp [alloc.vec.Vec.length])
  · exact WP.spec_mono h (fun r hr => by rw [hr]; simp)
  · intro v j hj hv
    unfold parents_of_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    simp only [alloc.vec.Vec.len]
    split
    · rename_i hlt
      have hlt' : j.val < rs.val.length := by scalar_tac
      rw [vec_index_eq rs j hlt']
      simp only [bind_ok]
      rw [find_payment_eq]
      simp only [bind_ok]
      have hj1 : j.val + 1 ≤ Usize.max := by scalar_tac
      cases hf : s.payments.val.find? (fun p => decide (p.id.val = rs.val[j.val].payment.val)) with
      | none =>
        simp only [bind_ok]
        step*
        simp only [FoldStep]
        refine ⟨hlt', ?_, by scalar_tac, by scalar_tac⟩
        show v.val = parStep s v.val rs.val[j.val]
        unfold parStep; rw [hf]
      | some p =>
        simp only [bind_ok]
        have : v.val.length < Usize.max := by scalar_tac
        step*
        simp only [FoldStep]
        refine ⟨hlt', by simp_all [parStep], by scalar_tac, ?_⟩
        simp_all [alloc.vec.Vec.length]
        try scalar_tac
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

theorem foldl_parStep_congr (s s' : Snapshot) (l : List Refund)
    (h : ∀ r ∈ l, s'.payments.val.find? (fun p => p.id.val = r.payment.val) = s.payments.val.find? (fun p => p.id.val = r.payment.val)) :
    ∀ acc, l.foldl (parStep s') acc = l.foldl (parStep s) acc := by
  induction l with
  | nil => intro acc; rfl
  | cons x xs ih =>
    intro acc
    simp only [List.foldl_cons]
    have e1 : parStep s' acc x = parStep s acc x := by simp only [parStep, h x (by simp)]
    rw [e1]; exact ih (fun r hr => h r (List.mem_cons_of_mem _ hr)) _

theorem range_page_congr (a : Principal) (s s' : Snapshot) (f : U64) (l : U32) (e : U64)
    (he : page_end f l = ok e) (hc : s'.counter = s.counter)
    (h1 : s'.payments.val.filter (pidP f e) = s.payments.val.filter (pidP f e)) :
    range_page a s' f l = range_page a s f l := by
  obtain ⟨ps, hps⟩ := ok_of (payment_ids_spec s f e)
  obtain ⟨ps', hps'⟩ := ok_of (payment_ids_spec s' f e)
  have ep := post_of_ok (payment_ids_spec s f e) hps
  have ep' := post_of_ok (payment_ids_spec s' f e) hps'
  have hp : ps' = ps := alloc.vec.Vec.ext _ _ (by rw [ep, ep', h1])
  unfold range_page
  simp only [he, hc, hps, hps', hp, bind_ok]

theorem refund_range_page_congr (a : Principal) (s s' : Snapshot) (f : U64) (l : U32) (e : U64)
    (he : page_end f l = ok e) (hc : s'.counter = s.counter)
    (h2 : s'.refunds.val.filter (ridP f e) = s.refunds.val.filter (ridP f e))
    (h3 : ∀ x ∈ s.refunds.val.filter (ridP f e),
      s'.payments.val.find? (fun p => p.id.val = x.payment.val) = s.payments.val.find? (fun p => p.id.val = x.payment.val)) :
    refund_range_page a s' f l = refund_range_page a s f l := by
  obtain ⟨rs, hrs⟩ := ok_of (refund_ids_spec s f e)
  obtain ⟨rs', hrs'⟩ := ok_of (refund_ids_spec s' f e)
  have er := post_of_ok (refund_ids_spec s f e) hrs
  have er' := post_of_ok (refund_ids_spec s' f e) hrs'
  have hr : rs' = rs := alloc.vec.Vec.ext _ _ (by rw [er, er', h2])
  obtain ⟨q, hq⟩ := ok_of (parents_of_spec s rs)
  obtain ⟨q', hq'⟩ := ok_of (parents_of_spec s' rs)
  have eq := post_of_ok (parents_of_spec s rs) hq
  have eq' := post_of_ok (parents_of_spec s' rs) hq'
  have hpar : q' = q := alloc.vec.Vec.ext _ _ (by
    rw [eq, eq', foldl_parStep_congr s s' _ (fun x hx => h3 x (by rw [← er]; exact hx))])
  unfold refund_range_page
  simp only [he, hc, hrs, hrs', hr, hq, hq', hpar, bind_ok]

/-! ## What a plan loads -/

theorem find_filter_kept {α : Type} (l : List α) (keep P : α → Bool) (h : ∀ x, P x = true → keep x = true) :
    (l.filter keep).find? P = l.find? P :=
  find?_slice l P keep (fun x _ hp => h x hp)

/-- `Val.Int` is the integer its bits are, read as signed. -/
theorem intOf_Int (i : I64) : H5iAppLib.Query.IntOf.intOf (h5i_app_sql.Val.Int i) = some i.val := rfl

/-- The `BIGINT` a `u64` column holds: its bits, read as signed. -/
theorem intOf_int (n : Nat) (h : n < 2 ^ 64) :
    H5iAppLib.Query.IntOf.intOf (int n) = some (if n < signN then (n : Int) else (n : Int) - 2 ^ 64) := by
  unfold int
  rw [intOf_Int]
  simp only [IScalar.val, Option.some.injEq]
  have h64 : IScalarTy.I64.numBits = 64 := rfl
  rw [h64, BitVec.toInt_eq_toNat_cond, BitVec.toNat_ofNat, Nat.mod_eq_of_lt h]
  simp only [signN]
  split <;> split <;> omega

/-- A row whose column `c` holds a number of the range satisfies the range. -/
theorem rangeA_holds_of (c f e y : Nat) (hfy : f ≤ y) (hye : y < e) (he : e ≤ U64.max)
    {tabs : H5iAppLib.Sql.Tables Val} {row : List Val}
    (hrow : row[c]? = some (int y)) : (rangeA c f e).holds tabs row = true := by
  have hmax : U64.max = 2 ^ 64 - 1 := by simp [U64.max, U64.numBits]
  have hy := intOf_int y (by omega)
  have hf := intOf_int f (by omega)
  have hE := intOf_int e (by omega)
  unfold rangeA
  split
  · rename_i hc
    simp only [Pred.holds, hrow, Option.bind_some, hy, hf, hE, Bool.or_eq_true, decide_eq_true_eq]
    by_cases hys : y < signN
    · left; simp only [hys, if_true, show f < signN by omega]; omega
    · right; simp only [hys, if_false, show ¬ e < signN by omega]; omega
  · rename_i hc
    simp only [Pred.holds, hrow, Option.bind_some, hy, hf, hE, Bool.and_eq_true, decide_eq_true_eq]
    by_cases hfs : f < signN
    · have : e < signN := by omega
      simp only [hfs, this, show y < signN by omega, if_true]; omega
    · simp only [hfs, show ¬ y < signN by omega, show ¬ e < signN by omega, if_false]; omega

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

/-- On what a command's plan loads, `decide_command` decides what it
decides on the whole tenant. -/
theorem frame0 (a : Principal) (c : Command) (plan : List (Read Val)) (s : Snapshot)
    (hp : readsA a c = some plan) : decide_command a (Snapshot.load plan s) c = decide_command a s c := by
  cases c with
  | Create amt cur m =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    apply create_congr a s _ (counter_load s _ (by simp)) amt cur m
    simp only [Snapshot.load_merchants, pickRows, List.find?, decide_true, decide_false, Window.apply_default]
    apply find_filter_kept; intro x hx; simp_all [eqU, Pred.holds, MerchantCounter.row]
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
  | List m f l =>
    simp only [decide_command]
    simp only [readsA] at hp
    split at hp
    · rename_i hc
      simp only [Option.some.injEq] at hp; subst hp
      obtain ⟨e, he, hev⟩ := ok_of' (page_end_spec f l)
      apply list_congr a s _ m f l e he
      · unfold pageA
        rw [← hev]
        by_cases hfe : f.val < e.val
        · simp only [hfe, if_true]
          rw [Snapshot.load_payments, pickRows_filter _ _ _ _ _ ⟨0, .and (eqU 1 m) (rangeA 13 f.val e.val), {}⟩ (by simp) rfl]
          apply filter_slice
          intro x _ hx
          simp only [rangeP, Bool.and_eq_true, decide_eq_true_eq] at hx
          obtain ⟨⟨h1, h2⟩, h3⟩ := hx
          simp only [Pred.holds, Bool.and_eq_true]
          refine ⟨by simp [eqU, Pred.holds, Payment.row, h1], ?_⟩
          apply rangeA_holds_of 13 f.val e.val x.seq.val (by scalar_tac) (by scalar_tac) (by scalar_tac)
          simp [Payment.row]
        · simp only [hfe, if_false]
          have : s.payments.val.filter (rangeP m f e) = [] := by
            rw [List.filter_eq_nil_iff]; intro x _; simp only [rangeP]; intro h
            simp only [Bool.and_eq_true, decide_eq_true_eq] at h; scalar_tac
          rw [this]
          simp [Snapshot.load_payments, pickRows]
      · unfold pageA
        split <;>
        · simp only [Snapshot.load_merchants, pickRows, List.find?, decide_true, decide_false, Window.apply_default]
          apply find_filter_kept; intro x hx; simp_all [eqU, Pred.holds, MerchantCounter.row]
    · rename_i hc
      unfold list
      by_cases h0 : l = 0#u32
      · simp [h0]
      have h0' : l.val ≠ 0 := fun h => h0 (by scalar_tac)
      by_cases hm : l > MAX_PAGE
      · simp [h0, hm]
      have hm' : l.val ≤ 64 := by simp only [MAX_PAGE] at hm; scalar_tac
      have hna : ¬ (a.operator = true ∨ m = a.user) := fun h => hc ⟨h0', hm', h⟩
      simp only [not_or] at hna
      simp [h0, hm, hna.1, hna.2, is_operator_eq, user_of_eq]
  | GetRefund id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    simp only [decide_command]
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
    simp only [decide_command]
    apply refunds_of_congr a s _ id (agreeP_load _ s id (by simp))
    rw [Snapshot.load_refunds, pickRows_filter _ _ _ _ _ ⟨1, eqU 1 id, {}⟩ (by simp) rfl]
    apply filter_slice
    intro x _ hx
    simp_all [eqU, Pred.holds, Refund.row]
  | GetRef id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact get_ref_congr a s _ id (agreeP_load _ s id (by simp)) (agreeF_load _ s id (by simp))
  | View id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    simp only [decide_command]
    apply view_congr a s _ id (agreeP_load _ s id (by simp)) (agreeF_load _ s id (by simp))
    rw [Snapshot.load_refunds, pickRows_filter _ _ _ _ _ ⟨1, eqU 1 id, {}⟩ (by simp) rfl]
    apply filter_slice
    intro x _ hx
    simp_all [eqU, Pred.holds, Refund.row]
  | MarkPosted id cap seen =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    simp only [decide_command]
    apply mark_posted_congr a s _ id cap seen (agreeP_load _ s id (by simp))
    rw [Snapshot.load_refunds, pickRows_filter _ _ _ _ _ ⟨1, eqU 1 id, {}⟩ (by simp) rfl]
    apply filter_slice
    intro x _ hx
    simp_all [eqU, Pred.holds, Refund.row]
  | History p f l =>
    simp only [decide_command]
    simp only [readsA] at hp
    split at hp
    · rename_i hc
      simp only [Option.some.injEq] at hp; subst hp
      obtain ⟨e, he, hev⟩ := ok_of' (page_end_spec f l)
      apply history_page_congr a s _ p f l e he
      · apply agreeP_load; unfold histA; split <;> simp
      · unfold histA
        rw [← hev]
        by_cases hfe : f.val < e.val
        · simp only [hfe, if_true]
          rw [Snapshot.load_history, pickRows_filter _ _ _ _ _ ⟨6, .and (eqU 0 p) (rangeA 1 f.val e.val), {}⟩ (by simp) rfl]
          apply filter_slice
          intro x _ hx
          simp only [hrangeP, Bool.and_eq_true, decide_eq_true_eq] at hx
          obtain ⟨⟨h1, h2⟩, h3⟩ := hx
          simp only [Pred.holds, Bool.and_eq_true]
          refine ⟨by simp [eqU, Pred.holds, HistoryEntry.row, h1], ?_⟩
          apply rangeA_holds_of 1 f.val e.val x.n.val (by scalar_tac) (by scalar_tac) (by scalar_tac)
          simp [HistoryEntry.row]
        · simp only [hfe, if_false]
          have : s.history.val.filter (hrangeP p f e) = [] := by
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
  | Range f l =>
    simp only [decide_command]
    simp only [readsA] at hp
    split at hp
    · rename_i hc
      simp only [Option.some.injEq] at hp; subst hp
      obtain ⟨e, he, hev⟩ := ok_of' (page_end_spec f l)
      apply range_page_congr a s _ f l e he
      · apply counter_load; unfold rangePageA; split <;> simp
      · unfold rangePageA
        rw [← hev]
        by_cases hfe : f.val < e.val
        · simp only [hfe, if_true]
          rw [Snapshot.load_payments, pickRows_filter _ _ _ _ _ ⟨0, rangeA 0 f.val e.val, {}⟩ (by simp) rfl]
          apply filter_slice
          intro x _ hx
          simp only [pidP, Bool.and_eq_true, decide_eq_true_eq] at hx
          apply rangeA_holds_of 0 f.val e.val x.id.val (by scalar_tac) (by scalar_tac) (by scalar_tac)
          simp [Payment.row]
        · simp only [hfe, if_false]
          have : s.payments.val.filter (pidP f e) = [] := by
            rw [List.filter_eq_nil_iff]; intro x _; simp only [pidP]; intro h
            simp only [Bool.and_eq_true, decide_eq_true_eq] at h; scalar_tac
          rw [this]
          simp [Snapshot.load_payments, pickRows]
    · rename_i hc
      unfold range_page
      by_cases h0 : l = 0#u32
      · simp [h0]
      have h0' : l.val ≠ 0 := fun h => h0 (by scalar_tac)
      by_cases hm : l > MAX_RANGE
      · simp [h0, hm]
      have hm' : l.val ≤ 4096 := by simp only [MAX_RANGE] at hm; scalar_tac
      have hna : ¬ a.operator = true := fun h => hc ⟨h0', hm', h⟩
      simp [h0, hm, hna, is_operator_eq]
  | RefundRange f l =>
    simp only [decide_command]
    simp only [readsA] at hp
    split at hp
    · rename_i hc
      simp only [Option.some.injEq] at hp; subst hp
      obtain ⟨e, he, hev⟩ := ok_of' (page_end_spec f l)
      have hrid : ∀ x : Refund, ridP f e x = true →
          (rangeA 0 f.val e.val).holds (Snapshot.enc s) (Refund.row x) = true := by
        intro x hx
        simp only [ridP, Bool.and_eq_true, decide_eq_true_eq] at hx
        apply rangeA_holds_of 0 f.val e.val x.id.val (by scalar_tac) (by scalar_tac) (by scalar_tac)
        simp [Refund.row]
      apply refund_range_page_congr a s _ f l e he
      · apply counter_load; unfold refundPageA; split <;> simp
      · unfold refundPageA
        rw [← hev]
        by_cases hfe : f.val < e.val
        · simp only [hfe, if_true]
          rw [Snapshot.load_refunds, pickRows_filter _ _ _ _ _ ⟨1, rangeA 0 f.val e.val, {}⟩ (by simp) rfl]
          exact filter_slice _ _ _ (fun x _ hx => hrid x hx)
        · simp only [hfe, if_false]
          have : s.refunds.val.filter (ridP f e) = [] := by
            rw [List.filter_eq_nil_iff]; intro x _; simp only [ridP]; intro h
            simp only [Bool.and_eq_true, decide_eq_true_eq] at h; scalar_tac
          rw [this]
          simp [Snapshot.load_refunds, pickRows]
      · intro x hx
        have hxr := (List.mem_filter.1 hx).2
        have hfe : f.val < e.val := by
          simp only [ridP, Bool.and_eq_true, decide_eq_true_eq] at hxr; scalar_tac
        have hq0 : (refundPageA f.val (pageEndN f.val l.val)).find? (fun q => decide (q.table = 0)) =
            some ⟨0, .isIn 0 1 (rangeA 0 f.val e.val) 1 {}, {}⟩ := by
          unfold refundPageA; rw [← hev]; simp [hfe]
        rw [Snapshot.load_payments, pickRows_filter _ _ _ _ _ _ hq0 rfl]
        apply find_filter_kept
        intro p hp
        simp only [decide_eq_true_eq] at hp
        rw [holds_in_1]
        simp only [Payment.row, List.getElem?_cons_zero, Window.apply_default, List.any_eq_true, List.mem_filter]
        exact ⟨x, ⟨(List.mem_filter.1 hx).1, hrid x hxr⟩, by simp [Refund.row, hp]⟩
    · rename_i hc
      unfold refund_range_page
      by_cases h0 : l = 0#u32
      · simp [h0]
      have h0' : l.val ≠ 0 := fun h => h0 (by scalar_tac)
      by_cases hm : l > MAX_RANGE
      · simp [h0, hm]
      have hm' : l.val ≤ 4096 := by simp only [MAX_RANGE] at hm; scalar_tac
      have hna : ¬ a.operator = true := fun h => hc ⟨h0', hm', h⟩
      simp [h0, hm, hna, is_operator_eq]

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

/-- `logged` reads the counter (for a new payment and a refund request's
number) and the changed payment, by id. -/
theorem logged_congr (a : Principal) (s s' : Snapshot) (c : Command) (ws : alloc.vec.Vec Write) (r : Reply)
    (hw : whatOf s' c = whatOf s c) (hs : ∀ id, subj c = some id → AgreeP s s' id)
    (hn : subj c = none → s'.counter = s.counter) :
    logged a s' c ws r = logged a s c ws r := by
  unfold logged
  simp only [what_of_eq, subject_eq, hw, bind_ok]
  cases h : subj c with
  | none => simp only [hn h]
  | some id =>
    have := hs id h
    simp only [AgreeP] at this
    simp only [find_payment_eq, this]

/-- `logged` on what a writing command's plan loads, as on the whole tenant. -/
theorem logged_frame (a : Principal) (c : Command) (plan : List (Read Val)) (s : Snapshot)
    (hp : readsA a c = some plan) (ws : alloc.vec.Vec Write) (r : Reply)
    (hx : decide_command a s c = ok (.Ok (ws, r))) (hne : ws.val ≠ []) :
    logged a (Snapshot.load plan s) c ws r = logged a s c ws r := by
  cases c with
  | Create amt cur m =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact logged_congr a s _ _ ws r (by simp [whatOf]) (by simp [subj]) (fun _ => counter_load s _ (by simp))
  | BeginAuthorize id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact logged_congr a s _ _ ws r (by simp [whatOf])
      (fun i hi => by simp [subj] at hi; subst hi; exact agreeP_load _ s _ (by simp)) (by simp [subj])
  | Cancel id =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact logged_congr a s _ _ ws r (by simp [whatOf])
      (fun i hi => by simp [subj] at hi; subst hi; exact agreeP_load _ s _ (by simp)) (by simp [subj])
  | RequestRefund id amt =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    have hc := counter_load s [⟨0, eqU 0 id, {}⟩, ⟨4, .all, {}⟩] (by simp)
    exact logged_congr a s _ _ ws r (by simp [whatOf, hc])
      (fun i hi => by simp [subj] at hi; subst hi; exact agreeP_load _ s _ (by simp)) (by simp [subj])
  | Record id res =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact logged_congr a s _ _ ws r (by simp [whatOf])
      (fun i hi => by simp [subj] at hi; subst hi; exact agreeP_load _ s _ (by simp)) (by simp [subj])
  | Event ev id res =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    refine logged_congr a s _ _ ws r (by simp [whatOf]) (fun i hi => ?_) (by simp [subj])
    simp [subj] at hi; subst hi
    apply agreeP_load
    have := find_append_res (t := 0) (by omega) [⟨2, eqU 0 ev, {}⟩, ⟨0, eqU 0 id, {}⟩] res
    simpa using this
  | AttachRef id ref =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact logged_congr a s _ _ ws r (by simp [whatOf])
      (fun i hi => by simp [subj] at hi; subst hi; exact agreeP_load _ s _ (by simp)) (by simp [subj])
  | MarkPosted id cap seen =>
    simp only [readsA, Option.some.injEq] at hp; subst hp
    exact logged_congr a s _ _ ws r (by simp [whatOf])
      (fun i hi => by simp [subj] at hi; subst hi; exact agreeP_load _ s _ (by simp)) (by simp [subj])
  | Get id => exact absurd (get_writes a s id ws r hx) hne
  | List m f l => exact absurd (list_writes a s m f l ws r hx) hne
  | GetRefund id => exact absurd (get_refund_writes a s id ws r hx) hne
  | Refunds id => exact absurd (refunds_of_writes a s id ws r hx) hne
  | GetRef id => exact absurd (get_ref_writes a s id ws r hx) hne
  | View id => exact absurd (view_writes a s id ws r hx) hne
  | History p f l => exact absurd (history_page_writes a s p f l ws r hx) hne
  | Range f l => exact absurd (range_page_writes a s f l ws r hx) hne
  | RefundRange f l => exact absurd (refund_range_page_writes a s f l ws r hx) hne

/-- **Frame.** On what a command's plan loads, `transition` (the decision
and its history entry) gives what it gives on the whole tenant. -/
theorem frame (a : Principal) (c : Command) (plan : List (Read Val)) (s : Snapshot)
    (hp : readsA a c = some plan) : transition a (Snapshot.load plan s) c = transition a s c := by
  unfold transition
  rw [frame0 a c plan s hp]
  apply bind_congr_ok
  intro x hx
  rcases x with ⟨ws, rep⟩ | e
  · change (if ws.len = 0#usize then _ else logged a (Snapshot.load plan s) c ws rep) =
      (if ws.len = 0#usize then _ else logged a s c ws rep)
    by_cases hz : ws.len = 0#usize
    · simp only [hz, if_true]
    · simp only [hz, if_false]
      have hne : ws.val ≠ [] := by
        intro e; apply hz; apply UScalar.eq_of_val_eq; simp [alloc.vec.Vec.len, e]
      exact logged_frame a c plan s hp ws rep hx hne
  · rfl

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
