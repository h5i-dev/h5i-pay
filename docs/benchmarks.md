# Benchmarks

Two parts: **end to end** (B4: the API, the scheduled recovery and the
batch worker through PostgreSQL, files and the PSP adapter, before and
after the B1–B3 data-path changes), and the **batch engines** alone (M3,
re-run after the GPU kernels became Rust in A). The end-to-end numbers say
where the time goes; read them first.

## End to end (B4)

Harness: `benchmarks/src/bin/e2e.rs` (`e2e seed` / `e2e measure`), raw
results in [`benchmarks/results/e2e-g5.xlarge.json`](../benchmarks/results/e2e-g5.xlarge.json).
One tenant with 1k, 10k or 100k captured payments (one in ten partly
refunded), seeded through the real orchestrator; each measurement runs on a
fresh copy of that database. The API runs in-process with the Mock PSP
(no network); PostgreSQL 17 in Docker on the same host. "Before" is the
same harness built at `e88c83f` (whole-tenant load per command, the old
recovery and orchestrator); "after" is `685abb0` (B1–B3). The seeded
databases have the B1 indexes in both cases (they do not help whole-tenant
loads). Caveat: another project's Lean builds kept 1–2 of the 4 vCPUs busy
during these runs (load average 3–6), so absolute times are pessimistic;
the before/after ratios are measured under the same conditions.

| | 1k before | 1k after | 10k before | 10k after | 100k after |
|---|---:|---:|---:|---:|---:|
| `POST /v1/payments` (auto capture), p50 ms | 52.9 | 17.6 | 461.2 | 15.9 | 19.4 |
| … p95 ms | 55.8 | 18.2 | 528.0 | 17.8 | 23.5 |
| … kernel transactions per payment | 11 | 7 | 11 | 7 | 7 |
| `GET /v1/payments/{id}`, p50 ms (transactions) | 13.24 (3) | 0.83 (1) | 113.5 (3) | 0.83 (1) | 0.96 (1) |
| `POST …/refunds`, p50 ms (transactions) | 60.3 (13) | 19.3 (7) | 522.5 (13) | 19.5 (7) | 22.5 (7) |
| 8 concurrent clients, payments/s | 18.3 | 164.0 | 2.2 | 158.5 | 123.2 |
| … serialization retries (whole run) | 0 | 132 | 0 | 108 | 88 |
| recovery pass, 5 payments to fix: ms | 31,805 | 65 | 1,771,638 (29.5 min) | 619 | 1,002 |
| … kernel transactions / PSP calls | 6,319 / 5 | 45 / 5 | 52,219 / 5 | 45 / 5 | 45 / 5 |
| recovery pass, nothing to fix: ms (transactions) | 33,034 (6,311) | 10 (0) | – | 78 (0) | 852 (0) |
| `POST /v1/reconcile` (both reconciliations), p50 ms | 7.8 | 10.3 | 68.5 | 65.4 | 583.1 |

Before, every command loaded the whole tenant, so a payment cost
O(tenant) and the time grew linearly with the tenant (53 → 461 ms from 1k
to 10k payments); writes took the tenant lock, so concurrency did not help
(2.2 payments/s at 10k). After, a payment costs the same at 1k, 10k and
100k (16–19 ms, seven SERIALIZABLE transactions, each reading its keys);
concurrent writes queue on per-key locks and retry on conflicts instead
(88–132 retries per 200 payments). Before at 100k was not measured: at the
measured linear growth a single payment would take about 4.6 s.

The recovery pass before re-synced every captured payment's ledger postings
on every pass (6,311 transactions with nothing to do at 1k; at 10k one pass
took 29.5 minutes and 52,219 transactions, measured in a separate run with
`E2E_RECOVERY_PASSES=1`, longer than its own 5-minute schedule). After, one
read-only query selects the unfinished work. It still costs O(captured
payments) inside PostgreSQL (the anti-join on the ledger's transfer ids):
10 ms at 1k, 78 ms at 10k, 852 ms at 100k payments with nothing to fix.
Since F2 a per-payment "ledger complete" marker makes it O(unfinished):
7.8 ms at 100k with nothing to fix (see "After F" below).

(Until G6; see "After G".) `POST /v1/reconcile` is unchanged by B1–B3 and is O(tenant) by nature: it
loads both kernels' full snapshots and the PSP report, classifies on one
CPU thread and renders every exception (110k at 100k payments, since the
Mock PSP of the measurement does not know the seeded payments). 583 ms at
100k payments is the reason large tenants go to the nightly batch job.

### After F (`b87ef95`)

The same harness and seeded databases, re-run after F1–F5 with
`E2E_CLIENTS=8,32`; three runs per size, ranges shown (raw results labeled
`after-F-*` and `ab-F5-*` in the JSON file). The host was busier than for
B4: another project's Lean build kept one to two of the 4 vCPUs busy (load
average 2.6–6.7). To separate code from load, the F4 commit's harness
(`a153d42`, labeled `ab-F4-*`) was run interleaved with these runs.

