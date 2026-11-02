//! The ledger kernel's shell: PostgreSQL tables, idempotent replies and JSON.
//! It decides nothing; every decision is `payment_ledger::transition`.
// The `schema!` mapping generates loaders over the kernel's `&Vec` types.
#![allow(clippy::ptr_arg)]

use axum::http::StatusCode;
use h5i_app_core::{Kernel, TenantId};
use h5i_app_json::Value as Out;
use h5i_app_pg::{DbError, Lock, ReplyCodec, Store, Tx};
use payment_ledger as k;
use serde::{Deserialize, Serialize};

/// Marker type for the ledger kernel.
pub struct LedgerApp;

impl Kernel for LedgerApp {
    type Principal = k::Principal;
    type Snapshot = k::Snapshot;
    type Command = k::Command;
    type WriteSet = Vec<k::Write>;
    type Reply = k::Reply;
    type Error = k::Error;

    fn tenant(actor: &k::Principal) -> TenantId {
        TenantId(actor.org)
    }

    fn transition(actor: &k::Principal, snap: &k::Snapshot, cmd: &k::Command) -> Result<(Vec<k::Write>, k::Reply), k::Error> {
        k::transition(actor, snap, cmd)
    }

    fn apply(snap: &k::Snapshot, ws: &Vec<k::Write>) -> k::Snapshot {
        k::apply(snap, ws)
    }

    /// The transaction's clock, for the history (G1).
    fn stamp(actor: &mut k::Principal, now: h5i_app_core::Timestamp) {
        actor.at = now.0;
    }

    /// Proven sound: `payment_ledger.ReadOnly.sound` (gated).
    fn read_only(cmd: &k::Command) -> bool {
        k::read_only(cmd)
    }
}

/// User `user` of tenant `org`.
pub fn user(org: u64, user: u64) -> k::Principal {
    k::Principal { org, actor: k::Actor::User(user), at: 0 }
}

/// Operator `id` of tenant `org`.
pub fn operator(org: u64, id: u64) -> k::Principal {
    k::Principal { org, actor: k::Actor::Operator(id), at: 0 }
}

// The `accounts`, `transfers` and `ledger` tables, from the kernel's `schema!`.
payment_ledger::ledger_tables!(LedgerApp);

pub struct LedgerStore;

/// G4: one history entry (`command` = `CMD_MIGRATED`, 3) for each leg of a
/// transfer that has none (transfers posted before G1), appended after the
/// account's existing entries in transfer-id order (debit leg first), and
/// the accounts' `changes` raised to match. Such an entry records the
/// leg's amount as its `debits_after` or `credits_after` (`…_before` 0),
/// not a balance, `at` 0 (unknown), and as actor the transfer's operator,
/// or the debit account's owner for a transfer a user posted (only the
/// owner may debit). Runs once (`h5i_pay_migrations`).
const JOURNAL_BACKFILL: &str = "DO $$ BEGIN
IF NOT EXISTS (SELECT 1 FROM h5i_pay_migrations WHERE name = 'g4-journal-history') THEN
  WITH legs AS (
    SELECT t.tenant_id, t.debit AS account, t.id AS transfer, t.code, t.amount, true AS is_debit, t.operator, t.debit AS payer FROM transfers t
    UNION ALL
    SELECT t.tenant_id, t.credit, t.id, t.code, t.amount, false, t.operator, t.debit FROM transfers t
  ), missing AS (
    SELECT l.* FROM legs l WHERE NOT EXISTS
      (SELECT 1 FROM history h WHERE h.tenant_id = l.tenant_id AND h.account = l.account AND h.transfer = l.transfer)
  ), numbered AS (
    SELECT m.*, a.changes + row_number() OVER (PARTITION BY m.tenant_id, m.account ORDER BY m.transfer, m.is_debit DESC) - 1 AS n,
           p.owner AS payer_owner
    FROM missing m
    JOIN accounts a ON a.tenant_id = m.tenant_id AND a.id = m.account
    JOIN accounts p ON p.tenant_id = m.tenant_id AND p.id = m.payer
  ), ins AS (
    INSERT INTO history (tenant_id, account, n, by_operator, actor, at, command, transfer, code,
                         debits_before, debits_after, credits_before, credits_after)
    SELECT tenant_id, account, n, operator <> 0, CASE WHEN operator <> 0 THEN operator ELSE payer_owner END, 0, 3,
           transfer, code, 0, CASE WHEN is_debit THEN amount ELSE 0 END, 0, CASE WHEN is_debit THEN 0 ELSE amount END
    FROM numbered
    RETURNING tenant_id, account
  )
  UPDATE accounts a SET changes = a.changes + c.k
    FROM (SELECT tenant_id, account, count(*) AS k FROM ins GROUP BY 1, 2) c
    WHERE a.tenant_id = c.tenant_id AND a.id = c.account;
  INSERT INTO h5i_pay_migrations (name) VALUES ('g4-journal-history');
