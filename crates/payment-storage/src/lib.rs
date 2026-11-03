//! PostgreSQL adapters for the h5i-pay kernels.
//!
//! Each kernel gets an h5i-app [`Kernel`](h5i_app_core::Kernel) marker, a
//! [`Store`](h5i_app_pg::Store) generated from its `schema!`, the
//! [`ReplyCodec`](h5i_app_pg::ReplyCodec) for idempotency keys and the JSON
//! API. Every request runs in one SERIALIZABLE PostgreSQL transaction through
//! [`h5i_app_pg::Engine`]: load the tenant, run the pure `transition`, write,
//! commit, with retries on serialization failures and dropped connections.
//!
//! Each kernel lives in its own PostgreSQL schema (`ledger`, `payments`), so
//! their tables never mix.

pub mod ledger;
pub mod payments;

/// Idempotency-key namespaces (G2).
///
/// h5i-app stores a keyed request's reply under `(tenant, "<scope>/<key>")`
/// in each kernel's `h5i_idempotency` table, the scope coming from
/// `ReplyCodec::scope`. Since G2 the scope is the whole principal: `u<id>`
/// for user `id`, `o<id>` for operator `id` (system operators included),
/// so two actors using the same key run two requests, and one actor's
/// retry replays only its own reply.
///
/// Before G2 every operator, people and the system's processes alike, used
/// `u0`. The migration (`migrate_system_keys`, run at startup with the
/// schema) moves the system's deterministic keys (`system-…`, the
/// orchestrator's accounts) to the orchestrator's scope, the operator that
/// now always sends them. The other `u0/…` keys were made by operators
/// through the API and cannot be attributed to one: they stay where they
/// are, are never written again (no principal has scope `u0`: user ids
/// start at 1), and an operator's keyed request first looks its key up
/// there ([`legacy_reply`]): the same command replays its stored reply, a
/// different one is refused as before. So a retry of a request sent before
/// the upgrade still replays, for any operator, as it did then.
pub mod idempotency {
    use super::{connect_direct, DbTls};
    use h5i_app_pg::DbError;

    /// The scope of a principal's keys: `u<id>` or `o<id>`.
    pub fn scope(operator: bool, id: u64) -> String {
        if operator { format!("o{id}") } else { format!("u{id}") }
    }

    /// The scope every operator shared before G2.
    pub const LEGACY_OPERATOR_SCOPE: &str = "u0";

    /// Rename the system's `u0/system-…` keys to the orchestrator's scope,
    /// once (a key already there wins; the statement is a no-op after the
    /// first run).
    pub fn migrate_system_keys() -> String {
        let to = scope(true, super::ORCHESTRATOR_OPERATOR);
        format!(
            "UPDATE h5i_idempotency AS i SET key = '{to}/' || substr(i.key, 4)              WHERE i.key LIKE 'u0/system-%' AND NOT EXISTS                (SELECT 1 FROM h5i_idempotency j WHERE j.tenant_id = i.tenant_id AND j.key = '{to}/' || substr(i.key, 4))"
        )
    }

