//! The h5i-pay double-entry ledger kernel.
//!
//! This crate is the *kernel*: every decision the ledger makes, as pure Rust
//! in the subset Charon and Aeneas translate to Lean 4 (no `?`, no closures,
//! no iterators, loops over indices). It has no I/O, no clock, no database
//! and no GPU code. The proofs in `proofs/Ledger` and `proofs/Batch` are
//! about the Lean that `h5i app extract` makes of this file.
//!
//! The model follows TigerBeetle: an account holds two monotone counters,
//! `debits` and `credits`, posted in one currency. A transfer moves `amount`
//! from the debit account to the credit account, so every transfer is a
//! balanced pair of postings. An account may be constrained so that its
//! debits never exceed its credits (`no_overdraft`, e.g. a customer wallet)
//! or its credits never exceed its debits (`no_overcredit`). Transfers carry
//! a client-chosen id: re-sending the same transfer is a no-op that reports
//! `Exists`, and re-using an id for a different transfer is refused.
//!
//! A batch is linked (all-or-nothing): its transfers run in order, each on
//! the state the previous ones left, and one refusal refuses the batch.

// The kernel is written in the Rust subset Charon and Aeneas translate:
// `&Vec` parameters, `Vec::new` + `push`, explicit index loops and spelled-out
// branches are deliberate, so these style lints are off for this crate.
#![allow(clippy::ptr_arg, clippy::vec_init_then_push, clippy::if_same_then_else, clippy::len_zero, clippy::manual_range_contains, clippy::single_match)]

/// Who acts inside a tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    /// A user (an account holder or merchant), by user id.
    User(u64),
    /// One of the tenant's operators (support staff, the orchestrator, the
    /// recovery job), by operator id. An operator may debit any account of
    /// the tenant; what it posts records its id.
    Operator(u64),
}

/// Who is calling: an organization (the tenant) and who acts in it, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Principal {
    pub org: u64,
    pub actor: Actor,
    /// The transaction's clock, in microseconds since the Unix epoch: the
    /// engine stamps it from PostgreSQL's `transaction_timestamp()` before
    /// every attempt (`Kernel::stamp`). Recorded in the history (G1).
    pub at: u64,
}

/// Is the caller one of the tenant's operators?
pub fn is_operator(actor: &Principal) -> bool {
    match actor.actor {
        Actor::User(_) => false,
        Actor::Operator(_) => true,
    }
}

/// The acting user's id; 0 (the tenant's own accounts) for an operator.
pub fn user_of(actor: &Principal) -> u64 {
    match actor.actor {
        Actor::User(u) => u,
        Actor::Operator(_) => 0,
    }
}

/// The acting operator's id, recorded on what it posts; 0 for a user.
pub fn operator_of(actor: &Principal) -> u64 {
    match actor.actor {
        Actor::User(_) => 0,
        Actor::Operator(o) => o,
    }
}

/// The id the history records for the caller: the user's or the
/// operator's (`HistoryEntry::by_operator` says which).
pub fn actor_id(actor: &Principal) -> u64 {
    match actor.actor {
        Actor::User(u) => u,
        Actor::Operator(o) => o,
    }
}

/// The most transfers in one linked batch. Every accepted batch reads only
/// its accounts and transfer ids (`reads`): a longer `OR` chain makes
/// PostgreSQL's planning time grow faster than linearly (F3).
pub const MAX_BATCH: usize = 64;

/// The most accounts one `Accounts` page covers (sequence numbers per page).
pub const MAX_PAGE: u32 = 64;

h5i_app_schema::schema! {
    mapping ledger_tables for payment_ledger, writes Write, lean "../../proofs/ledger/generated/LedgerSchema.lean";

    /// One tenant's ledger.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Snapshot {
        ledger: Ledger,
        accounts: Vec<Account>,
        transfers: Vec<Transfer>,
        owners: Vec<OwnerCounter>,
        history: Vec<HistoryEntry>,
    }

    /// An account: posted debits and credits in one currency (ISO 4217
    /// numeric code), in minor units.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Account in "accounts" {
        key { id: u64 }
        owner: u64,
        currency: u32,
        code: u32,
        no_overdraft: bool,
        no_overcredit: bool,
        debits: u64,
        credits: u64,
        /// The account's number among its owner's accounts (0, 1, …): the
        /// key `Accounts` pages by.
        seq: u64,
        /// How many history entries the account has: the next entry's `n`
        /// (G1). Opening it and every posting to it add one.
        changes: u64,
    }

    /// A committed transfer: the journal. Never updated or deleted.
    /// `operator` is the operator who posted it (0: a user did).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Transfer in "transfers" {
        key { id: u64 }
        debit: u64,
        credit: u64,
        amount: u64,
        currency: u32,
        code: u32,
        operator: u64,
    }

    /// The tenant's counters.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct Ledger in "ledger" {
        key {}
        next_account: u64,
    }

    /// How many accounts an owner has: the next account's `seq`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct OwnerCounter in "owner_counters" {
        key { owner: u64 }
        next: u64,
    }

    /// One change of an account (G1): entry `n` of account `account`,
    /// written with the change itself, never updated or deleted. Who
    /// (`by_operator`, `actor`: the user or operator id), when (`at`, the
    /// transaction's clock), what (`command`; the transfer id and code, or
    /// the account code when it was opened), and the account's debits and
    /// credits before and after.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HistoryEntry in "history" {
        key { account: u64, n: u64 }
        by_operator: bool,
        actor: u64,
        at: u64,
        command: u32,
        transfer: u64,
        code: u32,
        debits_before: u64,
        debits_after: u64,
        credits_before: u64,
        credits_after: u64,
    }
}