END IF;
END $$";

impl Store<LedgerApp> for LedgerStore {
    /// The kernel's tables, and an index for the per-owner read plan
    /// (`Accounts` for a user). Tables made before a column existed get it
    /// (`crate::add_column`).
    fn ddl() -> Vec<String> {
        let mut ddl = schema_ddl();
        // SEC-02 (F1): who posted a transfer. Older rows say 0 (unknown).
        ddl.push(crate::add_column("transfers", "operator", "BIGINT NOT NULL DEFAULT 0"));
        // F3: accounts numbered per owner, and the owners' counters.
        ddl.extend(crate::numbering("accounts", "owner", "owner_counters"));
        ddl.push("DROP INDEX IF EXISTS accounts_owner".into());
        ddl.push("CREATE INDEX IF NOT EXISTS accounts_owner_seq ON accounts (tenant_id, owner, seq)".into());
        // G1: each account's count of history entries; older accounts start
        // at 0 (their history begins with their next change). The history
        // table itself is `schema_ddl`'s, append-only.
        ddl.push(crate::add_column("accounts", "changes", "BIGINT NOT NULL DEFAULT 0"));
        ddl.extend(crate::append_only_history());
        // G4: the journal is paged per account through the history, so
        // transfers posted before the history existed get entries, once.
        ddl.push(crate::MIGRATIONS_DDL.into());
        ddl.push(JOURNAL_BACKFILL.into());
        // G2: the system operators' keys move out of the shared `u0`.
        ddl.push(crate::idempotency::migrate_system_keys());
        ddl
    }

    fn tables() -> Vec<&'static str> {
        schema_tables()
    }

    async fn load(tx: &Tx<'_>, t: TenantId) -> Result<k::Snapshot, DbError> {
        schema_load(tx, t).await
    }

    async fn write(tx: &Tx<'_>, t: TenantId, ws: &Vec<k::Write>) -> Result<(), DbError> {
        schema_store(tx, t, ws).await
    }

    /// The rows `k::reads` names, one declarative read per table (every
    /// command has a plan since F3). `proofs/ledger/Ledger/Frame.lean`
    /// (`Frame.load_for_sound`): `transition` gives the same result on them
    /// as on `load`.
    async fn load_for(tx: &Tx<'_>, t: TenantId, actor: &k::Principal, cmd: &k::Command) -> Result<k::Snapshot, DbError> {
        match k::reads(actor, cmd) {
            None => schema_load(tx, t).await,
            Some(plan) => schema_load_plan(tx, t, &plan).await,
        }
    }

    /// Writes queue on the rows they change: a transfer on its two accounts
    /// and its id, opening an account on the counter. Only a hint for fewer
    /// retries; SERIALIZABLE makes any interleaving serial.
    fn lock_for(_actor: &k::Principal, cmd: &k::Command) -> Lock {
        fn transfer_keys(t: &k::TransferReq, out: &mut Vec<u64>) {
            out.push(Lock::key("account", &t.debit.to_be_bytes()));
            out.push(Lock::key("account", &t.credit.to_be_bytes()));
            out.push(Lock::key("transfer", &t.id.to_be_bytes()));
        }
        match cmd {
            k::Command::Open { .. } => Lock::Keys(vec![Lock::key("ledger", &[])]),
            k::Command::Transfer { t } => {
                let mut keys = Vec::new();
                transfer_keys(t, &mut keys);
                Lock::Keys(keys)
            }
            k::Command::Batch { transfers } => {
                let mut keys = Vec::new();
                transfers.iter().take(k::MAX_BATCH).for_each(|t| transfer_keys(t, &mut keys));
                keys.sort_unstable();
                keys.dedup();
                Lock::Keys(keys)
            }
            // Reads write nothing; they need no queue.
            k::Command::Account { .. }
            | k::Command::Accounts { .. }
            | k::Command::Transfers { .. }
            | k::Command::AccountRange { .. }
            | k::Command::History { .. }
            | k::Command::TransferRange { .. } => Lock::None,
        }
    }
}