    /// What a pre-G2 operator key `key` stored in `schema`, if anything:
    /// `(fingerprint, reply)`. Read-only; legacy rows never change.
    pub async fn legacy_reply(url: &str, tls: &DbTls, schema: &str, tenant: u64, key: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>, DbError> {
        let (c, conn) = connect_direct(url, tls).await?;
        let t = i64::try_from(tenant).map_err(|_| DbError::Decode("tenant id out of range".into()))?;
        let sql = format!("SELECT fingerprint, reply FROM {schema}.h5i_idempotency WHERE tenant_id = $1 AND key = $2");
        let row = match c.query_opt(sql.as_str(), &[&t, &format!("{LEGACY_OPERATOR_SCOPE}/{key}")]).await {
            Ok(r) => r,
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => None,
            Err(e) => return Err(e.into()),
        };
        drop(c);
        let _ = conn.await;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }
}

use h5i_app_pg::{Clock, Engine, EngineConfig, Pool, DbError};
use std::sync::Arc;

pub use h5i_app_pg::tls::Roots;

pub type LedgerEngine = Engine<ledger::LedgerApp, ledger::LedgerStore>;
pub type PaymentsEngine = Engine<payments::PaymentsApp, payments::PaymentsStore>;

pub const LEDGER_SCHEMA: &str = "ledger";

/// Operator ids from here up name the system's own processes, which act in
/// process without a token; `payment_api::auth` refuses operator tokens for
/// them, so people and processes never share an id (SEC-02).
pub const SYSTEM_OPERATORS: u64 = 1 << 62;
/// The orchestrator (payments, captures, refunds, webhooks, ledger postings).
pub const ORCHESTRATOR_OPERATOR: u64 = SYSTEM_OPERATORS + 1;
/// The scheduled recovery job.
pub const RECOVERY_OPERATOR: u64 = SYSTEM_OPERATORS + 2;
/// The nightly reconciliation export (reads only).
pub const EXPORT_OPERATOR: u64 = SYSTEM_OPERATORS + 3;

/// `ALTER TABLE … ADD COLUMN IF NOT EXISTS`: a column added to a kernel
/// table after deployments created it (`CREATE TABLE IF NOT EXISTS` leaves
/// existing tables alone).
/// Only when the column is missing: `ALTER TABLE` takes an exclusive lock
/// even when it changes nothing, and engines installing their schema at
/// once would deadlock on it.
pub(crate) fn add_column(table: &str, column: &str, ty: &str) -> String {
    format!(
        "DO $$ BEGIN \
           IF NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema() \
                          AND table_name = '{table}' AND column_name = '{column}') THEN \
             ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {column} {ty}; \
           END IF; \
         END $$"
    )
}

/// One-off data migrations already run in this schema, by name.
pub(crate) const MIGRATIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS h5i_pay_migrations (name TEXT PRIMARY KEY, at TIMESTAMPTZ NOT NULL DEFAULT now())";

/// G1: the `history` table of the schema refuses `UPDATE`, `DELETE` and
/// `TRUNCATE`, so an entry, once committed, stays as written. The kernels
/// only append to it (`Theorems.history_appended`); this makes the database
/// refuse anything else, from any client.
pub(crate) fn append_only_history() -> Vec<String> {
    vec![
        "CREATE OR REPLACE FUNCTION history_append_only() RETURNS trigger LANGUAGE plpgsql AS $$            BEGIN RAISE EXCEPTION 'history is append-only (% on %)', TG_OP, TG_TABLE_NAME              USING ERRCODE = 'insufficient_privilege'; END $$"
            .into(),
        "DO $$ BEGIN            IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'history_append_only'                           AND tgrelid = 'history'::regclass) THEN              CREATE TRIGGER history_append_only BEFORE UPDATE OR DELETE ON history                FOR EACH ROW EXECUTE FUNCTION history_append_only();              CREATE TRIGGER history_no_truncate BEFORE TRUNCATE ON history                FOR EACH STATEMENT EXECUTE FUNCTION history_append_only();            END IF;          END $$"
            .into(),
    ]
}

/// G5: the payments not marked `ledger_done`, as rows of their own table
/// instead of a partial index on `payments`. A partial index on the marker
/// makes `MarkPosted` a non-HOT update, which adds primary-key entries on
/// the index page that concurrent `Create`s read and insert into, and
/// under SERIALIZABLE one of each such pair was aborted. No kernel command
/// reads `open_payments`; recovery does (`recovery_work`). Outside the
/// kernels' tables: it is an index the database maintains, not state a
/// kernel decides on.
///
/// I1: the triggers only insert: when a payment is inserted unmarked, and
/// when its marker is cleared. They never delete. `open_payments` holds
/// every payment not marked, and may also hold marked ones until
/// [`prune_open_payments`] removes them (the scheduled recovery does, first
/// thing in each pass). Before, marking deleted the row here, and that
/// `DELETE` is a read under SERIALIZABLE: of the whole (small) table, or of
/// the one index page every new payment's row goes to. Each `MarkPosted`
/// then read what concurrent `Create`s and `MarkPosted`s wrote, and was
/// aborted as the pivot of such pairs (I1 in `docs/benchmarks.md`).
///
/// The trigger is created before the backfill: `CREATE TRIGGER` locks
/// `payments` against writes until this transaction commits, so no payment
/// changes between the copy and the trigger. A table from before I1 has
/// the trigger `open_payments_mark`, which also fired on marking; it is
/// replaced once by `open_payments_unmark`.
pub(crate) fn open_payments() -> Vec<String> {
    vec![
        "CREATE OR REPLACE FUNCTION open_payments_sync() RETURNS trigger LANGUAGE plpgsql \
           SET search_path FROM CURRENT AS $$ \
         BEGIN \
           IF NOT NEW.ledger_done THEN \
             INSERT INTO open_payments (tenant_id, id) VALUES (NEW.tenant_id, NEW.id) ON CONFLICT DO NOTHING; \
           END IF; \
           RETURN NULL; \
         END $$"
            .into(),
        "DO $$ BEGIN \
           IF to_regclass('open_payments') IS NULL THEN \
             CREATE TABLE open_payments (tenant_id BIGINT NOT NULL, id BIGINT NOT NULL, PRIMARY KEY (tenant_id, id)); \
             CREATE TRIGGER open_payments_insert AFTER INSERT ON payments \
               FOR EACH ROW WHEN (NOT NEW.ledger_done) EXECUTE FUNCTION open_payments_sync(); \
             CREATE TRIGGER open_payments_unmark AFTER UPDATE ON payments \
               FOR EACH ROW WHEN (OLD.ledger_done AND NOT NEW.ledger_done) EXECUTE FUNCTION open_payments_sync(); \
             INSERT INTO open_payments (tenant_id, id) SELECT tenant_id, id FROM payments WHERE NOT ledger_done; \
             ANALYZE open_payments; \
           END IF; \
         END $$"
            .into(),
        "DO $$ BEGIN \
           IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'open_payments_unmark' \
                          AND tgrelid = 'payments'::regclass) THEN \
             DROP TRIGGER IF EXISTS open_payments_mark ON payments; \
             CREATE TRIGGER open_payments_unmark AFTER UPDATE ON payments \
               FOR EACH ROW WHEN (OLD.ledger_done AND NOT NEW.ledger_done) EXECUTE FUNCTION open_payments_sync(); \
           END IF; \
         END $$"
            .into(),
    ]
}

/// Rows of `open_payments` removed per statement by [`prune_open_payments`].
pub const PRUNE_BATCH: i64 = 10_000;

/// Remove from `open_payments` the payments marked `ledger_done` (I1: the
/// triggers never delete). Returns how many rows went.
///
/// `READ COMMITTED`, outside SERIALIZABLE, so it takes no predicate locks
/// and aborts no kernel transaction. A row goes only with its payment
/// locked `FOR SHARE` and seen marked: a command clearing the marker at the
/// same time either committed first (then the lock's re-check sees the
/// marker cleared and keeps the row) or waits for this transaction and
/// then inserts the row again by its trigger. So every payment not marked
/// keeps its row, as `recovery_work` needs.
pub async fn prune_open_payments(url: &str, tls: &DbTls) -> Result<u64, DbError> {
    let (mut client, conn) = connect_direct(url, tls).await?;
    let n = prune_with(&mut client).await;
    drop(client);
    let _ = conn.await;
    n
}

/// [`prune_open_payments`] on an open connection: `open_payments` in key
/// order, [`PRUNE_BATCH`] rows per transaction. Each batch reads its rows,
/// locks those of their payments that are marked (`FOR SHARE`, re-checked
/// on the newest version after any wait) and deletes just those. The
/// rows' payments are looked up by key: a join the planner orders itself
/// scans every payment of the tenant when it misjudges the table's size.
async fn prune_with(client: &mut tokio_postgres::Client) -> Result<u64, DbError> {
    let page = format!(
        "SELECT tenant_id, id FROM {PAYMENTS_SCHEMA}.open_payments \
         WHERE (tenant_id, id) > ($1, $2) ORDER BY tenant_id, id LIMIT {PRUNE_BATCH}"
    );
    let marked = format!(
        "SELECT p.tenant_id, p.id FROM unnest($1::bigint[], $2::bigint[]) AS c (tenant_id, id) \
           CROSS JOIN LATERAL (SELECT tenant_id, id FROM {PAYMENTS_SCHEMA}.payments q \
              WHERE q.tenant_id = c.tenant_id AND q.id = c.id AND q.ledger_done FOR SHARE) p"
    );
    let delete = format!(
        "DELETE FROM {PAYMENTS_SCHEMA}.open_payments o USING unnest($1::bigint[], $2::bigint[]) AS d (tenant_id, id) \
         WHERE o.tenant_id = d.tenant_id AND o.id = d.id"
    );
    let (mut total, mut after) = (0, (i64::MIN, i64::MIN));
    loop {
        let tx = client.build_transaction().isolation_level(tokio_postgres::IsolationLevel::ReadCommitted).start().await?;
        let rows = match tx.query(page.as_str(), &[&after.0, &after.1]).await {
            Ok(r) => r,
            // No tables yet: nothing to prune.
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let (ts, ids): (Vec<i64>, Vec<i64>) = rows.iter().map(|r| (r.get::<_, i64>(0), r.get::<_, i64>(1))).unzip();
        if let (Some(t), Some(id)) = (ts.last(), ids.last()) {
            after = (*t, *id);
        }
        let locked = tx.query(marked.as_str(), &[&ts, &ids]).await?;
        let (lt, li): (Vec<i64>, Vec<i64>) = locked.iter().map(|r| (r.get::<_, i64>(0), r.get::<_, i64>(1))).unzip();
        if !lt.is_empty() {
            total += tx.execute(delete.as_str(), &[&lt, &li]).await?;
        }
        tx.commit().await?;
        if rows.len() < PRUNE_BATCH as usize {
            return Ok(total);
        }
    }
}

/// `ALTER TABLE … SET (fillfactor = n)`, only when not set yet (it takes a
/// lock and would serialize engines installing at once). Applies to pages
/// written from then on.
pub(crate) fn fillfactor(table: &str, n: u32) -> String {
    format!(
        "DO $$ BEGIN \
           IF NOT EXISTS (SELECT 1 FROM pg_class WHERE oid = '{table}'::regclass \
                          AND reloptions @> ARRAY['fillfactor={n}']) THEN \
             ALTER TABLE {table} SET (fillfactor = {n}); \
           END IF; \
         END $$"
    )
}

/// F3: `table.seq` numbers each row among the rows with the same `scope`
/// column (0, 1, … by id), and `counters (scope, next)` holds the next
/// number. A table created before the column existed gets it, numbered by
/// id, and its counters, once: while `seq` is still nullable (it is `NOT
/// NULL` in tables `CREATE TABLE` made).
pub(crate) fn numbering(table: &str, scope: &str, counters: &str) -> Vec<String> {
    vec![
        add_column(table, "seq", "BIGINT"),
        format!(
            "DO $$ BEGIN \
               IF EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema() \
                          AND table_name = '{table}' AND column_name = 'seq' AND is_nullable = 'YES') THEN \
                 UPDATE {table} t SET seq = n.rn FROM \
                   (SELECT tenant_id, id, row_number() OVER (PARTITION BY tenant_id, {scope} ORDER BY id) - 1 AS rn FROM {table}) n \
                   WHERE t.tenant_id = n.tenant_id AND t.id = n.id; \
                 ALTER TABLE {table} ALTER COLUMN seq SET NOT NULL; \
                 INSERT INTO {counters} (tenant_id, {scope}, next) \
                   SELECT tenant_id, {scope}, max(seq) + 1 FROM {table} GROUP BY tenant_id, {scope} \
                   ON CONFLICT DO NOTHING; \
               END IF; \
             END $$"
        ),
    ]
}
pub const PAYMENTS_SCHEMA: &str = "payments";

/// How connections to PostgreSQL are secured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DbTls {
    /// Plain TCP. Only for a local database (tests, `scripts/dev-db.sh`).
    Off,
    /// TLS on every connection, the server's certificate chain checked
    /// against these roots and its name against the URL's host (libpq's
    /// `verify-full`). A server without TLS is refused, never used in the
    /// clear (h5i-app-pg `pool_tls`).
    Verified(Roots),
}

impl DbTls {
    /// `DB_CA_FILE` set: [`DbTls::Verified`] with only the CA certificates
    /// in that PEM file (in the images, the Amazon RDS global bundle at
    /// `/etc/h5i-pay/rds-global-bundle.pem`). Unset: [`DbTls::Off`].
    pub fn from_env() -> Result<Self, DbError> {
        match std::env::var_os("DB_CA_FILE") {
            Some(p) if !p.is_empty() => Self::from_ca_file(std::path::Path::new(&p)),
            _ => Ok(DbTls::Off),
        }
    }

