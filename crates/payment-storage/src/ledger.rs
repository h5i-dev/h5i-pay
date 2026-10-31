//! The ledger kernel's shell: PostgreSQL tables, idempotent replies and JSON.
//! It decides nothing; every decision is `payment_ledger::transition`.
// The `schema!` mapping generates loaders over the kernel's `&Vec` types.
#![allow(clippy::ptr_arg)]

use axum::http::StatusCode;
use h5i_app_core::{Kernel, TenantId};
use h5i_app_json::Value as Out;
use h5i_app_pg::{DbError, ReplyCodec, Store, Tx};
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
}

/// User 0 of every tenant is its operator.
pub fn principal(org: u64, user: u64) -> k::Principal {
    k::Principal { org, user, operator: user == 0 }
}

// The `accounts`, `transfers` and `ledger` tables, from the kernel's `schema!`.
payment_ledger::ledger_tables!(LedgerApp);

pub struct LedgerStore;

impl Store<LedgerApp> for LedgerStore {
    fn ddl() -> Vec<String> {
        schema_ddl()
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

    // `lock_for` keeps its default, `Lock::Tenant`: every command loads the
    // whole tenant, so under SERIALIZABLE any two writes of a tenant
    // conflict anyway, and queueing them is cheaper than retrying them.
    // The advisory lock is only a hint; SERIALIZABLE makes the result serial.
}

/// The JSON a client sends to `POST /v1/ledger`.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandJson {
    Open {
        /// Defaults to the caller.
        #[serde(default)]
        owner: Option<u64>,
        currency: u64,
        #[serde(default)]
        code: u64,
        #[serde(default)]
        no_overdraft: bool,
        #[serde(default)]
        no_overcredit: bool,
    },
    Transfer(TransferJson),
    Batch { transfers: Vec<TransferJson> },
    Account { id: u64 },
    Accounts,
    Transfers,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug)]
#[serde(deny_unknown_fields)]
pub struct TransferJson {
    pub id: u64,
    pub debit: u64,
    pub credit: u64,
    pub amount: u64,
    #[serde(default)]
    pub code: u64,
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
                k::Command::Open { owner: owner.unwrap_or(actor.user), currency, code, no_overdraft, no_overcredit }
            }
            CommandJson::Transfer(t) => k::Command::Transfer { t: t.into() },
            CommandJson::Batch { transfers } => k::Command::Batch { transfers: transfers.into_iter().map(Into::into).collect() },
            CommandJson::Account { id } => k::Command::Account { id },
            CommandJson::Accounts => k::Command::Accounts,
            CommandJson::Transfers => k::Command::Transfers,
        }
    }
}

/// Client transfer ids must leave the top bit clear: ids with it set are
/// derived by the orchestrator from payment and refund ids.
pub const SYSTEM_TRANSFER_BIT: u64 = 1 << 63;

/// Decode a client's ledger command. `Api::decode_command` has no caller,
/// so the HTTP handler uses this instead.
pub fn decode_for(actor: &k::Principal, body: serde_json::Value) -> Result<k::Command, String> {
    let c = serde_json::from_value::<CommandJson>(body).map_err(|e| e.to_string())?;
    let check = |t: &TransferJson| {
        if t.id & SYSTEM_TRANSFER_BIT != 0 && !actor.operator {
            Err(format!("transfer id {} is reserved (top bit set)", t.id))
        } else {
            Ok(())
        }
    };
    match &c {
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
        ("currency", a.currency.into()),
        ("code", a.code.into()),
        ("no_overdraft", a.no_overdraft.into()),
        ("no_overcredit", a.no_overcredit.into()),
        ("debits", a.debits.into()),
        ("credits", a.credits.into()),
    ])
}