/// The JSON a client sends to `POST /v1/ledger`.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandJson {
    Open {
        /// Defaults to the caller.
        #[serde(default)]
        owner: Option<u64>,
        /// ISO 4217 numeric code; anything past `u32::MAX` is a 400.
        currency: u32,
        #[serde(default)]
        code: u32,
        #[serde(default)]
        no_overdraft: bool,
        #[serde(default)]
        no_overcredit: bool,
    },
    Transfer(TransferJson),
    Batch { transfers: Vec<TransferJson> },
    Account { id: u64 },
    /// A page of an owner's accounts (default: the caller's), from
    /// sequence number `from` (default 0), at most `limit` (default and
    /// maximum `k::MAX_PAGE`).
    Accounts {
        #[serde(default)]
        owner: Option<u64>,
        #[serde(default)]
        from: u64,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// A page of an account's journal (operators only, G4): the transfers
    /// of its history entries `from` (default 0) on, at most `limit`
    /// (default and maximum `k::MAX_PAGE`).
    Transfers {
        account: u64,
        #[serde(default)]
        from: u64,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// A page of the tenant's accounts by id (operators only, G4).
    AccountsById {
        #[serde(default)]
        from: u64,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// A page of an account's history (operators only, G1), from entry
    /// `from` (default 0), at most `limit` (default and maximum `k::MAX_PAGE`).
    History {
        account: u64,
        #[serde(default)]
        from: u64,
        #[serde(default)]
        limit: Option<u32>,
    },
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug)]
#[serde(deny_unknown_fields)]
pub struct TransferJson {
    pub id: u64,
    pub debit: u64,
    pub credit: u64,
    pub amount: u64,
    #[serde(default)]
    pub code: u32,
}

impl From<TransferJson> for k::TransferReq {
    fn from(t: TransferJson) -> Self {
        k::TransferReq { id: t.id, debit: t.debit, credit: t.credit, amount: t.amount, code: t.code }
    }
}

impl CommandJson {
    /// The kernel command, with defaults filled in from the caller.
    pub fn into_command(self, actor: &k::Principal) -> k::Command {
        match self {
            CommandJson::Open { owner, currency, code, no_overdraft, no_overcredit } => {
                k::Command::Open { owner: owner.unwrap_or(k::user_of(actor)), currency, code, no_overdraft, no_overcredit }
            }
            CommandJson::Transfer(t) => k::Command::Transfer { t: t.into() },
            CommandJson::Batch { transfers } => k::Command::Batch { transfers: transfers.into_iter().map(Into::into).collect() },
            CommandJson::Account { id } => k::Command::Account { id },
            CommandJson::Accounts { owner, from, limit } => {
                k::Command::Accounts { owner: owner.unwrap_or(k::user_of(actor)), from, limit: limit.unwrap_or(k::MAX_PAGE) }
            }
            CommandJson::Transfers { account, from, limit } => k::Command::Transfers { account, from, limit: limit.unwrap_or(k::MAX_PAGE) },
            CommandJson::AccountsById { from, limit } => k::Command::AccountRange { from, limit: limit.unwrap_or(k::MAX_PAGE) },
            CommandJson::History { account, from, limit } => k::Command::History { account, from, limit: limit.unwrap_or(k::MAX_PAGE) },
        }
    }
}

/// Client transfer ids must leave the top bit clear: ids with it set are
/// derived by the orchestrator from payment and refund ids.
pub const SYSTEM_TRANSFER_BIT: u64 = 1 << 63;

/// Ledger transfer kinds the orchestrator posts.
pub const POST_CAPTURE: u32 = 1;
pub const POST_REFUND_RESERVE: u32 = 2;
pub const POST_REFUND_SETTLE: u32 = 3;
pub const POST_REFUND_RELEASE: u32 = 4;

/// The ledger transfer id of posting `kind` for payment or refund `id`.
pub fn system_transfer_id(kind: u32, id: u64) -> u64 {
    SYSTEM_TRANSFER_BIT | (u64::from(kind & 0x7f) << 56) | (id & ((1 << 56) - 1))
}

/// Account codes from here up are the orchestrator's (PSP clearing,
/// merchant payable, refund holding); only operators may open them.
pub const SYSTEM_ACCOUNT_CODES: u32 = 1000;

/// Decode a client's ledger command. `Api::decode_command` has no caller,
/// so the HTTP handler uses this instead.
pub fn decode_for(actor: &k::Principal, body: serde_json::Value) -> Result<k::Command, String> {
    let c = serde_json::from_value::<CommandJson>(body).map_err(|e| e.to_string())?;
    let check = |t: &TransferJson| {
        if t.id & SYSTEM_TRANSFER_BIT != 0 && !k::is_operator(actor) {
            Err(format!("transfer id {} is reserved (top bit set)", t.id))
        } else {
            Ok(())
        }
    };
    match &c {
        CommandJson::Open { code, .. } if *code >= SYSTEM_ACCOUNT_CODES && !k::is_operator(actor) => {
            return Err(format!("account code {code} is reserved"));
        }
        CommandJson::Transfer(t) => check(t)?,
        CommandJson::Batch { transfers } => transfers.iter().try_for_each(check)?,
        _ => {}
    }
    Ok(c.into_command(actor))
}

pub fn account_json(a: &k::Account) -> Out {
    Out::obj([
        ("id", a.id.into()),
        ("owner", a.owner.into()),
        ("currency", u64::from(a.currency).into()),
        ("code", u64::from(a.code).into()),
        ("no_overdraft", a.no_overdraft.into()),
        ("no_overcredit", a.no_overcredit.into()),
        ("debits", a.debits.into()),
        ("credits", a.credits.into()),
        ("seq", a.seq.into()),
        ("changes", a.changes.into()),
    ])
}

/// One history entry (G1). `at` is microseconds since the Unix epoch, the
/// transaction's clock.
pub fn history_json(h: &k::HistoryEntry) -> Out {
    Out::obj([
        ("account", h.account.into()),
        ("n", h.n.into()),
        ("actor", Out::obj([("kind", if h.by_operator { "operator" } else { "user" }.into()), ("id", h.actor.into())])),
        ("at", h.at.into()),
        ("command", if h.command == k::CMD_OPEN { "open" } else { "post" }.into()),
        ("transfer", if h.command == k::CMD_OPEN { Out::Null } else { h.transfer.into() }),
        ("code", u64::from(h.code).into()),
        ("debits_before", h.debits_before.into()),
        ("debits_after", h.debits_after.into()),
        ("credits_before", h.credits_before.into()),
        ("credits_after", h.credits_after.into()),
    ])
}

pub fn transfer_json(t: &k::Transfer) -> Out {
    Out::obj([
        ("id", t.id.into()),
        ("debit", t.debit.into()),
        ("credit", t.credit.into()),
        ("amount", t.amount.into()),
        ("currency", u64::from(t.currency).into()),
        ("code", u64::from(t.code).into()),
        ("operator", t.operator.into()),
    ])
}

pub fn error_code(e: &k::Error) -> (StatusCode, &'static str) {
    match e {
        k::Error::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        k::Error::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
        k::Error::InvalidId => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_id"),
        k::Error::ZeroAmount => (StatusCode::UNPROCESSABLE_ENTITY, "zero_amount"),
        k::Error::SameAccount => (StatusCode::UNPROCESSABLE_ENTITY, "same_account"),
        k::Error::CurrencyMismatch => (StatusCode::UNPROCESSABLE_ENTITY, "currency_mismatch"),
        k::Error::Insufficient => (StatusCode::CONFLICT, "insufficient_funds"),
        k::Error::ExceedsCredit => (StatusCode::CONFLICT, "exceeds_credit"),
        k::Error::Overflow => (StatusCode::UNPROCESSABLE_ENTITY, "overflow"),
        k::Error::IdConflict => (StatusCode::CONFLICT, "transfer_id_conflict"),
        k::Error::InvalidFlags => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_flags"),
        k::Error::EmptyBatch => (StatusCode::UNPROCESSABLE_ENTITY, "empty_batch"),
        k::Error::BatchTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "batch_too_large"),
        k::Error::BadPage => (StatusCode::BAD_REQUEST, "bad_page"),
    }
}