| | 1k | 10k | 100k |
|---|---:|---:|---:|
| `POST /v1/payments`, p50 ms | 20.7–21.6 | 16.9–21.8 | 18.6–19.6 |
| … p95 ms | 21.9–23.6 | 18.0–26.5 | 19.7–21.7 |
| … kernel transactions per payment | 8 | 8 | 8 |
| `GET /v1/payments/{id}`, p50 ms (transactions) | 0.77–0.84 (1) | 0.73–0.84 (1) | 0.84–0.85 (1) |
| `POST …/refunds`, p50 ms (transactions) | 20.6–22.5 (8) | 18.9–22.1 (8) | 20.0–22.5 (8) |
| 8 clients (200 payments), payments/s | 151–163 | 147–153 | 141–151 |
| … serialization retries | 21–29 | 20–25 | 18–28 |
| 32 clients (800 payments), payments/s | 150–155 | 147–155 | 143–148 |
| … serialization retries | 94–123 | 87–115 | 74–98 |
| recovery pass, 5 payments to fix: ms (transactions / PSP calls) | 64–71 (39–42 / 5) | 60–68 (41–44 / 5) | 66–67 (43–45 / 5) |
| recovery pass, nothing to fix: ms (transactions) | 6.5–7.1 (0) | 6.7–8.4 (0) | 7.0–7.8 (0) |
| `POST /v1/reconcile`, p50 ms | 14.1–16.3 | 70.9–71.5 | 593–616 |

Against B4's "after" column:

- A payment is one kernel transaction more (8 instead of 7; the refund
  too): F2's `MarkPosted` after the ledger sync. It still costs the same at
  1k, 10k and 100k payments.
- Serialization retries per 200 payments with 8 clients fell from 88–132
  to 18–29 (F4). At 32 clients, the first time this was measured, there are
  74–123 per 800 payments. The remaining retries are `MarkPosted` against
  `Create` on the same index pages (`docs/PROGRESS.md`, F4).
- Throughput was 141–163 payments/s, against 123–164 in B4. These runs are
  not comparable with F4's quiet-host measurement (181–198 payments/s,
  `docs/PROGRESS.md`): the F4 harness, run interleaved here, also gave
  143–154 payments/s. F5 changed nothing on this path. The difference is
  the host load.
- The recovery pass with nothing to fix is O(unfinished) since F2: 6.5–8.4
  ms at every size, against 10 / 78 / 852 ms. A pass that fixes 5 payments
  takes 60–71 ms at every size, against 65 / 619 / 1,002 ms.
- `POST /v1/reconcile` is unchanged within noise (the F4 harness gave
  70–76 ms at 10k and 602–626 ms at 100k). It runs `cpu_single`, now the
  proven join; it is still O(tenant), as above.

### After G (`405ffc9`)