    /// [`DbTls::Verified`] with the CA certificates in `path`, checked now
    /// (a missing or empty bundle fails at startup, not on the first request).
    pub fn from_ca_file(path: &std::path::Path) -> Result<Self, DbError> {
        let roots = Roots::PemFile(path.to_path_buf());
        h5i_app_pg::tls::RustlsConnect::new(&roots)?;
        Ok(DbTls::Verified(roots))
    }
}

/// A plain-TCP pool whose connections see only `schema`'s tables.
pub fn schema_pool(url: &str, schema: &str, size: usize) -> Result<Pool, DbError> {
    schema_pool_with(url, schema, size, &DbTls::Off)
}

/// A pool whose connections see only `schema`'s tables, secured as `tls` says.
pub fn schema_pool_with(url: &str, schema: &str, size: usize, tls: &DbTls) -> Result<Pool, DbError> {
    let url = h5i_app_pg::with_schema(url, schema)?;
    match tls {
        DbTls::Off => h5i_app_pg::pool(&url, size),
        DbTls::Verified(roots) => h5i_app_pg::pool_tls(&url, size, roots),
    }
}

/// Both engines over plain TCP, with their schemas installed.
pub async fn engines(url: &str, pool_size: usize, config: EngineConfig) -> Result<(Arc<LedgerEngine>, Arc<PaymentsEngine>), DbError> {
    engines_with(url, pool_size, config, &DbTls::Off).await
}

/// Both engines, secured as `tls` says, with their schemas installed.
/// They read the time from PostgreSQL (`Clock::Database`,
/// `transaction_timestamp()`), so the history (G1) records the
/// transaction's clock, the same for every API server of a database.
pub async fn engines_with(
    url: &str,
    pool_size: usize,
    config: EngineConfig,
    tls: &DbTls,
) -> Result<(Arc<LedgerEngine>, Arc<PaymentsEngine>), DbError> {
    let config = EngineConfig { clock: Clock::Database, ..config };
    let ledger = Arc::new(LedgerEngine::new(schema_pool_with(url, LEDGER_SCHEMA, pool_size, tls)?, config.clone()));
    ledger.install_schema().await?;
    let payments = Arc::new(PaymentsEngine::new(schema_pool_with(url, PAYMENTS_SCHEMA, pool_size, tls)?, config));
    payments.install_schema().await?;
    Ok((ledger, payments))
}

/// One tenant's unfinished work, by payment id (`recovery_work`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveryWork {
    pub tenant: u64,
    /// `PROCESSING`, or `AUTHORIZED` with automatic capture: ask the PSP.
    pub psp: Vec<u64>,
    /// A refund pending: ask the PSP about it.
    pub refunds: Vec<u64>,
    /// Settled (`CAPTURED`, `FAILED`, `CANCELED`) and not marked
    /// `ledger_done` ([`ledger_pending`]): sync and mark.
    pub ledger: Vec<u64>,
}