impl h5i_app_http::Api<LedgerApp> for LedgerStore {
    /// Without the caller: `owner` defaults to the tenant (0). The server
    /// routes ledger requests through [`decode_for`] instead.
    fn decode_command(body: serde_json::Value) -> Result<k::Command, String> {
        decode_for(&operator(0, crate::ORCHESTRATOR_OPERATOR), body)
    }

    fn encode_reply(r: &k::Reply) -> Out {
        match r {
            k::Reply::Created(id) => Out::obj([("id", (*id).into())]),
            k::Reply::Posted(id) => Out::obj([("id", (*id).into()), ("status", "posted".into())]),
            k::Reply::Exists(id) => Out::obj([("id", (*id).into()), ("status", "exists".into())]),
            k::Reply::Batch(n) => Out::obj([("posted", (*n).into())]),
            k::Reply::Account(a) => account_json(a),
            k::Reply::Accounts(accs, next) => Out::obj([
                ("accounts", Out::Arr(accs.iter().map(account_json).collect())),
                ("next", next.map_or(Out::Null, Out::from)),
            ]),
            k::Reply::Transfers(ts, next) => Out::obj([
                ("transfers", Out::Arr(ts.iter().map(transfer_json).collect())),
                ("next", next.map_or(Out::Null, Out::from)),
            ]),
            k::Reply::History(hs, next) => Out::obj([
                ("history", Out::Arr(hs.iter().map(history_json).collect())),
                ("next", next.map_or(Out::Null, Out::from)),
            ]),
        }
    }

