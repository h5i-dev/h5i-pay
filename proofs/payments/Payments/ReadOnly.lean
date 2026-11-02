import Payments.Logged
import H5iAppLib.ReadOnly
/-!
# The payment kernel's read-only commands (G6)

`payment_domain::read_only` names the commands `PaymentsApp` lets the
engine run in a `SERIALIZABLE READ ONLY DEFERRABLE` transaction. h5i-app's
obligation `H5iAppLib.ReadOnly.Sound`: whenever `read_only c` and
`transition` accepts `c`, the write set is empty, for every actor and state.
Each of those commands decides no writes (`*_writes`), and `transition`
adds a history entry only to a command that wrote (`transition_writes`).
-/
open Aeneas Aeneas.Std Result payment_domain payment_domain.Commands payment_domain.Logged

namespace payment_domain.ReadOnly

def writesOf (ws : alloc.vec.Vec Write) : List Write := ws.val

theorem decide_reads_nothing (a : Principal) (s : Snapshot) (c : Command) ws r
    (hro : read_only c = ok true) (h : decide_command a s c = ok (.Ok (ws, r))) : ws.val = [] := by
  cases c <;> simp [read_only] at hro <;> simp only [decide_command] at h
  all_goals first
    | exact get_writes a s _ ws r h
    | exact list_writes a s _ _ _ ws r h
    | exact get_refund_writes a s _ ws r h
    | exact refunds_of_writes a s _ ws r h
    | exact get_ref_writes a s _ ws r h
    | exact view_writes a s _ ws r h
    | exact history_page_writes a s _ _ _ ws r h
    | exact range_page_writes a s _ _ ws r h
    | exact refund_range_page_writes a s _ _ ws r h

theorem sound : H5iAppLib.ReadOnly.Sound read_only transition writesOf := by
  intro a s c ws r hro h
  obtain ⟨ws0, hd, -, hc⟩ := transition_writes a s c ws r h
  have h0 := decide_reads_nothing a s c ws0 r hro hd
  rcases hc with ⟨-, rfl⟩ | ⟨hne, -⟩
  · exact h0
  · exact absurd h0 hne

end payment_domain.ReadOnly

namespace payment_domain.Theorems

/-- Only an operator reads a `Range` page (the tenant's payments and refunds
by id, G6). -/
theorem range_page_operator (a : Principal) (s : Snapshot) (f : U64) (l : U32) ws r
    (h : range_page a s f l = ok (.Ok (ws, r))) : a.operator = true := by
  unfold range_page at h
  simp only [is_operator_eq, bind_ok] at h
  split at h
  · simp at h
  split at h
  · simp at h
  split at h
  · assumption
  · simp at h

theorem refund_range_page_operator (a : Principal) (s : Snapshot) (f : U64) (l : U32) ws r
    (h : refund_range_page a s f l = ok (.Ok (ws, r))) : a.operator = true := by
  unfold refund_range_page at h
  simp only [is_operator_eq, bind_ok] at h
  split at h
  · simp at h
  split at h
  · simp at h
  split at h
  · assumption
  · simp at h

/-- Only an operator reads a `Range` or `RefundRange` page (the tenant's
payments or refunds by id, G6). -/
theorem range_operator (a : Principal) (s : Snapshot) (f : U64) (l : U32) ws r
    (h : transition a s (.Range f l) = ok (.Ok (ws, r)) ∨ transition a s (.RefundRange f l) = ok (.Ok (ws, r))) :
    a.operator = true := by
  rcases h with h | h
  · obtain ⟨ws0, hd, -⟩ := transition_writes a s _ ws r h
    simp only [decide_command] at hd
    exact range_page_operator a s f l ws0 r hd
  · obtain ⟨ws0, hd, -⟩ := transition_writes a s _ ws r h
    simp only [decide_command] at hd
    exact refund_range_page_operator a s f l ws0 r hd

end payment_domain.Theorems