/// `HistoryEntry::command`: the account was opened.
pub const CMD_OPEN: u32 = 1;
/// `HistoryEntry::command`: a transfer was posted to it (alone or in a batch).
pub const CMD_POST: u32 = 2;
/// `HistoryEntry::command` of entries the G4 migration wrote for transfers
/// posted before the history existed (the kernel never writes it).
pub const CMD_MIGRATED: u32 = 3;

/// A transfer as a client asks for it. `code` is the application's
/// transfer type (capture, refund, payout, ...); the ledger only stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferReq {
    pub id: u64,
    pub debit: u64,
    pub credit: u64,
    pub amount: u64,
    pub code: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Open an account for `owner`: the caller itself, or anyone for an
    /// operator. `code` is the application's account type.
    Open { owner: u64, currency: u32, code: u32, no_overdraft: bool, no_overcredit: bool },
    Transfer { t: TransferReq },
    /// A linked batch: all transfers commit, in order, or none does.
    Batch { transfers: Vec<TransferReq> },
    /// One account; its owner or an operator may read it.
    Account { id: u64 },
    /// One page of `owner`'s accounts: those numbered `from` to
    /// `from + limit - 1` among the owner's accounts (`Account::seq`),
    /// `limit` at most `MAX_PAGE`. A user may list only its own.
    Accounts { owner: u64, from: u64, limit: u32 },
    /// One page of `account`'s journal (operators only, G4): the transfers
    /// of its history entries numbered `from` to `from + limit - 1`, in that
    /// order, `limit` at most `MAX_PAGE`. Every transfer is in the journal
    /// of its debit and of its credit account.
    Transfers { account: u64, from: u64, limit: u32 },
    /// One page of the tenant's accounts by id (operators only, G4): those
    /// with ids `from` to `from + limit - 1`.
    AccountRange { from: u64, limit: u32 },
    /// One page of an account's history (operators only): its entries
    /// numbered `from` to `from + limit - 1`, `limit` at most `MAX_PAGE`.
    History { account: u64, from: u64, limit: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    PutAccount(Account),
    PutTransfer(Transfer),
    SetLedger(Ledger),
    PutOwner(OwnerCounter),
    PutHistory(HistoryEntry),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The new account's id.
    Created(u64),
    /// The transfer committed.
    Posted(u64),
    /// The same transfer was committed before; nothing changed.
    Exists(u64),
    /// The number of transfers in a committed batch.
    Batch(u64),
    Account(Account),
    /// A page of accounts, and where the next page starts (`None`: this
    /// was the last).
    Accounts(Vec<Account>, Option<u64>),
    /// A page of an account's journal, and where the next page starts.
    Transfers(Vec<Transfer>, Option<u64>),
    /// A page of an account's history, and where the next page starts.
    History(Vec<HistoryEntry>, Option<u64>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotFound,
    Forbidden,
    /// Transfer id 0 is reserved.
    InvalidId,
    ZeroAmount,
    SameAccount,
    CurrencyMismatch,
    /// The debit would take a `no_overdraft` account below zero.
    Insufficient,
    /// The credit would take a `no_overcredit` account past its debits.
    ExceedsCredit,
    Overflow,
    /// The transfer id was used for a different transfer.
    IdConflict,
    /// An account cannot be both `no_overdraft` and `no_overcredit`.
    InvalidFlags,
    EmptyBatch,
    BatchTooLarge,
    /// A page limit of 0 or above `MAX_PAGE`.
    BadPage,
}

type Outcome = Result<(Vec<Write>, Reply), Error>;

fn four(a: Write, b: Write, c: Write, d: Write) -> Vec<Write> {
    let mut ws = Vec::new();
    ws.push(a);
    ws.push(b);
    ws.push(c);
    ws.push(d);
    ws
}

fn five(a: Write, b: Write, c: Write, d: Write, e: Write) -> Vec<Write> {
    let mut ws = Vec::new();
    ws.push(a);
    ws.push(b);
    ws.push(c);
    ws.push(d);
    ws.push(e);
    ws
}

/// Entry `n` of account `after.id`: `actor` changed it from `before` to
/// `after` (G1).
fn entry(actor: &Principal, command: u32, transfer: u64, code: u32, n: u64, before: &Account, after: &Account) -> HistoryEntry {
    HistoryEntry {
        account: after.id,
        n,
        by_operator: is_operator(actor),
        actor: actor_id(actor),
        at: actor.at,
        command,
        transfer,
        code,
        debits_before: before.debits,
        debits_after: after.debits,
        credits_before: before.credits,
        credits_after: after.credits,
    }
}

pub fn find_account(accounts: &Vec<Account>, id: u64) -> Option<Account> {
    let mut i = 0;
    while i < accounts.len() {
        if accounts[i].id == id {
            return Some(accounts[i]);
        }
        i += 1;
    }
    None
}

pub fn find_transfer(transfers: &Vec<Transfer>, id: u64) -> Option<Transfer> {
    let mut i = 0;
    while i < transfers.len() {
        if transfers[i].id == id {
            return Some(transfers[i]);
        }
        i += 1;
    }
    None
}

/// How many accounts `owner` has (its counter, 0 if it has none).
pub fn owner_next(cs: &Vec<OwnerCounter>, owner: u64) -> u64 {
    let mut i = 0;
    while i < cs.len() {
        if cs[i].owner == owner {
            return cs[i].next;
        }
        i += 1;
    }
    0
}

/// The first number past a page: `from + limit`, or `u64::MAX` if that
/// overflows.
pub fn page_end(from: u64, limit: u32) -> u64 {
    let n = limit as u64;
    if from > u64::MAX - n {
        u64::MAX
    } else {
        from + n
    }
}

/// The stored transfer is the one requested: a replay.
pub fn same_transfer(old: &Transfer, t: &TransferReq) -> bool {
    old.debit == t.debit && old.credit == t.credit && old.amount == t.amount && old.code == t.code
}

/// May `actor` debit (or read) account `a`?
pub fn may_debit(actor: &Principal, a: &Account) -> bool {
    is_operator(actor) || a.owner == user_of(actor)
}

fn open(actor: &Principal, s: &Snapshot, owner: u64, currency: u32, code: u32, no_overdraft: bool, no_overcredit: bool) -> Outcome {
    if !is_operator(actor) && owner != user_of(actor) {
        return Err(Error::Forbidden);
    }
    if no_overdraft && no_overcredit {
        return Err(Error::InvalidFlags);
    }
    let l = s.ledger;
    if l.next_account == u64::MAX {
        return Err(Error::Overflow);
    }
    let n = owner_next(&s.owners, owner);
    if n == u64::MAX {
        return Err(Error::Overflow);
    }
    let a = Account {
        id: l.next_account,
        owner,
        currency,
        code,
        no_overdraft,
        no_overcredit,
        debits: 0,
        credits: 0,
        seq: n,
        changes: 1,
    };
    let h = entry(actor, CMD_OPEN, 0, code, 0, &a, &a);
    let l2 = Ledger { next_account: l.next_account + 1 };
    let c2 = OwnerCounter { owner, next: n + 1 };
    Ok((four(Write::PutAccount(a), Write::PutHistory(h), Write::SetLedger(l2), Write::PutOwner(c2)), Reply::Created(a.id)))
}

/// Post one transfer, or recognise its replay. The caller must be allowed
/// to debit the debit account before the journal is consulted, so a caller
/// without that right learns nothing about which transfer ids exist
/// (SEC-03): it gets the answer it would get for an unused id.
pub fn transfer(actor: &Principal, s: &Snapshot, t: &TransferReq) -> Outcome {
    if t.id == 0 {
        return Err(Error::InvalidId);
    }
    if t.amount == 0 {
        return Err(Error::ZeroAmount);
    }
    if t.debit == t.credit {
        return Err(Error::SameAccount);
    }
    match find_account(&s.accounts, t.debit) {
        None => Err(Error::NotFound),
        Some(a) => {
            if !may_debit(actor, &a) {
                Err(Error::Forbidden)
            } else {
                match find_transfer(&s.transfers, t.id) {
                    Some(old) => {
                        if same_transfer(&old, t) {
                            Ok((Vec::new(), Reply::Exists(t.id)))
                        } else {
                            Err(Error::IdConflict)
                        }
                    }
                    None => match find_account(&s.accounts, t.credit) {
                        None => Err(Error::NotFound),
                        Some(b) => post(actor, s, t, a, b, operator_of(actor)),
                    },
                }
            }
        }
    }
}

/// The arithmetic of a transfer between two different, existing accounts,
/// posted by `actor`, operator `op` (0: a user); each account's change
/// with its history entry.
fn post(actor: &Principal, s: &Snapshot, t: &TransferReq, a: Account, b: Account, op: u64) -> Outcome {
    let _ = s;
    if a.currency != b.currency {
        Err(Error::CurrencyMismatch)
    } else if a.changes == u64::MAX || b.changes == u64::MAX {
        Err(Error::Overflow)
    } else if a.debits > u64::MAX - t.amount {
        Err(Error::Overflow)
    } else if b.credits > u64::MAX - t.amount {
        Err(Error::Overflow)
    } else {
        let debits = a.debits + t.amount;
        let credits = b.credits + t.amount;
        if a.no_overdraft && debits > a.credits {
            Err(Error::Insufficient)
        } else if b.no_overcredit && credits > b.debits {
            Err(Error::ExceedsCredit)
        } else {
            let a2 = Account {
                id: a.id,
                owner: a.owner,
                currency: a.currency,
                code: a.code,
                no_overdraft: a.no_overdraft,
                no_overcredit: a.no_overcredit,
                debits,
                credits: a.credits,
                seq: a.seq,
                changes: a.changes + 1,
            };
            let b2 = Account {
                id: b.id,
                owner: b.owner,
                currency: b.currency,
                code: b.code,
                no_overdraft: b.no_overdraft,
                no_overcredit: b.no_overcredit,
                debits: b.debits,
                credits,
                seq: b.seq,
                changes: b.changes + 1,
            };
            let tr = Transfer {
                id: t.id,
                debit: t.debit,
                credit: t.credit,
                amount: t.amount,
                currency: a.currency,
                code: t.code,
                operator: op,
            };
            let ha = entry(actor, CMD_POST, t.id, t.code, a.changes, &a, &a2);
            let hb = entry(actor, CMD_POST, t.id, t.code, b.changes, &b, &b2);
            Ok((
                five(Write::PutAccount(a2), Write::PutHistory(ha), Write::PutAccount(b2), Write::PutHistory(hb), Write::PutTransfer(tr)),
                Reply::Posted(t.id),
            ))
        }
    }
}

fn append(ws: &mut Vec<Write>, more: &Vec<Write>) {
    let mut i = 0;
    while i < more.len() {
        ws.push(more[i]);
        i += 1;
    }
}

/// A linked batch: each transfer runs on the state the earlier ones left.
pub fn batch(actor: &Principal, s: &Snapshot, ts: &Vec<TransferReq>) -> Outcome {
    if ts.len() == 0 {
        return Err(Error::EmptyBatch);
    }
    if ts.len() > MAX_BATCH {
        return Err(Error::BatchTooLarge);
    }
    let mut cur = s.clone();
    let mut ws = Vec::new();
    let mut i = 0;
    while i < ts.len() {
        match transfer(actor, &cur, &ts[i]) {
            Err(e) => return Err(e),
            Ok((w, _)) => {
                cur = apply(&cur, &w);
                append(&mut ws, &w);
            }
        }
        i += 1;
    }
    Ok((ws, Reply::Batch(ts.len() as u64)))
}

fn read_account(actor: &Principal, s: &Snapshot, id: u64) -> Outcome {
    match find_account(&s.accounts, id) {
        None => Err(Error::NotFound),
        Some(a) => {
            if may_debit(actor, &a) {
                Ok((Vec::new(), Reply::Account(a)))
            } else {
                // Not distinguishable from a missing account.
                Err(Error::NotFound)
            }
        }
    }
}

/// `owner`'s accounts numbered `from` up to (not including) `end`, in
/// table order.
fn owner_range(s: &Snapshot, owner: u64, from: u64, end: u64) -> Vec<Account> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.accounts.len() {
        let a = s.accounts[i];
        if a.owner == owner && from <= a.seq && a.seq < end {
            out.push(a);
        }
        i += 1;
    }
    out
}