    fn encode_error(e: &k::Error) -> (StatusCode, Out) {
        let (status, code) = error_code(e);
        (status, h5i_app_http::error_body(code))
    }
}

/// Stored replies for idempotent retries (`Idempotency-Key`).
#[derive(Serialize, Deserialize)]
enum StoredReply {
    Created(u64),
    Posted(u64),
    Exists(u64),
    Batch(u64),
    // Reads are stored too (a keyed read is a write of its reply).
    Account(StoredAccount),
    /// Before F3 (unpaged); decoded as the last page.
    Accounts(Vec<StoredAccount>),
    AccountPage(Vec<StoredAccount>, Option<u64>),
    /// Before SEC-02's operator column; decoded with operator 0.
    Transfers(Vec<(u64, u64, u64, u64, u32, u32)>),
    TransfersBy(Vec<(u64, u64, u64, u64, u32, u32, u64)>),
    /// G4: a page of an account's journal.
    TransferPage(Vec<(u64, u64, u64, u64, u32, u32, u64)>, Option<u64>),
    /// G1: a history page.
    History(Vec<StoredEntry>, Option<u64>),
}

/// A history entry in a stored reply.
type StoredEntry = (u64, u64, bool, u64, u64, u32, u64, u32, [u64; 4]);

fn store_entry(h: &k::HistoryEntry) -> StoredEntry {
    (
        h.account,
        h.n,
        h.by_operator,
        h.actor,
        h.at,
        h.command,
        h.transfer,
        h.code,
        [h.debits_before, h.debits_after, h.credits_before, h.credits_after],
    )
}

fn load_entry(e: StoredEntry) -> k::HistoryEntry {
    let (account, n, by_operator, actor, at, command, transfer, code, [debits_before, debits_after, credits_before, credits_after]) = e;
    k::HistoryEntry { account, n, by_operator, actor, at, command, transfer, code, debits_before, debits_after, credits_before, credits_after }
}

/// An account in a stored reply. Replies stored before F3 are arrays
/// without `seq`; serde reads them positionally, `seq` 0.
#[derive(Serialize, Deserialize)]
struct StoredAccount {
    id: u64,
    owner: u64,
    currency: u32,
    code: u32,
    no_overdraft: bool,
    no_overcredit: bool,
    debits: u64,
    credits: u64,
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    changes: u64,
}

fn store_account(a: &k::Account) -> StoredAccount {
    StoredAccount {
        id: a.id,
        owner: a.owner,
        currency: a.currency,
        code: a.code,
        no_overdraft: a.no_overdraft,
        no_overcredit: a.no_overcredit,
        debits: a.debits,
        credits: a.credits,
        seq: a.seq,
        changes: a.changes,
    }
}

fn load_account(a: StoredAccount) -> k::Account {
    k::Account {
        id: a.id,
        owner: a.owner,
        currency: a.currency,
        code: a.code,
        no_overdraft: a.no_overdraft,
        no_overcredit: a.no_overcredit,
        debits: a.debits,
        credits: a.credits,
        seq: a.seq,
        changes: a.changes,
    }
}

