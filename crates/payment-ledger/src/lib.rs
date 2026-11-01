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
#![allow(clippy::ptr_arg, clippy::vec_init_then_push, clippy::if_same_then_else, clippy::len_zero, clippy::manual_range_contains)]

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

/// Who is calling: an organization (the tenant) and who acts in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Principal {
    pub org: u64,
    pub actor: Actor,
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
}

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
    /// The journal; operators only.
    Transfers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    PutAccount(Account),
    PutTransfer(Transfer),
    SetLedger(Ledger),
    PutOwner(OwnerCounter),
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
    Transfers(Vec<Transfer>),
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

fn three(a: Write, b: Write, c: Write) -> Vec<Write> {
    let mut ws = Vec::new();
    ws.push(a);
    ws.push(b);
    ws.push(c);
    ws
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
    };
    let l2 = Ledger { next_account: l.next_account + 1 };
    let c2 = OwnerCounter { owner, next: n + 1 };
    Ok((three(Write::PutAccount(a), Write::SetLedger(l2), Write::PutOwner(c2)), Reply::Created(a.id)))
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
                        Some(b) => post(s, t, a, b, operator_of(actor)),
                    },
                }
            }
        }
    }
}

/// The arithmetic of a transfer between two different, existing accounts,
/// posted by operator `op` (0: a user).
fn post(s: &Snapshot, t: &TransferReq, a: Account, b: Account, op: u64) -> Outcome {
    let _ = s;
    if a.currency != b.currency {
        Err(Error::CurrencyMismatch)
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
            Ok((
                three(Write::PutAccount(a2), Write::PutAccount(b2), Write::PutTransfer(tr)),
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

/// The ledger's logic: a command's writes and reply, or a refusal that
/// commits nothing.
pub fn transition(actor: &Principal, s: &Snapshot, cmd: &Command) -> Outcome {
    match cmd {
        Command::Open { owner, currency, code, no_overdraft, no_overcredit } => {
            open(actor, s, *owner, *currency, *code, *no_overdraft, *no_overcredit)
        }
        Command::Transfer { t } => transfer(actor, s, t),
        Command::Batch { transfers } => batch(actor, s, transfers),
        Command::Account { id } => read_account(actor, s, *id),
        Command::Accounts { owner, from, limit } => accounts_page(actor, s, *owner, *from, *limit),
        Command::Transfers => {
            if is_operator(actor) {
                Ok((Vec::new(), Reply::Transfers(s.transfers.clone())))
            } else {
                Err(Error::Forbidden)
            }
        }
    }
}

// ---------------------------------------------------------------- read plans

/// Table numbers, in `schema!` declaration order (`Account::TABLE`, ...).
pub const T_ACCOUNTS: u32 = 0;
pub const T_TRANSFERS: u32 = 1;
pub const T_LEDGER: u32 = 2;
pub const T_OWNERS: u32 = 3;
/// Column numbers: key columns first, then fields in declaration order.
pub const ACCOUNT_ID: u32 = 0;
pub const ACCOUNT_OWNER: u32 = 1;
pub const ACCOUNT_SEQ: u32 = 8;
pub const TRANSFER_ID: u32 = 0;
pub const OWNER_KEY: u32 = 0;

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
        Command::Transfers => {
            let mut out = Vec::new();
            if is_operator(actor) {
                out.push(rd_where(T_TRANSFERS, h5i_app_sql::Pred::All));
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
    }
}

/// The table writes one write makes.
fn sql_write(w: &Write, out: &mut Vec<h5i_app_sql::Write>) {
    match w {
        Write::PutAccount(a) => out.push(a.sql_put()),
        Write::PutTransfer(t) => out.push(t.sql_put()),
        Write::SetLedger(l) => out.push(l.sql_put()),
        Write::PutOwner(c) => out.push(c.sql_put()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op() -> Principal {
        Principal { org: 1, actor: Actor::Operator(9) }
    }

    fn user(u: u64) -> Principal {
        Principal { org: 1, actor: Actor::User(u) }
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
        assert_eq!(run(&s, user(9), Command::Transfers).1, Err(Error::Forbidden));
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
        assert_eq!(run(&s, user(2), Command::Transfers).1, Err(Error::Forbidden));
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
