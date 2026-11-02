import Ledger.Invariants
import H5iAppLib.ReadOnly
/-!
# The ledger's read-only commands (G4)

`payment_ledger::read_only` names the commands `LedgerApp` lets the engine
run in a `SERIALIZABLE READ ONLY DEFERRABLE` transaction. h5i-app's
obligation `H5iAppLib.ReadOnly.Sound`: whenever `read_only c` and
`transition` accepts `c`, the write set is empty, for every actor and state.
It follows from `reads_nothing`.
-/
open Aeneas Aeneas.Std Result payment_ledger

namespace payment_ledger.ReadOnly

def writesOf (ws : alloc.vec.Vec Write) : List Write := ws.val

theorem sound : H5iAppLib.ReadOnly.Sound read_only transition writesOf := by
  intro a s c ws r hro h
  apply Theorems.reads_nothing a s c ws r h
  cases c <;> simp_all [read_only, Theorems.IsRead]

end payment_ledger.ReadOnly
