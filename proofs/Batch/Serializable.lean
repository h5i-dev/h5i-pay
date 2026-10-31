import Ledger.Theorems
/-!
# Batch serializability

A linked batch commits exactly what running its transfers one at a time,
as separate `Transfer` commands, would commit: each transfer succeeds on
the state the earlier ones left, and the batch's write set is their write
sets concatenated in order. A batch is therefore never more permissive than
the serial execution it stands for (it cannot, say, check every transfer
against the opening balances), and every theorem about single transfers
carries over to batches.

`Serial a s ts ws s'` is that serial execution. `batch_serial` is the
theorem about the extracted loop in `payment_ledger::batch`.
-/
open Aeneas Aeneas.Std Result payment_ledger payment_ledger.Spec payment_ledger.Commands
  payment_ledger.Theorems payment_ledger.ApplyProofs payment_ledger.Schema H5iAppLib

namespace payment_ledger.Batch

/-- Transfers `ts` run one at a time from snapshot `s`, as separate
commands, writing `ws` in total and ending in `s'`. -/
inductive Serial (a : Principal) : Snapshot → List TransferReq → List Write → Snapshot → Prop
  | nil (s : Snapshot) : Serial a s [] [] s
  | cons {s : Snapshot} {t : TransferReq} {ts : List TransferReq} {w : alloc.vec.Vec Write} {r : Reply}
      {s1 : Snapshot} {ws : List Write} {s2 : Snapshot} :
      transition a s (.Transfer t) = ok (.Ok (w, r)) →
      apply s w = ok s1 → Snapshot.toSt s1 = applyAll (Snapshot.toSt s) w.val →
      Serial a s1 ts ws s2 → Serial a s (t :: ts) (w.val ++ ws) s2

