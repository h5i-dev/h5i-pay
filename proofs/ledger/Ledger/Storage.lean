import Ledger.Invariants
/-!
# What PostgreSQL holds

The server stores a write set by running the plan of `sql_writes`, and loads
rows through `decode`. `H5iAppLib.Store` proves, for any schema, that the
database then holds the encoding of the state `applyAll` computes, and
`H5iAppLib.Pg` that this holds for one tenant's tables whatever other
tenants' statements run in between. This file gives the ledger's encoding
and table writes, and instantiates the result: the ledger invariant (so
conservation, the balanced journal and the account limits) holds for the
rows in PostgreSQL and for every snapshot a later load decodes.
-/
open Aeneas Aeneas.Std Result payment_ledger payment_ledger.Spec payment_ledger.Schema H5iAppLib
  H5iAppLib.Sql H5iAppLib.Store

namespace payment_ledger.Storage

/-- A state's rows, table by table. -/
def enc (s : St) : Tables Val
  | 0 => s.accounts.map Account.row
  | 1 => s.transfers.map Transfer.row
  | 2 => [[int s.next]]
  | 3 => s.owners.map OwnerCounter.row
  | _ => []

/-- The table writes of one write. -/
def sqlA : Write → List (AWrite Val)
  | .PutAccount a => [Account.putA a]
  | .PutTransfer t => [Transfer.putA t]
  | .SetLedger l => [Ledger.putA l]
  | .PutOwner c => [OwnerCounter.putA c]

def app : App St Write Val where
  kl := kl
  enc := enc
  sql := sqlA
  step := applyWrite
  init := init
  IsRow := IsRow
  enc_step := by schema_step [enc, sqlA, applyWrite]
  sql_ok := by schema_ok [sqlA]
  init_ok := by schema_init [enc, init]
  init_rows := by schema_rows [enc, init]

theorem fits : Schema.Fits app Snapshot.toSt where
  kl := rfl
  rows := rfl
  enc s := by funext t; cases_table t <;> rfl
  init := by funext t; cases_table t <;> rfl
  nil s t h := by cases_table t <;> first | omega | rfl

theorem sql_fits : SqlFits app := by
  intro w out h
  unfold sql_write; cases w <;> simp only [app, sqlA, List.length_singleton] at h ⊢ <;> step* <;> simp_all

/-- The store holds what `apply` computes: the rows of the state `applyAll`
gives for its commits, and a load decodes to that state, up to row order. -/
theorem stored {db : Db Val} {s : St} (h : Served app Snapshot.toSt db s) :
    app.Holds db (enc s) ∧
      ∀ r, Lists kl db (Rows.tabs r) → decode r ⦃ o => ∃ snap, o = some snap ∧ app.Equiv (Snapshot.toSt snap) s ⦄ :=
  Schema.stored fits sql_fits h

/-! ## The invariant on PostgreSQL -/

/-- Write sets the kernel returns for a command it accepts on a snapshot with
room for a batch. -/
def Accepted (snap : Snapshot) (ws : alloc.vec.Vec Write) : Prop :=
  HasRoom snap ∧ ∃ a c r, transition a snap c = .ok (.Ok (ws, r))

/-- The invariant, and the account counter fits its `BIGINT`. -/
def DbInv (s : St) : Prop := Inv s ∧ s.next < 2 ^ 64

theorem next_bound (s : St) (ws : List Write) (h : s.next < 2 ^ 64) : (applyAll s ws).next < 2 ^ 64 := by
  induction ws generalizing s with
  | nil => exact h
  | cons w ws ih =>
    apply ih
    cases w <;> simp only [applyWrite] <;> first | exact h | scalar_tac

theorem sum_perm {α} {l l' : List α} (f : α → Nat) (h : l.Perm l') : (l.map f).sum = (l'.map f).sum :=
  (h.map f).sum_eq

theorem dbInv_equiv (snap : Snapshot) (s : St) (h : app.Equiv (Snapshot.toSt snap) s) (hi : DbInv s) :
    DbInv (Snapshot.toSt snap) := by
  have h0 := h 0
  have h1 := h 1
  have h2 := h 2
  simp only [app, enc, Snapshot.toSt] at h0 h1 h2
  rw [List.map_perm_map_iff Account.row_inj] at h0
  rw [List.map_perm_map_iff Transfer.row_inj] at h1
  rw [List.perm_singleton, List.cons.injEq] at h2
  obtain ⟨⟨hf, hb, hl⟩, hn⟩ := hi
  have e1 := List.cons.inj h2.1
  have hnext : snap.ledger.next_account.val = s.next := int_inj (by scalar_tac) hn e1.1
  refine ⟨⟨fun a ha => ?_, fun c => ?_, fun a ha => ?_⟩, ?_⟩
  · simp only [Snapshot.toSt, hnext]; exact hf a (h0.subset ha)
  · obtain ⟨b1, b2⟩ := hb c
    simp only [debits, credits, journal, Snapshot.toSt] at *
    rw [sum_perm _ h0, sum_perm _ h0, sum_perm _ h1]
    exact ⟨b1, b2⟩
  · exact hl a (h0.subset ha)
  · simp only [Snapshot.toSt, hnext]; exact hn

/-- **The invariants hold on the database.** For the server's schema and any
tenant: accepted commands, interleaved with any other tenants' commits,
leave a state satisfying `Inv`, and so does every snapshot a later load
decodes from the rows. -/
theorem db_inv {ts : List Pg.Tab} {tv : Val} (hv : Pg.Valid ts) (htv : valKind tv = some .int)
    (hkl : Pg.klOf ts = kl) (hlen : ts.length = 4) {db : Pg.PgDb Val} {s : St}
    (h : PgServed app Snapshot.toSt ts tv Accepted db s) :
    Inv s ∧ ∀ r, Loads ts tv db r → decode r ⦃ o => ∃ snap, o = some snap ∧ Inv (Snapshot.toSt snap) ⦄ := by
  obtain ⟨hi, hload⟩ := pg_loaded_inv sql_fits fits hv htv hkl hlen DbInv
    ⟨Theorems.init_inv, by simp [app, init]⟩
    dbInv_equiv (fun snap ws ⟨hroom, a, c, r, ht⟩ hi => ⟨Theorems.inv_preserved a snap c ws r hi.1 hroom ht,
      next_bound _ _ (by simp only [Snapshot.toSt]; scalar_tac)⟩) h
  refine ⟨hi.1, fun r hr => WP.spec_mono (hload r hr) ?_⟩
  rintro o ⟨snap, rfl, hs⟩
  exact ⟨snap, rfl, hs.1⟩

end payment_ledger.Storage