fn accounts_page(actor: &Principal, s: &Snapshot, owner: u64, from: u64, limit: u32) -> Outcome {
    if limit == 0 || limit > MAX_PAGE {
        return Err(Error::BadPage);
    }
    if !is_operator(actor) && owner != user_of(actor) {
        return Err(Error::Forbidden);
    }
    let end = page_end(from, limit);
    let next = if end < owner_next(&s.owners, owner) { Some(end) } else { None };
    Ok((Vec::new(), Reply::Accounts(owner_range(s, owner, from, end), next)))
}

/// `account`'s history entries numbered `from` up to (not including)
/// `end`, in table order.
fn history_range(s: &Snapshot, account: u64, from: u64, end: u64) -> Vec<HistoryEntry> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.history.len() {
        let h = s.history[i];
        if h.account == account && from <= h.n && h.n < end {
            out.push(h);
        }
        i += 1;
    }
    out
}

fn history_page(actor: &Principal, s: &Snapshot, account: u64, from: u64, limit: u32) -> Outcome {
    if limit == 0 || limit > MAX_PAGE {
        return Err(Error::BadPage);
    }
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    match find_account(&s.accounts, account) {
        None => Err(Error::NotFound),
        Some(a) => {
            let end = page_end(from, limit);
            let next = if end < a.changes { Some(end) } else { None };
            Ok((Vec::new(), Reply::History(history_range(s, account, from, end), next)))
        }
    }
}