The same harness and B4-era seeded databases (`e2e_seed_*`), the build
before G (`f889488`, "pre-G") and after G6 (`405ffc9`) interleaved, order
alternating, three rounds per size, `E2E_CLIENTS=8,32`, medians [range]
(raw: `benchmarks/results/e2e-afterG-g5.xlarge.json`). Load average was
2–3.5 during the series (it reached 5–9 at other times today, from
another session's job; those runs are not used). The G builds migrate the
pre-G seeds at startup (history backfill, `open_payments`), before timing.

| | build | 1k | 10k | 100k |
|---|---|---:|---:|---:|
| `POST /v1/payments`, p50 ms | pre-G | 19.0 | 17.4 | 16.2 |
| | G | 23.6 | 21.4 | 21.1 |
| … kernel transactions per payment | both | 8 | 8 | 8 |
| `GET /v1/payments/{id}`, p50 ms | pre-G | 0.7 | 0.7 | 0.7 |
| | G | 1.1 | 1.1 | 1.1 |
| 8 clients, payments/s | pre-G | 183.9 [179–188] | 182.8 [182–183] | 187.3 [184–188] |
| | G | 143.1 [141–144] | 140.1 [138–143] | 151.3 [145–152] |
| … serialization retries per run | pre-G | 25–34 | 17–28 | 16–27 |
| | G | 3–8 | 0–10 | 0–15 |
| 32 clients, payments/s | pre-G | 185.0 [183–186] | 180.2 [180–183] | 180.9 [177–186] |
| | G | 143.9 [142–145] | 142.0 [140–144] | 141.9 [141–147] |
| … serialization retries per run | pre-G | 110–114 | 107–121 | 100–107 |
| | G | 0–3 | 0 | 0–3 |
| recovery pass, 5 to fix, ms | pre-G | 57.7 | 58.4 | 55.0 |
| | G | 74.1 | 74.4 | 91.9 |
| recovery pass, nothing to fix, ms | pre-G | 6.5 | 6.5 | 6.6 |
| | G | 7.6 | 10.0 | 25.5 |
| `POST /v1/reconcile`, one call, p50 ms | pre-G (whole tenant) | 12.9 | 61.5 | 509 |
| | G (first period, ≤ 4096 ids) | 288 | 423 | 350 |
| whole tenant, ms | pre-G (one call) | 12.9 | 61.5 | 509 |
| | G (all periods) | 254 (1 period) | 1256 (3) | 9615 (28) |

What changed, and what it cost:

- **Serialization retries** are gone from the payment flow: 0–15 per run
  against 16–121 (G5: `MarkPosted` is a HOT update; `open_payments`).
- **Throughput fell 19–23%** (about 183 to 142 payments/s), and every
  kernel transaction got about 0.4–0.6 ms slower (view +0.4 ms with one
  transaction, create +4–5 ms with eight). A bisect at 10k, interleaved,
  two rounds (`e2e-afterG-bisect-g5.xlarge.json`): pre-G 187–191 / 179–181
  payments/s (8 / 32 clients), G1 (`ce20fde`) 141–145 / 139–141, G3
  (`95fc7d2`) 139–140 / 141–142, G6 140–142 / 142–143. The whole drop is
  G1. G1 makes the engines use the database's clock (`Clock::Database`, for
  the history's `at`), which runs one more statement, `SELECT
  transaction_timestamp()`, in every kernel transaction (visible in
  PostgreSQL's log), and every change now also inserts a history row. The
  per-transaction cost matches the extra round trip; I did not measure the
  two parts separately. G1's PROGRESS entry did not measure throughput;
  this is the first measurement of it. Reading the clock with the first
  query of the load (or `now()` in the write) would remove the round trip;
  not done.
- **Recovery** stays O(unfinished) but is slower: the empty pass 7.6 /
  10.0 / 25.5 ms against 6.5–6.6 (the `open_payments` join; the query
  itself took 0.3 ms in `EXPLAIN ANALYZE` at 100k, so most of it is not
  the query; not traced), and a pass fixing 5 payments 74–92 ms against
  55–58 (more transactions per payment, each slower).
- **Reconciliation** is bounded per call and no longer loads the tenant,
  but costs much more in total. One call reads at most 4096 ids, in 64-id
  pages (h5i-app's read predicates have no range comparison, so a page is
  an `OR` of 64 keys; F3): 64 `Range` + 64 `RefundRange` + 128
  `TransferRange` kernel transactions, 290–420 ms whatever the tenant's
  size. Reading the whole tenant that way takes 20× (10k) and 19× (100k)
  the pre-G single call. Part of that cost came with G4, not G6: G4 reads
  the ledger side through each system account's journal in 64-entry
  pages instead of one snapshot, and the whole-tenant call of the G5
  build took 606 ms at 10k and 5.7 s at 100k (`docs/PROGRESS.md`, G6),
  10× the pre-G call; G4's PROGRESS entry did not measure that. The same
  paged journal read is in `POST /v1/recover` (`find_work`), not measured. Nightly runs only need the periods since
  the last closed one. A range predicate in h5i-app (a read of `id ≥ a AND
  id < b`) would make a period one read per table; that is a change to
  h5i-app's query language and its Lean semantics, not done.

### H1: the clock and the history row (h5i `a45b6e3`)

G1's two costs measured apart at 10k (`e2e_seed_10000`), every build
interleaved with the others, order rotating, three rounds per series,
`E2E_CLIENTS=8,32`, medians [range] (raw:
`benchmarks/results/e2e-H1-clock-g5.xlarge.json`). Load average 2–4.
"seq" is h5i `92a2fbf` (the clock pipelined with the load, writes still
one round trip each); "pipelined" is `a45b6e3` (the writes pipelined too),
the H1 commit. `Clock::System` and "no history" are experimental builds
(not committed) that drop the clock statement and then also the history
INSERTs; without history the audit fails, so only throughput is used.

| build | 8 clients, payments/s | 32 clients, payments/s | create p50 ms | view p50 ms |
|---|---:|---:|---:|---:|
| pre-G `f889488` | 184.3 [181–199], 183.4 [178–187] | 181.7 [181–184], 178.9 [177–179] | 18.6–18.8 | 0.72 |
| G `74225cd` | 144.5 [144–145], 141.3 [137–144], 142.5 [142–143] | 142.4 [141–143], 138.8 [137–143], 139.6 [139–141] | 23.1–23.5 | 1.09–1.10 |
| seq | 156.5 [155–159], 154.6 [150–155] | 157.6 [156–158], 153.9 [152–155] | 18.5–20.9 | 0.78–0.79 |
| seq, `Clock::System` | 161.7 [156–163] | 159.7 [160–160] | 20.6 | 0.72 |
| seq, `Clock::System`, no history | 176.9 [177–178] | 179.0 [178–180] | – | – |
| pipelined | 170.0 [168–173], 171.1 [159–173] | 176.5 [176–179], 180.3 [179–182] | 19.1–19.4 | 0.78 |
| pipelined, `Clock::System` | 174.8 [173–178] | 183.7 [184–188] | 19.2 | 0.73 |
| pipelined, `Clock::System`, no history | 182.6 [173–186] | 196.5 [193–197] | – | – |

(Several values in a cell: one per series that ran the build.)

- **The clock**: its round trip (a Parse, then the query) was worth 12–15
  payments/s (8–11%); H1 removed it. The statement itself, now prepared
  and pipelined, still costs 2–5 payments/s (2–3%) and 0.05–0.06 ms of the
  view.
- **The history row**: with one round trip per write it cost 15–19
  payments/s (9–12%); with pipelined writes, 8–13 (4–7%), the INSERT's
  time in the server.
- **Together** H1 brings G from 139–145 to 170–180 payments/s, 1–7%
  under pre-G (8 clients 170–171 vs 183–184, 32 clients 177–180 vs
  179–182). Create p50 19.1–19.4 ms (pre-G 18.7), refund 19.3–19.4 (19.3),
  a recovery pass fixing 5 payments 61–64 ms (G 76–80, pre-G 55).
- **Retries** went up again: 5–31 per run against 0–13 for G. PostgreSQL's
  log names `Create`, aborted as a pivot on its `payments` insert; about 1%
  of the transactions. Shorter transactions overlap more. Not fixed.
- The empty recovery pass did not move (9.8–10.0 ms; pre-G 6.5).

### After H (h5i `a98bcbd`)

The B4-era seeds (`e2e_seed_*`), three builds interleaved, order rotating,
three rounds per size, `E2E_CLIENTS=8,32`, medians [range] (raw:
`benchmarks/results/e2e-afterH-g5.xlarge.json`): G (`74225cd`), H1
(`ca8bbca`, h5i `a45b6e3`: clock and writes pipelined) and H2 (range reads,
h5i `a98bcbd`). All three ran the same harness, which since H2 also times
the paged audit (`GET /v1/ledger/audit`, two calls after the first) and 20
pages of 64 of a merchant's payments (`List`) and of account 0's journal
(`Transfers`), compiled against each revision. Load average 1.6–2.5.

| | build | 1k | 10k | 100k |
|---|---|---:|---:|---:|
| 8 clients, payments/s | G | 140.0 [140–143] | 142.7 [141–144] | 150.7 [151–153] |
| | H1 | 174.2 [171–176] | 172.4 [163–174] | 190.0 [188–190] |
| | H2 | 172.0 [170–175] | 171.4 [170–173] | 190.0 [188–191] |
| … retries per run | G / H1 / H2 | 0–8 / 4–12 / 13–42 | 0–2 / 15–27 / 13–24 | 0 / 0–8 / 5–13 |
| 32 clients, payments/s | G | 141.4 [141–143] | 142.3 [142–144] | 141.5 [141–143] |
| | H1 | 181.6 [178–182] | 179.5 [176–181] | 181.7 [181–183] |
| | H2 | 181.2 [176–182] | 179.9 [180–183] | 182.0 [180–185] |
| … retries per run | G / H1 / H2 | 9–15 / 13–22 / 14–24 | 5–18 / 14–18 / 10–17 | 2–13 / 10–21 / 13–29 |
| `POST /v1/payments`, p50 ms | G / H1 / H2 | 23.5 / 19.2 / 19.1 | 23.4 / 19.2 / 18.1 | 21.1 / 16.6 / 16.9 |
| `GET /v1/payments/{id}`, p50 ms | G / H1 / H2 | 1.09 / 0.78 / 0.77 | 1.09 / 0.78 / 0.78 | 1.09 / 0.78 / 0.78 |
| recovery pass, 5 to fix, ms | G / H1 / H2 | 75.2 / 61.9 / 61.3 | 73.3 / 60.8 / 62.1 | 89.1 / 74.1 / 74.5 |
| recovery pass, nothing to fix, ms | G / H1 / H2 | 7.8 / 7.8 / 7.9 | 9.9 / 10.0 / 10.0 | 25.0 / 23.5 / 22.7 |
| `POST /v1/reconcile`, one call (first period), p50 ms | G | 284.7 | 423.3 | 348.1 |
| | H1 | 221.0 | 341.1 | 267.1 |
| | H2 | 14.6 | 27.0 | 24.2 |
| whole tenant, all periods, ms | G | 252.4 | 1251.1 | 9618.1 |
| | H1 | 188.3 | 1004.3 | 7301.1 |
| | H2 | 14.3 | 72.4 | 529.7 |
| … kernel transactions | G and H1 / H2 | 196 / 4 | 762 / 12 | 7056 / 112 |
| paged audit, ms | G | 376.1 | 8482.2 | 7577.6 |
| | H1 | 355.0 | 8343.8 | 6340.1 |
| | H2 | 78.1 | 404.9 | 4558.6 |
| `List` page of 64, p50 ms | G / H1 / H2 | 4.35 / 3.98 / 1.68 | 4.37 / 4.03 / 1.67 | 4.34 / 4.03 / 1.70 |
| journal page of 64, p50 ms | G / H1 / H2 | 5.77 / 5.47 / 1.39 | 22.4 / 22.1 / 1.40 | 2.42 / 2.08 / 1.60 |

(The pre-G single reconciliation call, from "After G": 12.9 / 61.5 / 509
ms. The sweep's exceptions are the same in every build: the Mock PSP does
not know the seeded payments, 1,100 / 11,000 / 110,000 PSP-side, none
ledger-side.)

- **Throughput** is H1's: H2 changes no write path. Against G, +20–29%;
  against pre-G (183–184 / 179–182 at 10k, "H1") within 1–7%.
- **Retries** stay at H1's level or a little above (5–42 per run at 8
  clients), about 1% of transactions; the aborts are `Create` pivots on the
  `payments` insert (see "H1"). Not fixed.
- **Reconciliation**: a period is now 4 kernel transactions (one `Range`,
  one `RefundRange`, two `TransferRange`s) instead of up to 256, each one index
  range. One call 15–27 ms (G 285–423), the whole tenant 14 / 72 / 530 ms
  (G 252 / 1251 / 9618): 1.1–1.2× the pre-G single whole-tenant call at
  10k and 100k, and bounded per call.
- **Paging**: a 64-row page is 1.4–1.7 ms at every size (G 2.4–22 ms; the
  OR of 64 equalities cost planning time, and at 10k the journal's plan
  was 22 ms, not traced). The paged audit is 5–21× faster at 1k and 10k;
  at 100k only 1.7× (4.6 s): it reads about 3,400 journal pages of 64 and
  is now bound by their number. Larger journal pages for the audit would
  cut that; not done.
- The empty recovery pass did not move (it is `open_payments`, G5).

### I1: the last serialization retries (h5i `9ec26b5`)

The retries left after H were not `Create`'s. PostgreSQL's log with
`log_statement = all` (one 32-client run at 10k: 121 aborts) names the
aborted transactions by their statements: all were `MarkPosted` (reads of
`payments` and `refunds`, a write of `payments`), aborted as the pivot on
its `payments` write or at commit. Its trigger deleted the payment's row
from `open_payments`, and under SERIALIZABLE that `DELETE` is a read: of
the whole table (`pg_locks` showed a relation-level SIRead lock on
`open_payments` during the runs; the table is small, so the planner scans
it) or of the one index page every new payment's row is inserted into. So
each `MarkPosted` read what concurrent `Create`s (their insert trigger)
and other `MarkPosted`s (their deletes) wrote. (H1 and H2 named `Create`
from the failing `INSERT INTO "payments"` statement, which is also the
upsert `MarkPosted` updates with.)

I1 makes the triggers insert only (a payment inserted unmarked, a marker
cleared); the scheduled recovery prunes marked payments from
`open_payments` at the start of each pass, in `READ COMMITTED`, each row
deleted only with its payment locked `FOR SHARE` and seen marked
(`payment_storage::prune_open_payments`). The same runs also exposed an
h5i-app-pg bug from H1: a pipelined group whose first statement failed
could report a later statement's "current transaction is aborted"
(25P02, not retryable) instead of the serialization failure; under load
3 of 9 runs of the `concurrency` tests failed so once the new pruning test ran beside them. h5i branch `app/pipeline-errors`
(`9ec26b5`, on main `3ab10b3`) awaits every reply and returns the cause.

The B4-era seeds, `d71881b` (H2) and I1 interleaved, order rotating, three
rounds per size, `E2E_CLIENTS=8,32,32` (a second 32-client run after the
first), medians [range] (raw: `benchmarks/results/e2e-I1-g5.xlarge.json`).
Other users' jobs ran on the machine during the 10k rounds (load average
4–6 instead of about 2), so the 10k throughput is noisy; the 100k rounds
ran at load 2–4.

| | build | 10k | 100k |
|---|---|---:|---:|
| 8 clients, payments/s | H2 | 171.9 [169–179] | 187.8 [186–190] |
| | I1 | 170.8 [146–178] | 188.9 [189–193] |
| 32 clients, payments/s | H2 | 169.1 [135–177] | 181.5 [181–182] |
| | I1 | 173.5 [130–180] | 183.3 [182–184] |
| 32 clients again, payments/s | H2 | 135.1 [95–177] | 186.1 [186–186] |
| | I1 | 181.7 [164–183] | 183.7 [170–186] |
| retries per run (8 / 32 / 32 clients) | H2 | 3–16 / 20–24 / 18–45 | 0–7 / 18–27 / 4–12 |
| | I1 | 0 / 0 / 0 | 0 / 0 / 0 |
| `POST /v1/payments`, p50 ms | H2 / I1 | 17.3 / 21.0 | 16.9 / 16.9 |
| recovery pass, 5 to fix, ms | H2 / I1 | 68.4 / 75.8 | 80.5 / 97.9 |
| recovery pass, nothing to fix, ms | H2 / I1 | 10.6 / 14.5 | 26.9 / 30.5 |

- **Retries**: none in any I1 run, against 3–45 per run for H2.
- **Throughput** is H2's at 100k (within 2% either way); at 10k the
  noise is larger than any difference. An earlier series at load 2.5–4
  (three rounds, I1 with the first version of the pruning and without the
  h5i fix) gave the same picture: 182 / 185 / 187 against 177 / 182 / 188
  payments/s at 10k and 194 / 186 / 188 against 189 / 180 / 191 at 100k,
  0 retries against 0–27.
- **Recovery** pays for the deletes `MarkPosted` no longer does: the first
  pass after the throughput runs prunes about 1,100 rows (+7 to +17 ms),
  and an empty pass costs one more short transaction (+3 to +4 ms). A
  first version joined `open_payments` to `payments` and let the planner
  order the join; it scanned every payment of the tenant (an empty pass
  20 ms at 10k, 57 ms at 100k) and was replaced by lookups by key.

### I2: larger journal pages for the system's readers

The ledger kernel takes `Transfers` (an account's journal) and
`AccountRange` (accounts by id) pages of up to 4096 (`MAX_RANGE`; before
64), operator-only as before; over HTTP they stay at most 64. The paged
audit, reconciliation's whole-tenant inputs and `find_work` read pages of
1024 (`payment_storage::ledger::JOURNAL_PAGE`). One audit at 100k payments
per page size (one run each, the I2 build otherwise): 64 entries 4.40 s,
512 1.55 s, 1024 1.42–1.43 s, 2048 1.46 s, 4096 1.51–1.53 s. Past a few
hundred entries the audit is bound by the rows it reads (every transfer
twice, once per account), and the kernel's journal page looks each
entry's transfer up among the page's transfers, which grows with the page.

`d71881b` (H2) and I2 interleaved, order rotating, two rounds per size,
`E2E_CLIENTS=8`, medians [range] (raw:
`benchmarks/results/e2e-I2-g5.xlarge.json`), load average 1.4–2.6:

| | build | 1k | 10k | 100k |
|---|---|---:|---:|---:|
| paged audit, ms | H2 | 46.4 [46.2–46.5] | 369.6 [369.2–370.0] | 4393 [4376–4410] |
| | I2 | 20.9 [20.8–20.9] | 143.5 [143.5–143.5] | 1454 [1452–1456] |
| journal page of 64 over HTTP, p50 ms | H2 / I2 | 1.36 / 1.57 | 1.38 / 1.38 | 1.59 / 1.38 |
| whole tenant reconciled, all periods, ms | H2 / I2 | 9.9 / 9.3 | 65.5 / 63.6 | 514 / 522 |
| 8 clients, payments/s | H2 / I2 | 178.3 / 177.9 | 176.3 / 180.3 | 195.7 / 196.2 |

- **The paged audit** is 2.2–3.0× faster (100k: 4.4 s → 1.45 s); before,
  at 100k it read about 3,400 journal pages of 64.
- Everything else is unchanged: the HTTP journal page, the reconciliation
  sweep (its period reads were already ranges of 4096) and throughput.

### I3: `POST /v1/recover` by id pages

`Orchestrator::find_work` (the work `POST /v1/recover` does for one
tenant) loaded the tenant's payments in one snapshot and every journal
page of the PSP clearing and refund holding accounts. Since I3 it reads
pages of 4096 ids: per page a `Range` and a `RefundRange` of the payment
kernel and the four `TransferRange`s of the ledger's posting kinds, six
read-only transactions, so memory is one page. The harness times three
calls of `POST /v1/recover` after the recovery passes (nothing left to do,
so the time is `find_work`).

`d71881b` with this harness and I3 (which includes I1 and I2), two rounds
per size, `E2E_CLIENTS=8`, medians [range] (raw:
`benchmarks/results/e2e-I3-g5.xlarge.json`), load average about 2:

| | build | 10k | 100k |
|---|---|---:|---:|
| `POST /v1/recover`, p50 ms | H2 | 245.9 [245.7–246.1] | 3323 [3321–3325] |
| | I3 | 65.7 [64.9–66.5] | 552.9 [552.6–553.2] |
| … kernel transactions per call | H2 / I3 | 209 / 18 | 2038 / 162 |

- **3.7× at 10k, 6.0× at 100k**, and bounded memory. H2's call was one
  snapshot of the payments plus 209 and 2,038 kernel transactions, nearly
  all journal pages of 64; I3's is 3 and 27 pages of ids, six
  transactions each.
- It is still O(tenant): the journal check (a marked payment whose
  postings the ledger lost, as after an older ledger backup was restored)
  has to look at every captured payment.

### The batch worker, with its files

`h5i-pay-batch reconcile` on CSV files in the page cache (local disk, not
S3), 1% amount mismatches and 0.5% missing rows; `elapsed_ms` is the
engine (including CUDA context creation for the GPU), `total_ms` the whole
process (CSV parsing and report writing included):

| rows per side | engine | engine ms | process ms |
|---:|---|---:|---:|
| 1M | gpu | 266 | 899 |
| 1M | parallel | 100 | 738 |
| 10M | gpu | 477 | 6,695 |
| 10M | parallel | 2,219 | 8,974 |

Parsing the two CSV files is most of a job: about 0.6 s at 1M rows and
6.2 s at 10M (single-threaded `str::parse`). The GPU saves 1.7 s of 9 s at
10M rows; at 1M a one-shot job is faster on the CPU, which is why the
worker's `--min-gpu-rows` default is now 2,000,000. `h5i-pay export`
(PostgreSQL snapshot to CSV) took 0.09 s at 10k and 0.51 s at 100k
payments. S3 transfer times were not measured (no AWS account here); at
the 10M-row size the two files are 580 MB.

Bulk payout screening (`h5i-pay-batch policy`, 300k requests): GPU 209 ms
engine time, almost all context creation; 4-thread CPU 2.5 ms.

**Conclusions.** For the online path the bottleneck was the data path,
not computation: B1–B3 made a payment 3× faster at 1k payments and 29×
at 10k, and independent of tenant size. For the batch path the GPU is the
fastest engine from about 100k rows when its context is warm, but a
one-shot job spends most of its time parsing text; binary inputs (or a
long-lived worker) would matter more than any engine change.

## Batch engines (M3, re-run after A)

Four implementations of each batch workload, measured on the same machine
and the same inputs, with every engine's output checked against the
reference before it is timed. Raw results:
[`benchmarks/results/g5.xlarge.json`](../benchmarks/results/g5.xlarge.json)
(table: [`g5.xlarge.md`](../benchmarks/results/g5.xlarge.md); M3, quiet
host, CUDA C kernels from nvcc).

**After A** (kernels compiled from the Rust source by rustc_codegen_nvvm):
[`g5.xlarge-nvvm.json`](../benchmarks/results/g5.xlarge-nvvm.json)
([table](../benchmarks/results/g5.xlarge-nvvm.md)), same command, but with
2 of 4 vCPUs busy with another workload, so its CPU rows are slower than
M3's and only the GPU rows compare. GPU p50, nvcc → nvvm:
reconciliation 1M 16.9 → 17.4 ms (kernels 4.09 → 4.19), 10M 199.8 → 202.0
ms (kernels 48.2 → 49.4); policy 1M 4.86 → 5.21 ms (kernel 0.29 → 0.54),
10M 57.6 → 62.3 ms (kernel 2.55 → 5.00). The reconciliation kernels are
as fast as the CUDA C; the policy kernel takes twice as long (the Rust
`check` with its bounds-checked slices and binary search), which moves
the end-to-end policy time by 7% because transfers dominate. Context and
JIT: 150 ms (`bench init`; 136 ms with the nvcc PTX). The tables below are
M3's.

## Setup

| | |
|---|---|
| Instance | AWS g5.xlarge: 4 vCPU AMD EPYC 7R32 (2 cores × SMT), 16 GiB, 1× NVIDIA A10G 24 GiB, driver 595.112, CUDA 13.2 |
| Price used for cost | $1.006/hour, g5.xlarge on-demand in us-east-1 ([Vantage](https://instances.vantage.sh/aws/ec2/g5.xlarge), [devzero](https://www.devzero.io/instances/aws/g5.xlarge)) |
| Build | `cargo build --release` (Rust 1.99.1, `debug = 1`), kernels `nvcc -O3 -arch=compute_75` (JIT to sm_86) |
| Runs | one warm-up run, then 21 (≤ 200k rows), 11 (≤ 2M) or 5 (10M) timed runs; p50 and p95 of whole-batch time |
| Input | synthetic, deterministic (`payment_batch::synth`); input already in host memory |
| Command | `cargo run --release -p h5i-pay-benchmarks --features gpu -- all --out benchmarks/results/g5.xlarge.json` |

Engines:

- **single** — one thread: `cpu_single` (open-addressing hash join; since
  F5 the extracted, proven `join::join`) and the verified `policy::check`
  per row.
- **parallel** — rayon on 4 threads: radix-partition by key hash, then one
  hash join per partition (since F5 `join::join` on the partition's rows);
  `check` with `par_iter`. The tables below predate F5; F5's own
  before/after engine times are in `docs/PROGRESS.md` (F5).
- **simd** — one thread, AVX2: reconciliation probes four rows per step
  with 64-bit gathers; policy evaluates every rule for four rows as lane
  masks and blends the first failing code.
- **gpu** — CUDA kernels through h5i-app-gpu's `backend-nvvm` (`cust`):
  hash tables built with `atomicCAS`/`atomicAdd`/`atomicMin`, one thread
  per row for probes and policy checks. The time includes allocation,
  host→device copies (pageable memory), table initialization, all kernels
  and device→host copies. It does not include creating the CUDA context and
  JIT-compiling the PTX, which costs **136 ms once per process**
  (`bench init`, three runs: 137.8, 136.3, 135.6 ms).

Rows: for reconciliation, *rows* is the internal side; the external side
has about the same number (discrepancies add or drop ~1%), and throughput
counts both sides. The synthetic report has about 1% amount, 0.2%
currency and 0.5% status mismatches, 0.5% missing, 0.5% extra and 0.1%
duplicated rows, and is shuffled. The policy batch draws from a policy with
4 currencies, a 2,000-merchant allowlist and 32 blocked recipients, and
contains every decision code.

## Results

### Reconciliation

| rows | engine | p50 ms | p95 ms | Mrows/s | vs single | H2D / kernel / D2H ms | peak RSS +MiB | device MiB | $ / M rows |
|---:|---|---:|---:|---:|---:|---|---:|---:|---:|
| 10 000 | single | 0.90 | 0.96 | 22.2 | 1.00× | – | 1 | – | 0.000013 |
| 10 000 | parallel | **0.35** | 0.40 | 57.4 | 2.58× | – | 1 | – | 0.000005 |
| 10 000 | simd | 1.22 | 1.31 | 16.4 | 0.74× | – | 1 | – | 0.000017 |
| 10 000 | gpu | 0.39 | 0.41 | 51.7 | 2.33× | 0.19 / 0.02 / 0.05 | 1 | 2 | 0.000005 |
| 100 000 | single | 12.75 | 13.08 | 15.7 | 1.00× | – | 9 | – | 0.000018 |
| 100 000 | parallel | 3.67 | 4.15 | 54.5 | 3.47× | – | 3 | – | 0.000005 |
| 100 000 | simd | 17.89 | 18.87 | 11.2 | 0.71× | – | 9 | – | 0.000025 |
| 100 000 | gpu | **2.16** | 2.21 | 92.7 | 5.90× | 0.95 / 0.13 / 0.27 | 1 | 14 | 0.000003 |
| 1 000 000 | single | 308.66 | 325.51 | 6.5 | 1.00× | – | 75 | – | 0.000043 |
| 1 000 000 | parallel | 88.57 | 90.52 | 22.6 | 3.48× | – | 27 | – | 0.000012 |
| 1 000 000 | simd | 477.23 | 481.33 | 4.2 | 0.65× | – | 75 | – | 0.000067 |
| 1 000 000 | gpu | **16.87** | 22.29 | 118.7 | 18.30× | 4.97 / 4.09 / 1.87 | 12 | 121 | 0.000002 |
| 10 000 000 | single | 3964.33 | 3999.51 | 5.0 | 1.00× | – | 1138 | – | 0.000055 |
| 10 000 000 | parallel | 1726.26 | 1815.62 | 11.6 | 2.30× | – | 319 | – | 0.000024 |
| 10 000 000 | simd | 5834.50 | 5855.56 | 3.4 | 0.68× | – | 1139 | – | 0.000081 |
| 10 000 000 | gpu | **199.78** | 200.34 | 100.2 | 19.84× | 39.82 / 48.18 / 68.56 | 115 | 1597 | 0.000003 |

### Batch policy validation

| rows | engine | p50 ms | p95 ms | Mrows/s | vs single | H2D / kernel / D2H ms | peak RSS +MiB | device MiB | $ / M rows |
|---:|---|---:|---:|---:|---:|---|---:|---:|---:|
| 10 000 | single | 0.62 | 0.64 | 16.0 | 1.00× | – | 0 | – | 0.000017 |
| 10 000 | parallel | 0.41 | 0.46 | 24.6 | 1.54× | – | 0 | – | 0.000011 |
| 10 000 | simd | 0.32 | 0.33 | 30.9 | 1.93× | – | 0 | – | 0.000009 |
| 10 000 | gpu | **0.29** | 0.32 | 34.7 | 2.16× | 0.16 / 0.02 / 0.02 | 0 | 0 | 0.000008 |
| 100 000 | single | 6.09 | 7.15 | 16.4 | 1.00× | – | 1 | – | 0.000017 |
| 100 000 | parallel | 1.94 | 2.78 | 51.4 | 3.13× | – | 1 | – | 0.000005 |
| 100 000 | simd | 3.25 | 3.30 | 30.8 | 1.88× | – | 0 | – | 0.000009 |
| 100 000 | gpu | **0.77** | 0.82 | 129.3 | 7.88× | 0.45 / 0.06 / 0.09 | 1 | 3 | 0.000002 |
| 1 000 000 | single | 59.93 | 63.19 | 16.7 | 1.00× | – | 4 | – | 0.000017 |
| 1 000 000 | parallel | 18.94 | 20.59 | 52.8 | 3.16× | – | 4 | – | 0.000005 |
| 1 000 000 | simd | 34.69 | 37.27 | 28.8 | 1.73× | – | 4 | – | 0.000010 |
| 1 000 000 | gpu | **4.86** | 6.24 | 205.9 | 12.34× | 3.55 / 0.29 / 0.63 | 4 | 34 | 0.000001 |
| 10 000 000 | single | 617.84 | 621.51 | 16.2 | 1.00× | – | 38 | – | 0.000017 |
| 10 000 000 | parallel | 193.07 | 203.67 | 51.8 | 3.20× | – | 38 | – | 0.000005 |
| 10 000 000 | simd | 366.67 | 368.14 | 27.3 | 1.69× | – | 38 | – | 0.000010 |
| 10 000 000 | gpu | **57.59** | 57.68 | 173.7 | 10.73× | 29.98 / 2.55 / 23.32 | 38 | 343 | 0.000002 |

"peak RSS +MiB" is the increase of the process's peak resident set during
the first run over the resident set after the input was built (for the GPU
after the context existed). For policy validation the increase is the
output vector itself (4 bytes per request), the same in every engine.

## What the numbers say

**Where the GPU wins.** With the CUDA context already up (a long-lived
worker), the GPU is the fastest engine for reconciliation from about
100,000 rows per side (1.7× the 4-thread CPU engine at 100k, 5.2× at 1M,
8.6× at 10M) and for policy validation from about 100,000 requests (2.5×,
3.9×, 3.4×). At 10,000 rows all engines finish in under a millisecond and
the 4-thread CPU engine matches or beats the GPU for reconciliation.

**Where it does not.**

- *One-shot jobs pay 136 ms for the CUDA context and PTX JIT.* An AWS Batch
  job that starts a process per settlement file is faster on the CPU below
  roughly 2–3 million rows per side: at 1M rows the GPU job takes
  17 + 136 ≈ 153 ms against 89 ms on 4 CPU threads; at 10M it takes
  200 + 136 ≈ 336 ms against 1,726 ms. The worker therefore uses the GPU
  only above `--min-gpu-rows` (default 2,000,000 since B4; it was 200,000)
  and only on a GPU instance. With CSV parsing included the break-even
  moves further up (see "The batch worker, with its files").
- *Policy validation is transfer-bound.* At 10M requests the kernel runs
  2.55 ms of the 57.6 ms; the rest is copying 320 MB in and 40 MB out
  through pageable memory. The GPU only pays off when requests arrive in
  large batches, and would gain most from pinned buffers or from data that
  already lives on the device.
- *Reconciliation's device→host copy is the largest part at 10M*
  (68.6 ms for three 40 MB output columns, about 1.7 GB/s from pageable
  memory). Narrower codes (u8) and pinned buffers are the obvious next
  steps; neither is implemented.
- *The SIMD reconciliation engine is slower than the scalar one*
  (0.65–0.74×). Its probe loop gathers four table slots at a time; on this
  Zen 2 CPU those gathers are microcoded and, it appears, slower than the
  scalar loads an out-of-order core already overlaps. The SIMD policy engine helps
  (1.7–1.9×), because there the work is comparisons on values already in
  registers. Both are kept as measured baselines, not recommendations.
- *Four vCPUs are two cores.* The parallel engines scale 3.1–3.5× on
  in-cache sizes and 2.3× for 10M-row reconciliation, which is bound by
  memory bandwidth and the partition pass. A larger CPU instance would
  narrow the GPU's lead; it was not measured.
- *The batch is rarely the bottleneck end to end*: measured in B4 above
  (CSV parsing in the worker; snapshot loading in `POST /v1/reconcile`
  before G6, page reads since).
- *Cost per million rows is negligible for every engine* (below $0.0001 at
  the g5.xlarge price). What costs money is a GPU instance that idles: the
  AWS Batch compute environment scales GPU capacity from zero per job.

**Memory.** The GPU engine keeps host memory flat (+115 MiB at 10M rows,
mostly the output columns) and uses 1.6 GiB of device memory at 10M rows
per side (inputs, two hash tables at ≤ 50% load, outputs). The single-thread
CPU engine needs +1.1 GiB of host memory for the same input.

## Correctness of the comparison

Before timing, each engine's output is compared with `cpu_single` (proven
equal to the reference's per-row answer since F5, `join_eq_reference`, and
checked against the `BTreeMap` reference in `tests/differential`) or with
the verified `policy::check`; the benchmark aborts on any difference. The
differential suite (`cargo test -p h5i-pay-tests --features gpu --test
differential`) additionally compares every engine, row by row, on empty
batches, 255/256/257-row block boundaries, a hot key shared by thousands of
rows (atomic contention), hash-colliding keys, all-missing and all-extra
sides, malformed policies and unsorted allowlists, and checks repeat
determinism and partition independence (h5i-app-gpu `check_batch`). This is
evidence for the inputs tested, not a proof of CPU/GPU equivalence
(`docs/proof-assumptions.md`).

The GPU never commits financial state: reconciliation output is a report,
and an approved policy decision is only a candidate that the ledger kernel
still has to accept in a SERIALIZABLE transaction.