impl ReplyCodec<LedgerApp> for LedgerStore {
    fn fingerprint(cmd: &k::Command) -> Vec<u8> {
        format!("{cmd:?}").into_bytes()
    }

    /// Idempotency keys are per principal (G2): `u<id>` for a user,
    /// `o<id>` for an operator, the system operators included. Two actors'
    /// keys never collide, and no actor replays another's reply. Before G2
    /// every operator used `u0`; see [`crate::idempotency`].
    fn scope(actor: &k::Principal) -> String {
        crate::idempotency::scope(k::is_operator(actor), k::actor_id(actor))
    }

    fn encode(r: &k::Reply) -> Vec<u8> {
        let stored = match r {
            k::Reply::Created(id) => StoredReply::Created(*id),
            k::Reply::Posted(id) => StoredReply::Posted(*id),
            k::Reply::Exists(id) => StoredReply::Exists(*id),
            k::Reply::Batch(n) => StoredReply::Batch(*n),
            k::Reply::Account(a) => StoredReply::Account(store_account(a)),
            k::Reply::Accounts(accs, next) => StoredReply::AccountPage(accs.iter().map(store_account).collect(), *next),
            k::Reply::Transfers(ts, next) => StoredReply::TransferPage(
                ts.iter().map(|t| (t.id, t.debit, t.credit, t.amount, t.currency, t.code, t.operator)).collect(),
                *next,
            ),
            k::Reply::History(hs, next) => StoredReply::History(hs.iter().map(store_entry).collect(), *next),
        };
        serde_json::to_vec(&stored).expect("stored replies serialize")
    }

    fn decode(b: &[u8]) -> Result<k::Reply, String> {
        Ok(match serde_json::from_slice(b).map_err(|e| e.to_string())? {
            StoredReply::Created(id) => k::Reply::Created(id),
            StoredReply::Posted(id) => k::Reply::Posted(id),
            StoredReply::Exists(id) => k::Reply::Exists(id),
            StoredReply::Batch(n) => k::Reply::Batch(n),
            StoredReply::Account(a) => k::Reply::Account(load_account(a)),
            StoredReply::Accounts(accs) => k::Reply::Accounts(accs.into_iter().map(load_account).collect(), None),
            StoredReply::AccountPage(accs, next) => k::Reply::Accounts(accs.into_iter().map(load_account).collect(), next),
            // Before G4 the journal was one unpaged reply: decoded as the last page.
            StoredReply::Transfers(ts) => k::Reply::Transfers(
                ts.into_iter()
                    .map(|(id, debit, credit, amount, currency, code)| k::Transfer { id, debit, credit, amount, currency, code, operator: 0 })
                    .collect(),
                None,
            ),
            StoredReply::TransfersBy(ts) => k::Reply::Transfers(
                ts.into_iter()
                    .map(|(id, debit, credit, amount, currency, code, operator)| k::Transfer { id, debit, credit, amount, currency, code, operator })
                    .collect(),
                None,
            ),
            StoredReply::TransferPage(ts, next) => k::Reply::Transfers(
                ts.into_iter()
                    .map(|(id, debit, credit, amount, currency, code, operator)| k::Transfer { id, debit, credit, amount, currency, code, operator })
                    .collect(),
                next,
            ),
            StoredReply::History(hs, next) => k::Reply::History(hs.into_iter().map(load_entry).collect(), next),
        })
    }
}

/// Per-currency totals of a snapshot, for audits and tests: (debits, credits, journal).
pub fn currency_totals(s: &k::Snapshot, currency: u32) -> (u128, u128, u128) {
    let d = s.accounts.iter().filter(|a| a.currency == currency).map(|a| u128::from(a.debits)).sum();
    let c = s.accounts.iter().filter(|a| a.currency == currency).map(|a| u128::from(a.credits)).sum();
    let j = s.transfers.iter().filter(|t| t.currency == currency).map(|t| u128::from(t.amount)).sum();
    (d, c, j)
}