/// The transfers `entries` name, in their order (an opening names none).
fn transfers_of(s: &Snapshot, entries: &Vec<HistoryEntry>) -> Vec<Transfer> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        match find_transfer(&s.transfers, entries[i].transfer) {
            Some(t) => out.push(t),
            None => {}
        }
        i += 1;
    }
    out
}

fn journal_page(actor: &Principal, s: &Snapshot, account: u64, from: u64, limit: u32) -> Outcome {
    if limit == 0 || limit > MAX_PAGE {
        return Err(Error::BadPage);
    }
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    match find_account(&s.accounts, account) {
        None => Err(Error::NotFound),
        Some(a) => {
            let end = page_end(from, limit);
            let next = if end < a.changes { Some(end) } else { None };
            let entries = history_range(s, account, from, end);
            Ok((Vec::new(), Reply::Transfers(transfers_of(s, &entries), next)))
        }
    }
}

/// The accounts with ids `from` up to (not including) `end`, in table order.
fn id_range(s: &Snapshot, from: u64, end: u64) -> Vec<Account> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.accounts.len() {
        let a = s.accounts[i];
        if from <= a.id && a.id < end {
            out.push(a);
        }
        i += 1;
    }
    out
}

fn account_range(actor: &Principal, s: &Snapshot, from: u64, limit: u32) -> Outcome {
    if limit == 0 || limit > MAX_PAGE {
        return Err(Error::BadPage);
    }
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    let end = page_end(from, limit);
    let next = if end < s.ledger.next_account { Some(end) } else { None };
    Ok((Vec::new(), Reply::Accounts(id_range(s, from, end), next)))
}

/// The ledger's logic: a command's writes and reply, or a refusal that
/// commits nothing. Every account a command changes gets one history entry
/// per change, written right after the account (`Theorems.every_change_logged`).
pub fn transition(actor: &Principal, s: &Snapshot, cmd: &Command) -> Outcome {
    match cmd {
        Command::Open { owner, currency, code, no_overdraft, no_overcredit } => {
            open(actor, s, *owner, *currency, *code, *no_overdraft, *no_overcredit)
        }
        Command::Transfer { t } => transfer(actor, s, t),
        Command::Batch { transfers } => batch(actor, s, transfers),
        Command::Account { id } => read_account(actor, s, *id),
        Command::Accounts { owner, from, limit } => accounts_page(actor, s, *owner, *from, *limit),
        Command::Transfers { account, from, limit } => journal_page(actor, s, *account, *from, *limit),
        Command::AccountRange { from, limit } => account_range(actor, s, *from, *limit),
        Command::History { account, from, limit } => history_page(actor, s, *account, *from, *limit),
    }
}

// ---------------------------------------------------------------- read plans

/// Table numbers, in `schema!` declaration order (`Account::TABLE`, ...).
pub const T_ACCOUNTS: u32 = 0;
pub const T_TRANSFERS: u32 = 1;
pub const T_LEDGER: u32 = 2;
pub const T_OWNERS: u32 = 3;
pub const T_HISTORY: u32 = 4;
/// Column numbers: key columns first, then fields in declaration order.
pub const ACCOUNT_ID: u32 = 0;
pub const ACCOUNT_OWNER: u32 = 1;
pub const ACCOUNT_SEQ: u32 = 8;
pub const TRANSFER_ID: u32 = 0;
pub const OWNER_KEY: u32 = 0;
pub const HISTORY_ACCOUNT: u32 = 0;
pub const HISTORY_N: u32 = 1;
pub const HISTORY_TRANSFER: u32 = 6;