impl RecoveryWork {
    pub fn is_empty(&self) -> bool {
        self.psp.is_empty() && self.refunds.is_empty() && self.ledger.is_empty()
    }
}

/// Whether a `CAPTURED` payment's ledger postings are incomplete, given its
/// refunds and which transfer ids the ledger has. The rule
/// `Orchestrator::sync_ledger` follows: the capture posting; for each
/// refund, its reservation, plus the settlement once it succeeded, or the
/// release once it failed (unless the reservation never happened: then
/// there is nothing to release).
pub fn needs_ledger_sync(p: &payment_domain::Payment, refunds: &[payment_domain::Refund], has: impl Fn(u64) -> bool) -> bool {
    capture_needs_sync(p, &has)
        || (p.status == payment_domain::CAPTURED && refunds.iter().filter(|r| r.payment == p.id).any(|r| refund_needs_sync(r, &has)))
}

/// The capture part of [`needs_ledger_sync`]: `p` is `CAPTURED` and its
/// capture posting is missing.
pub fn capture_needs_sync(p: &payment_domain::Payment, has: impl Fn(u64) -> bool) -> bool {
    p.status == payment_domain::CAPTURED && !has(ledger::system_transfer_id(ledger::POST_CAPTURE, p.id))
}

/// The refund part of [`needs_ledger_sync`], for one refund of a `CAPTURED`
/// payment: its reservation is missing, or its settlement once it
/// succeeded, or its release once it failed after a reservation.
pub fn refund_needs_sync(r: &payment_domain::Refund, has: impl Fn(u64) -> bool) -> bool {
    use ledger::{system_transfer_id as id, POST_REFUND_RELEASE, POST_REFUND_RESERVE, POST_REFUND_SETTLE};
    let reserved = has(id(POST_REFUND_RESERVE, r.id));
    match r.status {
        payment_domain::REFUND_SUCCEEDED => !reserved || !has(id(POST_REFUND_SETTLE, r.id)),
        payment_domain::REFUND_FAILED => reserved && !has(id(POST_REFUND_RELEASE, r.id)),
        _ => !reserved,
    }
}