/// The invariants the Lean proofs establish, checked on a loaded snapshot:
/// per-currency debits = credits = journal total, and no constrained
/// account past its limit. Used by tests and by the audit endpoint.
pub fn audit(s: &k::Snapshot) -> Result<(), String> {
    let mut currencies: Vec<u32> = s.accounts.iter().map(|a| a.currency).chain(s.transfers.iter().map(|t| t.currency)).collect();
    currencies.sort_unstable();
    currencies.dedup();
    for c in currencies {
        let (d, cr, j) = currency_totals(s, c);
        if d != cr || d != j {
            return Err(format!("currency {c}: debits {d}, credits {cr}, journal {j}"));
        }
    }
    for a in &s.accounts {
        if a.no_overdraft && a.debits > a.credits {
            return Err(format!("account {} overdrawn: debits {} > credits {}", a.id, a.debits, a.credits));
        }
        if a.no_overcredit && a.credits > a.debits {
            return Err(format!("account {} overcredited: credits {} > debits {}", a.id, a.credits, a.debits));
        }
    }
    Ok(())
}

// ------------------------------------------------------------- paged reads (G4)

fn refused(e: k::Error) -> DbError {
    DbError::Decode(format!("ledger refused a page: {e:?}"))
}

/// Entries `from` up to `upto` (all if `None`) of `account`'s journal,
/// read a page (at most `k::MAX_PAGE` entries) per kernel command.
/// `actor` must be an operator. Also returns the number of pages read.
pub async fn journal_range(
    engine: &crate::LedgerEngine,
    actor: &k::Principal,
    account: u64,
    from: u64,
    upto: Option<u64>,
) -> Result<(Vec<k::Transfer>, u64), DbError> {
    let mut out = Vec::new();
    let mut at = from;
    let mut pages = 0;
    loop {
        let limit = match upto {
            Some(u) if u <= at => break,
            Some(u) => (u - at).min(u64::from(k::MAX_PAGE)) as u32,
            None => k::MAX_PAGE,
        };
        pages += 1;
        match engine.execute(actor, &k::Command::Transfers { account, from: at, limit }).await?.map_err(refused)? {
            k::Reply::Transfers(ts, next) => {
                out.extend(ts);
                match next {
                    Some(n) => at = n,
                    None => break,
                }
            }
            r => return Err(DbError::Decode(format!("journal page replied {r:?}"))),
        }
    }
    Ok((out, pages))
}

/// All of `account`'s journal, paged.
pub async fn journal(engine: &crate::LedgerEngine, actor: &k::Principal, account: u64) -> Result<Vec<k::Transfer>, DbError> {
    Ok(journal_range(engine, actor, account, 0, None).await?.0)
}

/// Every page of a paged account listing.
async fn all_pages(engine: &crate::LedgerEngine, actor: &k::Principal, cmd: impl Fn(u64) -> k::Command) -> Result<(Vec<k::Account>, u64), DbError> {
    let mut out = Vec::new();
    let mut from = 0;
    let mut pages = 0;
    loop {
        pages += 1;
        match engine.execute(actor, &cmd(from)).await?.map_err(refused)? {
            k::Reply::Accounts(accs, next) => {
                out.extend(accs);
                match next {
                    Some(n) => from = n,
                    None => return Ok((out, pages)),
                }
            }
            r => return Err(DbError::Decode(format!("account page replied {r:?}"))),
        }
    }
}

/// All of `owner`'s accounts, paged (an operator, or the owner itself).
pub async fn accounts_of(engine: &crate::LedgerEngine, actor: &k::Principal, owner: u64) -> Result<Vec<k::Account>, DbError> {
    Ok(all_pages(engine, actor, |from| k::Command::Accounts { owner, from, limit: k::MAX_PAGE }).await?.0)
}

/// What a paged audit read and found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuditReport {
    pub accounts: u64,
    pub transfers: u64,
    /// Kernel reads (pages) the audit took.
    pub pages: u64,
    /// Transfers seen on one side only because they were posted while the
    /// audit ran (found on the other side's journal past what it had read).
    pub in_flight: u64,
}

