import PaymentDomain
import H5iAppLib
/-!
# What the payment kernel should do

The file to review. The state as lists, what each write does, the status
transitions the state machine allows, and the amount invariant:

```text
CREATED(0) ──► PROCESSING(1) ──► AUTHORIZED(2) ──► CAPTURED(3)
    │              │  │  └────────────────────────► CAPTURED(3)
    │              │  └──► FAILED(5)
    └──────────────┴───────────────┴──► CANCELED(4)
```

For every payment: refunded + in-flight refunds ≤ captured ≤ authorized ≤
amount, and nothing is refunded or reserved unless the payment is captured.
-/
open Aeneas Aeneas.Std payment_domain

/-! ## Who acts

The kernel's principal says who acts: `Actor.User u` or `Actor.Operator o`.
`a.operator` and `a.user` read it as the kernel's `is_operator` and
`user_of` do (an operator's `user` is 0, only consulted after
`a.operator`); `a.opId` is the operator id the kernel records on the rows
it writes (`operator_of`: 0 for a user). -/

def payment_domain.Principal.operator (a : Principal) : Bool :=
  match a.actor with
  | .User _ => false
  | .Operator _ => true

def payment_domain.Principal.user (a : Principal) : U64 :=
  match a.actor with
  | .User u => u
  | .Operator _ => 0#u64

def payment_domain.Principal.opId (a : Principal) : U64 :=
  match a.actor with
  | .User _ => 0#u64
  | .Operator o => o

@[simp] theorem is_operator_eq (a : Principal) : is_operator a = .ok a.operator := by
  unfold is_operator Principal.operator; cases a.actor <;> rfl

@[simp] theorem user_of_eq (a : Principal) : user_of a = .ok a.user := by
  unfold user_of Principal.user; cases a.actor <;> rfl

@[simp] theorem operator_of_eq (a : Principal) : operator_of a = .ok a.opId := by
  unfold operator_of Principal.opId; cases a.actor <;> rfl

theorem operator_iff (a : Principal) : a.operator = true ↔ ∃ o, a.actor = .Operator o := by
  rcases a with ⟨org, act, t⟩
  cases act <;> simp [Principal.operator]

namespace payment_domain.Spec

structure St where
  next : Nat
  payments : List Payment
  refunds : List Refund
  events : List Event
  refs : List PspRef
  /-- Per-merchant payment counters; they only number payments for paging
  (`List`), and no theorem here is about them. -/
  merchants : List MerchantCounter
  /-- The payments' history (G1): appended to by every change. -/
  history : List HistoryEntry

def Snapshot.toSt (s : Snapshot) : St :=
  ⟨s.counter.next_id.val, s.payments.val, s.refunds.val, s.events.val, s.refs.val, s.merchants.val, s.history.val⟩

def findPay (s : St) (id : Nat) : Option Payment := s.payments.find? (fun p => p.id.val = id)
def findRefund (s : St) (id : Nat) : Option Refund := s.refunds.find? (fun r => r.id.val = id)
def findEvent (s : St) (id : Nat) : Option Event := s.events.find? (fun e => e.id.val = id)

/-- The status transitions the state machine allows (codes as in the kernel:
0 created, 1 processing, 2 authorized, 3 captured, 4 canceled, 5 failed). -/
def Allowed (a b : Nat) : Prop :=
  (a = 0 ∧ (b = 1 ∨ b = 4)) ∨
  (a = 1 ∧ (b = 2 ∨ b = 3 ∨ b = 4 ∨ b = 5)) ∨
  (a = 2 ∧ (b = 3 ∨ b = 4))

/-- A payment's amounts are consistent. -/
def PayInv (p : Payment) : Prop :=
  p.refunded.val + p.refund_pending.val ≤ p.captured.val ∧ p.captured.val ≤ p.authorized.val ∧
  p.authorized.val ≤ p.amount.val ∧ (p.status.val ≠ 3 → p.refunded.val = 0 ∧ p.refund_pending.val = 0)

/-- What one command may do to an existing payment: keep who and how much,
change the status only along `Allowed`, and keep the amounts consistent. -/
def Step (p p' : Payment) : Prop :=
  p'.id = p.id ∧ p'.merchant = p.merchant ∧ p'.amount = p.amount ∧ p'.currency = p.currency ∧
  (p'.status = p.status ∨ Allowed p.status.val p'.status.val) ∧ (PayInv p → PayInv p')

/-- Refunds `x` and `y` are the same refund (the audit field aside). -/
abbrev SameRefund (x y : Refund) : Prop :=
  x.id = y.id ∧ x.payment = y.payment ∧ x.amount = y.amount ∧ x.status = y.status

/-- Every refund of `xs` is in `ys` (found by id), the same. -/
def RefundsCover (xs ys : List Refund) : Prop :=
  ∀ x ∈ xs, ∃ y, ys.find? (fun r => r.id.val = x.id.val) = some y ∧ SameRefund x y

/-- The refund rows of payment `id`. -/
def refundsOf (s : St) (id : Nat) : List Refund := s.refunds.filter (fun r => r.payment.val = id)

def applyWrite (s : St) : Write → St
  | .PutPayment p => { s with payments := H5iAppLib.upsert (·.id) p s.payments }
  | .PutRefund r => { s with refunds := H5iAppLib.upsert (·.id) r s.refunds }
  | .PutEvent e => { s with events := H5iAppLib.upsert (·.id) e s.events }
  | .PutRef r => { s with refs := H5iAppLib.upsert (·.payment) r s.refs }
  | .SetCounter c => { s with next := c.next_id.val }
  | .PutMerchant c => { s with merchants := H5iAppLib.upsert (·.merchant) c s.merchants }
  | .PutHistory h => { s with history := H5iAppLib.upsert (fun y => (y.payment, y.n)) h s.history }

def applyAll (s : St) (ws : List Write) : St := ws.foldl applyWrite s

/-- The operator id a write records on its row (`none` for the counter,
which records none). -/
def _root_.payment_domain.Write.recorded : Write → Option U64
  | .PutPayment p => some p.operator
  | .PutRefund r => some r.operator
  | .PutEvent e => some e.operator
  | .PutRef r => some r.operator
  | .SetCounter _ => none
  | .PutMerchant _ => none
  | .PutHistory _ => none

/-- Facts that hold in every reachable state. -/
structure Inv (s : St) : Prop where
  fresh_pay : ∀ p ∈ s.payments, p.id.val < s.next
  fresh_refund : ∀ r ∈ s.refunds, r.id.val < s.next
  amounts : ∀ p ∈ s.payments, PayInv p

def init : St := ⟨0, [], [], [], [], [], []⟩

/-- The states reachable from no payments by successful commands of any
callers (merchants, the orchestrator, webhooks), in any order. -/
inductive Reachable : St → Prop
  | init : Reachable init
  | step {a s c ws r} : Reachable (Snapshot.toSt s) → transition a s c = .ok (.Ok (ws, r)) →
      Reachable (applyAll (Snapshot.toSt s) ws.val)

end payment_domain.Spec
