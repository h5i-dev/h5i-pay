import Ledger.Invariants
/-!
# The engine contract: tenant isolation and idempotency keys

`h5i-app-pg`'s engine is trusted Rust, not extracted. This file states, as
a small Lean model, the contract the server relies on and that the
integration tests check against PostgreSQL:

* each request runs the kernel on the snapshot of the caller's tenant only
  (`TenantId(actor.org)`), and writes only that tenant's rows;
* a request with an idempotency key runs at most once per
  (tenant, scope, key): a repeat with the same command fingerprint returns
  the stored reply, a repeat with another command is refused, and refusals
  are not stored.

From the model: commands of one tenant never change another tenant's state
or reply (`isolation`, `noninterference`), and replaying a keyed request any
number of times has the effect of running it once (`keyed_replay`). The
ledger instance (`ledger_isolation`) combines this with the kernel: no
principal of tenant `t` can change any account of a tenant `t' ≠ t`.

What is proven here is the model. That `h5i_app_pg::Engine` implements it
is an assumption, listed in `docs/proof-assumptions.md`.
-/
namespace Models

/-- A kernel: a pure step on one tenant's state. `none` is a refusal. -/
structure Kernel (P St C R : Type) where
  tenant : P → Nat
  step : P → St → C → Option (St × R)

variable {P St C R : Type} (K : Kernel P St C R)

/-- The whole database: one state per tenant. -/
abbrev Db (St : Type) := Nat → St

/-- One request through the engine: load the caller's tenant, run the
kernel, write that tenant back (or nothing, on a refusal). -/
def exec (db : Db St) (a : P) (c : C) : Db St × Option R :=
  match K.step a (db (K.tenant a)) c with
  | some (s', r) => (Function.update db (K.tenant a) s', some r)
  | none => (db, none)

/-- **Tenant isolation.** A request changes no other tenant's state. -/
theorem isolation (db : Db St) (a : P) (c : C) (t : Nat) (ht : t ≠ K.tenant a) :
    (exec K db a c).1 t = db t := by
  unfold exec
  split <;> simp [Function.update, ht]

/-- **Noninterference.** What a request does and replies depends only on its
own tenant's state: two databases that agree on that tenant give the same
reply and the same new state for it. -/
theorem noninterference (db db' : Db St) (a : P) (c : C) (h : db (K.tenant a) = db' (K.tenant a)) :
    (exec K db a c).2 = (exec K db' a c).2 ∧ (exec K db a c).1 (K.tenant a) = (exec K db' a c).1 (K.tenant a) := by
  unfold exec
  rw [h]
  split <;> simp_all

/-- Requests of other tenants, run in between, leave a tenant's state alone. -/
theorem others_commute (db : Db St) (reqs : List (P × C)) (t : Nat) (h : ∀ x ∈ reqs, K.tenant x.1 ≠ t) :
    (reqs.foldl (fun d x => (exec K d x.1 x.2).1) db) t = db t := by
  induction reqs generalizing db with
  | nil => rfl
  | cons x xs ih =>
    simp only [List.foldl_cons]
    rw [ih _ (fun y hy => h y (List.mem_cons_of_mem _ hy))]
    exact isolation K db x.1 x.2 t (fun e => h x (List.mem_cons_self) e.symm)

/-! ## Idempotency keys -/

/-- Stored replies: per (tenant, scope, key), the command's fingerprint and reply. -/
abbrev Replies (C R : Type) := Nat × String × String → Option (C × R)

inductive Keyed (R : Type) where
  | ran (r : Option R)
  | replayed (r : R)
  | conflict

/-- A request with key `k`: replay a stored reply for the same command,
refuse another command, or run it and store a successful reply. -/
def execKeyed [DecidableEq C] (scope : P → String) (db : Db St) (st : Replies C R) (a : P) (k : String) (c : C) :
    Db St × Replies C R × Keyed R :=
  let key := (K.tenant a, scope a, k)
  match st key with
  | some (c', r) => if c' = c then (db, st, .replayed r) else (db, st, .conflict)
  | none =>
    match exec K db a c with
    | (db', some r) => (db', Function.update st key (some (c, r)), .ran (some r))
    | (db', none) => (db', st, .ran none)

/-- **Idempotency.** Sending the same keyed request again, after it
succeeded, changes nothing and returns the first reply. -/
theorem keyed_replay [DecidableEq C] (scope : P → String) (db : Db St) (st : Replies C R) (a : P) (k : String) (c : C)
    (r : R) (h : (execKeyed K scope db st a k c).2.2 = .ran (some r)) :
    let x := execKeyed K scope db st a k c
    execKeyed K scope x.1 x.2.1 a k c = (x.1, x.2.1, .replayed r) := by
  simp only [execKeyed] at h ⊢
  split at h
  · split at h <;> simp at h
  · rename_i hnone
    split at h
    · rename_i db' r' he
      simp at h; subst h
      simp
    · simp at h

/-- A key reused for a different command is refused and changes nothing. -/
theorem keyed_conflict [DecidableEq C] (scope : P → String) (db : Db St) (st : Replies C R) (a : P) (k : String)
    (c c' : C) (r : R) (hs : st (K.tenant a, scope a, k) = some (c', r)) (hne : c' ≠ c) :
    execKeyed K scope db st a k c = (db, st, .conflict) := by
  simp [execKeyed, hs, hne]

/-! ## The ledger instance -/

open Aeneas Aeneas.Std payment_ledger payment_ledger.Spec

/-- The ledger kernel as the engine runs it: the caller's tenant is
`actor.org`; a refusal or a failed run commits nothing. -/
noncomputable def ledgerKernel : Kernel Principal Snapshot Command Reply where
  tenant a := a.org.val
  step a s c :=
    open Classical in
    if h : ∃ x : alloc.vec.Vec Write × Reply × Snapshot,
        transition a s c = .ok (.Ok (x.1, x.2.1)) ∧ apply s x.1 = .ok x.2.2 then
      some (h.choose.2.2, h.choose.2.1)
    else none

/-- **Tenant isolation for the ledger.** A command of any principal of
tenant `t` leaves every account, transfer and counter of every other tenant
exactly as it was. -/
theorem ledger_isolation (db : Db Snapshot) (a : Principal) (c : Command) (t : Nat) (ht : t ≠ a.org.val) :
    (exec ledgerKernel db a c).1 t = db t :=
  isolation ledgerKernel db a c t ht

end Models