/// Whether recovery must sync a payment's ledger postings (or, for a
/// failed or canceled payment, which needs none, just mark it): it is
/// `CAPTURED`, `FAILED` or `CANCELED` and not marked complete. The payment
/// kernel clears `ledger_done` on every other write of the payment, so a
/// payment changed since its last successful sync is always pending
/// (`payment_domain.Theorems.changes_clear_ledger_done`).
pub fn ledger_pending(p: &payment_domain::Payment) -> bool {
    use payment_domain::{CANCELED, CAPTURED, FAILED};
    (p.status == CAPTURED || p.status == FAILED || p.status == CANCELED) && !p.ledger_done
}

/// A connection outside the engines (cross-tenant reads, the `ops` schema).
async fn connect_direct(url: &str, tls: &DbTls) -> Result<(tokio_postgres::Client, tokio::task::JoinHandle<()>), DbError> {
    let mut cfg: tokio_postgres::Config = url.parse()?;
    match tls {
        DbTls::Off => {
            let (c, conn) = cfg.connect(tokio_postgres::NoTls).await?;
            Ok((c, tokio::spawn(async move { let _ = conn.await; })))
        }
        DbTls::Verified(roots) => {
            if cfg.get_ssl_mode() == tokio_postgres::config::SslMode::Disable {
                return Err(DbError::Pool("TLS requested but the URL says sslmode=disable".into()));
            }
            cfg.ssl_mode(tokio_postgres::config::SslMode::Require);
            let (c, conn) = cfg.connect(h5i_app_pg::tls::RustlsConnect::new(roots)?).await?;
            Ok((c, tokio::spawn(async move { let _ = conn.await; })))
        }
    }
}

/// Every tenant's unfinished work, in one read-only query: payments to ask
/// the PSP about, payments with a pending refund, and captured payments
/// whose ledger postings are not marked complete ([`ledger_pending`]).
/// The parts read `open_payments` (every payment not `ledger_done`, kept by
/// triggers, G5, and maybe marked ones not pruned yet, I1) and the partial index `payments_refunding`, so a pass
/// costs O(unfinished payments), not O(captured) (F2; before, an anti-join
/// on the ledger's transfer ids per captured payment).
///
/// This query crosses tenants and is outside the kernels: it reads ids and
/// statuses, writes nothing, and runs in a read-only transaction. It only
/// says where to look; every change is made per tenant through the engines.
/// It trusts the marker: postings lost from the ledger after a payment was
/// marked (e.g. a ledger restored to an older point) are not found here but
/// by reconciliation and by `POST /v1/recover` (`Orchestrator::find_work`
/// also checks the journal).
pub async fn recovery_work(url: &str, tls: &DbTls) -> Result<Vec<RecoveryWork>, DbError> {
    recovery_work_with(url, tls, false).await
}