/// Every batch the kernel accepts has a plan (F3: no whole-tenant loads).
pub const MAX_PLANNED_BATCH: usize = MAX_BATCH;

fn v_int(x: u64) -> h5i_app_sql::Val {
    h5i_app_sql::Column::to_val(&x)
}

fn p_eq(col: u32, v: h5i_app_sql::Val) -> h5i_app_sql::Pred {
    h5i_app_sql::Pred::Eq { col, val: v }
}

fn p_or(a: h5i_app_sql::Pred, b: h5i_app_sql::Pred) -> h5i_app_sql::Pred {
    h5i_app_sql::Pred::Or(Box::new(a), Box::new(b))
}

fn rd_where(table: u32, pred: h5i_app_sql::Pred) -> h5i_app_sql::Read {
    h5i_app_sql::Read { table, pred, window: h5i_app_sql::whole() }
}

/// The two accounts of a transfer.
fn accounts_of(t: &TransferReq) -> h5i_app_sql::Pred {
    p_or(p_eq(ACCOUNT_ID, v_int(t.debit)), p_eq(ACCOUNT_ID, v_int(t.credit)))
}

/// `col` holds one of `from`, …, `end - 1` (`from < end`).
fn seq_range(col: u32, from: u64, end: u64) -> h5i_app_sql::Pred {
    let mut p = p_eq(col, v_int(from));
    let mut x = from + 1;
    while x < end {
        p = p_or(p, p_eq(col, v_int(x)));
        x += 1;
    }
    p
}

/// `account`'s history entries numbered `from` to `end - 1` (`from < end`).
fn entry_range(account: u64, from: u64, end: u64) -> h5i_app_sql::Pred {
    h5i_app_sql::Pred::And(Box::new(p_eq(HISTORY_ACCOUNT, v_int(account))), Box::new(seq_range(HISTORY_N, from, end)))
}

/// The plan of a non-empty batch: every account and transfer id it names.
fn batch_reads(ts: &Vec<TransferReq>) -> Vec<h5i_app_sql::Read> {
    let mut accts = accounts_of(&ts[0]);
    let mut trs = p_eq(TRANSFER_ID, v_int(ts[0].id));
    let mut i = 1;
    while i < ts.len() {
        accts = p_or(accts, accounts_of(&ts[i]));
        trs = p_or(trs, p_eq(TRANSFER_ID, v_int(ts[i].id)));
        i += 1;
    }
    let mut out = Vec::new();
    out.push(rd_where(T_ACCOUNTS, accts));
    out.push(rd_where(T_TRANSFERS, trs));
    out
}

/// `cmd` only reads: whenever `transition` accepts it, it writes nothing
/// (`payment_ledger.ReadOnly.sound`). The engine runs these in a
/// `SERIALIZABLE READ ONLY DEFERRABLE` transaction, which takes no
/// predicate locks and never aborts a concurrent write (G4: the paged
/// audit runs next to live transfers).
pub fn read_only(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Account { .. } | Command::Accounts { .. } | Command::Transfers { .. } | Command::AccountRange { .. } | Command::History { .. }
    )
}

/// The rows `transition(actor, _, cmd)` reads, one declarative read per
/// table (`Store::load_for`); `None` for the whole tenant. A table without
/// a read loads empty. `proofs/ledger/Ledger/Frame.lean` proves that
/// `transition` gives the same result on what the plan loads as on the
/// whole tenant.
pub fn reads(actor: &Principal, cmd: &Command) -> Option<Vec<h5i_app_sql::Read>> {
    match cmd {
        Command::Open { owner, .. } => {
            let mut out = Vec::new();
            out.push(rd_where(T_LEDGER, h5i_app_sql::Pred::All));
            out.push(rd_where(T_OWNERS, p_eq(OWNER_KEY, v_int(*owner))));
            Some(out)
        }
        Command::Transfer { t } => {
            let mut out = Vec::new();
            out.push(rd_where(T_ACCOUNTS, accounts_of(t)));
            out.push(rd_where(T_TRANSFERS, p_eq(TRANSFER_ID, v_int(t.id))));
            Some(out)
        }
        Command::Batch { transfers } => {
            // Empty or too long: refused whatever the state, so nothing is read.
            if transfers.len() == 0 || transfers.len() > MAX_BATCH {
                Some(Vec::new())
            } else {
                Some(batch_reads(transfers))
            }
        }
        Command::Account { id } => {
            let mut out = Vec::new();
            out.push(rd_where(T_ACCOUNTS, p_eq(ACCOUNT_ID, v_int(*id))));
            Some(out)
        }
        Command::Accounts { owner, from, limit } => {
            let mut out = Vec::new();
            // Refused pages read nothing.
            if *limit != 0 && *limit <= MAX_PAGE && (is_operator(actor) || *owner == user_of(actor)) {
                let end = page_end(*from, *limit);
                if *from < end {
                    let range = seq_range(ACCOUNT_SEQ, *from, end);
                    let pred = h5i_app_sql::Pred::And(Box::new(p_eq(ACCOUNT_OWNER, v_int(*owner))), Box::new(range));
                    out.push(rd_where(T_ACCOUNTS, pred));
                }
                out.push(rd_where(T_OWNERS, p_eq(OWNER_KEY, v_int(*owner))));
            }
            Some(out)
        }
        Command::Transfers { account, from, limit } => {
            let mut out = Vec::new();
            // Refused pages read nothing.
            if *limit != 0 && *limit <= MAX_PAGE && is_operator(actor) {
                out.push(rd_where(T_ACCOUNTS, p_eq(ACCOUNT_ID, v_int(*account))));
                let end = page_end(*from, *limit);
                if *from < end {
                    out.push(rd_where(T_HISTORY, entry_range(*account, *from, end)));
                    // The transfers those entries name.
                    let named = h5i_app_sql::Pred::In {
                        col: TRANSFER_ID,
                        table: T_HISTORY,
                        pred: Box::new(entry_range(*account, *from, end)),
                        of: HISTORY_TRANSFER,
                        window: h5i_app_sql::whole(),
                    };
                    out.push(rd_where(T_TRANSFERS, named));
                }
            }
            Some(out)
        }
        Command::AccountRange { from, limit } => {
            let mut out = Vec::new();
            if *limit != 0 && *limit <= MAX_PAGE && is_operator(actor) {
                let end = page_end(*from, *limit);
                if *from < end {
                    out.push(rd_where(T_ACCOUNTS, seq_range(ACCOUNT_ID, *from, end)));
                }
                out.push(rd_where(T_LEDGER, h5i_app_sql::Pred::All));
            }
            Some(out)
        }
        Command::History { account, from, limit } => {
            let mut out = Vec::new();
            // Refused pages read nothing.
            if *limit != 0 && *limit <= MAX_PAGE && is_operator(actor) {
                out.push(rd_where(T_ACCOUNTS, p_eq(ACCOUNT_ID, v_int(*account))));
                let end = page_end(*from, *limit);
                if *from < end {
                    out.push(rd_where(T_HISTORY, entry_range(*account, *from, end)));
                }
            }
            Some(out)
        }
    }
}

