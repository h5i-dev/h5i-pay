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

namespace payment_domain.Spec

structure St where
  next : Nat
  payments : List Payment
  refunds : List Refund
  events : List Event
  refs : List PspRef

def Snapshot.toSt (s : Snapshot) : St :=
  ⟨s.counter.next_id.val, s.payments.val, s.refunds.val, s.events.val, s.refs.val⟩

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

def applyWrite (s : St) : Write → St
  | .PutPayment p => { s with payments := H5iAppLib.upsert (·.id) p s.payments }
  | .PutRefund r => { s with refunds := H5iAppLib.upsert (·.id) r s.refunds }
  | .PutEvent e => { s with events := H5iAppLib.upsert (·.id) e s.events }
  | .PutRef r => { s with refs := H5iAppLib.upsert (·.payment) r s.refs }
  | .SetCounter c => { s with next := c.next_id.val }

def applyAll (s : St) (ws : List Write) : St := ws.foldl applyWrite s

/-- Facts that hold in every reachable state. -/
structure Inv (s : St) : Prop where
  fresh_pay : ∀ p ∈ s.payments, p.id.val < s.next
  fresh_refund : ∀ r ∈ s.refunds, r.id.val < s.next
  amounts : ∀ p ∈ s.payments, PayInv p

def init : St := ⟨0, [], [], [], []⟩

/-- The states reachable from no payments by successful commands of any
callers (merchants, the orchestrator, webhooks), in any order. -/
inductive Reachable : St → Prop
  | init : Reachable init
  | step {a s c ws r} : Reachable (Snapshot.toSt s) → transition a s c = .ok (.Ok (ws, r)) →
      Reachable (applyAll (Snapshot.toSt s) ws.val)

end payment_domain.Spec