theorem Serial.snoc {a : Principal} {s cur cur' : Snapshot} {l : List TransferReq} {ws : List Write}
    {t : TransferReq} {w : alloc.vec.Vec Write} {r : Reply}
    (h : Serial a s l ws cur) (ht : transition a cur (.Transfer t) = ok (.Ok (w, r)))
    (hap : apply cur w = ok cur') (hst : Snapshot.toSt cur' = applyAll (Snapshot.toSt cur) w.val) :
    Serial a s (l ++ [t]) (ws ++ w.val) cur' := by
  induction h with
  | nil s => simpa using Serial.cons ht hap hst (Serial.nil cur')
  | cons h1 h2 h3 _ ih => simpa [List.append_assoc] using Serial.cons h1 h2 h3 (ih ht hap hst)

/-- The serial run's end state is what its writes do to the start. -/
theorem Serial.toSt {a : Principal} {s s' : Snapshot} {l : List TransferReq} {ws : List Write}
    (h : Serial a s l ws s') : Snapshot.toSt s' = applyAll (Snapshot.toSt s) ws := by
  induction h with
  | nil s => rfl
  | cons _ _ h3 _ ih => rw [ih, h3]; simp [applyAll, List.foldl_append]

/-- Every transfer writes nothing (a replay) or three rows. -/
theorem transfer_writes_le (a : Principal) (s : Snapshot) (t : TransferReq) ws r
    (h : transfer a s t = ok (.Ok (ws, r))) : ws.length ≤ 3 := by
  rcases post_of_ok (transfer_spec a s t) h ws r rfl with
    ⟨_, _, _, _, _, _, hws, _⟩ | ⟨_, _, _, _, _, _, _, _, _, _, _, _, _, _, _, _, _, hws⟩ <;>
    simp [alloc.vec.Vec.length, hws]

@[step] theorem append_spec (ws more : alloc.vec.Vec Write) (h : ws.length + more.length ≤ Usize.max) :
    append ws more ⦃ v => v.val = ws.val ++ more.val ⦄ := by
  unfold append append_loop
  apply WP.spec_mono (loop_fold more.val (fun v : alloc.vec.Vec Write => v.val) (fun acc x => acc ++ [x])
    (fun v k => v.length + (more.length - k) ≤ Usize.max)
    (fun x => append_loop.body more x.1 x.2) ?_ ws 0#usize (by simp) (by simpa using h))
  · intro r hr; rw [hr, foldl_snoc]; simp
  · intro v j hj hv
    unfold append_loop.body
    simp only [alloc.vec.Vec.length] at hj hv
    dsimp only
    split
    · rename_i hlt
      step*
      simp only [FoldStep]
      have : j.val < more.val.length := by scalar_tac
      refine ⟨this, ?_, by scalar_tac, ?_⟩
      · simp_all
      · simp only [alloc.vec.Vec.length] at *; scalar_tac
    · simp only [WP.spec_ok, FoldStep, and_true]
      scalar_tac

/-- The batch loop: what it commits is a serial run of all the transfers. -/
theorem batch_loop_spec (a : Principal) (s : Snapshot) (ts : alloc.vec.Vec TransferReq)
    (cur : Snapshot) (ws : alloc.vec.Vec Write) (i : Usize) (hi : i.val ≤ ts.length)
    (hser : Serial a s (ts.val.take i.val) ws.val cur)
    (hsz : Snapshot.size cur + 3 * (ts.length - i.val) < Usize.max)
    (hws : ws.length + 3 * (ts.length - i.val) ≤ Usize.max) :
    batch_loop a ts cur ws i ⦃ r => ∀ ws' rep, r = .Ok (ws', rep) → ∃ s', Serial a s ts.val ws'.val s' ⦄ := by
  unfold batch_loop
  apply loop.spec_decr_nat (measure := fun (x : Snapshot × alloc.vec.Vec Write × Usize) => ts.length - x.2.2.val)
    (inv := fun x => x.2.2.val ≤ ts.length ∧ Serial a s (ts.val.take x.2.2.val) x.2.1.val x.1 ∧
      Snapshot.size x.1 + 3 * (ts.length - x.2.2.val) < Usize.max ∧
      x.2.1.length + 3 * (ts.length - x.2.2.val) ≤ Usize.max)
  · rintro ⟨c, w, j⟩ ⟨hj, hs, hc, hw⟩
    simp only at hj hs hc hw
    dsimp only
    unfold batch_loop.body
    dsimp only
    split
    · rename_i hlt
      have hlt' : j.val < ts.val.length := by simp only [alloc.vec.Vec.length] at *; scalar_tac
      step as ⟨ tr, htr ⟩
      obtain ⟨r0, hr0⟩ := ok_of (transfer_spec a c tr)
      rw [hr0]
      simp only [bind_ok]
      rcases r0 with ⟨w1, rep⟩ | e
      · dsimp only
        have hT : transition a c (.Transfer tr) = ok (.Ok (w1, rep)) := by simp only [transition]; exact hr0
        have h3 := transfer_writes_le a c tr w1 rep hr0
        have hfit : Snapshot.size c + w1.length < Usize.max := by simp only [alloc.vec.Vec.length] at *; omega
        obtain ⟨c1, hap⟩ := ok_of (apply_spec c w1 hfit)
        have hst := post_of_ok (apply_spec c w1 hfit) hap
        show WP.spec ((do let cur1 ← apply c w1; let ws1 ← append w w1; let i2 ← j + 1#usize; ok (ControlFlow.cont (cur1, ws1, i2))) : Result _) _
        rw [hap]
        simp only [bind_ok]
        have hfit2 : w.length + w1.length ≤ Usize.max := by simp only [alloc.vec.Vec.length] at *; omega
        obtain ⟨w2, hap2⟩ := ok_of (append_spec w w1 hfit2)
        have hw2 := post_of_ok (append_spec w w1 hfit2) hap2
        rw [hap2]
        simp only [bind_ok]
        step*
        have hsz1 : Snapshot.size c1 ≤ Snapshot.size c + w1.length := by
          rw [← rows_toSt, ← rows_toSt, hst]; exact rows_applyAll _ _
        have htake : ts.val.take i2.val = ts.val.take j.val ++ [tr] := by
          rw [i2_post, List.take_add_one, List.getElem?_eq_getElem hlt', htr]; rfl
        refine ⟨by scalar_tac, ?_, ?_, ?_, by scalar_tac⟩
        · rw [htake, hw2]; exact Serial.snoc hs hT hap hst
        · simp only [alloc.vec.Vec.length] at *; omega
        · simp only [alloc.vec.Vec.length, hw2, List.length_append] at *; omega
      · show WP.spec (ok (ControlFlow.done (core.result.Result.Err e))) _
        simp
    · rename_i hge
      step*
      intro ws' rep h
      obtain ⟨rfl, -⟩ := ok_inj h
      have : j.val = ts.val.length := by simp only [alloc.vec.Vec.length] at *; scalar_tac
      rw [this, List.take_length] at hs
      exact ⟨c, hs⟩
  · exact ⟨hi, hser, hsz, hws⟩

/-- **Batch serializability.** A committed batch is a serial run of its
transfers, each as a separate `Transfer` command, in order. The size bound
says the tenant's tables are not within 3 · 8192 rows of `usize::MAX`. -/
theorem batch_serial (a : Principal) (s : Snapshot) (ts : alloc.vec.Vec TransferReq) ws r
    (hsz : Snapshot.size s + 3 * 8192 < Usize.max)
    (h : transition a s (.Batch ts) = ok (.Ok (ws, r))) :
    ∃ s', Serial a s ts.val ws.val s' := by
  simp only [transition, batch] at h
  split at h
  · simp at h
  · split at h
    · simp at h
    · rename_i h0 hmax
      rw [Snapshot.clone_eq] at h
      simp only [bind_ok] at h
      have hlen : ts.length ≤ 8192 := by
        simp only [MAX_BATCH, alloc.vec.Vec.len, alloc.vec.Vec.length] at *; scalar_tac
      exact post_of_ok (batch_loop_spec a s ts s (alloc.vec.Vec.new Write) 0#usize (by simp)
        (by simpa using Serial.nil s) (by simp only [alloc.vec.Vec.length] at *; simp; omega)
        (by simp only [alloc.vec.Vec.length] at *; simp; scalar_tac)) h ws r rfl

end payment_ledger.Batch
