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

/// Who is calling: an organization (the tenant), a user in it, and whether
/// the user is the tenant's operator (the payment orchestrator or an admin),
/// who may debit any account of the tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Principal {
    pub org: u64,
    pub user: u64,
    pub operator: bool,
}

/// The most transfers in one linked batch.
pub const MAX_BATCH: usize = 8192;

h5i_app_schema::schema! {
    mapping ledger_tables for payment_ledger, writes Write, lean "../../proofs/generated/LedgerSchema.lean";

    /// One tenant's ledger.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Snapshot {
        ledger: Ledger,
        accounts: Vec<Account>,
        transfers: Vec<Transfer>,
    }

    /// An account: posted debits and credits in one currency (ISO 4217
    /// numeric code), in minor units.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Account in "accounts" {
        key { id: u64 }
        owner: u64,
        currency: u64,
        code: u64,
        no_overdraft: bool,
        no_overcredit: bool,
        debits: u64,
        credits: u64,
    }

    /// A committed transfer: the journal. Never updated or deleted.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Transfer in "transfers" {
        key { id: u64 }
        debit: u64,
        credit: u64,
        amount: u64,
        currency: u64,
        code: u64,
    }

    /// The tenant's counters.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct Ledger in "ledger" {
        key {}
        next_account: u64,
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
    pub code: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Open an account for `owner`: the caller itself, or anyone for an
    /// operator. `code` is the application's account type.
    Open { owner: u64, currency: u64, code: u64, no_overdraft: bool, no_overcredit: bool },
    Transfer { t: TransferReq },
    /// A linked batch: all transfers commit, in order, or none does.
    Batch { transfers: Vec<TransferReq> },
    /// One account; its owner or an operator may read it.
    Account { id: u64 },
    /// The caller's accounts (every account, for an operator).
    Accounts,
    /// The journal; operators only.
    Transfers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    PutAccount(Account),
    PutTransfer(Transfer),
    SetLedger(Ledger),
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
    Accounts(Vec<Account>),
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
}

type Outcome = Result<(Vec<Write>, Reply), Error>;

fn three(a: Write, b: Write, c: Write) -> Vec<Write> {
    let mut ws = Vec::new();
    ws.push(a);
    ws.push(b);
    ws.push(c);
    ws
}

fn two(a: Write, b: Write) -> Vec<Write> {
    let mut ws = Vec::new();
    ws.push(a);
    ws.push(b);
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

/// The stored transfer is the one requested: a replay.
pub fn same_transfer(old: &Transfer, t: &TransferReq) -> bool {
    old.debit == t.debit && old.credit == t.credit && old.amount == t.amount && old.code == t.code
}

/// May `actor` debit (or read) account `a`?
pub fn may_debit(actor: &Principal, a: &Account) -> bool {
    actor.operator || a.owner == actor.user
}

fn open(actor: &Principal, s: &Snapshot, owner: u64, currency: u64, code: u64, no_overdraft: bool, no_overcredit: bool) -> Outcome {
    if !actor.operator && owner != actor.user {
        return Err(Error::Forbidden);
    }
    if no_overdraft && no_overcredit {
        return Err(Error::InvalidFlags);
    }
    let l = s.ledger;
    if l.next_account == u64::MAX {
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
    };
    let l2 = Ledger { next_account: l.next_account + 1 };
    Ok((two(Write::PutAccount(a), Write::SetLedger(l2)), Reply::Created(a.id)))
}

/// Post one transfer, or recognise its replay.
pub fn transfer(actor: &Principal, s: &Snapshot, t: &TransferReq) -> Outcome {
    if t.id == 0 {
        return Err(Error::InvalidId);
    }
    if t.amount == 0 {
        return Err(Error::ZeroAmount);
    }
    match find_transfer(&s.transfers, t.id) {
        Some(old) => {
            if same_transfer(&old, t) {
                Ok((Vec::new(), Reply::Exists(t.id)))
            } else {
                Err(Error::IdConflict)
            }
        }
        None => {
            if t.debit == t.credit {
                return Err(Error::SameAccount);
            }
            match find_account(&s.accounts, t.debit) {
                None => Err(Error::NotFound),
                Some(a) => {
                    if !may_debit(actor, &a) {
                        Err(Error::Forbidden)
                    } else {
                        match find_account(&s.accounts, t.credit) {
                            None => Err(Error::NotFound),
                            Some(b) => post(s, t, a, b),
                        }
                    }
                }
            }
        }
    }
}

/// The arithmetic of a transfer between two different, existing accounts.
fn post(s: &Snapshot, t: &TransferReq, a: Account, b: Account) -> Outcome {
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
            };
            let tr = Transfer {
                id: t.id,
                debit: t.debit,
                credit: t.credit,
                amount: t.amount,
                currency: a.currency,
                code: t.code,
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

fn visible_accounts(actor: &Principal, s: &Snapshot) -> Vec<Account> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.accounts.len() {
        if may_debit(actor, &s.accounts[i]) {
            out.push(s.accounts[i]);
        }
        i += 1;
    }
    out
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
        Command::Accounts => Ok((Vec::new(), Reply::Accounts(visible_accounts(actor, s)))),
        Command::Transfers => {
            if actor.operator {
                Ok((Vec::new(), Reply::Transfers(s.transfers.clone())))
            } else {
                Err(Error::Forbidden)
            }
        }
    }
}

/// What one write does to the state; `schema!` builds `apply` from it.
fn apply_write(s: &mut Snapshot, w: Write) {
    match w {
        Write::PutAccount(a) => Account::put(&mut s.accounts, a),
        Write::PutTransfer(t) => Transfer::put(&mut s.transfers, t),
        Write::SetLedger(l) => s.ledger = l,
    }
}

/// The table writes one write makes.
fn sql_write(w: &Write, out: &mut Vec<h5i_app_sql::Write>) {
    match w {
        Write::PutAccount(a) => out.push(a.sql_put()),
        Write::PutTransfer(t) => out.push(t.sql_put()),
        Write::SetLedger(l) => out.push(l.sql_put()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op() -> Principal {
        Principal { org: 1, user: 0, operator: true }
    }

    fn user(u: u64) -> Principal {
        Principal { org: 1, user: u, operator: false }
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

    fn sums(s: &Snapshot, cur: u64) -> (u128, u128, u128) {
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
        // A duplicate inside a batch is a replay of the earlier one.
        let (_, r) = run(&s2, op(), Command::Batch { transfers: vec![tr(6, 0, 1, 1), tr(6, 0, 1, 1)] });
        assert_eq!(r, Ok(Reply::Batch(2)));
    }

    #[test]
    fn reads_are_scoped_to_owners() {
        let s = setup();
        assert_eq!(run(&s, user(2), Command::Account { id: 1 }).1, Err(Error::NotFound));
        assert!(matches!(run(&s, user(1), Command::Account { id: 1 }).1, Ok(Reply::Account(_))));
        match run(&s, user(2), Command::Accounts).1 {
            Ok(Reply::Accounts(v)) => assert_eq!(v.iter().map(|a| a.id).collect::<Vec<_>>(), vec![2, 3]),
            r => panic!("{r:?}"),
        }
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
