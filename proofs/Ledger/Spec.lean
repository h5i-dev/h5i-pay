import PaymentLedger
import H5iAppLib
/-!
# What the ledger should do

The file to review. The state as plain lists and numbers, what each write
does to it, the per-currency sums, and the invariant every reachable state
satisfies:

* `balanced`: in every currency, the debits posted to all accounts, the
  credits posted to all accounts and the amounts in the journal are the same
  number (double entry, and the balances are explained by the journal);
* `limits`: no `no_overdraft` account has more debits than credits, and no
  `no_overcredit` account more credits than debits;
* `fresh`: account ids are below the counter (so opening never overwrites).
-/
open Aeneas Aeneas.Std payment_ledger

namespace payment_ledger.Spec

/-- The state: the account counter, the accounts and the journal. -/
structure St where
  next : Nat
  accounts : List Account
  transfers : List Transfer

def Snapshot.toSt (s : Snapshot) : St :=
  ⟨s.ledger.next_account.val, s.accounts.val, s.transfers.val⟩

def findAcc (s : St) (id : Nat) : Option Account :=
  s.accounts.find? (fun a => a.id.val = id)

def findTr (s : St) (id : Nat) : Option Transfer :=
  s.transfers.find? (fun t => t.id.val = id)

/-- An account's debits, counted in currency `c` only. -/
def debitIn (c : Nat) (a : Account) : Nat := if a.currency.val = c then a.debits.val else 0
def creditIn (c : Nat) (a : Account) : Nat := if a.currency.val = c then a.credits.val else 0
def amountIn (c : Nat) (t : Transfer) : Nat := if t.currency.val = c then t.amount.val else 0

/-- Debits posted to all accounts in currency `c`. -/
def debits (c : Nat) (s : St) : Nat := (s.accounts.map (debitIn c)).sum
/-- Credits posted to all accounts in currency `c`. -/
def credits (c : Nat) (s : St) : Nat := (s.accounts.map (creditIn c)).sum
/-- The journal's total in currency `c`. -/
def journal (c : Nat) (s : St) : Nat := (s.transfers.map (amountIn c)).sum

/-- What a write does. `H5iAppLib.upsert` replaces the row with the same key,
or appends it. -/
def applyWrite (s : St) : Write → St
  | .PutAccount a => { s with accounts := H5iAppLib.upsert (·.id) a s.accounts }
  | .PutTransfer t => { s with transfers := H5iAppLib.upsert (·.id) t s.transfers }
  | .SetLedger l => { s with next := l.next_account.val }

def applyAll (s : St) (ws : List Write) : St := ws.foldl applyWrite s

/-- An account within its constraints. -/
def Limited (a : Account) : Prop :=
  (a.no_overdraft = true → a.debits.val ≤ a.credits.val) ∧
  (a.no_overcredit = true → a.credits.val ≤ a.debits.val)

/-- Facts that hold in every state the ledger can reach. -/
structure Inv (s : St) : Prop where
  fresh : ∀ a ∈ s.accounts, a.id.val < s.next
  balanced : ∀ c, debits c s = journal c s ∧ credits c s = journal c s
  limits : ∀ a ∈ s.accounts, Limited a

def init : St := ⟨0, [], []⟩

/-- Rows in a loaded snapshot leave room for one maximal batch (3 · 8192
writes) below `usize::MAX`. An assumption about memory, not about the
ledger: on a 64-bit machine no snapshot comes near 2^64 rows. -/
def HasRoom (s : Snapshot) : Prop := s.accounts.length + s.transfers.length + 3 * 8192 < Usize.max

/-- The states reachable from an empty ledger by successful commands of any
callers, in any order. -/
inductive Reachable : St → Prop
  | init : Reachable init
  | step {a s c ws r} : Reachable (Snapshot.toSt s) → HasRoom s → transition a s c = .ok (.Ok (ws, r)) →
      Reachable (applyAll (Snapshot.toSt s) ws.val)

end payment_ledger.Spec