/// [`recovery_work`], after [`prune_open_payments`] on the same connection
/// when `prune` (the scheduled pass, `jobs::recover_all`).
pub async fn recovery_work_with(url: &str, tls: &DbTls, prune: bool) -> Result<Vec<RecoveryWork>, DbError> {
    use payment_domain::{AUTHORIZED, CANCELED, CAPTURED, FAILED, PROCESSING};
    // A payment in PROCESSING or AUTHORIZED is never marked, so it is in
    // `open_payments`. Rows there may be marked payments not pruned yet
    // (I1): `NOT p.ledger_done` filters them out.
    let sql = format!(
        "SELECT p.tenant_id, p.id, 0 FROM {PAYMENTS_SCHEMA}.open_payments o \
           JOIN {PAYMENTS_SCHEMA}.payments p ON p.tenant_id = o.tenant_id AND p.id = o.id \
           WHERE NOT p.ledger_done AND (p.status = {PROCESSING} OR (p.status = {AUTHORIZED} AND NOT p.manual_capture)) \
         UNION ALL \
         SELECT tenant_id, id, 1 FROM {PAYMENTS_SCHEMA}.payments WHERE refund_pending > 0 \
         UNION ALL \
         SELECT p.tenant_id, p.id, 2 FROM {PAYMENTS_SCHEMA}.open_payments o \
           JOIN {PAYMENTS_SCHEMA}.payments p ON p.tenant_id = o.tenant_id AND p.id = o.id \
           WHERE NOT p.ledger_done AND p.status IN ({CAPTURED}, {FAILED}, {CANCELED}) \
         ORDER BY 1, 3, 2"
    );
    let (mut client, conn) = connect_direct(url, tls).await?;
    if prune {
        prune_with(&mut client).await?;
    }
    let tx = client.build_transaction().read_only(true).start().await?;
    let rows = match tx.query(sql.as_str(), &[]).await {
        Ok(r) => r,
        // No tables yet: nothing to recover.
        Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    drop(tx);
    drop(client);
    let _ = conn.await;
    let mut out: Vec<RecoveryWork> = Vec::new();
    for r in &rows {
        let (t, id, kind): (i64, i64, i32) = (r.get(0), r.get(1), r.get(2));
        let t = u64::try_from(t).map_err(|_| DbError::Decode(format!("tenant id {t} out of range")))?;
        let id = id as u64;
        if out.last().is_none_or(|w| w.tenant != t) {
            out.push(RecoveryWork { tenant: t, ..Default::default() });
        }
        let w = out.last_mut().expect("pushed");
        match kind {
            0 => w.psp.push(id),
            1 => w.refunds.push(id),
            _ => w.ledger.push(id),
        }
    }
    Ok(out)
}

/// Tenants with unfinished work ([`recovery_work`]).
pub async fn tenants_to_recover(url: &str, tls: &DbTls) -> Result<Vec<u64>, DbError> {
    Ok(recovery_work(url, tls).await?.into_iter().map(|w| w.tenant).collect())
}

/// Tenants with at least one payment (the nightly export's tenants). Like
/// [`recovery_work`], a read-only cross-tenant query outside the kernels.
pub async fn tenants_with_payments(url: &str, tls: &DbTls) -> Result<Vec<u64>, DbError> {
    let sql = format!("SELECT DISTINCT tenant_id FROM {PAYMENTS_SCHEMA}.payments ORDER BY 1");
    let (mut client, conn) = connect_direct(url, tls).await?;
    let tx = client.build_transaction().read_only(true).start().await?;
    let rows = match tx.query(sql.as_str(), &[]).await {
        Ok(r) => r,
        Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    drop(tx);
    drop(client);
    let _ = conn.await;
    rows.iter()
        .map(|r| {
            let t: i64 = r.get(0);
            u64::try_from(t).map_err(|_| DbError::Decode(format!("tenant id {t} out of range")))
        })
        .collect()
}

/// Operator-facing records that are not kernel state: ingested batch
/// reconciliation reports (schema `ops`). Nothing here moves money or is
/// read by a kernel; corrections are still ledger transfers by an operator.
pub mod ops {
    use super::{connect_direct, DbTls};
    use h5i_app_pg::DbError;

    pub const SCHEMA: &str = "ops";

    const DDL: &str = "CREATE SCHEMA IF NOT EXISTS ops;
CREATE TABLE IF NOT EXISTS ops.reconciliation_reports (
  tenant_id BIGINT NOT NULL,
  run TEXT NOT NULL,
  engine TEXT NOT NULL,
  ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  internal_rows BIGINT NOT NULL,
  external_rows BIGINT NOT NULL,
  exceptions BIGINT NOT NULL,
  counts TEXT NOT NULL,
  PRIMARY KEY (tenant_id, run)
);
CREATE TABLE IF NOT EXISTS ops.revocations (
  id BIGSERIAL PRIMARY KEY,
  tenant_id BIGINT NOT NULL,
  jti TEXT,
  actor_kind TEXT,
  actor_id BIGINT,
  before BIGINT,
  expires_at BIGINT,
  revoked_by BIGINT NOT NULL,
  revoked_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  reason TEXT NOT NULL DEFAULT '',
  CHECK (jti IS NOT NULL OR before IS NOT NULL),
  CHECK ((actor_kind IS NULL) = (actor_id IS NULL))
);
CREATE INDEX IF NOT EXISTS revocations_tenant ON ops.revocations (tenant_id, id);
CREATE TABLE IF NOT EXISTS ops.reconciliation_exceptions (
  tenant_id BIGINT NOT NULL,
  run TEXT NOT NULL,
  n INTEGER NOT NULL,
  side TEXT NOT NULL,
  code TEXT NOT NULL,
  key BIGINT NOT NULL,
  amount BIGINT NOT NULL,
  PRIMARY KEY (tenant_id, run, n)
)";

    /// A token revocation (G3), for one tenant. `jti`: that token. `actor`
    /// (`("user" | "operator", id)`) with `before`: that actor's tokens
    /// issued at or before `before` (seconds since the Unix epoch). `before`
    /// alone: every token of the tenant issued then or earlier. v1 tokens
    /// have no issue time and count as issued at 0, so a revocation by
    /// actor or by date covers them whatever its date.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Revocation {
        pub id: i64,
        pub tenant: u64,
        pub jti: Option<String>,
        pub actor: Option<(String, u64)>,
        pub before: Option<u64>,
        /// For a token: when it expires anyway (the entry can be forgotten then).
        pub expires_at: Option<u64>,
        pub revoked_by: u64,
        pub reason: String,
    }

    /// Record a revocation; its id.
    pub async fn revoke(url: &str, tls: &DbTls, r: &Revocation) -> Result<i64, DbError> {
        let (c, conn) = connect(url, tls).await?;
        let t = i64::try_from(r.tenant).map_err(|_| DbError::Decode("tenant id out of range".into()))?;
        let int = |x: Option<u64>| x.map(|v| v as i64);
        let row = c
            .query_one(
                "INSERT INTO ops.revocations (tenant_id, jti, actor_kind, actor_id, before, expires_at, revoked_by, reason) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
                &[
                    &t,
                    &r.jti,
                    &r.actor.as_ref().map(|a| a.0.clone()),
                    &int(r.actor.as_ref().map(|a| a.1)),
                    &int(r.before),
                    &int(r.expires_at),
                    &(r.revoked_by as i64),
                    &r.reason,
                ],
            )
            .await?;
        drop(c);
        let _ = conn.await;
        Ok(row.get(0))
    }

    fn revocation_of(r: &tokio_postgres::Row) -> Revocation {
        let kind: Option<String> = r.get(3);
        let id: Option<i64> = r.get(4);
        Revocation {
            id: r.get(0),
            tenant: r.get::<_, i64>(1) as u64,
            jti: r.get(2),
            actor: kind.zip(id.map(|i| i as u64)),
            before: r.get::<_, Option<i64>>(5).map(|v| v as u64),
            expires_at: r.get::<_, Option<i64>>(6).map(|v| v as u64),
            revoked_by: r.get::<_, i64>(7) as u64,
            reason: r.get(8),
        }
    }

    const REVOCATION_COLS: &str = "id, tenant_id, jti, actor_kind, actor_id, before, expires_at, revoked_by, reason";

    /// Every revocation with an id above `after`, in id order, on an open
    /// connection (the API's refresh task keeps one).
    pub async fn revocations_since(c: &tokio_postgres::Client, after: i64) -> Result<Vec<Revocation>, DbError> {
        let sql = format!("SELECT {REVOCATION_COLS} FROM ops.revocations WHERE id > $1 ORDER BY id");
        Ok(c.query(sql.as_str(), &[&after]).await?.iter().map(revocation_of).collect())
    }

    /// A tenant's revocations, newest first, at most `limit`.
    pub async fn revocations_of(url: &str, tls: &DbTls, tenant: u64, limit: i64) -> Result<Vec<Revocation>, DbError> {
        let (c, conn) = connect(url, tls).await?;
        let sql = format!("SELECT {REVOCATION_COLS} FROM ops.revocations WHERE tenant_id = $1 ORDER BY id DESC LIMIT $2");
        let rows = c.query(sql.as_str(), &[&(tenant as i64), &limit]).await?;
        drop(c);
        let _ = conn.await;
        Ok(rows.iter().map(revocation_of).collect())
    }

    /// A connection for [`revocations_since`].
    pub async fn connect_ops(url: &str, tls: &DbTls) -> Result<(tokio_postgres::Client, tokio::task::JoinHandle<()>), DbError> {
        connect(url, tls).await
    }

    /// One exception row of a worker report.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Exception {
        pub side: String,
        pub code: String,
        pub key: u64,
        pub amount: u64,
    }

    /// A worker report (`h5i-pay-batch reconcile`'s JSON), as ingested.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Report {
        pub tenant: u64,
        pub run: String,
        pub engine: String,
        pub internal_rows: u64,
        pub external_rows: u64,
        /// The two per-code count objects, as the worker wrote them.
        pub counts: String,
        /// Every exception the report lists.
        pub exceptions_total: u64,
        /// The exceptions (on read, at most the requested number).
        pub exceptions: Vec<Exception>,
    }

    async fn connect(url: &str, tls: &DbTls) -> Result<(tokio_postgres::Client, tokio::task::JoinHandle<()>), DbError> {
        connect_direct(url, tls).await
    }

    pub async fn install(url: &str, tls: &DbTls) -> Result<(), DbError> {
        let (c, conn) = connect(url, tls).await?;
        c.batch_execute(DDL).await?;
        drop(c);
        let _ = conn.await;
        Ok(())
    }

    /// Store a report once: ingesting the same `(tenant, run)` again changes
    /// nothing (`false`).
    pub async fn ingest(url: &str, tls: &DbTls, r: &Report) -> Result<bool, DbError> {
        let (mut c, conn) = connect(url, tls).await?;
        let tx = c.transaction().await?;
        let tenant = i64::try_from(r.tenant).map_err(|_| DbError::Decode("tenant id out of range".into()))?;
        let n = tx
            .execute(
                "INSERT INTO ops.reconciliation_reports (tenant_id, run, engine, internal_rows, external_rows, exceptions, counts) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (tenant_id, run) DO NOTHING",
                &[&tenant, &r.run, &r.engine, &(r.internal_rows as i64), &(r.external_rows as i64), &(r.exceptions_total as i64), &r.counts],
            )
            .await?;
        if n == 1 {
            let stmt = tx
                .prepare("INSERT INTO ops.reconciliation_exceptions (tenant_id, run, n, side, code, key, amount) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                .await?;
            for (i, e) in r.exceptions.iter().enumerate() {
                tx.execute(&stmt, &[&tenant, &r.run, &(i as i32), &e.side, &e.code, &(e.key as i64), &(e.amount as i64)]).await?;
            }
        }
        tx.commit().await?;
        drop(c);
        let _ = conn.await;
        Ok(n == 1)
    }

    /// The tenant's report for `run`, or its latest; with at most `limit`
    /// exceptions.
    pub async fn report(url: &str, tls: &DbTls, tenant: u64, run: Option<&str>, limit: i64) -> Result<Option<Report>, DbError> {
        let (c, conn) = connect(url, tls).await?;
        let t = tenant as i64;
        let row = match run {
            Some(run) => {
                c.query_opt(
                    "SELECT run, engine, internal_rows, external_rows, counts, exceptions FROM ops.reconciliation_reports WHERE tenant_id = $1 AND run = $2",
                    &[&t, &run],
                )
                .await
            }
            None => {
                c.query_opt(
                    "SELECT run, engine, internal_rows, external_rows, counts, exceptions FROM ops.reconciliation_reports WHERE tenant_id = $1 \
                     ORDER BY ingested_at DESC, run DESC LIMIT 1",
                    &[&t],
                )
                .await
            }
        };
        let row = match row {
            Ok(r) => r,
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => None,
            Err(e) => return Err(e.into()),
        };
        let Some(row) = row else {
            drop(c);
            let _ = conn.await;
            return Ok(None);
        };
        let run: String = row.get(0);
        let ex = c
            .query(
                "SELECT side, code, key, amount FROM ops.reconciliation_exceptions WHERE tenant_id = $1 AND run = $2 ORDER BY n LIMIT $3",
                &[&t, &run, &limit],
            )
            .await?;
        let exceptions = ex
            .iter()
            .map(|e| Exception { side: e.get(0), code: e.get(1), key: e.get::<_, i64>(2) as u64, amount: e.get::<_, i64>(3) as u64 })
            .collect();
        let out = Report {
            tenant,
            run,
            engine: row.get(1),
            internal_rows: row.get::<_, i64>(2) as u64,
            external_rows: row.get::<_, i64>(3) as u64,
            counts: row.get(4),
            exceptions_total: row.get::<_, i64>(5) as u64,
            exceptions,
        };
        drop(c);
        let _ = conn.await;
        Ok(Some(out))
    }
}