/// The audit of [`audit`], from paged reads instead of one snapshot (G4):
/// every account (by id), and each account's journal up to the number of
/// entries it had when read, so each account is checked against its own
/// journal at one point: its debits are the sum of the transfers it is the
/// debit account of, its credits likewise, every transfer is in its
/// account's currency, and the limits hold. Then every transfer must be in
/// the journals of both its accounts; one seen on one side only is looked
/// up past the other account's bound (posted during the audit), and only
/// if it is not there either is it an error. Per currency, debits, credits
/// (counting those transfers on both sides) and the journal total agree.
/// `actor` must be an operator. `Ok(Err(..))`: an invariant is violated.
pub async fn audit_paged(engine: &crate::LedgerEngine, actor: &k::Principal) -> Result<Result<AuditReport, String>, DbError> {
    use std::collections::{BTreeMap, HashMap};
    let mut rep = AuditReport::default();
    let (accounts, pages) = all_pages(engine, actor, |from| k::Command::AccountRange { from, limit: k::MAX_PAGE }).await?;
    rep.pages += pages;
    rep.accounts = accounts.len() as u64;
    let bound: HashMap<u64, u64> = accounts.iter().map(|a| (a.id, a.changes)).collect();
    let mut debit_side: HashMap<u64, k::Transfer> = HashMap::new();
    let mut credit_side: HashMap<u64, k::Transfer> = HashMap::new();
    // currency -> (debits, credits)
    let mut sums: BTreeMap<u32, (u128, u128)> = BTreeMap::new();
    for a in &accounts {
        if a.no_overdraft && a.debits > a.credits {
            return Ok(Err(format!("account {} overdrawn: debits {} > credits {}", a.id, a.debits, a.credits)));
        }
        if a.no_overcredit && a.credits > a.debits {
            return Ok(Err(format!("account {} overcredited: credits {} > debits {}", a.id, a.credits, a.debits)));
        }
        let (ts, pages) = journal_range(engine, actor, a.id, 0, Some(a.changes)).await?;
        rep.pages += pages;
        let (mut d, mut c) = (0u128, 0u128);
        for t in ts {
            if t.currency != a.currency {
                return Ok(Err(format!("transfer {} in currency {} on account {} of currency {}", t.id, t.currency, a.id, a.currency)));
            }
            if t.debit == a.id {
                d += u128::from(t.amount);
                debit_side.insert(t.id, t);
            } else if t.credit == a.id {
                c += u128::from(t.amount);
                credit_side.insert(t.id, t);
            } else {
                return Ok(Err(format!("journal of account {} lists transfer {}, which is not its", a.id, t.id)));
            }
        }
        if d != u128::from(a.debits) || c != u128::from(a.credits) {
            return Ok(Err(format!("account {}: debits {} credits {}, its journal {d} / {c}", a.id, a.debits, a.credits)));
        }
        let e = sums.entry(a.currency).or_default();
        e.0 += u128::from(a.debits);
        e.1 += u128::from(a.credits);
    }
    // Transfers on one side only: posted after the other account was read?
    let mut late: HashMap<u64, Vec<u64>> = HashMap::new();
    let one_sided: Vec<(k::Transfer, bool)> = debit_side
        .values()
        .filter(|t| !credit_side.contains_key(&t.id))
        .map(|t| (*t, true))
        .chain(credit_side.values().filter(|t| !debit_side.contains_key(&t.id)).map(|t| (*t, false)))
        .collect();
    for (t, seen_debit) in &one_sided {
        let other = if *seen_debit { t.credit } else { t.debit };
        let ids = match late.get(&other) {
            Some(ids) => ids.clone(),
            None => {
                let from = bound.get(&other).copied().unwrap_or(0);
                let (ts, pages) = journal_range(engine, actor, other, from, None).await?;
                rep.pages += pages;
                let ids: Vec<u64> = ts.iter().map(|x| x.id).collect();
                late.insert(other, ids.clone());
                ids
            }
        };
        if !ids.contains(&t.id) {
            return Ok(Err(format!("transfer {} is in the journal of account {} only", t.id, if *seen_debit { t.debit } else { t.credit })));
        }
        rep.in_flight += 1;
        let e = sums.entry(t.currency).or_default();
        if *seen_debit {
            e.1 += u128::from(t.amount);
        } else {
            e.0 += u128::from(t.amount);
        }
    }
    let mut journal: BTreeMap<u32, u128> = BTreeMap::new();
    let mut all: HashMap<u64, k::Transfer> = debit_side;
    all.extend(credit_side);
    for t in all.values() {
        *journal.entry(t.currency).or_default() += u128::from(t.amount);
    }
    rep.transfers = all.len() as u64;
    for (cur, (d, c)) in &sums {
        let j = journal.get(cur).copied().unwrap_or(0);
        if d != c || *d != j {
            return Ok(Err(format!("currency {cur}: debits {d}, credits {c}, journal {j}")));
        }
    }
    Ok(Ok(rep))
}