/// What one write does to the state; `schema!` builds `apply` from it.
fn apply_write(s: &mut Snapshot, w: Write) {
    match w {
        Write::PutAccount(a) => Account::put(&mut s.accounts, a),
        Write::PutTransfer(t) => Transfer::put(&mut s.transfers, t),
        Write::SetLedger(l) => s.ledger = l,
        Write::PutOwner(c) => OwnerCounter::put(&mut s.owners, c),
        Write::PutHistory(h) => HistoryEntry::put(&mut s.history, h),
    }
}

/// The table writes one write makes.
fn sql_write(w: &Write, out: &mut Vec<h5i_app_sql::Write>) {
    match w {
        Write::PutAccount(a) => out.push(a.sql_put()),
        Write::PutTransfer(t) => out.push(t.sql_put()),
        Write::SetLedger(l) => out.push(l.sql_put()),
        Write::PutOwner(c) => out.push(c.sql_put()),
        Write::PutHistory(h) => out.push(h.sql_put()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op() -> Principal {
        Principal { org: 1, actor: Actor::Operator(9), at: 5 }
    }

    fn user(u: u64) -> Principal {
        Principal { org: 1, actor: Actor::User(u), at: 3 }
    }

    fn run(s: &Snapshot, a: Principal, c: Command) -> (Snapshot, Result<Reply, Error>) {
        match transition(&a, s, &c) {
            Ok((ws, r)) => (apply(s, &ws), Ok(r)),
            Err(e) => (s.clone(), Err(e)),
        }
    }

    fn tr(id: u64, debit: u64, credit: u64, amount: u64) -> TransferReq {
        TransferReq { id, debit, credit, amount, code: 1 }
    }

    fn sums(s: &Snapshot, cur: u32) -> (u128, u128, u128) {
        let d = s.accounts.iter().filter(|a| a.currency == cur).map(|a| a.debits as u128).sum();
        let c = s.accounts.iter().filter(|a| a.currency == cur).map(|a| a.credits as u128).sum();
        let j = s.transfers.iter().filter(|t| t.currency == cur).map(|t| t.amount as u128).sum();
        (d, c, j)
    }

    /// Accounts: 0 operator funding (unconstrained), 1 alice wallet, 2 bob wallet, 3 EUR.
    fn setup() -> Snapshot {
        let s = Snapshot::default();
        let (s, r) = run(&s, op(), Command::Open { owner: 0, currency: 392, code: 1, no_overdraft: false, no_overcredit: false });
        assert_eq!(r, Ok(Reply::Created(0)));
        let (s, _) = run(&s, user(1), Command::Open { owner: 1, currency: 392, code: 2, no_overdraft: true, no_overcredit: false });
        let (s, _) = run(&s, user(2), Command::Open { owner: 2, currency: 392, code: 2, no_overdraft: true, no_overcredit: false });
        let (s, r) = run(&s, user(2), Command::Open { owner: 2, currency: 978, code: 2, no_overdraft: true, no_overcredit: false });
        assert_eq!(r, Ok(Reply::Created(3)));
        s
    }

    #[test]
    fn transfers_balance_and_refuse() {
        let s = setup();
        let (s, r) = run(&s, op(), Command::Transfer { t: tr(10, 0, 1, 100) });
        assert_eq!(r, Ok(Reply::Posted(10)));
        // Replay: no effect.
        let (s2, r) = run(&s, op(), Command::Transfer { t: tr(10, 0, 1, 100) });
        assert_eq!(r, Ok(Reply::Exists(10)));
        assert_eq!(s2, s);
        assert_eq!(run(&s, op(), Command::Transfer { t: tr(10, 0, 1, 99) }).1, Err(Error::IdConflict));
        assert_eq!(run(&s, user(2), Command::Transfer { t: tr(11, 1, 2, 1) }).1, Err(Error::Forbidden));
        assert_eq!(run(&s, user(1), Command::Transfer { t: tr(11, 1, 2, 101) }).1, Err(Error::Insufficient));
        assert_eq!(run(&s, user(1), Command::Transfer { t: tr(11, 1, 1, 1) }).1, Err(Error::SameAccount));
        assert_eq!(run(&s, user(1), Command::Transfer { t: tr(11, 1, 3, 1) }).1, Err(Error::CurrencyMismatch));
        assert_eq!(run(&s, user(1), Command::Transfer { t: tr(0, 1, 2, 1) }).1, Err(Error::InvalidId));
        assert_eq!(run(&s, user(1), Command::Transfer { t: tr(11, 1, 2, 0) }).1, Err(Error::ZeroAmount));
        assert_eq!(run(&s, user(1), Command::Transfer { t: tr(11, 1, 9, 1) }).1, Err(Error::NotFound));
        let (s, r) = run(&s, user(1), Command::Transfer { t: tr(11, 1, 2, 30) });
        assert_eq!(r, Ok(Reply::Posted(11)));
        let (d, c, j) = sums(&s, 392);
        assert_eq!((d, c, j), (130, 130, 130));
        for a in &s.accounts {
            if a.no_overdraft {
                assert!(a.debits <= a.credits);
            }
        }
    }

    #[test]
    fn replay_does_not_reveal_ids_to_others() {
        let s = setup();
        let (s, _) = run(&s, op(), Command::Transfer { t: tr(10, 0, 1, 100) });
        let (s, r) = run(&s, user(1), Command::Transfer { t: tr(11, 1, 2, 30) });
        assert_eq!(r, Ok(Reply::Posted(11)));
        // User 2 may not debit account 1: the same answer as for an unused id.
        assert_eq!(run(&s, user(2), Command::Transfer { t: tr(11, 1, 2, 30) }).1, Err(Error::Forbidden));
        assert_eq!(run(&s, user(2), Command::Transfer { t: tr(11, 1, 2, 31) }).1, Err(Error::Forbidden));
        assert_eq!(run(&s, user(2), Command::Transfer { t: tr(99, 1, 2, 30) }).1, Err(Error::Forbidden));
        // The owner and the operator still get the replay.
        assert_eq!(run(&s, user(1), Command::Transfer { t: tr(11, 1, 2, 30) }).1, Ok(Reply::Exists(11)));
        assert_eq!(run(&s, op(), Command::Transfer { t: tr(11, 1, 2, 30) }).1, Ok(Reply::Exists(11)));
    }

    #[test]
    fn the_journal_names_the_operator() {
        let s = setup();
        let (s, _) = run(&s, op(), Command::Transfer { t: tr(10, 0, 1, 100) });
        let (s, _) = run(&s, user(1), Command::Transfer { t: tr(11, 1, 2, 30) });
        let by = |s: &Snapshot, id| find_transfer(&s.transfers, id).unwrap().operator;
        assert_eq!((by(&s, 10), by(&s, 11)), (9, 0));
        // A user is never an operator, whatever its id.
        assert_eq!(run(&s, user(9), Command::Transfers { account: 1, from: 0, limit: 8 }).1, Err(Error::Forbidden));
    }

    #[test]
    fn every_account_change_has_one_history_entry() {
        let s = setup();
        let (s, _) = run(&s, op(), Command::Transfer { t: tr(10, 0, 1, 100) });
        // A batch touching account 1 twice: two entries, numbered in order.
        let (s, r) = run(&s, user(1), Command::Batch { transfers: vec![tr(11, 1, 2, 30), tr(12, 1, 2, 20)] });
        assert_eq!(r, Ok(Reply::Batch(2)));
        let of = |s: &Snapshot, acc: u64| -> Vec<_> {
            s.history
                .iter()
                .filter(|h| h.account == acc)
                .map(|h| (h.n, h.command, h.transfer, h.by_operator, h.actor, h.at, h.debits_before, h.debits_after, h.credits_after))
                .collect()
        };
        assert_eq!(
            of(&s, 1),
            vec![
                (0, CMD_OPEN, 0, false, 1, 3, 0, 0, 0),
                (1, CMD_POST, 10, true, 9, 5, 0, 0, 100),
                (2, CMD_POST, 11, false, 1, 3, 0, 30, 100),
                (3, CMD_POST, 12, false, 1, 3, 30, 50, 100),
            ]
        );
        assert_eq!(find_account(&s.accounts, 1).unwrap().changes, 4);
        // A replay writes nothing, so it records nothing.
        let (s2, r) = run(&s, user(1), Command::Transfer { t: tr(11, 1, 2, 30) });
        assert_eq!((r, &s2), (Ok(Reply::Exists(11)), &s));
        // Paged, operators only.
        let page = run(&s, op(), Command::History { account: 1, from: 1, limit: 2 }).1;
        let Ok(Reply::History(hs, next)) = page else { panic!("{page:?}") };
        assert_eq!((hs.iter().map(|h| h.n).collect::<Vec<_>>(), next), (vec![1, 2], Some(3)));
        assert_eq!(run(&s, user(1), Command::History { account: 1, from: 0, limit: 2 }).1, Err(Error::Forbidden));
        assert_eq!(run(&s, op(), Command::History { account: 99, from: 0, limit: 2 }).1, Err(Error::NotFound));
    }

    #[test]
    fn the_journal_is_paged_per_account() {
        let s = setup();
        let (s, _) = run(&s, op(), Command::Transfer { t: tr(10, 0, 1, 100) });
        let (s, _) = run(&s, user(1), Command::Transfer { t: tr(11, 1, 2, 30) });
        let (s, _) = run(&s, user(1), Command::Transfer { t: tr(12, 1, 2, 20) });
        let page = |s: &Snapshot, acc, from, limit| match run(s, op(), Command::Transfers { account: acc, from, limit }).1 {
            Ok(Reply::Transfers(ts, next)) => (ts.iter().map(|t| t.id).collect::<Vec<_>>(), next),
            r => panic!("{r:?}"),
        };
        // Account 1: opened (entry 0, no transfer), then 10, 11, 12.
        assert_eq!(page(&s, 1, 0, 2), (vec![10], Some(2)));
        assert_eq!(page(&s, 1, 2, 2), (vec![11, 12], None));
        assert_eq!(page(&s, 2, 0, 64), (vec![11, 12], None));
        assert_eq!(page(&s, 0, 0, 64), (vec![10], None));
        assert_eq!(run(&s, op(), Command::Transfers { account: 1, from: 0, limit: 65 }).1, Err(Error::BadPage));
        assert_eq!(run(&s, op(), Command::Transfers { account: 9, from: 0, limit: 1 }).1, Err(Error::NotFound));
        // Accounts by id, operators only.
        let ids = |r: Result<Reply, Error>| match r {
            Ok(Reply::Accounts(v, n)) => (v.iter().map(|a| a.id).collect::<Vec<_>>(), n),
            r => panic!("{r:?}"),
        };
        assert_eq!(ids(run(&s, op(), Command::AccountRange { from: 0, limit: 3 }).1), (vec![0, 1, 2], Some(3)));
        assert_eq!(ids(run(&s, op(), Command::AccountRange { from: 3, limit: 3 }).1), (vec![3], None));
        assert_eq!(run(&s, user(1), Command::AccountRange { from: 0, limit: 3 }).1, Err(Error::Forbidden));
    }

    #[test]
    fn batches_are_all_or_nothing_and_sequential() {
        let s = setup();
        let (s, _) = run(&s, op(), Command::Transfer { t: tr(1, 0, 1, 50) });
        // The second transfer only succeeds because the first ran before it.
        let b = vec![tr(2, 0, 2, 20), tr(3, 2, 1, 20)];
        let (s2, r) = run(&s, op(), Command::Batch { transfers: b.clone() });
        assert_eq!(r, Ok(Reply::Batch(2)));
        let mut seq = s.clone();
        for t in &b {
            let (n, r) = run(&seq, op(), Command::Transfer { t: *t });
            assert!(r.is_ok());
            seq = n;
        }
        assert_eq!(s2, seq);
        // One refusal refuses everything.
        let bad = vec![tr(4, 0, 2, 5), tr(5, 2, 1, 999)];
        let (s3, r) = run(&s2, op(), Command::Batch { transfers: bad });
        assert_eq!(r, Err(Error::Insufficient));
        assert_eq!(s3, s2);
        assert_eq!(run(&s2, op(), Command::Batch { transfers: vec![] }).1, Err(Error::EmptyBatch));
        let long: Vec<TransferReq> = (0..=MAX_BATCH as u64).map(|i| tr(100 + i, 0, 1, 1)).collect();
        assert_eq!(run(&s2, op(), Command::Batch { transfers: long }).1, Err(Error::BatchTooLarge));
        // A duplicate inside a batch is a replay of the earlier one.
        let (_, r) = run(&s2, op(), Command::Batch { transfers: vec![tr(6, 0, 1, 1), tr(6, 0, 1, 1)] });
        assert_eq!(r, Ok(Reply::Batch(2)));
    }

    #[test]
    fn reads_are_scoped_to_owners() {
        let s = setup();
        assert_eq!(run(&s, user(2), Command::Account { id: 1 }).1, Err(Error::NotFound));
        assert!(matches!(run(&s, user(1), Command::Account { id: 1 }).1, Ok(Reply::Account(_))));
        match run(&s, user(2), Command::Accounts { owner: 2, from: 0, limit: MAX_PAGE }).1 {
            Ok(Reply::Accounts(v, None)) => assert_eq!(v.iter().map(|a| (a.id, a.seq)).collect::<Vec<_>>(), vec![(2, 0), (3, 1)]),
            r => panic!("{r:?}"),
        }
        assert_eq!(run(&s, user(2), Command::Accounts { owner: 1, from: 0, limit: 1 }).1, Err(Error::Forbidden));
        assert_eq!(run(&s, user(2), Command::Accounts { owner: 2, from: 0, limit: 0 }).1, Err(Error::BadPage));
        assert_eq!(run(&s, op(), Command::Accounts { owner: 2, from: 0, limit: MAX_PAGE + 1 }).1, Err(Error::BadPage));
        // Pages of one: the cursor is the next sequence number.
        assert_eq!(run(&s, op(), Command::Accounts { owner: 2, from: 0, limit: 1 }).1.map(|r| match r {
            Reply::Accounts(v, n) => (v.len(), n),
            _ => panic!(),
        }), Ok((1, Some(1))));
        assert!(matches!(run(&s, op(), Command::Accounts { owner: 2, from: 1, limit: 1 }).1, Ok(Reply::Accounts(ref v, None)) if v[0].id == 3));
        assert!(matches!(run(&s, op(), Command::Accounts { owner: 2, from: u64::MAX, limit: 5 }).1, Ok(Reply::Accounts(ref v, None)) if v.is_empty()));
        assert_eq!(run(&s, user(2), Command::Transfers { account: 2, from: 0, limit: 8 }).1, Err(Error::Forbidden));
        assert_eq!(
            run(&s, user(2), Command::Open { owner: 2, currency: 1, code: 2, no_overdraft: true, no_overcredit: true }).1,
            Err(Error::InvalidFlags)
        );
        assert_eq!(
            run(&s, user(2), Command::Open { owner: 1, currency: 392, code: 2, no_overdraft: true, no_overcredit: false }).1,
            Err(Error::Forbidden)
        );
    }

    #[test]
    fn overflow_is_refused_not_wrapped() {
        let s = setup();
        let (s, _) = run(&s, op(), Command::Transfer { t: tr(1, 0, 2, u64::MAX) });
        assert_eq!(run(&s, op(), Command::Transfer { t: tr(2, 0, 1, 1) }).1, Err(Error::Overflow));
        assert_eq!(run(&s, op(), Command::Transfer { t: tr(2, 3, 2, 1) }).1, Err(Error::CurrencyMismatch));
    }
}
