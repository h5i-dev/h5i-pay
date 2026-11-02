# h5i-pay progress log

Status of the milestones in `docs/design-v0.1.md`, kept so work can resume
after a disconnect. Newest entries first within each milestone.

## Environment (verified 2026-10-31)

| Tool | Version / location | Notes |
|---|---|---|
| Rust | 1.99.1 | workspace edition 2024 |
| h5i CLI | 0.5.2, `~/.local/bin/h5i` | built from `/root/h5i-upstream` with `H5I_SKIP_WEB_BUILD=1` (no Node on the host, so the `h5i ui` web bundle is not embedded) |
| h5i-app | 0.5.2 from crates.io, pinned `=0.5.2` | published from h5i commit `02ccdde4ccfc` (#771; includes #769 and #770). `h5i-app-gpu`, `-gpu-backend-nvvm` and the `h5i_app_lib` Lean library come from git at that commit (not on crates.io). Earlier: crates.io 0.1.4 + tag `v0.5.2` (M1–M5), then git rev `a19667c`. The h5i CLI is still the tag-v0.5.2 build |
| Charon / Aeneas | 4bd5a29f6e97 / b86120db3183 at `/opt/aeneas/{charon,aeneas}/bin` | built with `nix build github:AeneasVerif/aeneas/<rev>` (Determinate Nix installer); `h5i app doctor` is green when both are on `PATH` |
| Lean | leanprover/lean4:v4.31.0 via elan (`~/.elan/bin`) | Mathlib cache from `lake exe cache get` |
| PostgreSQL | 17 in docker (`scripts/dev-db.sh`, port 55432) | |
| CUDA | 13.2, NVIDIA A10G (sm_86) | |

`h5i` v0.5.2 has no `init` or `capture` commands (`h5i --help` lists
`box ui browser plugin project app skill completion`), so commits are
plain `git commit`; provenance for the proofs is `h5i app check` /
`h5i app mutate`, whose run records live under `crates/*/.h5i/app/runs/`.

PATH for proofs: `export PATH=/opt/aeneas/aeneas/bin:/opt/aeneas/charon/bin:$HOME/.elan/bin:$PATH`.

## M1 — Verified Ledger: done

- `crates/payment-ledger`: double-entry kernel (accounts with posted
  debits/credits per currency, `no_overdraft`/`no_overcredit` limits,
  client-chosen transfer ids with replay detection, linked batches,
  owner/operator authorization). Extracted with `h5i app extract crates/payment-ledger`.
- `crates/payment-storage`: h5i-app `Kernel`/`Store`/`ReplyCodec`/JSON for the
  ledger (PostgreSQL schema `ledger`), plus `audit()` that re-checks the
  proven invariants on loaded rows.
- `crates/payment-api`: axum server, `POST /v1/ledger`, `GET /v1/ledger/audit`.
- Lean (`proofs/ledger/{Ledger,Batch,Models}`), all on the
  extracted code except `Models/Engine.lean` (a model of the trusted engine):
  `conservation`, `journal_balanced`, `no_overdraft`, `no_overcredit`,
  `posting_balanced`, `replay_is_noop`, `only_owner_or_operator_debits`,
  `Batch.batch_serial`, `Storage.db_inv`, `Models.isolation`,
  `Models.noninterference`, `Models.keyed_replay`, `Models.ledger_isolation`.
  `h5i app check crates/payment-ledger`: 145 theorems in 13 modules on the
  standard axioms only; no `sorry`.
- Mutants (`h5i app mutate crates/payment-ledger`): 5/5 caught
  (self transfer, overdraft, anyone may debit, short credit side, replay
  ignoring the amount).
- Tests (`H5I_APP_TEST_DATABASE_URL=postgres://h5i:h5i@127.0.0.1:55432/h5i`):
  - concurrency: 16 tasks × 40 random transfers with SERIALIZABLE retries
    (1119 retries in one run) and with the tenant lock; concurrent duplicates
    commit once; concurrent linked batches are atomic; PostgreSQL agrees with
    the in-memory reference engine on 300 random commands.
  - fault injection: backend killed mid-transaction (retried once), killed
    before COMMIT (`CommitUnknown`, nothing half-applied, client retry posts
    once), same with an idempotency key (engine retries itself), persistent
    failure (retries exhausted, ledger untouched), unreachable database (503).
  - integration: the ledger over HTTP, idempotency keys, tenant isolation.
- `cargo build`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `lake build` pass.

## M2 — Payment Orchestration: done

- `crates/payment-domain`: payment kernel (statuses CREATED → PROCESSING →
  AUTHORIZED → CAPTURED, CANCELED, FAILED; refunds reserved against the
  capture; PSP references; webhook events deduplicated by id and tolerant of
  reordering) and the stateless batch policy rules (`policy::check`).
- `crates/payment-connectors`: `Psp` trait; `MockPsp` (idempotency keys,
  manual/automatic capture, partial capture, refunds, Stripe-shaped signed
  webhooks, settlement report, fault injection: fail before effect, lose the
  response, pending payments, tampered settlement); `StripePsp` (test keys
  only, live keys refused; PaymentIntents, capture, cancel, refunds, search,
  hosted Checkout; `Stripe-Signature` verification with 300 s tolerance);
  raw card numbers refused before any PSP call.
- `crates/payment-api/src/orchestrator.rs`: create/authorize/capture/cancel/
  refund/checkout, webhooks, recovery (`POST /v1/recover`), ledger postings
  with deterministic transfer ids (capture, refund reserve/settle/release),
  reconciliation inputs (payments vs PSP report, payments vs ledger journal).
- `crates/payment-reconcile`: rules, reference oracle, CPU single-thread
  (M3 adds the other engines' benchmarks).
- Lean (`proofs/payments/{Payments,Policies}`), on the extracted kernel:
  `status_steps_allowed`, `terminal_final`, `captured_final`,
  `refund_safety`, `refunded_le_captured`, `event_dedup`, `reachable_inv`,
  `Policies.check_sound`. `h5i app check crates/payment-domain`: 121
  theorems in 6 modules, standard axioms only. `h5i app mutate
  crates/payment-domain`: 6/6 caught.
- The proofs are now two Lake projects, `proofs/ledger` and
  `proofs/payments`: `h5i app check`'s axiom gate imports every module of a
  project into one Lean environment, and the two extracted kernels both
  declare `h5i_app_sql.*`, so they cannot share one. Both have the same
  `require`s; here `proofs/payments/.lake/packages` is a symlink to the
  ledger's packages and `H5I_LAKE_CACHE=off` (the disk has room for one
  Mathlib); with h5i's default shared cache no symlink is needed.
- Tests: `tests/integration/payments_api.rs` (11 HTTP end-to-end tests with
  the Mock PSP: automatic and manual capture, partial capture and refunds,
  refund limits, declines, cancels, card-number refusal, a pending payment
  completed by 10 concurrent deliveries of one webhook (1 processed, 9
  duplicates), out-of-order webhooks, lost PSP responses recovered for
  payments and refunds, PSP outage, merchant balance too low for a refund,
  reconciliation with tampered settlement under all four CPU engines,
  idempotency key creating one payment); Stripe connector against a local
  fake of the Stripe API (request encoding, declines, refunds, 404s,
  signatures) and a live test-mode round trip that runs only when
  `STRIPE_SECRET_KEY=sk_test_…` is set (no key on this machine, so it was
  skipped).

## M3 — GPU Batch Processing: done

- `crates/payment-reconcile`: four engines with one `Report` — reference
  (`BTreeMap` oracle), CPU single-thread (open addressing), CPU
  multithread (rayon radix partition + per-partition hash join), SIMD
  (AVX2 4-lane probes).
- `crates/payment-batch`: the batch API on h5i-app-gpu's `Backend`;
  policy validation with the verified `policy::check` single-thread,
  rayon, and AVX2 (`policy_simd.rs`); `run_policy` / `run_reconcile`
  with `ExecutionPolicy` (CPU / GPU / Auto with a row threshold, no CPU
  replay after a GPU failure, output length validated); synthetic data.
- `crates/payment-gpu` (feature `cuda`): CUDA C kernels
  (`kernels/batch.cu`: atomic hash-table build, probes, policy check)
  compiled to PTX by nvcc (`compute_75`, JIT on T4/A10G/L4), launched
  through `h5i-app-gpu-backend-nvvm`/`cust` at the pinned Rust-CUDA rev;
  `Gpu` implements `Backend` for both workloads with H2D/kernel/D2H timing.
  `h5i-pay-batch` worker binary (CSV in, JSON report out; Auto selection).
  (Superseded: see "A" under "After M5"; the kernels are now Rust compiled by
  rustc_codegen_nvvm.) libclang (`dnf install clang-libs clang-devel`) is needed for cust.
- `tests/differential`: every CPU engine vs the reference, and with
  `--features gpu` the GPU vs the reference on every row (empty batches,
  block boundaries, hot keys, hash collisions, malformed policies, unsorted
  allowlists, 100k/200k synthetic sets), repeat determinism, partition
  independence, corrupted-output rejection. All pass on the A10G.
- `benchmarks` (`bench all`): 10k/100k/1M/10M rows, p50/p95, throughput,
  H2D/kernel/D2H, peak RSS increase, device memory, $/M rows; results and
  analysis in `docs/benchmarks.md`. Summary: with a warm context the GPU
  wins from ~100k rows (reconciliation 5.2× / 8.6× the 4-thread CPU at
  1M / 10M; policy 3.9× / 3.4×); below ~10k rows, and for one-shot jobs
  under ~2–3M rows (136 ms context + JIT), the CPU wins; policy is
  transfer-bound (kernel 2.55 of 57.6 ms at 10M); SIMD reconciliation is
  slower than scalar on this Zen 2 CPU.

## M4 — AWS Deployment (IaC only): done (files only; nothing applied)

- `infra/terraform`: VPC (public / private / isolated data subnets, NAT,
  VPC endpoints, flow logs), security groups, KMS, Secrets Manager, RDS
  PostgreSQL 17 Multi-AZ, S3 (settlements, batch, Object-Lock audit),
  SQS + DLQ, ECR, ALB + WAF + Route 53, ECS Fargate API (circuit breaker,
  alarm rollback, autoscaling, ADOT sidecar), AWS Batch GPU (g4dn/g5 from
  zero) and Fargate CPU queues, IAM per actor, GitHub OIDC deploy role,
  CloudWatch alarms (ledger invariant, posting refused, reconciliation
  exceptions, 5xx, DLQ, queue lag, RDS), AWS Backup + restore testing.
  `terraform fmt -check` and `terraform validate` pass (Terraform 1.9.8,
  providers downloaded to /tmp and removed; no credentials, no apply).
- `infra/docker`: API and worker Dockerfiles, Batch entrypoint, compose.
  Not built locally (disk); CI builds them.
- `.github/workflows/ci.yml`: Rust (PostgreSQL service), CUDA build, GPU
  runner tests (opt-in), proofs (extract --check, check, mutate),
  Terraform checks, ECR images via OIDC, deploy to `test` with approval.
- `payment-api` reads RDS-managed credentials (`DB_HOST`/`DB_NAME`/
  `DB_CREDENTIALS`); `h5i-pay-batch` prints a JSON summary line for the
  reconciliation-exceptions metric filter.
- Known gap at the time: h5i-app-pg connected without TLS, so
  `rds.force_ssl = 0`. Closed after M5 (SEC-01, below).

## M5 — Production Hardening (docs): done

- `docs/failure-recovery.md`: each failure (database connection lost
  mid-transaction or before COMMIT, races, PSP timeouts and outages, lost
  refund responses, duplicate/reordered/forged webhooks, crash between
  capture and ledger posting, settlement mismatch, GPU failure, bad
  deploy, database loss), what keeps money correct, and the test or proof
  that shows it.
- `docs/security-review.md`: threat model and findings SEC-01..07.
- `docs/compliance.md`: PCI DSS scope (no card data handled: tokens and
  hosted Checkout only), data retention, audit trail, what is still needed
  before real funds.
- `docs/runbooks/`: ledger imbalance, reconciliation exceptions, PSP
  incidents, webhooks, backup/restore, deploy/rollback, batch jobs, secret
  rotation.
- `docs/proof-assumptions.md`: trusted base, gate results, the mutant
  table (first failing proof per mutant, read from the run records), and
  the limits of the GPU and reconciliation evidence.
- README updated with the full layout and commands.

## After M5 (newest first)

### G5: the last serialization retries removed, throughput kept

- Cause, from PostgreSQL's log with every statement (32 clients, 10k
  payments, G4 build): of 99 aborted transactions, 43 were `MarkPosted`
  and 41 `Create`, the rest `AttachRef` and in-flight upserts. `MarkPosted`
  flips `ledger_done`, which the partial index `payments_open … WHERE NOT
  ledger_done` names, so it was a non-HOT update: a new tuple and new
  entries in every index of `payments`, including the primary key's
  rightmost page, which each concurrent `Create` inserts into and
  `MarkPosted` reads (the payment by key). Each read what the other wrote.