pub fn transfer_json(t: &k::Transfer) -> Out {
    Out::obj([
        ("id", t.id.into()),
        ("debit", t.debit.into()),
        ("credit", t.credit.into()),
        ("amount", t.amount.into()),
        ("currency", t.currency.into()),
        ("code", t.code.into()),
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
    }
}

impl h5i_app_http::Api<LedgerApp> for LedgerStore {
    /// Without the caller: `owner` defaults to the operator (0). The server
    /// routes ledger requests through [`decode_for`] instead.
    fn decode_command(body: serde_json::Value) -> Result<k::Command, String> {
        decode_for(&principal(0, 0), body)
    }

    fn encode_reply(r: &k::Reply) -> Out {
        match r {
            k::Reply::Created(id) => Out::obj([("id", (*id).into())]),
            k::Reply::Posted(id) => Out::obj([("id", (*id).into()), ("status", "posted".into())]),
            k::Reply::Exists(id) => Out::obj([("id", (*id).into()), ("status", "exists".into())]),
            k::Reply::Batch(n) => Out::obj([("posted", (*n).into())]),
            k::Reply::Account(a) => account_json(a),
            k::Reply::Accounts(accs) => Out::Arr(accs.iter().map(account_json).collect()),
            k::Reply::Transfers(ts) => Out::Arr(ts.iter().map(transfer_json).collect()),
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
    Accounts(Vec<StoredAccount>),
    Transfers(Vec<(u64, u64, u64, u64, u64, u64)>),
}

type StoredAccount = (u64, u64, u64, u64, bool, bool, u64, u64);

fn store_account(a: &k::Account) -> StoredAccount {
    (a.id, a.owner, a.currency, a.code, a.no_overdraft, a.no_overcredit, a.debits, a.credits)
}

fn load_account((id, owner, currency, code, no_overdraft, no_overcredit, debits, credits): StoredAccount) -> k::Account {
    k::Account { id, owner, currency, code, no_overdraft, no_overcredit, debits, credits }
}

impl ReplyCodec<LedgerApp> for LedgerStore {
    fn fingerprint(cmd: &k::Command) -> Vec<u8> {
        format!("{cmd:?}").into_bytes()
    }

    fn scope(actor: &k::Principal) -> String {
        format!("u{}", actor.user)
    }

    fn encode(r: &k::Reply) -> Vec<u8> {
        let stored = match r {
            k::Reply::Created(id) => StoredReply::Created(*id),
            k::Reply::Posted(id) => StoredReply::Posted(*id),
            k::Reply::Exists(id) => StoredReply::Exists(*id),
            k::Reply::Batch(n) => StoredReply::Batch(*n),
            k::Reply::Account(a) => StoredReply::Account(store_account(a)),
            k::Reply::Accounts(accs) => StoredReply::Accounts(accs.iter().map(store_account).collect()),
            k::Reply::Transfers(ts) => {
                StoredReply::Transfers(ts.iter().map(|t| (t.id, t.debit, t.credit, t.amount, t.currency, t.code)).collect())
            }
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
            StoredReply::Accounts(accs) => k::Reply::Accounts(accs.into_iter().map(load_account).collect()),
            StoredReply::Transfers(ts) => k::Reply::Transfers(
                ts.into_iter()
                    .map(|(id, debit, credit, amount, currency, code)| k::Transfer { id, debit, credit, amount, currency, code })
                    .collect(),
            ),
        })
    }
}

/// Per-currency totals of a snapshot, for audits and tests: (debits, credits, journal).
pub fn currency_totals(s: &k::Snapshot, currency: u64) -> (u128, u128, u128) {
    let d = s.accounts.iter().filter(|a| a.currency == currency).map(|a| u128::from(a.debits)).sum();
    let c = s.accounts.iter().filter(|a| a.currency == currency).map(|a| u128::from(a.credits)).sum();
    let j = s.transfers.iter().filter(|t| t.currency == currency).map(|t| u128::from(t.amount)).sum();
    (d, c, j)
}

/// The invariants the Lean proofs establish, checked on a loaded snapshot:
/// per-currency debits = credits = journal total, and no constrained
/// account past its limit. Used by tests and by the audit endpoint.
pub fn audit(s: &k::Snapshot) -> Result<(), String> {
    let mut currencies: Vec<u64> = s.accounts.iter().map(|a| a.currency).chain(s.transfers.iter().map(|t| t.currency)).collect();
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