- Change (shell only; no kernel, proof or marker change): `ledger_done` is
  in no index any more, so `MarkPosted` is a HOT update like the other
  status changes. The unmarked payments are kept in a table of their own,
  `open_payments (tenant_id, id)`, written only by two triggers on
  `payments` (`AFTER INSERT … WHEN (NOT NEW.ledger_done)` and `AFTER
  UPDATE … WHEN (OLD.ledger_done IS DISTINCT FROM NEW.ledger_done)`:
  insert when unmarked, delete when marked). No kernel command reads it,
  so the `Create` (insert there) and `MarkPosted` (delete there) pair has
  no read/write cycle. `recovery_work` joins it with `payments` (and
  re-checks `NOT ledger_done`); still O(unfinished). Migration: the table,
  the triggers and the backfill from `payments` run in the install
  transaction, triggers first (`CREATE TRIGGER` blocks writes to
  `payments` until commit), then `ANALYZE` (without it the planner kept
  using the old index's shape). `payments_open` is dropped.
- The marker means what it meant (F2): `ledger_done` is set by
  `MarkPosted` in the same transaction as before, only the index that
  finds the unmarked payments moved. `open_payments` holds exactly the
  payments with `ledger_done = false`; that rests on the triggers (not
  proven; checked in `payments_load_for_matches_the_whole_tenant`, which
  compares the table with the kernel's snapshot every 50 random commands,
  after marking every settled payment, and after a refund clears a mark).
- Measured: `benchmarks/src/bin/e2e.rs`, fresh seeds made by the G4 build
  (`e2e_g4_10000`, `e2e_g4_100000`), G4 and G5 builds interleaved, order
  alternating, 4 rounds; the host was shared with another session's
  CPU-bound job (load 4–5 on 4 cores). Payments/s median [range] and
  serialization retries per run (200 payments at 8 clients, 800 at 32):

| tenant | clients | G4 payments/s | G4 retries | G5 payments/s | G5 retries |
|---|---:|---:|---|---:|---|
| 10k | 8 | 122.8 [112.5–124.9] | 17, 30, 17, 23 | 117.8 [115.8–123.5] | 0, 1, 0, 0 |
| 10k | 32 | 117.1 [115.2–120.5] | 161, 145, 138, 150 | 116.4 [115.6–118.6] | 1, 1, 2, 1 |
| 100k | 8 | 121.9 [118.3–123.8] | 33, 11, 23, 23 | 123.5 [120.9–125.3] | 0, 0, 0, 0 |
| 100k | 32 | 114.1 [113.5–116.4] | 140, 132, 165, 142 | 117.6 [115.9–117.8] | 2, 1, 0, 1 |

  Throughput is the same within the spread (−4% to +3% by median), and
  kernel transactions per payment fell from 8.10–8.18 to 8.00 (the
  minimum: no retried attempt). Updates of `payments` that were HOT: 99%
  (4172 of 4212 at 100k). The few left are `AttachRef` (it reads and
  inserts `psp_refs` by key; F4's tenant key only queues `AttachRef`s
  among themselves). Recovery: first pass (5 payments) 82–104 ms vs
  78–85 ms; the empty pass 10–28 ms vs 7 ms (the query itself is 0.3 ms
  in `EXPLAIN ANALYZE` at 100k; not explained further).
  On the older seeds (made before G1; they migrate at every startup) a
  first series of 6 rounds was noisier: of G5's 24 runs, 17 had 0–4
  retries and 7 had 8–57 (G4: 18–194 in every run), which I did not
  trace further; the table above uses fresh seeds.
- The harness now reports how many updates of `payments` were HOT
  (`payments_updates`). Raw results:
  `benchmarks/results/e2e-G5-markposted-g5.xlarge.json`.

### G4: the Transfers journal paged

- Ledger kernel: `Command::Transfers{account, from, limit}` returns one
  page of an account's journal, the transfers named by its G1 history
  entries `from..from+limit` (in order), and `next` while the account's
  `changes` counter is beyond the page. `Command::AccountRange{from,
  limit}` lists accounts by id, one page. `limit` is 1 to `MAX_PAGE` (64);
  anything else is `BadPage` before reading. Both are operator-only, like
  `History`. The read plans name one account row plus its history rows
  `(account, from..end)` plus the transfers those name
  (`HISTORY_TRANSFER`), or one id range of accounts. Frame theorems
  (`journal_page_congr`, `account_range_congr`, with `transfers_of_spec`,
  `id_range_spec`): the planned rows give the whole-tenant reply. New
  gated theorem `payment_ledger.Theorems.operator_reads`: only operators
  run `Transfers`, `AccountRange` and `History`; they write nothing
  (`journal_page_writes`, `account_range_writes`).
- Transfers from before G1 had no history entries. A one-off backfill
  (`h5i_pay_migrations` row `g4-journal-history`, in one transaction)
  gives every such transfer an entry on both legs: command
  `CMD_MIGRATED` (3), time 0, the leg's amount, actor the transfer's
  operator or else the debit account's owner; `changes` is set to the
  entry count. The entries are new rows only; the append-only triggers
  stay on.
- Callers moved to paged reads (`payment_storage::ledger::{journal,
  accounts_of, all_pages, audit_paged}`):
  - the HTTP route: `{"cmd":"transfers"}` now requires `account` (400
    without it), takes `from`/`limit` and returns `next`;
  - the audit (`GET /v1/ledger/audit`, operator): walks the accounts by
    id in pages and each account's journal in pages up to the `changes`
    it saw on the account row, so each account's balance is checked
    against exactly the entries it had then. A transfer seen on one leg
    only was committed after the other account's page was read; the
    audit reads that account's journal past its bound to find it and
    counts it `in_flight`. The reply reports accounts, transfers, pages
    and in-flight transfers.
  - the orchestrator: `system_postings(tenant, codes)` reads the system
    accounts' journals (PSP clearing, refund holding) for `find_work`
    and `reconcile_inputs` instead of the ledger snapshot.
  The export reads no journal. No caller takes a ledger snapshot any more
  except the benchmark report's row count (`benchmarks/src/bin/e2e.rs`).
  The payments snapshot in `reconcile_inputs` and in the export is G6.
- Tests: `ledger_api` (paged journal, `next`, limit 65 refused,
  users refused, accounts by id, the audit's counts),
  `transfers_from_before_the_history_are_backfilled` (a scratch database
  with transfers written as before G1, the marker removed, the migration
  run: journal pages and the audit agree), `run_random_transfers` now
  runs the paged audit continuously while transfers commit (always
  balanced), and `paged_audit_across_account_pages` (`MAX_PAGE`+16
  wallets, transfers across the two account pages during the audit, until
  an audit sees a transfer in flight: it does, and still balances).
- Read-only transactions: with the paged audit running next to 16
  writers, the concurrency suite ran PostgreSQL out of predicate locks
  ("out of shared memory", `max_pred_locks_per_transaction` 64): every
  page was a SERIALIZABLE read-write transaction holding SIRead locks
  (28k at the peak). The ledger now declares `Account`, `Accounts`,
  `Transfers`, `AccountRange` and `History` read-only
  (`payment_ledger::read_only`, `LedgerApp::read_only`), so h5i-app-pg
  runs them `SERIALIZABLE READ ONLY DEFERRABLE`: no predicate locks, no
  aborts caused for writers, never retried for serialization. h5i-app's
  obligation `H5iAppLib.ReadOnly.Sound` is proven and gated
  (`payment_ledger.ReadOnly.sound`, from `reads_nothing`); `h5i app lint`
  ok. The suite then passes 8/8 (twice in a row).
- Proofs: ledger gate 362 theorems in 15 modules; mutants 19/19 caught
  (18 in the full run, then `transfer_declared_read_only` alone), the new
  ones `journal_plan_wrong_column`, `account_range_past_end`,
  `journal_open_to_users`, `transfer_declared_read_only`. Tests: 101
  passed in the workspace.

### G3: scoped tokens, an OIDC identity provider, revocation

- Token format, versioned (`payment_api::auth`): the API now issues v2
  HMAC tokens `v2.<payload>.<mac>` (payload base64url JSON with tenant,
  kind `u`/`o`, id, `iat`, `exp`, a random 128-bit `jti` and the scopes;
  HMAC-SHA256 with the user or the operator key by kind, checked in
  constant time against current and previous keys). h5i-app's v1 tokens
  still verify (users with the user scopes, operators with all scopes, no
  `jti`, issue time 0). h5i-app itself is unchanged: the new format lives
  here, next to it.
- Scopes: `payments:read`, `payments:write`, `refunds:write`,
  `ledger:read`, `ledger:write` (the only ones a user may hold),
  `reconcile:read`, `recover`, `tokens:revoke`; `operator:*` is all. Every
  route authenticates with `routes::Authed` (no route uses h5i-app's
  extractor any more) and checks its scope before calling a kernel (ledger
  commands: `open`/`transfer`/`batch` need `ledger:write`, the reads
  `ledger:read`); a missing scope is 403 `insufficient_scope`. CLI:
  `H5I_APP_ISSUE=<tenant>:<user>[:<ttl>[:<scopes>]]`,
  `H5I_PAY_ISSUE_OPERATOR=<tenant>:<id>[:<ttl>[:<scopes>]]`.
- Identity provider (`payment_api::oidc`): with `H5I_PAY_OIDC_ISSUER` and
  `H5I_PAY_OIDC_AUDIENCE`, RS256/ES256 access tokens of that issuer are
  accepted. The JWKS comes from discovery (or `H5I_PAY_OIDC_JWKS_URL`), is
  cached ten minutes and re-read for an unknown `kid` (at most every 30 s).
  Checked: algorithm (RS256/ES256 only, key type must match; `none` and
  HS256 refused), signature (ring), `iss`, `aud` (or `client_id` without
  `aud`, as Cognito access tokens have), `exp`, `nbf`, `iat`, `jti`; claims
  mapped to tenant (`h5i_tenant`), operator (`h5i_operator`, 1 to 2^62 − 1,
  at most 12 h) or user (`h5i_user`), scopes from `scope`/`scp` with a
  configurable prefix removed (users keep only user scopes). Verified with
  ring directly (no JWT crate: the ones on crates.io either pull the `rsa`
  crate, with an open advisory, or aws-lc).
- Revocation (`payment_api::revocation`, table `ops.revocations`): by token
  (`jti`), by actor (its tokens issued at or before a time, default now)
  and by date (all of a tenant's tokens issued then or earlier); v1 tokens
  count as issued at 0. Routes `POST /v1/tokens/revoke` and
  `GET /v1/tokens/revocations` (operators with `tokens:revoke`, own tenant
  only; the revoker is recorded). Each process caches the whole list and
  re-reads new rows every 2 s (`H5I_PAY_REVOCATION_REFRESH_MS`); the
  recording process applies it at once. Stated staleness: within the
  refresh interval plus one query on every server; a server that cannot
  read the list for five intervals (10 s) refuses every token (503).
- Terraform: variables `oidc_issuer_url`, `oidc_audience`,
  `oidc_scope_prefix`, `create_cognito_user_pool` (user pool without
  self-sign-up, MFA on, a resource server with the API's scopes, an
  operators' client with one-hour access tokens and revocation on) and
  `cognito_pre_token_lambda_arn` (the V2_0 trigger that must add the
  tenant and actor claims; not in this repository). `terraform validate`
  ok; checkov 400 passed, 0 failed, 44 skipped.
- Tests (98 now): auth unit tests (v2 round trip, scopes, forged scopes or
  kind refused, v1 compatibility), OIDC claim and JWKS unit tests,
  revocation-list unit tests, and `tests/integration/tokens.rs`:
  `every_route_checks_its_scope` (read-only merchant, ledger read vs
  write, refunds, a reconciliation-only operator, v1 tokens),
  `revocation_by_token_by_actor_and_by_date` (with a second API process
  that picks the revocation up within its refresh), and
  `access_tokens_from_an_oidc_issuer` (a local issuer with an RSA key from
  `openssl` and a P-256 key from ring: operator and merchant tokens and
  their scopes; wrong issuer, audience, expired, not yet valid, unknown
  key, changed payload, HS256 with the public key as secret, `none`
  refused; key rotation picked up; IdP tokens revoked by `jti` and by
  actor).
- Docs: `docs/security-review.md` (SEC-02 closed, what remains about v1
  tokens and shared HMAC keys), `docs/runbooks/secret-rotation.md`
  (revocation, IdP keys), README.

### G2: idempotency keys per principal

- `ReplyCodec::scope` (both kernels) is now the whole principal: `u<id>`
  for a user, `o<id>` for an operator, system operators included
  (`payment_storage::idempotency::scope`). Before, every operator used
  `u0`, so two operators (or an operator and the orchestrator) with the
  same key got each other's replies or a conflict.
- System jobs: the orchestrator opens the system's ledger accounts
  (`system-account-…` keys) always as `ORCHESTRATOR_OPERATOR`, also when it
  acts for the recovery job or a calling operator (`acting_as`), so the key
  is found whichever process opens the account first.
- Migration, at startup with the schema: `u0/system-…` keys are renamed to
  the orchestrator's scope (once; a key already there wins). Other `u0/…`
  keys were made by operators through the API and cannot be attributed to
  one: they stay, are never written again (no principal has scope `u0`;
  user ids start at 1), and an operator's keyed request on
  `/v1/ledger` or `/v1/payments/rpc` looks its key up there first
  (`routes::legacy_key`, read-only): the same command replays the stored
  reply, another is refused (422), as before the change.
- Tests (`tests/integration/idempotency.rs`, 89 tests in all now):
  `keys_are_per_principal` (operator 3 replays its own reply, operator 4
  with the same key and command opens its own account, a user's keys are
  separate; the stored keys are `o3/…`, `o4/…`, `u7/…`) and
  `keys_from_before_are_migrated_or_still_replay` (pre-G2 rows written by
  hand; after startup the system key is under `o<orchestrator>/`, the
  orchestrator replays its old reply instead of opening a second account,
  an operator's old key replays over HTTP or is refused for another
  command, users are unaffected, nothing new is written under `u0`).
- Kernels unchanged (`h5i app extract --check` up to date); clippy plain,
  mock-psp and CUDA clean.

### G1: append-only operator audit history, written by the kernels

- Both kernels have a `history` table (`HistoryEntry`, key `(payment, n)`
  or `(account, n)`) and a per-row count `changes` on `payments` and
  `accounts` (the next entry's `n`). Each entry says who (`by_operator`,
  `actor`: the user's or operator's id), when (`at`: the transaction's
  clock), what (payments: command, PSP result, webhook event, refund;
  ledger: open or post, transfer id and code) and the status and amounts
  (payments) or debits and credits (ledger) before and after.
- The clock: `Principal` has a field `at`; the engine stamps it before
  every attempt (`Kernel::stamp`) and `payment_storage::engines_with` now
  runs both engines on `Clock::Database` (`transaction_timestamp()`), so
  every API server of a database records the same clock.
- Payments: `transition` is now `decide_command` (what every command
  decided before) followed, when the write set is not empty, by `logged`:
  the decision's writes without its payment row, then the payment row it
  wrote (or the row as it was, for a stale webhook or a PSP reference)
  with `changes + 1`, the caller as `operator` and the ledger mark kept
  only by `MarkPosted`, then entry `n` = the old `changes` (0 for a new
  payment). `logged` checks that the row is the command's payment
  (`Corrupt` otherwise; unreachable). A stale webhook and `AttachRef` now
  also write the payment row (to count the entry), clearing its ledger
  mark; recovery re-marks such a payment (its postings are replays).
- Ledger: `open` and `post` write each account row followed by its entry
  (`[account, entry, account, entry, transfer]` per transfer), so a batch
  that touches an account twice numbers its two entries in order.
- Read: `History { payment | account, from, limit }`, operators only,
  paged like F3 (at most 64; `next` from `changes`), with read plans and
  frame theorems; HTTP `{"cmd":"history", …}` on `/v1/payments/rpc` and
  `/v1/ledger`.
- Database: triggers on both `history` tables refuse `UPDATE`, `DELETE`
  and `TRUNCATE` from any role (`payment_storage::append_only_history`).
  Existing payments and accounts get `changes = 0`; their history starts
  with their next change.
- Proven (`h5i app check`; ledger 317 theorems in 14 modules, payments 405
  in 9; standard axioms only):
  - payments `every_change_logged`: a successful command writes nothing,
    or exactly one payment row and exactly one history entry, entry `n` of
    that payment leaving it with `n + 1`, naming the acting principal
    (`User u` / `Operator o` and the id) and `a.at`, with the row's status
    and amounts after the change; `history_appended`: in every reachable
    state the entry is appended under a key no earlier entry has (every
    earlier entry stays, unchanged and in order), and its "before" fields
    are the row as it was (`NO_STATUS` and `n = 0` for a new payment);
  - ledger `every_change_logged` (inductive `Logged`: every account row in
    the write set is immediately followed by exactly its entry, no other
    history row, so as many entries as account writes) and
    `history_appended` (the history after = before ++ the command's
    entries, keys unique), batches included through `Batch.Serial`;
  - the earlier theorems restated over the wrapper (payments: `effect`,
    `writes_record_operator`, `marked_only_as_seen` now with
    `changes = p.changes + 1`, `changes_clear_ledger_done`, `event_dedup`,
    `operator_only`; frame theorems for `transition`, `logged` included).
- Mutants (`h5i app mutate`): payments 19/19 caught, ledger 15/15. New in
  both kernels: `change_without_entry` (payments: `logged` drops the entry;
  ledger: opening an account writes no entry), `history_wrong_actor` (a
  user recorded as 0), `history_entry_overwritten` (the count not bumped,
  so the next change reuses the key). `mark_plan_misses_refunds` was
  re-anchored (the `History` plan now follows the `MarkPosted` plan).
- Tests: kernel unit tests for both histories; `ledger_over_http` reads an
  account's history (database clock within ten minutes of the host's);
  `payments_api::every_change_is_in_the_history` reads a payment's history
  (entries numbered 0…, the creating user, the orchestrator's capture)
  and checks that PostgreSQL refuses `UPDATE`, `DELETE` and `TRUNCATE`
  on both history tables; the PostgreSQL-vs-memory differential test
  compares the histories (clock aside).

### After F: full checks and the B4 end-to-end runs

- Checks at `b87ef95`: `h5i app extract --check` and `h5i app check` on
  all three kernels (payments 293 theorems in 8 modules, ledger 257 in 13,
  reconciliation 64 in 2; axioms `propext`, `Classical.choice`,
  `Quot.sound`); clippy plain, `mock-psp` and CUDA with `-D warnings`;
  84 tests; GPU differential test (6) on the A10G; `build-device.sh
  --check`. Mutants last run per kernel: payments 16/16 (F4), ledger 12/12
  (F3, unchanged since), reconciliation 6/6 (F5).
- B4 re-run (`docs/benchmarks.md`, "After F"): a payment takes 17–22 ms
  and 8 kernel transactions at 1k, 10k and 100k payments. Recovery with
  nothing to fix takes 6.5–8.4 ms at every size (was 10 / 78 / 852 ms).
  Serialization retries with 8 clients are 18–29 per 200 payments (were
  88–132). Throughput was 141–163 payments/s on a host loaded by another
  project; the F4 harness, run interleaved, gave the same.

### F5: the single-thread hash join proven against the reference

- `crates/payment-reconcile/src/join.rs`: the hash join in the Rust subset
  Aeneas extracts (`Vec`, index loops, `vec!`, no iterators): one
  open-addressing table per side (`JoinTable { keys, count, first }`,
  capacity a power of two, at least 16 and twice the rows), linear probing
  from `rules::hash(k) & (cap − 1)`, the same start as `Table`,
  `find_slot` and the GPU tables; per key the row count and the lowest row
  index. `cpu_single` is `join::join`; `cpu_parallel` now runs `join::join`
  on each partition (the partition's rows copied in index order, local
  indices mapped back), so it no longer uses `Table`.
- `proofs/reconcile/Reconcile/Join.lean` (no `sorry`; axioms `propext`,
  `Classical.choice`, `Quot.sound`):
  - `probe_spec`: a probe returns the key's slot, or an empty slot reached
    through full slots when no slot holds the key;
  - `insert_spec` / `build_spec`: the table invariant `TInv` (distinct keys,
    each slot's count = rows with its key, first = lowest such row, every
    row's key has a slot, probe paths full, an empty slot left);
  - `lookup_spec`: `(count, first)` = `(|rows with k|, index of the first
    such row or NONE)`;
  - `join_eq_reference` / `join_rows`: every internal row gets `rowCode`
    and `matchIdx` (the first external index), every external row
    `extCode`: the reference's per-row answer.
  Preconditions are `validate`'s (no `EMPTY` key, fewer than `u32::MAX`
  rows a side) and `4n + 32 ≤ usize::MAX`. The proof is independent of the
  start slot. `h5i app check`: 64 theorems in 2 modules (was 15 in 1).
- Mutants (`h5i app mutate crates/payment-reconcile`): 6/6 caught, the
  three earlier ones plus `duplicate_count_reset` (a duplicate resets the
  count to 1), `first_row_overwritten` (a duplicate replaces the first
  row) and `probe_ignores_empty` (the probe does not stop at an empty
  slot).
- Not covered (differential-tested, `docs/proof-assumptions.md`): the radix
  partition and index mapping of `cpu_parallel`, the SIMD engine's `Table`
  and vector probe, the GPU's atomic build `recon_build` and its probe
  `find_slot`. The GPU build implements the proven sequential algorithm
  (same start, probe order, per-key count and lowest index via
  `atomicAdd`/`atomicMin`); its concurrent interleavings are not proven.
- Start slot. The proof holds for any start below the capacity, so
  `k % cap` (considered to simplify the proof) was measured against the
  kept start, `rules::hash(k) & (cap − 1)`, with the final code (p50 ms,
  three runs; the multiplicative start `k·0x9e3779b97f4a7c15 % cap` for
  comparison):

| engine, rows | hash & mask (kept) | `k % cap` | multiplicative % |
|---|---:|---:|---:|
| single 1M | 326–328 | 201–202 | 279–282 |
| single 10M | 3874–3890 | 2557–2581 | 3583–3608 |
| parallel 1M | 71–74 | 72–76 | 81–84 |
| parallel 10M | 2016–2024 | 1573–1593 | 1855–1867 |

  `k % cap` is 34–38% faster single-threaded on the benchmark's keys,
  because ids with an odd stride map to distinct slots in order (no
  collisions, sequential memory access). It falls apart on other key
  patterns: with ids strided by 4096 (200k rows a side, `cpu_single`) it
  took 976 ms against 38.5 ms for the hash start (25 times slower), because
  all keys share their low bits and probe through one run. The hash start
  costs the same 38–40 ms for strides 1, 7 and 4096. It is kept: the
  external side is PSP data whose id pattern we do not control. Without
  `#[inline(always)]` on `probe`/`lookup`/`insert` and with element-wise
  pushes instead of `vec!`, the extracted code was 25% slower than `Table`
  at 10M; with them it matches it.
- Measured (`bench one recon <engine> <rows>`, p50 ms, three runs each,
  same host, another project's Lean build running at the same time; before =
  `b55fbff`'s `Table` engines):

| rows | single before | single after | parallel before | parallel after |
|---:|---:|---:|---:|---:|
| 10k | 1.21–1.26 | 0.93–1.25 | 0.40–0.48 | 0.42–0.46 |
| 100k | 14.7–15.1 | 14.3–14.4 | 3.7–3.9 | 3.9–4.0 |
| 1M | 320–322 | 322–325 | 94–97 | 59–63 |
| 10M | 4087–4123 | 3827–3907 | 2286–3004 | 2024–2051 |

  The single engine is unchanged within noise up to 1M and 5% faster at
  10M. The parallel engine is 35% faster at 1M and 11% at 10M (the copied
  partition rows are contiguous). Its peak RSS at 10M rose from +319 to
  +387 MiB, because of those copies.
- GPU differential test (6 tests, A10G) and `build-device.sh --check` pass
  (the device crate is unchanged: it compiles `rules.rs`, not `join.rs`).

### F4: serialization retries found and reduced

- Found from PostgreSQL's log (`could not serialize access … pivot`, with
  the statement): every retry was in the payments schema (upserts of
  `payments` and `psp_refs`, and `COMMIT`); none in the ledger, whose
  per-account advisory locks already queue the postings. Cause: SERIALIZABLE
  tracks key reads per B-tree page, recent payments share a page of the
  primary-key index, and every status change was a non-HOT update (the
  status was in the predicates of the partial indexes `payments_unfinished`
  and `payments_unposted`), so it added an index entry to the page every
  concurrent command on a neighbouring payment had read; two such commands
  each read what the other wrote, and one was aborted. Same for
  `AttachRef` (reads `psp_refs` by key, inserts there).
- Changes (shell only, correctness still from SERIALIZABLE):
  - the partial indexes no longer mention the status:
    `payments_open (tenant_id, id) WHERE NOT ledger_done` and
    `payments_refunding … WHERE refund_pending > 0` replace them, and
    `payments` gets `fillfactor = 70`, so `BeginAuthorize` and `Record`
    (status changes) are HOT updates that add no index entry;
  - so that the open index does not keep failed and canceled payments
    forever, `MarkPosted` also marks a `FAILED` or `CANCELED` payment
    (nothing to post; kernel change, re-proven: `marked_only_as_seen` now
    says "settled": captured, failed or canceled), and the orchestrator
    marks them after recording a decline or cancel; recovery's ledger part
    selects settled, unmarked payments;
  - `AttachRef` takes a tenant-wide advisory key (`psp-refs`).
- Measured (same harness as B4, quiet host, one run each; payments/s and
  serialization retries per run of 25 payments per client):

| tenant | clients | before (F3) | after | payments per run |
|---|---:|---:|---:|---:|
| 10k | 8 | 168.8 / 225 | 183.6 / 29 | 200 |
| 10k | 32 | 166.7 / 1416 | 181.8 / 124 | 800 |
| 100k | 8 | 175.7 / 227 | 197.5 / 18 | 200 |
| 100k | 32 | 167.1 / 1335 | 181.4 / 109 | 800 |

  Retries fell by 8 to 12 times and throughput rose 7–12%. The rest are
  `MarkPosted` (still a non-HOT update: it flips the indexed `ledger_done`)
  against `Create` (inserts into the same page). Serializing those two with
  the counter key removed them too (0 and 1 retries at 10k) but cost 20% of
  the throughput (142 instead of 179 payments/s), so it is not done.
- Payments mutants 16/16 caught (`h5i app mutate`, run 1793564254); `h5i app check` 293 theorems; tests and clippy (plain, mock-psp, CUDA) pass.

### F3: cursor pagination; no whole-tenant loads for batches

- Payments `List { merchant, from, limit }` and ledger
  `Accounts { owner, from, limit }`: a page of one merchant's payments (one
  owner's accounts) numbered `from` to `from + limit − 1`, `limit` 1 to
  `MAX_PAGE` = 64 (else `BadPage`, HTTP 400); users list only their own
  (`Forbidden`); the reply carries `next` (the next page's `from`, `null`
  on the last page). The cursor is the key `(merchant, seq)` /
  `(owner, seq)`: new fields `Payment.seq` and `Account.seq` number rows per
  merchant/owner from new counter tables (`merchant_counters`,
  `owner_counters`) that `Create`/`Open` read and bump.
- Why sequence numbers: h5i-app 0.5.2's read predicates are `=`, `IN`
  (subquery), `AND`, `OR` and windows (offset/limit) — no `>` — so a
  keyset page by id is not expressible as a plan with a frame theorem, and
  an offset window cannot give the same reply on the page as on the whole
  tenant. A page's plan is `merchant = m AND (seq = from OR … OR seq =
  from+limit−1)` plus the merchant's counter. PostgreSQL's planning time for
  such `OR` chains grows quickly (measured on 10k payments: 7 ms at 64
  terms, 89 ms at 256, 650 ms at 1000), hence the maximum of 64. A `>`
  predicate in h5i-app would allow larger pages; not done (it is a change to
  h5i-app's query language and its Lean semantics).
- Batches: `MAX_BATCH` is now 64 (was 8192), equal to the planned size, so
  every accepted batch reads only its accounts and transfer ids and locks
  only their keys; longer batches are refused (`batch_too_large`, 413)
  without reading. `Lock::Tenant` is gone. The orchestrator already split
  its postings at 64.
- Migration: at startup, a table without `seq` gets it, numbered by id per
  merchant/owner, and the counters are filled, once (guarded by the
  column's nullability); measured 360 ms on a copy of the 10k-payment
  database, 125 ms for later startups. Column additions now run only when
  the column is missing (`ALTER TABLE` takes an exclusive lock even as a
  no-op; concurrent installers deadlocked on it in the tests).
- Proofs: ledger 257 theorems / 13 modules, payments 293 / 8. `reads_abs`
  covers the paged plans (`seq_range` proven to build the `OR` chain of
  exactly the range, `rangeA`), and `frame`/`load_for_sound` the pages and
  the counter reads of `Create`/`Open`; the refused pages and batches read
  nothing. Every theorem about `Open`/`Create` was re-proven with the
  counter write; the owner/merchant counters are part of the specs' state.
- Mutants: ledger 12/12 caught, payments 16/16 (new `page_plan_skips_one`
  in both kernels, `page_lists_other_owner`, `list_other_merchant`).
- Clients: the orchestrator, recovery and export never listed; tests and
  the HTTP routes (`POST /v1/ledger {"cmd":"accounts", owner?, from?, limit?}`,
  `POST /v1/payments/rpc {"cmd":"list", merchant?, from?, limit?}`) use the
  pages. Stored idempotent replies from before still decode. The journal
  read `Transfers` (operators) is still unpaged.
- Tests: 84 pass; the PG differential tests send pages (own, others',
  refused) and batches over 64.

### F2: per-payment "ledger complete" marker; recovery O(unfinished)

- Payment field `ledger_done` (column `BOOLEAN NOT NULL DEFAULT false`,
  added to existing tables at startup; old rows start unmarked and the first
  recovery pass syncs and marks them). New operator command
  `MarkPosted { payment, captured, refunds }`: marks the payment only if it
  is `CAPTURED` for exactly `captured` and its refund rows are exactly
  `refunds` (both directions, by id: same payment, amount, status);
  otherwise `Stale`, nothing written. Every other write of a payment row
  sets `ledger_done = false`.
- `Orchestrator::sync_ledger` sends `MarkPosted` with the `View` it synced
  from after every batch was accepted; it skips marked payments (recovery
  forces). A refund refused for the merchant's balance is now recorded and
  synced, so the payment is marked again.
- `recovery_work`'s ledger part is `status = CAPTURED AND NOT ledger_done`
  on the partial index `payments_unposted` (the index on every captured
  payment is dropped). `POST /v1/recover` (`find_work`) also still checks
  the journal (`needs_ledger_sync`), so postings lost after marking are
  found there and by reconciliation, not by the scheduled pass.
- Proven (payments 271 theorems / 8 modules): `marked_only_as_seen` (only
  an operator's `MarkPosted` writes a marked row, and only for the captured
  amount and exactly the current refund rows it names) and
  `changes_clear_ledger_done` (every other command writes each payment row
  unmarked, and writes a refund row only with its payment's row). What the
  marker guarantees, precisely: a marked payment's captured amount and
  refund rows are those the orchestrator confirmed in the ledger; that the
  ledger then holds them is the orchestrator's part
  (`docs/proof-assumptions.md` item 7). Frame theorem extended to the new
  command (plan: the payment and its refunds).
- Mutants: 14/14 caught (new `mark_ignores_refunds`, `refund_keeps_mark`,
  `mark_plan_misses_refunds`; two older plan mutants re-anchored because
  their text became ambiguous).
- Measured at 100k payments (same harness as B4, one run, host otherwise
  idle): recovery pass with nothing to fix 852 → 7.8 ms, with 5 payments to
  fix 1,002 → 69.6 ms (51 kernel transactions). A payment now takes 8
  kernel transactions instead of 7 (the mark); `POST /v1/payments` p50
  18.7 ms; 8 clients 145.5 payments/s with 252 serialization retries per 200
  payments (F4 looks at those).
- Tests: 83 pass; the PG differential test of plans now sends `MarkPosted`
  (true, short and wrong refund lists); recovery test checks the mark.

### F1: operators distinct inside the kernels (SEC-02 finished)

- Both kernels' `Principal` is `{org, actor}` with `Actor::User(id)` or
  `Actor::Operator(id)` (`is_operator`, `user_of`, `operator_of`). A user
  is never an operator whatever its id; user ids no longer collide with
  operators. `payment_storage::{ledger,payments}::{user, operator}` build
  them; `payment_api::auth` maps an operator token to `Operator(id)`.
- Audit field `operator` (the acting operator's id, 0 for a user) on ledger
  `transfers` and on `payments`, `refunds`, `psp_events`, `psp_refs`; added
  to existing tables by `ALTER TABLE … ADD COLUMN IF NOT EXISTS … DEFAULT 0`
  at startup (old rows read 0). Shown in the JSON of transfers, payments
  and refunds. Stored idempotent replies from before still decode (test
  `replies_stored_before_the_operator_column_still_decode`); operators keep
  one idempotency namespace per tenant (`u0`).
- System operators (ids from 2^62, refused in tokens): orchestrator
  2^62+1, recovery job 2^62+2, export 2^62+3 (`payment_storage::*_OPERATOR`).
  `Orchestrator::acting_as(id)` shares engines and the account cache;
  `POST /v1/recover` acts as the calling operator.
- Proofs: ledger 236 theorems / 13 modules (was 227), payments 248 / 8
  (was 228). Restated for the new principal: `only_owner_or_operator_debits`
  and `replay_needs_debit_right` (the actor is `Operator _` or
  `User owner`). New: ledger `transfers_record_operator`, payments
  `operator_only` (`Record`, `Event`, `AttachRef`) and
  `writes_record_operator`. The Lean readers `Principal.operator`/`.user`/
  `.opId` in each `Spec.lean` mirror the kernel functions (proven equal).
- Mutants: ledger 10/10 caught (new: `user_acts_as_operator`,
  `operator_id_dropped`), payments 11/11 (same two new).
- Tests: 82 pass (`cargo test --workspace`); new
  `ledger_api::the_journal_names_the_operator`, operator ids asserted in
  `payments_api` (orchestrator; recovery run by operator 1), kernel unit
  tests, auth test for reserved ids. Clippy clean (plain, mock-psp, CUDA).
- Left open (security-review SEC-02): the row's `operator` is the last
  writer, not a history; scoped tokens, an identity provider, revoking one
  operator.


### E (rest): static infrastructure checks; what still needs credentials

- checkov 3.4.0 on `infra/terraform`: 54 findings at first. Fixed: rule
  descriptions on every security-group rule, the VPC's default security
  group closed (`aws_default_security_group`), the GPU launch template's
  IMDS hop limit 2 → 1 (containers cannot reach the instance role), API
  and batch logs kept 365 days, WAF request logging (authorization header
  redacted), Step Functions logging and X-Ray tracing, multipart-upload
  abort on the batch bucket, and the CI deploy role's `UpdateService`
  scoped to the API service. The other 44 are inline
  `#checkov:skip=ID:reason`: for_each resources checkov does not follow
  (S3 versioning, encryption, public-access blocks; the WAF's dynamic
  Log4j rule), APIs without resource ARNs, and decisions (manual token
  rotation runbook, single region, no S3 access logging yet, 30-day flow
  logs). Result: 400 passed, 0 failed, 44 skipped. CI's terraform job runs
  it. tflint was not run (not installed; checkov covers the AWS rules
  that matter here).
- The ops image (`Dockerfile.ops`) was built here (424 MB) and starts;
  the binary and the entrypoint refuse unknown commands and bad run ids.
- Needs AWS or Stripe credentials, not done here: `terraform plan` /
  `apply`; pushing images to ECR; the recovery schedule and the nightly
  Step Functions run on AWS; S3 transfer times; the ECS JSON-key secret
  injection; the Stripe test-mode round trip (`STRIPE_SECRET_KEY`).

### B4: end-to-end measurements; GPU benchmarks re-run after A

- `benchmarks/src/bin/e2e.rs` seeds a tenant through the orchestrator
  (1k / 10k / 100k payments) and measures, on a copy, the API's payment,
  view and refund latency, kernel transactions per request, 8-client
  throughput, a recovery pass with 5 lost PSP answers and one with nothing
  to do, and `POST /v1/reconcile`. Built at `e88c83f` for "before" and at
  `685abb0` for "after". Results and the analysis: `docs/benchmarks.md`
  "End to end"; raw: `benchmarks/results/e2e-g5.xlarge.json`.
- Measured (p50): a payment 52.9 → 17.6 ms at 1k payments, 461 → 15.9 ms
  at 10k, 19.4 ms at 100k (after only); 11 → 7 transactions; `GET` 3 → 1
  transaction (13.2 → 0.83 ms at 1k, 113 → 0.83 ms at 10k); throughput
  with 8 clients 18.3 → 164 payments/s at 1k, 2.2 → 158.5 at 10k; a
  recovery pass 31.8 s → 65 ms at 1k and 29.5 min → 0.62 s at 10k. Not
  measured: "before" at 100k (extrapolated ~4.6 s per payment).
- Worker end to end (CSV in page cache): 10M rows per side 6.7 s (GPU) vs
  9.0 s (CPU), of which ~6.2 s is parsing; at 1M the CPU job is faster, so
  the default `--min-gpu-rows` became 2,000,000. Export: 0.51 s at 100k
  payments. S3 transfer not measured.
- Remaining O(tenant) work found: the recovery query is O(captured
  payments) in SQL (852 ms at 100k with nothing to fix), and
  `POST /v1/reconcile` (583 ms at 100k). Both documented, not changed.
- Synthetic engines re-run after A (`g5.xlarge-nvvm.json`): GPU
  reconciliation as fast as the old CUDA C, the policy kernel 2× slower
  (5.0 vs 2.55 ms at 10M; end to end +7%). The host had 2 of 4 vCPUs busy
  with another workload during this run and the e2e runs; CPU rows of that
  file are not comparable with M3's, which stay in `g5.xlarge.json`.

### E (part): SEC-03, SEC-07, SEC-09, recovery interval

- SEC-03 closed: `payment_ledger::transfer` checks `SameAccount`, the debit
  account and `may_debit` before the journal lookup, so a caller without
  the right to debit gets `Forbidden`/`NotFound` whether or not the id was
  used. `transfer_spec` re-proven; `replay_is_noop` now assumes the
  replaying caller may debit; new `replay_needs_debit_right`. Gate 227
  theorems / 13 modules; mutate 8/8 (new `replay_answers_anyone`, caught
  by `transfer_spec`). Kernel test `replay_does_not_reveal_ids_to_others`.
- SEC-07 closed: ADOT sidecar pinned to `v0.51.0@sha256:955e240b…e07d`.
- SEC-09 closed: `deny.toml` and `cargo-deny-action` in CI; `cargo deny
  check` passes here; RUSTSEC-2026-0173 (unmaintained `proc-macro-error2`,
  build-time, via libcrux-hmac, no fix) ignored with that reason.
- Recovery interval: the variable's default was already `rate(5 minutes)`;
  the `rate(1 hour)` in `recovery.tf` was only the placeholder for a
  disabled schedule. It now reads `rate(5 minutes)` too, so docs and
  Terraform agree.

### C: the batch path connected to the system

- `h5i-pay export <dir>` (`payment_api::batch::export`): for every tenant
  with payments (`payment_storage::tenants_with_payments`, a read-only
  cross-tenant query), `internal/<tenant>.csv` from the payment kernel's
  snapshot (the records `POST /v1/reconcile` compares), `external/<tenant>.csv`
  from the PSP settlement report, and `manifest.json`; a tenant whose PSP
  report fails is listed and skipped (exit 1).
- The worker: `batch-entrypoint.sh reconcile-run <run>` reconciles every
  pair of a run; `h5i-pay-batch` file formats moved to
  `payment_batch::files` (shared with export/ingest and the tests).
- `h5i-pay ingest <run> <dir>`: reports and exceptions into a new `ops`
  schema (`payment_storage::ops`; not kernel state, read by no kernel),
  idempotent per (tenant, run). `GET /v1/reconcile/reports[?run=&limit=]`
  shows a tenant's operators their latest (or a given) report, 403 for
  users, 404 for another tenant. Corrections stay manual ledger transfers.
- AWS (`infra/terraform/reconcile.tf`): ECR repo and image `ops`
  (`Dockerfile.ops`: production `h5i-pay` + AWS CLI, `ops-entrypoint.sh`),
  ECS task definition `ops` (database and PSP secrets, no token keys; task
  role: write `settlements/runs/*`, read `batch/runs/*`), Batch job
  definitions `reconcile-run-{gpu,cpu}`, a Step Functions state machine
  Export → WorkerGpu (Catch → WorkerCpu) → Ingest with the execution name
  as run id, a nightly EventBridge schedule, and an alarm on failed
  executions. CI pushes the ops image. `terraform fmt`/`validate` pass; not
  applied (no AWS credentials).
- Bulk payout screening: `h5i-pay-batch policy <requests.csv> <policy.json>
  <decisions.csv>` runs `run_policy` (the verified `policy::check`; GPU
  through the nvvm PTX, or CPU) and is in the Batch entrypoint. 300k
  requests: GPU, single and multithread decisions byte-identical.
  Measured there: the GPU took 209 ms of which most is the CUDA context and
  JIT that a one-shot job pays, the multithread CPU 2.5 ms. The worker's
  default `--min-gpu-rows` is therefore 2,000,000 (was 200,000).
- `POST /v1/reconcile` stays CPU, by decision (one tenant on demand; the
  API image has no CUDA; a per-request context costs more than the join).
- Test `integration::batch_job`: two tenants, a tampered settlement line;
  export → worker code → ingest (twice: second is a no-op) → the API shows
  tenant a one `amount_mismatch`, tenant b none, 403/404 as above.

### B3: fewer kernel round trips per payment

- Payment kernel: `Command::View` returns the payment, its refunds and its
  PSP reference in one command (`Reply::View`), with its read plan
  (payment by id, refunds by payment, reference by payment), and
  `BeginAuthorize` replies with the payment it moved to `PROCESSING`
  (`Reply::Payment`) instead of only the status. Proofs re-checked:
  `Commands.view_writes`, `Frame.view_congr`, the `View` case of
  `Frame.frame`, `Plan.reads_abs`; gate 228 theorems / 8 modules; mutate
  9/9 (new: a `View` plan missing the reference).
- Orchestrator: `view` is one command (was Get + Refunds + GetRef);
  `authorize` uses BeginAuthorize's reply (was a second Get); `refund` and
  `recover_refunds` read with one `View`; `sync_ledger` reads with one
  `View` and posts the capture and refund legs as linked `Command::Batch`es
  of at most 64 transfers (was one command per object), falling back to
  per-object posts only when a batch with a possibly refused release fails.
  Kernel transactions per automatic-capture `POST /v1/payments` with a
  warm account cache: 7 (measured in B4, with the "before" numbers).

### A: GPU kernels compiled from the Rust the CPU and the proofs use

- Toolchain (A1) works natively on this Amazon Linux 2023 host, no
  container: rust-cuda `3a82c35a`, `nightly-2026-04-02`, the prebuilt LLVM
  7.1 and CUDA 12.6's libNVVM (`cuda-nvvm-12-6` etc. from NVIDIA's rhel9
  repo, beside CUDA 13.2; `/usr/local/cuda` put back on 13.2 with
  `alternatives`). The one Ubuntu-only piece was `libedit.so.2` /
  `libbsd.so.0` for the prebuilt LLVM; Ubuntu 20.04's packages (24.04's need
  glibc 2.38) are extracted to `/var/tmp`, pinned by SHA-256. Details and
  the environment file: `docs/gpu-toolchain.md`.
- `crates/payment-gpu/device` (its own workspace, `no_std`, `cuda_std`)
  includes `payment-domain/src/policy.rs` and `payment-reconcile/src/rules.rs`
  with `#[path]` and exports `policy_check`, `recon_build`,
  `recon_internal`, `recon_external`. `build-device.sh` compiles it to
  `kernels/payment_gpu_device.ptx` (committed, compute_75, 14.5 KB, sha256
  `4cedff02…764f`); `--check` rebuilds and compares (two builds were
  byte-identical). `payment-gpu` embeds that PTX; `kernels/batch.cu`,
  `build.rs` and the nvcc step are deleted (A2).
- Reconciliation (A3): `rules.rs` now also holds the per-row probe
  (`find_slot`, `TableRef`, `internal_row`, `external_row`) that the CPU
  single-thread and multithread engines and the GPU all call;
  `classify_internal` lost its match guard (same semantics) so Aeneas takes
  it. `crates/payment-reconcile/h5i-app.toml` extracts the two classifiers
  into the new Lake project `proofs/reconcile`. Proven
  (`proofs/reconcile/Reconcile.lean`, 15 theorems, standard axioms): the
  extracted functions compute decision tables `specInternal` /
  `specExternal`; codes are in range; over whole inputs (counts and first
  external record of the key, what the reference computes),
  `internal_matched_iff`, `external_matched_iff`, `duplicates_first`,
  `exception_reported_once`, `pair_matched_both_sides`. `h5i app mutate
  crates/payment-reconcile`: 3/3 caught (duplicate external ignored, status
  not compared, extra external reported matched). The hash joins
  themselves (CPU `Table`, GPU `recon_build` atomics) stay a trusted,
  tested shell.
- Differential tests (A4) unchanged and passing on the A10G with the
  Rust-compiled PTX: every CPU engine and the GPU against the reference on
  every row (6 tests). They now check the compiler and the hardware, not a
  transcription. `docs/proof-assumptions.md` has the new trusted base.
- Cost on the CPU: routing the CPU joins through the shared `TableRef`
  probe made them about 8% slower until the probe was `#[inline(always)]`;
  now 2–3% (single) and ~5% (multithread) slower than before at 1M rows
  (paired A/B runs of the old and new `bench one recon`, 1M rows, 11 runs:
  single 309 → 316–319 ms, multithread 89–90 → 95 ms).
- GPU timings with the nvvm PTX (`bench all`, first run, the host was
  otherwise busy so its CPU rows are not comparable): reconciliation 1M
  rows p50 18.8 ms (kernels 4.18 ms; nvcc build 16.9 ms / 4.09 ms), 10M
  206 ms (kernels 49.4 vs 48.2 ms); policy 1M 5.2 ms (kernel 0.54 vs 0.29
  ms), 10M 59.4 ms (kernel 5.0 vs 2.55 ms). So the reconciliation kernels
  are as fast as the CUDA C; the policy kernel is about 2× slower than
  nvcc's, which changes the end-to-end time by a few percent because
  transfers dominate. A clean re-run of all benchmarks is part of B4.
- CI: the proofs job extracts, checks and mutates `crates/payment-reconcile`
  too; a new `device-ptx` job (Ubuntu 24.04, CUDA 12.6 container) builds
  the backend and runs `build-device.sh --check`. Written, not run here.

### B2: recovery selects only unfinished work

- `payment_storage::recovery_work` (one read-only query over every
  tenant, outside the kernels like `tenants_to_recover` before it) returns,
  per tenant, the payments to ask the PSP about (`PROCESSING`, `AUTHORIZED`
  with automatic capture), those with a pending refund, and `CAPTURED`
  payments with a ledger posting missing: an anti-join on the ledger's
  deterministic transfer ids by the rule `sync_ledger` follows
  (`payment_storage::needs_ledger_sync`, the same rule in Rust). Settled
  payments are no longer re-synced on every pass. Partial indexes on the
  unfinished and captured payments; `payments_merchant`,
  `refunds_payment` and `accounts_owner` indexes back the B1 plans.
- `Orchestrator::recover_work` handles one tenant's work with at most
  `PSP_CONCURRENCY` (8) PSP calls or ledger syncs at once, re-reading each
  payment through the kernel first (work finished meanwhile is skipped);
  `jobs::recover_all` runs `TENANT_CONCURRENCY` (4) tenants at once.
  `POST /v1/recover` finds its tenant's work from the two kernels'
  snapshots (`find_work`, same rule) and runs the same executor.
- The posting kinds and `system_transfer_id` moved to
  `payment_storage::ledger` (the orchestrator re-exports them) so the query
  and the orchestrator share one definition.
- Test `recovery_job`: four tenants (two lost PSP answers, one healthy,
  one `CAPTURED` with its capture posting lost); the query selects exactly
  the three with work and the right payment per category, one pass fixes
  all (2 resolved, 1 ledger sync), audits pass, `find_work` agrees with the
  query on the healthy tenant, and the next pass selects no tenant. Timings
  are in B4.

### B1: per-command reads with frame theorems (SEC-06)

- Both kernels have an extracted `reads(actor, cmd) -> Option<Vec<h5i_app_sql::Read>>`
  (one declarative read per table); `LedgerStore::load_for` and
  `PaymentsStore::load_for` run it with `schema_load_plan`. Ledger: `Open`
  reads the counter row; `Transfer` its transfer id and two accounts;
  `Batch` (≤ 64 transfers, `MAX_PLANNED_BATCH`) every id and account it
  names, longer batches the whole tenant; `Account` its row; `Accounts` all
  rows for an operator, the caller's own otherwise; `Transfers` the journal
  for an operator, nothing otherwise. Payments: every command by key, `List`
  by merchant (all for an operator), `Refunds` the payment and its refunds,
  `GetRefund` the refund and, with `IN (SELECT payment ...)`, its payment.
- Proofs (`proofs/ledger/Ledger/{Plan,Frame}.lean`,
  `proofs/payments/Payments/{Plan,Frame}.lean`): `Plan.reads_abs` (the
  extracted `reads` computes the plan `readsA` written in Lean),
  `Frame.frame` (on `Snapshot.load plan s`, what loading the plan decodes to
  by the generated `decode_load`, `transition` gives exactly its result on
  `s`) and `Frame.load_for_sound` (the two together). The ledger batch case
  carries the slice through the batch loop: each transfer's writes touch
  only rows the slice keeps, so the slice stays a slice
  (`keeps_applyAll`, `batch_loop_congr`); it assumes `HasRoom s`. The
  payments frame needs no assumption.
- Writes take per-key advisory locks (`lock_for`: accounts and transfer id
  of a transfer, the counter for `Open`/`Create`, the payment for payment
  commands) instead of the tenant lock; reads take none.
- Gates: `h5i app check crates/payment-ledger` 224 theorems in 13 modules,
  `crates/payment-domain` 222 in 8, standard axioms only.
  `h5i app mutate`: ledger 7/7, payments 8/8 (new mutants: a plan that
  misses the credit account, a plan on the wrong column, a `Record` plan
  that misses the refund, a `Refunds` plan by refund id; each is caught by
  `Plan.reads_abs`, i.e. the Rust plan no longer matches the plan the
  frame theorem is proved for).
- Tests against PostgreSQL (`tests/concurrency`):
  `load_for_matches_the_whole_tenant` (400 random ledger commands, batches
  up to 70 legs) and `payments_load_for_matches_the_whole_tenant` (500
  random payment commands): at every step `transition` on the rows
  `snapshot_for` returns equals `transition` on the whole tenant. The
  existing `postgres_agrees_with_the_reference_engine` now also runs
  through `load_for`.

### SEC-02: separate user and operator token keys

- `crates/payment-api/src/auth.rs`: user tokens are signed with
  `H5I_APP_SECRET`, operator tokens with `H5I_PAY_OPERATOR_SECRET`; the API
  refuses equal keys, and keys under 32 bytes on the deployed
  (`DB_CREDENTIALS`) path. A user-key token for user 0 gets 401, so holding
  the user key no longer makes an operator. Operator tokens name the operator
  (id ≥ 1, logged per request with the tenant as `operator request`) and
  are refused at verification if they expire more than 12 h (+60 s skew)
  after the time of use, whoever signed them (`issue_operator` also refuses
  a longer TTL). `…_PREVIOUS` values are accepted during a rotation.
  `H5I_PAY_ISSUE_OPERATOR=<tenant>:<id>[:<ttl>]` issues operator tokens with
  the operator key alone; `H5I_APP_ISSUE` issues user tokens (user ≥ 1) with
  the user key alone. `h5i-pay recover` needs neither key.
- Deployment: Terraform has a second secret `h5i-pay-operator-secret`; both
  token secrets are JSON `{current, previous}` (initially equal; Terraform
  ignores later changes) and the task injects the four variables by JSON
  key (`<arn>:current::`, `<arn>:previous::`). The execution role may read
  the new secret. Compose and the CI "refuses mock" step set both keys.
  `docs/runbooks/secret-rotation.md` has the two-step rotation. Not run on
  AWS: ECS JSON-key injection is written from the AWS docs and checked only
  by `terraform validate`.
- Decision on the operator id in the kernels: not done. Kernel `Principal`
  is `{org, user, operator}` and operator-created rows record `actor.user`
  (a payment's `merchant`), so putting the operator id in `user` would
  collide with user ids; it needs a new field and re-checked authorization
  proofs. The per-operator trail is the API log. Documented in
  `docs/security-review.md` (SEC-02 now "partly closed") and
  `docs/proof-assumptions.md`.
- Verified: `cargo test --workspace` (with PostgreSQL and both TLS servers),
  clippy with and without `mock-psp`, `terraform fmt -check` and `validate`,
  the production binary refuses `mock` and refuses to start without the
  operator key. Kernels untouched, so no proof re-run was needed.

### h5i-app 0.5.2 from crates.io

- h5i-app 0.5.2 is on crates.io (published from h5i `02ccdde`, a
  descendant of `a19667c` that only bumps versions). The seven published
  crates are now `=0.5.2` from crates.io; `h5i-app-gpu`, the nvvm backend
  and both `h5i_app_lib` requires are at git `02ccdde`. Cargo.lock has
  one copy of each h5i-app crate. The crate's git tag `v0.5.2` is not used:
  it is the CLI release and has h5i-app 0.1.4, without TLS or u32 support.
- Re-extraction changed only `Source:` path comments. Verified:
  extract --check, check (145 / 122 theorems), mutate (5/5, 6/6),
  lake build, `cargo test --workspace` (67), clippy (default, mock-psp,
  CUDA), GPU differential tests, `cargo build --locked`.

### The API process killed mid-request

- `crates/payment-api/tests/process_kill.rs` runs the `h5i-pay` binary as
  the production image builds it (no Mock PSP; `H5I_PAY_PSP=stripe`)
  against its own scratch PostgreSQL database and a fake Stripe API in the
  test process (`STRIPE_API_BASE`), which implements PaymentIntents,
  search, refunds and Stripe's idempotency-key replay, and can hold one
  request open before or after acting. The test SIGKILLs the process
  while it waits:
  - after the charge: the payment is left `PROCESSING`, no reference, no
    ledger entry; a client retry with the same `Idempotency-Key` on a new
    process returns the same payment, captured, with the PSP's original
    charge (one PSP effect) and one ledger posting; the recovery job then
    has nothing to do;
  - after the PSP made a refund: the refund stays pending and reserved
    (a second refund past the remaining amount gets 409); `h5i-pay
    recover`, with no API running, settles it;
  - before the PSP acted: the job reports it unresolved and leaves it; the
    operator's `fail_unsubmitted=true` fails it; no money moved.
  Each ends with the ledger audit and both reconciliations clean. Passed
  8 runs in a row. Making the PSP idempotency key non-deterministic makes
  the first test fail, so it does check that money moves at most once.

### Scheduled recovery

- `h5i-pay recover` (same binary and task definition as the API, command
  override): one pass over every tenant `payment_storage::tenants_to_recover`
  finds (payments `PROCESSING`, `AUTHORIZED` with automatic capture,
  `CAPTURED`, or with a refund pending), running `Orchestrator::recover`
  per tenant. A failing tenant does not stop the others; exit 1 if any
  failed. It never passes `fail_unsubmitted`. Prints one JSON line
  (`"event":"recovery"`). `tenants_to_recover` is the only cross-tenant
  query: read-only transaction, reads `tenant_id` only, outside the
  kernels; it also works over verified TLS.
- `infra/terraform/recovery.tf`: EventBridge Scheduler every 5 minutes
  (`recover_schedule`), Fargate RunTask of the API family's latest
  revision (so it follows CI deploys) in the private subnets with the API
  security group; a scheduler role limited to RunTask on that family in
  this cluster plus PassRole of the task's two roles; metric filters and
  alarms (tenants failed, payments unresolved for an hour, no pass in an
  hour). `terraform validate` passes. Terraform `required_version` is now
  `>= 1.9.0` (the SEC-05 cross-variable validation needs it).
- Test `recovery_job::scheduled_recovery_covers_every_tenant` (own scratch
  database, since the job sees all tenants): three tenants, two with a
  lost PSP response; one pass resolves both, audits pass, a second pass
  changes nothing; an empty database has nothing to recover. The binary
  was also run against the dev database (65 tenants, 97 payments re-synced,
  exit 0).

### SEC-05 closed: the Mock PSP is out of production builds

- `payment-connectors` feature `mock` gates `mock::MockPsp`;
  `payment-api` feature `mock-psp` enables it. Both are off by default.
  Without it, `H5I_PAY_PSP=mock` stops the binary at startup and the
  default PSP is `stripe`. With it, the default stays `mock`, and a warning
  is logged.
- `Dockerfile.api` has `ARG API_FEATURES=""`: the production image is built
  without the Mock PSP, and CI also pushes `<sha>-mock` (built with
  `API_FEATURES=mock-psp`). Terraform runs the `-mock` image and injects
  the mock webhook secret only for `psp_mode = "mock"`, and rejects
  `psp_mode = "mock"` in `prod*` environments (cross-variable validation;
  checked with a scratch plan: `prod-1`+`mock` rejected, `test`/`staging`
  +`mock` and `prod`+`stripe` accepted). The deploy job keeps the image
  variant matching the task's `H5I_PAY_PSP`.
- CI builds `payment-api` alone and fails unless the binary refuses
  `mock`, and runs clippy on the `mock-psp` path too.
- Checked here: the production binary refuses `mock`, defaults to Stripe
  (`STRIPE_SECRET_KEY is not set`), and `strings` finds no Mock PSP
  markers in it (2 in the `mock-psp` build); both image variants were
  built and run.

### u32 columns

- Ledger: `Account.currency`, `Account.code`, `Transfer.currency`,
  `Transfer.code`, `TransferReq.code`, `Command::Open { currency, code }`.
  Payments: `Payment.currency`, `Payment.status`, `Refund.status`, the
  status constants and `Reply::Status`. Amounts, ids and counters stay
  `u64`.
- No migration: h5i-app-schema stores `u32` as `BIGINT` and range-checks it
  on load (`u32_from_val`).
- Proof changes: `journalRow`/`open_spec` take `U32`; `post_spec` closes
  `a.currency = b.currency` with `UScalar.eq_of_val_eq`; `Payments.Commands`
  adds `u32_val_eq` (h5i_app_lib has only `u64_val_eq`) and retypes
  `with_status_spec`/`create_spec`.
- Side effect: the orchestrator used to pass `currency as u32` to the PSP
  from a `u64` kernel field, so a currency of 2³²+392 would have been
  charged as 392 (JPY) while the kernel stored another value. The casts
  are gone; the API answers 400 for a currency past `u32::MAX` (checked
  by hand on `/v1/payments` and `/v1/ledger`).
- Verified: extract --check, check (145 / 122 theorems), mutate (5/5,
  6/6), lake build, `cargo test --workspace` (including `tests/tls`),
  clippy with and without CUDA, GPU differential tests.

### SEC-01 closed: verified TLS to PostgreSQL

- `payment-storage` enables h5i-app-pg's `tls` feature. `DbTls::from_env`
  reads `DB_CA_FILE`; when set, every pool uses `pool_tls` with
  `Roots::PemFile` (chain and host name verified, `sslmode` forced to
  `require`, `sslmode=disable` an error, no plaintext fallback). A missing
  or certificate-less bundle fails at startup. The API refuses to start on
  the RDS-secret path (`DB_CREDENTIALS`) without `DB_CA_FILE`; with
  `DATABASE_URL` and no CA it uses plain TCP (local development).
- The Amazon RDS global CA bundle (114 CAs, sha256 `90a6f1a7…f21b`) is
  vendored at `infra/docker/rds-global-bundle.pem`
  (`infra/docker/update-rds-ca.sh` refreshes it) and shipped in the API
  image at `/etc/h5i-pay/rds-global-bundle.pem`; the ECS task sets
  `DB_CA_FILE` to it. Not added to the worker image: the batch worker has
  no database credentials or network path to RDS.
- Terraform: `rds.force_ssl = 1`. `terraform fmt -check` and `validate` pass.
- `tests/tls` (7 tests) against h5i's `scripts/app/pg-tls.sh` servers
  (TLS-only `pg_hba.conf`; ports 55433 default protocols, 55434 TLS 1.2):
  payment, capture, refund, reconciliation and audit over TLS with every
  server-side connection TLS (`pg_stat_ssl`); plain TCP refused by the
  server; a certificate from another CA refused (`UnknownIssuer`); the IP
  instead of the certified name refused (`NotValidForName`);
  `sslmode=disable` refused; bad CA files refused.
- The API image was built locally (90.7 MB) and run against the TLS-only
  server: with the test CA it served `/v1/ledger` and `audit` with
  `db_tls=true`; with the shipped RDS bundle the handshake failed; without
  `DB_CA_FILE` it refused to start.
- CI starts the TLS servers with h5i's script at the pinned rev and fails if
  the TLS suite skipped.

### h5i-app from git rev a19667c

- Every h5i-app crate (including `h5i-app-gpu` and
  `h5i-app-gpu-backend-nvvm`) and both `h5i_app_lib` Lake requires now come
  from h5i `a19667cbbb5a1be14c8d16579eb00e679910dc57`. The h5i_app_lib Lean
  sources and the Aeneas pin are identical to v0.5.2; the re-extraction
  changed only the `Source:` path comments in `proofs/*/generated/`.
- Verified: `h5i app extract --check` (both up to date), `h5i app check`
  (ledger 145 theorems / 11 modules, payments 121 / 6, standard axioms
  only), `h5i app mutate` (5/5 and 6/6 caught), `lake build` in both
  projects, `cargo test --workspace`, clippy with and without CUDA, GPU
  differential tests.

## Open items / decisions

- Not done or not verifiable here:
  - SEC-03, SEC-07, SEC-09 closed (E). SEC-02 closed for kernel identity
    and audit (F1); scoped tokens, an identity provider and per-operator
    revocation remain. See `docs/security-review.md`.
  - The recovery schedule and the nightly reconciliation are written and
    validated (terraform validate, checkov) but have not run on AWS.
  - Docker images are built locally now. The worker image did not build
    before (Ubuntu 24.04 has no `awscli` package); fixed by installing AWS
    CLI v2.39.0 from its release zip, pinned by SHA-256 after a PGP check.
    The worker image (793 MB) ran a 200k-row reconciliation on the A10G
    and on CPU with identical reports. Not pushed to any registry.
  - The Stripe test-mode round trip was skipped (no `STRIPE_SECRET_KEY`).
  - Terraform was validated, never planned against or applied to AWS.

- u32 columns broke the Lean that `h5i-app-schema` 0.1.4 rendered, so M1–M5
  used `u64` everywhere. Fixed upstream (#769); see "u32 columns" above.
- Every kernel command reads only its plan's rows (B1, F3); listings are
  paged by per-merchant/per-owner sequence numbers, at most 64 per page.
