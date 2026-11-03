//! The h5i-pay payment kernel: the payment state machine, refunds and
//! deduplicated PSP events, as pure Rust in the subset Charon and Aeneas
//! translate to Lean 4. The proofs in `proofs/Payments` are about this file.
//!
//! The kernel never talks to a PSP. The orchestrator (in `payment-api`)
//! moves a payment to `PROCESSING` before it calls the PSP, then records
//! what the PSP said with [`Command::Record`] (a synchronous response) or
//! [`Command::Event`] (a webhook, deduplicated by event id and tolerant of
//! out-of-order delivery). A payment stuck in `PROCESSING` after a timeout
//! is recovered by asking the PSP for its status and recording the answer.
//!
//! Statuses and the transitions the kernel allows:
//!
//! ```text
//! CREATED ──► PROCESSING ──► AUTHORIZED ──► CAPTURED
//!    │            │  │  └─────────┼──────────►  ▲
//!    │            │  └──► FAILED  │             │ (auto-capture reported directly)
//!    └────────────┴───────────────┴──► CANCELED
//! ```
//!
//! Refunds are rows of their own (`PENDING` → `SUCCEEDED` | `FAILED`). A
//! refund is reserved against the captured amount when it is requested, so
//! the refunded plus the in-flight refund amounts never exceed the capture.

// The kernel is written in the Rust subset Charon and Aeneas translate:
// `&Vec` parameters, `Vec::new` + `push`, explicit index loops and spelled-out
// branches are deliberate, so these style lints are off for this crate.
#![allow(
    clippy::ptr_arg,
    clippy::vec_init_then_push,
    clippy::if_same_then_else,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::single_match,
    clippy::match_like_matches_macro
)]

pub mod policy;

/// Who acts inside a tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    /// A user (a merchant), by user id.
    User(u64),
    /// One of the tenant's operators (support staff, the orchestrator,
    /// webhooks, the recovery job), by operator id. Only operators record
    /// PSP results; every row an operator writes records its id.
    Operator(u64),
}

/// Who is calling: the tenant and who acts in it, and when.
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

/// The acting user's id; 0 for an operator.
pub fn user_of(actor: &Principal) -> u64 {
    match actor.actor {
        Actor::User(u) => u,
        Actor::Operator(_) => 0,
    }
}

/// The acting operator's id, recorded on the rows it writes; 0 for a user.
pub fn operator_of(actor: &Principal) -> u64 {
    match actor.actor {
        Actor::User(_) => 0,
        Actor::Operator(o) => o,
    }
}

pub const CREATED: u32 = 0;
pub const PROCESSING: u32 = 1;
pub const AUTHORIZED: u32 = 2;
pub const CAPTURED: u32 = 3;
pub const CANCELED: u32 = 4;
pub const FAILED: u32 = 5;

pub const REFUND_PENDING: u32 = 0;
pub const REFUND_SUCCEEDED: u32 = 1;
pub const REFUND_FAILED: u32 = 2;

h5i_app_schema::schema! {
    mapping payment_tables for payment_domain, writes Write, lean "../../proofs/payments/generated/PaymentsSchema.lean";

    /// One tenant's payments.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Snapshot {
        counter: Counter,
        payments: Vec<Payment>,
        refunds: Vec<Refund>,
        events: Vec<Event>,
        refs: Vec<PspRef>,
        merchants: Vec<MerchantCounter>,
        history: Vec<HistoryEntry>,
    }

    /// A payment. Amounts are minor units of `currency` (ISO 4217 numeric).
    /// `operator` (in every table): the operator whose command last wrote
    /// the row, 0 if a user's did.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Payment in "payments" {
        key { id: u64 }
        merchant: u64,
        amount: u64,
        currency: u32,
        status: u32,
        manual_capture: bool,
        attempts: u64,
        authorized: u64,
        captured: u64,
        refunded: u64,
        refund_pending: u64,
        operator: u64,
        /// The ledger holds every posting this payment needs, as of its
        /// current state: set only by `MarkPosted`, cleared by every other
        /// command that writes the payment.
        ledger_done: bool,
        /// The payment's number among its merchant's payments (0, 1, …):
        /// the key `List` pages by.
        seq: u64,
        /// How many history entries the payment has: the next entry's `n`
        /// (G1). Every command that changes the payment adds one.
        changes: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Refund in "refunds" {
        key { id: u64 }
        payment: u64,
        amount: u64,
        status: u32,
        operator: u64,
    }

    /// A PSP event that has been processed (webhook deduplication).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Event in "psp_events" {
        key { id: u64 }
        payment: u64,
        operator: u64,
    }

    /// The PSP's reference for a payment (e.g. a Stripe PaymentIntent id).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct PspRef in "psp_refs" {
        key { payment: u64 }
        reference: Vec<u8>,
        operator: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct Counter in "payment_counter" {
        key {}
        next_id: u64,
    }

    /// How many payments a merchant has: the next payment's `seq`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MerchantCounter in "merchant_counters" {
        key { merchant: u64 }
        next: u64,
    }

    /// One change of a payment (G1): entry `n` of payment `payment`, written
    /// by `transition` with the change itself, never updated or deleted.
    /// Who (`by_operator`, `actor`: the user or operator id), when (`at`,
    /// the transaction's clock), what (`command`, `result`, `event`,
    /// `refund`), and the payment's status and amounts before and after.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HistoryEntry in "history" {
        key { payment: u64, n: u64 }
        by_operator: bool,
        actor: u64,
        at: u64,
        command: u32,
        result: u32,
        event: u64,
        refund: u64,
        status_before: u32,
        status_after: u32,
        captured_before: u64,
        captured_after: u64,
        refunded_before: u64,
        refunded_after: u64,
        pending_before: u64,
        pending_after: u64,
    }
}

/// The most payments one `List` page covers (sequence numbers per page).
pub const MAX_PAGE: u32 = 64;

/// The most ids one `Range` or `RefundRange` covers (H2): one
/// reconciliation period (`payment_api::orchestrator::MAX_PERIOD`), read as
/// one id range per table.
pub const MAX_RANGE: u32 = 4096;

/// What a PSP reported about a payment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PspResult {
    Authorized { amount: u64 },
    Declined,
    Captured { amount: u64 },
    Canceled,
    RefundSucceeded { refund: u64 },
    RefundFailed { refund: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// A merchant creates a payment.
    Create { amount: u64, currency: u32, manual_capture: bool },
    /// The orchestrator is about to ask the PSP to authorize.
    BeginAuthorize { id: u64 },
    /// Cancel a payment that never reached the PSP.
    Cancel { id: u64 },
    /// Reserve a refund against the captured amount.
    RequestRefund { payment: u64, amount: u64 },
    /// A synchronous PSP response (operator only).
    Record { payment: u64, result: PspResult },
    /// A PSP webhook (operator only): deduplicated by `event`, and a result
    /// that no longer applies (out of order) is recorded as stale.
    Event { event: u64, payment: u64, result: PspResult },
    /// Remember the PSP's reference for a payment (operator only). Setting
    /// the same reference again is a no-op; a different one is refused.
    AttachRef { payment: u64, reference: Vec<u8> },
    Get { id: u64 },
    /// One page of `merchant`'s payments: those numbered `from` to
    /// `from + limit - 1` among the merchant's payments (`Payment::seq`),
    /// `limit` at most `MAX_PAGE`. A merchant may list only its own.
    List { merchant: u64, from: u64, limit: u32 },
    /// One refund.
    GetRefund { id: u64 },
    /// A payment's refunds.
    Refunds { payment: u64 },
    /// A payment's PSP reference.
    GetRef { payment: u64 },
    /// A payment with its refunds and PSP reference: what `Get`, `Refunds`
    /// and `GetRef` return, in one command.
    View { id: u64 },
    /// The ledger holds every posting of `payment` as of `captured` and
    /// `refunds` (operator only: the orchestrator, after its ledger sync
    /// succeeded; for a failed or canceled payment, which needs none, after
    /// recording it). Marks the payment `ledger_done` if that is still its
    /// state; if it changed meanwhile, nothing is written (`Stale`).
    MarkPosted { payment: u64, captured: u64, refunds: Vec<Refund> },
    /// One page of a payment's history (operators only): its entries
    /// numbered `from` to `from + limit - 1`, `limit` at most `MAX_PAGE`.
    History { payment: u64, from: u64, limit: u32 },
    /// One page of the tenant's payments by id (operators only, G6): those
    /// with ids `from` to `from + limit - 1`, `limit` at most `MAX_RANGE`.
    /// Reconciliation reads a period of ids with it and `RefundRange`
    /// (payments and refunds are numbered by one counter).
    Range { from: u64, limit: u32 },
    /// One page of the tenant's refunds by id (operators only, G6): those
    /// with ids `from` to `from + limit - 1`, and the payment of each
    /// (refunds carry no currency).
    RefundRange { from: u64, limit: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    PutPayment(Payment),
    PutRefund(Refund),
    PutEvent(Event),
    PutRef(PspRef),
    SetCounter(Counter),
    PutMerchant(MerchantCounter),
    PutHistory(HistoryEntry),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Created(u64),
    /// The payment's new status.
    Status(u32),
    /// The result was already recorded; nothing changed.
    Unchanged,
    /// The event was processed before.
    Duplicate,
    /// The event no longer applies to the payment's status; recorded only.
    Stale,
    Payment(Payment),
    /// A page of payments, and where the next page starts (`None`: this
    /// was the last).
    Payments(Vec<Payment>, Option<u64>),
    Refund(Refund),
    Refunds(Vec<Refund>),
    Ref(Vec<u8>),
    /// A payment, its refunds, and its PSP reference if it has one.
    View(Payment, Vec<Refund>, Option<Vec<u8>>),
    /// A page of a payment's history, and where the next page starts.
    History(Vec<HistoryEntry>, Option<u64>),
    /// A page by id: payments (`Range`), or refunds and the payment of
    /// each, in the refunds' order (`RefundRange`); and where the next page
    /// starts (`None`: no id is past this page yet).
    Range(Vec<Payment>, Vec<Refund>, Vec<Payment>, Option<u64>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotFound,
    Forbidden,
    InvalidAmount,
    /// The payment's status does not allow this.
    InvalidTransition,
    /// The PSP reported an amount the payment does not allow.
    AmountMismatch,
    RefundExceedsCaptured,
    Overflow,
    /// A stored refund does not fit its payment (never happens in a reachable state).
    Corrupt,
    /// The payment already has a different PSP reference.
    RefConflict,
    /// A page limit of 0 or above `MAX_PAGE` (`MAX_RANGE` for `Range` and
    /// `RefundRange`).
    BadPage,
}

type Outcome = Result<(Vec<Write>, Reply), Error>;

fn one(a: Write) -> Vec<Write> {
    let mut ws = Vec::new();
    ws.push(a);
    ws
}

fn two(a: Write, b: Write) -> Vec<Write> {
    let mut ws = Vec::new();
    ws.push(a);
    ws.push(b);
    ws
}

fn three(a: Write, b: Write, c: Write) -> Vec<Write> {
    let mut ws = Vec::new();
    ws.push(a);
    ws.push(b);
    ws.push(c);
    ws
}

/// How many payments `merchant` has (its counter, 0 if it has none).
pub fn merchant_next(cs: &Vec<MerchantCounter>, merchant: u64) -> u64 {
    let mut i = 0;
    while i < cs.len() {
        if cs[i].merchant == merchant {
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

pub fn find_payment(ps: &Vec<Payment>, id: u64) -> Option<Payment> {
    let mut i = 0;
    while i < ps.len() {
        if ps[i].id == id {
            return Some(ps[i]);
        }
        i += 1;
    }
    None
}

pub fn find_refund(rs: &Vec<Refund>, id: u64) -> Option<Refund> {
    let mut i = 0;
    while i < rs.len() {
        if rs[i].id == id {
            return Some(rs[i]);
        }
        i += 1;
    }
    None
}

pub fn find_event(es: &Vec<Event>, id: u64) -> Option<Event> {
    let mut i = 0;
    while i < es.len() {
        if es[i].id == id {
            return Some(es[i]);
        }
        i += 1;
    }
    None
}

/// May `actor` act on payment `p` as its merchant?
pub fn may_manage(actor: &Principal, p: &Payment) -> bool {
    is_operator(actor) || p.merchant == user_of(actor)
}

/// `p` with a new status (and the authorized and captured amounts),
/// written by operator `op` (0: a user).
fn with_status(p: Payment, status: u32, authorized: u64, captured: u64, op: u64) -> Payment {
    Payment {
        id: p.id,
        merchant: p.merchant,
        amount: p.amount,
        currency: p.currency,
        status,
        manual_capture: p.manual_capture,
        attempts: p.attempts,
        authorized,
        captured,
        refunded: p.refunded,
        refund_pending: p.refund_pending,
        operator: op,
        ledger_done: false,
        seq: p.seq,
        changes: p.changes,
    }
}

/// `p` with new refund counters, written by operator `op` (0: a user).
fn with_refunds(p: Payment, refunded: u64, refund_pending: u64, op: u64) -> Payment {
    Payment {
        id: p.id,
        merchant: p.merchant,
        amount: p.amount,
        currency: p.currency,
        status: p.status,
        manual_capture: p.manual_capture,
        attempts: p.attempts,
        authorized: p.authorized,
        captured: p.captured,
        refunded,
        refund_pending,
        operator: op,
        ledger_done: false,
        seq: p.seq,
        changes: p.changes,
    }
}

fn create(actor: &Principal, s: &Snapshot, amount: u64, currency: u32, manual_capture: bool) -> Outcome {
    if amount == 0 {
        return Err(Error::InvalidAmount);
    }
    let c = s.counter;
    if c.next_id == u64::MAX {
        return Err(Error::Overflow);
    }
    let merchant = user_of(actor);
    let n = merchant_next(&s.merchants, merchant);
    if n == u64::MAX {
        return Err(Error::Overflow);
    }
    let p = Payment {
        id: c.next_id,
        merchant,
        amount,
        currency,
        status: CREATED,
        manual_capture,
        attempts: 0,
        authorized: 0,
        captured: 0,
        refunded: 0,
        refund_pending: 0,
        operator: operator_of(actor),
        ledger_done: false,
        seq: n,
        changes: 0,
    };
    let c2 = Counter { next_id: c.next_id + 1 };
    let m2 = MerchantCounter { merchant, next: n + 1 };
    Ok((three(Write::PutPayment(p), Write::SetCounter(c2), Write::PutMerchant(m2)), Reply::Created(p.id)))
}

fn begin_authorize(actor: &Principal, s: &Snapshot, id: u64) -> Outcome {
    match find_payment(&s.payments, id) {
        None => Err(Error::NotFound),
        Some(p) => {
            if !may_manage(actor, &p) {
                Err(Error::NotFound)
            } else if p.status != CREATED {
                Err(Error::InvalidTransition)
            } else if p.attempts == u64::MAX {
                Err(Error::Overflow)
            } else {
                let p2 = Payment {
                    id: p.id,
                    merchant: p.merchant,
                    amount: p.amount,
                    currency: p.currency,
                    status: PROCESSING,
                    manual_capture: p.manual_capture,
                    attempts: p.attempts + 1,
                    authorized: p.authorized,
                    captured: p.captured,
                    refunded: p.refunded,
                    refund_pending: p.refund_pending,
                    operator: operator_of(actor),
                    ledger_done: false,
                    seq: p.seq,
                    changes: p.changes,
                };
                Ok((one(Write::PutPayment(p2)), Reply::Payment(p2)))
            }
        }
    }
}

fn cancel(actor: &Principal, s: &Snapshot, id: u64) -> Outcome {
    match find_payment(&s.payments, id) {
        None => Err(Error::NotFound),
        Some(p) => {
            if !may_manage(actor, &p) {
                Err(Error::NotFound)
            } else if p.status == CANCELED {
                Ok((Vec::new(), Reply::Unchanged))
            } else if p.status != CREATED {
                // Once the PSP may have seen it, cancel through the PSP (void) and `Record`.
                Err(Error::InvalidTransition)
            } else {
                let p2 = with_status(p, CANCELED, p.authorized, p.captured, operator_of(actor));
                Ok((one(Write::PutPayment(p2)), Reply::Status(CANCELED)))
            }
        }
    }
}

fn request_refund(actor: &Principal, s: &Snapshot, id: u64, amount: u64) -> Outcome {
    match find_payment(&s.payments, id) {
        None => Err(Error::NotFound),
        Some(p) => {
            if !may_manage(actor, &p) {
                Err(Error::NotFound)
            } else if p.status != CAPTURED {
                Err(Error::InvalidTransition)
            } else if amount == 0 {
                Err(Error::InvalidAmount)
            } else if p.refund_pending > u64::MAX - amount {
                Err(Error::Overflow)
            } else if p.refunded > u64::MAX - (p.refund_pending + amount) {
                Err(Error::Overflow)
            } else if p.refunded + (p.refund_pending + amount) > p.captured {
                Err(Error::RefundExceedsCaptured)
            } else {
                let c = s.counter;
                if c.next_id == u64::MAX {
                    Err(Error::Overflow)
                } else {
                    let op = operator_of(actor);
                    let r = Refund { id: c.next_id, payment: p.id, amount, status: REFUND_PENDING, operator: op };
                    let p2 = with_refunds(p, p.refunded, p.refund_pending + amount, op);
                    let c2 = Counter { next_id: c.next_id + 1 };
                    Ok((
                        three(Write::PutRefund(r), Write::PutPayment(p2), Write::SetCounter(c2)),
                        Reply::Created(r.id),
                    ))
                }
            }
        }
    }
}

fn record_authorized(p: Payment, amount: u64, op: u64) -> Outcome {
    if p.status == PROCESSING {
        if amount != p.amount {
            Err(Error::AmountMismatch)
        } else {
            Ok((one(Write::PutPayment(with_status(p, AUTHORIZED, amount, p.captured, op))), Reply::Status(AUTHORIZED)))
        }
    } else if p.status == AUTHORIZED && p.authorized == amount {
        Ok((Vec::new(), Reply::Unchanged))
    } else {
        Err(Error::InvalidTransition)
    }
}

fn record_declined(p: Payment, op: u64) -> Outcome {
    if p.status == PROCESSING {
        Ok((one(Write::PutPayment(with_status(p, FAILED, p.authorized, p.captured, op))), Reply::Status(FAILED)))
    } else if p.status == FAILED {
        Ok((Vec::new(), Reply::Unchanged))
    } else {
        Err(Error::InvalidTransition)
    }
}

fn record_captured(p: Payment, amount: u64, op: u64) -> Outcome {
    if p.status == AUTHORIZED {
        if amount == 0 || amount > p.authorized {
            Err(Error::AmountMismatch)
        } else {
            Ok((one(Write::PutPayment(with_status(p, CAPTURED, p.authorized, amount, op))), Reply::Status(CAPTURED)))
        }
    } else if p.status == PROCESSING {
        // Automatic capture reported without a separate authorization.
        if amount == 0 || amount > p.amount {
            Err(Error::AmountMismatch)
        } else {
            Ok((one(Write::PutPayment(with_status(p, CAPTURED, amount, amount, op))), Reply::Status(CAPTURED)))
        }
    } else if p.status == CAPTURED && p.captured == amount {
        Ok((Vec::new(), Reply::Unchanged))
    } else {
        Err(Error::InvalidTransition)
    }
}

fn record_canceled(p: Payment, op: u64) -> Outcome {
    if p.status == CREATED || p.status == PROCESSING || p.status == AUTHORIZED {
        Ok((one(Write::PutPayment(with_status(p, CANCELED, p.authorized, p.captured, op))), Reply::Status(CANCELED)))
    } else if p.status == CANCELED {
        Ok((Vec::new(), Reply::Unchanged))
    } else {
        Err(Error::InvalidTransition)
    }
}

fn record_refund(s: &Snapshot, p: Payment, refund: u64, succeeded: bool, op: u64) -> Outcome {
    match find_refund(&s.refunds, refund) {
        None => Err(Error::NotFound),
        Some(r) => {
            if r.payment != p.id {
                Err(Error::NotFound)
            } else if r.status == REFUND_PENDING {
                if r.amount > p.refund_pending {
                    Err(Error::Corrupt)
                } else if succeeded {
                    if p.refunded > u64::MAX - r.amount {
                        Err(Error::Overflow)
                    } else {
                        let r2 = Refund { id: r.id, payment: r.payment, amount: r.amount, status: REFUND_SUCCEEDED, operator: op };
                        let p2 = with_refunds(p, p.refunded + r.amount, p.refund_pending - r.amount, op);
                        Ok((two(Write::PutRefund(r2), Write::PutPayment(p2)), Reply::Status(p.status)))
                    }
                } else {
                    let r2 = Refund { id: r.id, payment: r.payment, amount: r.amount, status: REFUND_FAILED, operator: op };
                    let p2 = with_refunds(p, p.refunded, p.refund_pending - r.amount, op);
                    Ok((two(Write::PutRefund(r2), Write::PutPayment(p2)), Reply::Status(p.status)))
                }
            } else if (succeeded && r.status == REFUND_SUCCEEDED) || (!succeeded && r.status == REFUND_FAILED) {
                Ok((Vec::new(), Reply::Unchanged))
            } else {
                Err(Error::InvalidTransition)
            }
        }
    }
}

/// What a PSP result does to payment `id`.
pub fn record(actor: &Principal, s: &Snapshot, id: u64, result: &PspResult) -> Outcome {
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    let op = operator_of(actor);
    match find_payment(&s.payments, id) {
        None => Err(Error::NotFound),
        Some(p) => match result {
            PspResult::Authorized { amount } => record_authorized(p, *amount, op),
            PspResult::Declined => record_declined(p, op),
            PspResult::Captured { amount } => record_captured(p, *amount, op),
            PspResult::Canceled => record_canceled(p, op),
            PspResult::RefundSucceeded { refund } => record_refund(s, p, *refund, true, op),
            PspResult::RefundFailed { refund } => record_refund(s, p, *refund, false, op),
        },
    }
}

fn append(ws: &mut Vec<Write>, more: &Vec<Write>) {
    let mut i = 0;
    while i < more.len() {
        ws.push(more[i].clone());
        i += 1;
    }
}

/// A webhook: at most one effect per event id, and stale results recorded
/// without effect.
pub fn event(actor: &Principal, s: &Snapshot, event: u64, id: u64, result: &PspResult) -> Outcome {
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    match find_event(&s.events, event) {
        Some(_) => Ok((Vec::new(), Reply::Duplicate)),
        None => {
            let e = Event { id: event, payment: id, operator: operator_of(actor) };
            match record(actor, s, id, result) {
                Ok((ws, reply)) => {
                    let mut out = Vec::new();
                    append(&mut out, &ws);
                    out.push(Write::PutEvent(e));
                    Ok((out, reply))
                }
                Err(Error::InvalidTransition) => Ok((one(Write::PutEvent(e)), Reply::Stale)),
                Err(err) => Err(err),
            }
        }
    }
}

fn get(actor: &Principal, s: &Snapshot, id: u64) -> Outcome {
    match find_payment(&s.payments, id) {
        None => Err(Error::NotFound),
        Some(p) => {
            if may_manage(actor, &p) {
                Ok((Vec::new(), Reply::Payment(p)))
            } else {
                Err(Error::NotFound)
            }
        }
    }
}

fn get_refund(actor: &Principal, s: &Snapshot, id: u64) -> Outcome {
    match find_refund(&s.refunds, id) {
        None => Err(Error::NotFound),
        Some(r) => match find_payment(&s.payments, r.payment) {
            None => Err(Error::NotFound),
            Some(p) => {
                if may_manage(actor, &p) {
                    Ok((Vec::new(), Reply::Refund(r)))
                } else {
                    Err(Error::NotFound)
                }
            }
        },
    }
}

pub fn find_ref(rs: &Vec<PspRef>, payment: u64) -> Option<PspRef> {
    let mut i = 0;
    while i < rs.len() {
        if rs[i].payment == payment {
            return Some(rs[i].clone());
        }
        i += 1;
    }
    None
}

fn same_bytes(a: &Vec<u8>, b: &Vec<u8>) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

fn attach_ref(actor: &Principal, s: &Snapshot, payment: u64, reference: &Vec<u8>) -> Outcome {
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    match find_payment(&s.payments, payment) {
        None => Err(Error::NotFound),
        Some(_) => match find_ref(&s.refs, payment) {
            Some(old) => {
                if same_bytes(&old.reference, reference) {
                    Ok((Vec::new(), Reply::Unchanged))
                } else {
                    Err(Error::RefConflict)
                }
            }
            None => Ok((one(Write::PutRef(PspRef { payment, reference: reference.clone(), operator: operator_of(actor) })), Reply::Unchanged)),
        },
    }
}

fn get_ref(actor: &Principal, s: &Snapshot, payment: u64) -> Outcome {
    match find_payment(&s.payments, payment) {
        None => Err(Error::NotFound),
        Some(p) => {
            if !may_manage(actor, &p) {
                Err(Error::NotFound)
            } else {
                match find_ref(&s.refs, payment) {
                    None => Err(Error::NotFound),
                    Some(r) => Ok((Vec::new(), Reply::Ref(r.reference))),
                }
            }
        }
    }
}

/// The refunds of `payment`, in table order.
pub fn refunds_for(rs: &Vec<Refund>, payment: u64) -> Vec<Refund> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rs.len() {
        if rs[i].payment == payment {
            out.push(rs[i]);
        }
        i += 1;
    }
    out
}

fn refunds_of(actor: &Principal, s: &Snapshot, payment: u64) -> Outcome {
    match find_payment(&s.payments, payment) {
        None => Err(Error::NotFound),
        Some(p) => {
            if !may_manage(actor, &p) {
                Err(Error::NotFound)
            } else {
                Ok((Vec::new(), Reply::Refunds(refunds_for(&s.refunds, payment))))
            }
        }
    }
}

fn view(actor: &Principal, s: &Snapshot, id: u64) -> Outcome {
    match find_payment(&s.payments, id) {
        None => Err(Error::NotFound),
        Some(p) => {
            if !may_manage(actor, &p) {
                Err(Error::NotFound)
            } else {
                let reference = match find_ref(&s.refs, id) {
                    None => None,
                    Some(r) => Some(r.reference),
                };
                Ok((Vec::new(), Reply::View(p, refunds_for(&s.refunds, id), reference)))
            }
        }
    }
}

/// Do `a` and `b` describe the same refund (the audit field aside)?
pub fn same_refund(a: &Refund, b: &Refund) -> bool {
    a.id == b.id && a.payment == b.payment && a.amount == b.amount && a.status == b.status
}

/// Does `ys` hold refund `x` (by id), the same?
fn has_same(ys: &Vec<Refund>, x: &Refund) -> bool {
    match find_refund(ys, x.id) {
        None => false,
        Some(y) => same_refund(x, &y),
    }
}

/// Is every refund of `xs` in `ys`, the same?
pub fn covers(xs: &Vec<Refund>, ys: &Vec<Refund>) -> bool {
    let mut all = true;
    let mut i = 0;
    while i < xs.len() {
        if !has_same(ys, &xs[i]) {
            all = false;
        }
        i += 1;
    }
    all
}

/// `p` marked as fully posted in the ledger, by operator `op`.
fn with_ledger_done(p: Payment, op: u64) -> Payment {
    Payment {
        id: p.id,
        merchant: p.merchant,
        amount: p.amount,
        currency: p.currency,
        status: p.status,
        manual_capture: p.manual_capture,
        attempts: p.attempts,
        authorized: p.authorized,
        captured: p.captured,
        refunded: p.refunded,
        refund_pending: p.refund_pending,
        operator: op,
        ledger_done: true,
        seq: p.seq,
        changes: p.changes,
    }
}

fn mark_posted(actor: &Principal, s: &Snapshot, id: u64, captured: u64, seen: &Vec<Refund>) -> Outcome {
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    match find_payment(&s.payments, id) {
        None => Err(Error::NotFound),
        Some(p) => {
            if p.status != CAPTURED && p.status != FAILED && p.status != CANCELED {
                Err(Error::InvalidTransition)
            } else if p.ledger_done {
                Ok((Vec::new(), Reply::Unchanged))
            } else {
                let rows = refunds_for(&s.refunds, id);
                if p.captured == captured && covers(&rows, seen) && covers(seen, &rows) {
                    Ok((one(Write::PutPayment(with_ledger_done(p, operator_of(actor)))), Reply::Status(p.status)))
                } else {
                    Ok((Vec::new(), Reply::Stale))
                }
            }
        }
    }
}

/// `merchant`'s payments numbered `from` up to (not including) `end`, in
/// table order.
fn merchant_range(s: &Snapshot, merchant: u64, from: u64, end: u64) -> Vec<Payment> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.payments.len() {
        let p = s.payments[i];
        if p.merchant == merchant && from <= p.seq && p.seq < end {
            out.push(p);
        }
        i += 1;
    }
    out
}

fn list(actor: &Principal, s: &Snapshot, merchant: u64, from: u64, limit: u32) -> Outcome {
    if limit == 0 || limit > MAX_PAGE {
        return Err(Error::BadPage);
    }
    if !is_operator(actor) && merchant != user_of(actor) {
        return Err(Error::Forbidden);
    }
    let end = page_end(from, limit);
    let next = if end < merchant_next(&s.merchants, merchant) { Some(end) } else { None };
    Ok((Vec::new(), Reply::Payments(merchant_range(s, merchant, from, end), next)))
}

/// `payment`'s history entries numbered `from` up to (not including)
/// `end`, in table order.
fn history_range(s: &Snapshot, payment: u64, from: u64, end: u64) -> Vec<HistoryEntry> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.history.len() {
        let h = s.history[i];
        if h.payment == payment && from <= h.n && h.n < end {
            out.push(h);
        }
        i += 1;
    }
    out
}

fn history_page(actor: &Principal, s: &Snapshot, payment: u64, from: u64, limit: u32) -> Outcome {
    if limit == 0 || limit > MAX_PAGE {
        return Err(Error::BadPage);
    }
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    match find_payment(&s.payments, payment) {
        None => Err(Error::NotFound),
        Some(p) => {
            let end = page_end(from, limit);
            let next = if end < p.changes { Some(end) } else { None };
            Ok((Vec::new(), Reply::History(history_range(s, payment, from, end), next)))
        }
    }
}

/// The payments with ids `from` up to (not including) `end`, in table order.
fn payment_ids(s: &Snapshot, from: u64, end: u64) -> Vec<Payment> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.payments.len() {
        let p = s.payments[i];
        if from <= p.id && p.id < end {
            out.push(p);
        }
        i += 1;
    }
    out
}

/// The refunds with ids `from` up to (not including) `end`, in table order.
fn refund_ids(s: &Snapshot, from: u64, end: u64) -> Vec<Refund> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.refunds.len() {
        let r = s.refunds[i];
        if from <= r.id && r.id < end {
            out.push(r);
        }
        i += 1;
    }
    out
}

/// The payment of each of `rs`, in their order (one the tenant lacks is skipped).
fn parents_of(s: &Snapshot, rs: &Vec<Refund>) -> Vec<Payment> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rs.len() {
        match find_payment(&s.payments, rs[i].payment) {
            Some(p) => out.push(p),
            None => {}
        }
        i += 1;
    }
    out
}

fn range_page(actor: &Principal, s: &Snapshot, from: u64, limit: u32) -> Outcome {
    if limit == 0 || limit > MAX_RANGE {
        return Err(Error::BadPage);
    }
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    let end = page_end(from, limit);
    let next = if end < s.counter.next_id { Some(end) } else { None };
    Ok((Vec::new(), Reply::Range(payment_ids(s, from, end), Vec::new(), Vec::new(), next)))
}

fn refund_range_page(actor: &Principal, s: &Snapshot, from: u64, limit: u32) -> Outcome {
    if limit == 0 || limit > MAX_RANGE {
        return Err(Error::BadPage);
    }
    if !is_operator(actor) {
        return Err(Error::Forbidden);
    }
    let end = page_end(from, limit);
    let next = if end < s.counter.next_id { Some(end) } else { None };
    let rs = refund_ids(s, from, end);
    let parents = parents_of(s, &rs);
    Ok((Vec::new(), Reply::Range(Vec::new(), rs, parents, next)))
}

/// What the commands decide, before the history entry is added
/// (`transition`): writes and reply, or a refusal that commits nothing.
pub fn decide_command(actor: &Principal, s: &Snapshot, cmd: &Command) -> Outcome {
    match cmd {
        Command::Create { amount, currency, manual_capture } => create(actor, s, *amount, *currency, *manual_capture),
        Command::BeginAuthorize { id } => begin_authorize(actor, s, *id),
        Command::Cancel { id } => cancel(actor, s, *id),
        Command::RequestRefund { payment, amount } => request_refund(actor, s, *payment, *amount),
        Command::Record { payment, result } => record(actor, s, *payment, result),
        Command::Event { event: ev, payment, result } => event(actor, s, *ev, *payment, result),
        Command::AttachRef { payment, reference } => attach_ref(actor, s, *payment, reference),
        Command::Get { id } => get(actor, s, *id),
        Command::List { merchant, from, limit } => list(actor, s, *merchant, *from, *limit),
        Command::GetRefund { id } => get_refund(actor, s, *id),
        Command::Refunds { payment } => refunds_of(actor, s, *payment),
        Command::GetRef { payment } => get_ref(actor, s, *payment),
        Command::View { id } => view(actor, s, *id),
        Command::MarkPosted { payment, captured, refunds } => mark_posted(actor, s, *payment, *captured, refunds),
        Command::History { payment, from, limit } => history_page(actor, s, *payment, *from, *limit),
        Command::Range { from, limit } => range_page(actor, s, *from, *limit),
        Command::RefundRange { from, limit } => refund_range_page(actor, s, *from, *limit),
    }
}

// ------------------------------------------------------------------ history

/// `HistoryEntry::command`: which command made the change.
pub const CMD_CREATE: u32 = 1;
pub const CMD_BEGIN_AUTHORIZE: u32 = 2;
pub const CMD_CANCEL: u32 = 3;
pub const CMD_REQUEST_REFUND: u32 = 4;
pub const CMD_RECORD: u32 = 5;
pub const CMD_EVENT: u32 = 6;
pub const CMD_ATTACH_REF: u32 = 7;
pub const CMD_MARK_POSTED: u32 = 8;
/// `HistoryEntry::result`: the PSP result a `Record` or `Event` carried.
pub const RES_NONE: u32 = 0;
pub const RES_AUTHORIZED: u32 = 1;
pub const RES_DECLINED: u32 = 2;
pub const RES_CAPTURED: u32 = 3;
pub const RES_CANCELED: u32 = 4;
pub const RES_REFUND_SUCCEEDED: u32 = 5;
pub const RES_REFUND_FAILED: u32 = 6;
/// `HistoryEntry::status_before` of a payment's first entry (`Create`).
pub const NO_STATUS: u32 = 255;

/// A PSP result's code, and the refund it names (0 if none).
fn result_code(r: &PspResult) -> (u32, u64) {
    match r {
        PspResult::Authorized { .. } => (RES_AUTHORIZED, 0),
        PspResult::Declined => (RES_DECLINED, 0),
        PspResult::Captured { .. } => (RES_CAPTURED, 0),
        PspResult::Canceled => (RES_CANCELED, 0),
        PspResult::RefundSucceeded { refund } => (RES_REFUND_SUCCEEDED, *refund),
        PspResult::RefundFailed { refund } => (RES_REFUND_FAILED, *refund),
    }
}

/// What a command is, for its history entry: (command, result, event,
/// refund). A refund request names the refund it creates (the counter).
fn what_of(s: &Snapshot, cmd: &Command) -> (u32, u32, u64, u64) {
    match cmd {
        Command::Create { .. } => (CMD_CREATE, RES_NONE, 0, 0),
        Command::BeginAuthorize { .. } => (CMD_BEGIN_AUTHORIZE, RES_NONE, 0, 0),
        Command::Cancel { .. } => (CMD_CANCEL, RES_NONE, 0, 0),
        Command::RequestRefund { .. } => (CMD_REQUEST_REFUND, RES_NONE, 0, s.counter.next_id),
        Command::Record { result, .. } => {
            let (r, refund) = result_code(result);
            (CMD_RECORD, r, 0, refund)
        }
        Command::Event { event: ev, result, .. } => {
            let (r, refund) = result_code(result);
            (CMD_EVENT, r, *ev, refund)
        }
        Command::AttachRef { .. } => (CMD_ATTACH_REF, RES_NONE, 0, 0),
        Command::MarkPosted { .. } => (CMD_MARK_POSTED, RES_NONE, 0, 0),
        _ => (0, RES_NONE, 0, 0),
    }
}

/// The payment an existing-payment command changes; `None` for `Create`
/// (and for reads, which write nothing).
fn subject(cmd: &Command) -> Option<u64> {
    match cmd {
        Command::BeginAuthorize { id } => Some(*id),
        Command::Cancel { id } => Some(*id),
        Command::RequestRefund { payment, .. } => Some(*payment),
        Command::Record { payment, .. } => Some(*payment),
        Command::Event { payment, .. } => Some(*payment),
        Command::AttachRef { payment, .. } => Some(*payment),
        Command::MarkPosted { payment, .. } => Some(*payment),
        _ => None,
    }
}

/// The id the history records for the acting principal: the user's or
/// the operator's (`HistoryEntry::by_operator` says which).
pub fn actor_id(actor: &Principal) -> u64 {
    match actor.actor {
        Actor::User(u) => u,
        Actor::Operator(o) => o,
    }
}

/// The last payment row `ws` writes, if any.
fn last_payment(ws: &Vec<Write>) -> Option<Payment> {
    let mut out = None;
    let mut i = 0;
    while i < ws.len() {
        match &ws[i] {
            Write::PutPayment(p) => out = Some(*p),
            _ => {}
        }
        i += 1;
    }
    out
}

/// `ws` without its payment and history writes.
fn others(ws: &Vec<Write>) -> Vec<Write> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ws.len() {
        match &ws[i] {
            Write::PutPayment(_) => {}
            Write::PutHistory(_) => {}
            w => out.push(w.clone()),
        }
        i += 1;
    }
    out
}

/// Only `MarkPosted` writes a payment marked `ledger_done` (F2).
fn keeps_mark(cmd: &Command) -> bool {
    match cmd {
        Command::MarkPosted { .. } => true,
        _ => false,
    }
}

/// `p` as entry `n + 1` of its history leaves it: written by `actor`, the
/// ledger marker kept only by `MarkPosted`.
fn changed(p: Payment, op: u64, done: bool, changes: u64) -> Payment {
    Payment {
        id: p.id,
        merchant: p.merchant,
        amount: p.amount,
        currency: p.currency,
        status: p.status,
        manual_capture: p.manual_capture,
        attempts: p.attempts,
        authorized: p.authorized,
        captured: p.captured,
        refunded: p.refunded,
        refund_pending: p.refund_pending,
        operator: op,
        ledger_done: done,
        seq: p.seq,
        changes,
    }
}

/// Add the history entry to what a command decided: the payment row it
/// wrote (or, if it wrote none, its row as it was) with `changes` bumped,
/// and entry `n` (the old `changes`; 0 for a new payment) saying who, when,
/// what, and the status and amounts before and after. Every other write is
/// kept. `transition` calls this for every non-empty write set, so no
/// change goes unrecorded (`Theorems.every_change_logged`).
fn logged(actor: &Principal, s: &Snapshot, cmd: &Command, ws: &Vec<Write>, reply: Reply) -> Outcome {
    let (command, result, event, refund) = what_of(s, cmd);
    let last = last_payment(ws);
    let (before, base) = match subject(cmd) {
        None => match last {
            None => return Err(Error::Corrupt),
            Some(p) => {
                if p.id != s.counter.next_id {
                    return Err(Error::Corrupt);
                }
                (None, p)
            }
        },
        Some(id) => match find_payment(&s.payments, id) {
            None => return Err(Error::NotFound),
            Some(old) => {
                let base = match last {
                    None => old,
                    Some(p) => p,
                };
                if base.id != id {
                    return Err(Error::Corrupt);
                }
                (Some(old), base)
            }
        },
    };
    let n = match before {
        None => 0,
        Some(old) => old.changes,
    };
    if n == u64::MAX {
        return Err(Error::Overflow);
    }
    let done = keeps_mark(cmd) && base.ledger_done;
    let after = changed(base, operator_of(actor), done, n + 1);
    let (status_before, b) = match before {
        None => (NO_STATUS, after),
        Some(old) => (old.status, old),
    };
    let h = HistoryEntry {
        payment: after.id,
        n,
        by_operator: is_operator(actor),
        actor: actor_id(actor),
        at: actor.at,
        command,
        result,
        event,
        refund,
        status_before,
        status_after: after.status,
        captured_before: b.captured,
        captured_after: after.captured,
        refunded_before: b.refunded,
        refunded_after: after.refunded,
        pending_before: b.refund_pending,
        pending_after: after.refund_pending,
    };
    let mut out = others(ws);
    out.push(Write::PutPayment(after));
    out.push(Write::PutHistory(h));
    Ok((out, reply))
}

/// The payment logic: a command's writes and reply, or a refusal that
/// commits nothing. Every write set that changes anything carries exactly
/// one history entry, for the payment it changes, naming the caller.
pub fn transition(actor: &Principal, s: &Snapshot, cmd: &Command) -> Outcome {
    match decide_command(actor, s, cmd) {
        Err(e) => Err(e),
        Ok((ws, reply)) => {
            if ws.len() == 0 {
                Ok((ws, reply))
            } else {
                logged(actor, s, cmd, &ws, reply)
            }
        }
    }
}

// ---------------------------------------------------------------- read plans

/// Table numbers, in `schema!` declaration order (`Payment::TABLE`, ...).
pub const T_PAYMENTS: u32 = 0;
pub const T_REFUNDS: u32 = 1;
pub const T_EVENTS: u32 = 2;
pub const T_REFS: u32 = 3;
pub const T_COUNTER: u32 = 4;
pub const T_MERCHANTS: u32 = 5;
pub const T_HISTORY: u32 = 6;
/// Column numbers: key columns first, then fields in declaration order.
pub const PAYMENT_ID: u32 = 0;
pub const PAYMENT_MERCHANT: u32 = 1;
pub const PAYMENT_SEQ: u32 = 13;
pub const MERCHANT_KEY: u32 = 0;
pub const REFUND_ID: u32 = 0;
pub const REFUND_PAYMENT: u32 = 1;
pub const EVENT_ID: u32 = 0;
pub const REF_PAYMENT: u32 = 0;
pub const HISTORY_PAYMENT: u32 = 0;
pub const HISTORY_N: u32 = 1;

fn v_int(x: u64) -> h5i_app_sql::Val {
    h5i_app_sql::Column::to_val(&x)
}

fn p_eq(col: u32, v: h5i_app_sql::Val) -> h5i_app_sql::Pred {
    h5i_app_sql::Pred::Eq { col, val: v }
}

fn p_or(a: h5i_app_sql::Pred, b: h5i_app_sql::Pred) -> h5i_app_sql::Pred {
    h5i_app_sql::Pred::Or(Box::new(a), Box::new(b))
}

/// The first `u64` stored as a negative `BIGINT`: `Column::to_val` keeps a
/// `u64`'s 64 bits, so keys from 2^63 up sort below 0 in PostgreSQL.
pub const SIGN: u64 = 0x8000_0000_0000_0000;

/// `col` holds one of `from`, …, `end - 1` (`from < end`): one index range
/// (`col >= from AND col < end`). In `BIGINT` order the ids wrap at 2^63, so
/// a range that crosses it is `col >= from OR col < end`.
fn seq_range(col: u32, from: u64, end: u64) -> h5i_app_sql::Pred {
    let ge = h5i_app_sql::Pred::Ge { col, val: v_int(from) };
    let lt = h5i_app_sql::Pred::Lt { col, val: v_int(end) };
    if from < SIGN && end >= SIGN {
        p_or(ge, lt)
    } else {
        h5i_app_sql::Pred::And(Box::new(ge), Box::new(lt))
    }
}

fn rd_where(table: u32, pred: h5i_app_sql::Pred) -> h5i_app_sql::Read {
    h5i_app_sql::Read { table, pred, window: h5i_app_sql::whole() }
}

/// What `record` reads besides the payment: the refund a refund result names.
fn result_reads(result: &PspResult, out: &mut Vec<h5i_app_sql::Read>) {
    match result {
        PspResult::RefundSucceeded { refund } => out.push(rd_where(T_REFUNDS, p_eq(REFUND_ID, v_int(*refund)))),
        PspResult::RefundFailed { refund } => out.push(rd_where(T_REFUNDS, p_eq(REFUND_ID, v_int(*refund)))),
        _ => {}
    }
}

/// `cmd` only reads: whenever `transition` accepts it, it writes nothing
/// (`payment_domain.ReadOnly.sound`). The engine runs these in a
/// `SERIALIZABLE READ ONLY DEFERRABLE` transaction: no predicate locks, and
/// no aborts caused for concurrent writes (G6: reconciliation pages read
/// while payments are made).
pub fn read_only(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Get { .. }
            | Command::List { .. }
            | Command::GetRefund { .. }
            | Command::Refunds { .. }
            | Command::GetRef { .. }
            | Command::View { .. }
            | Command::History { .. }
            | Command::Range { .. }
            | Command::RefundRange { .. }
    )
}

/// The rows `transition(actor, _, cmd)` reads, one declarative read per
/// table (`Store::load_for`); a table without a read loads empty.
/// `proofs/payments/Payments/Frame.lean` proves that `transition` gives the
/// same result on what the plan loads as on the whole tenant.
pub fn reads(actor: &Principal, cmd: &Command) -> Option<Vec<h5i_app_sql::Read>> {
    let mut out = Vec::new();
    match cmd {
        Command::Create { .. } => {
            out.push(rd_where(T_COUNTER, h5i_app_sql::Pred::All));
            out.push(rd_where(T_MERCHANTS, p_eq(MERCHANT_KEY, v_int(user_of(actor)))));
        }
        Command::BeginAuthorize { id } => out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*id)))),
        Command::Cancel { id } => out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*id)))),
        Command::RequestRefund { payment, .. } => {
            out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*payment))));
            out.push(rd_where(T_COUNTER, h5i_app_sql::Pred::All));
        }
        Command::Record { payment, result } => {
            out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*payment))));
            result_reads(result, &mut out);
        }
        Command::Event { event: ev, payment, result } => {
            out.push(rd_where(T_EVENTS, p_eq(EVENT_ID, v_int(*ev))));
            out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*payment))));
            result_reads(result, &mut out);
        }
        Command::AttachRef { payment, .. } => {
            out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*payment))));
            out.push(rd_where(T_REFS, p_eq(REF_PAYMENT, v_int(*payment))));
        }
        Command::Get { id } => out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*id)))),
        Command::List { merchant, from, limit } => {
            // Refused pages read nothing.
            if *limit != 0 && *limit <= MAX_PAGE && (is_operator(actor) || *merchant == user_of(actor)) {
                let end = page_end(*from, *limit);
                if *from < end {
                    let range = seq_range(PAYMENT_SEQ, *from, end);
                    let pred = h5i_app_sql::Pred::And(Box::new(p_eq(PAYMENT_MERCHANT, v_int(*merchant))), Box::new(range));
                    out.push(rd_where(T_PAYMENTS, pred));
                }
                out.push(rd_where(T_MERCHANTS, p_eq(MERCHANT_KEY, v_int(*merchant))));
            }
        }
        Command::GetRefund { id } => {
            out.push(rd_where(T_REFUNDS, p_eq(REFUND_ID, v_int(*id))));
            // The payment of that refund.
            let of_refund = h5i_app_sql::Pred::In {
                col: PAYMENT_ID,
                table: T_REFUNDS,
                pred: Box::new(p_eq(REFUND_ID, v_int(*id))),
                of: REFUND_PAYMENT,
                window: h5i_app_sql::whole(),
            };
            out.push(rd_where(T_PAYMENTS, of_refund));
        }
        Command::Refunds { payment } => {
            out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*payment))));
            out.push(rd_where(T_REFUNDS, p_eq(REFUND_PAYMENT, v_int(*payment))));
        }
        Command::GetRef { payment } => {
            out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*payment))));
            out.push(rd_where(T_REFS, p_eq(REF_PAYMENT, v_int(*payment))));
        }
        Command::View { id } => {
            out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*id))));
            out.push(rd_where(T_REFUNDS, p_eq(REFUND_PAYMENT, v_int(*id))));
            out.push(rd_where(T_REFS, p_eq(REF_PAYMENT, v_int(*id))));
        }
        Command::MarkPosted { payment, .. } => {
            out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*payment))));
            out.push(rd_where(T_REFUNDS, p_eq(REFUND_PAYMENT, v_int(*payment))));
        }
        Command::History { payment, from, limit } => {
            // Refused pages read nothing.
            if *limit != 0 && *limit <= MAX_PAGE && is_operator(actor) {
                out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_ID, v_int(*payment))));
                let end = page_end(*from, *limit);
                if *from < end {
                    let range = seq_range(HISTORY_N, *from, end);
                    let pred = h5i_app_sql::Pred::And(Box::new(p_eq(HISTORY_PAYMENT, v_int(*payment))), Box::new(range));
                    out.push(rd_where(T_HISTORY, pred));
                }
            }
        }
        Command::Range { from, limit } => {
            // Refused pages read nothing.
            if *limit != 0 && *limit <= MAX_RANGE && is_operator(actor) {
                let end = page_end(*from, *limit);
                if *from < end {
                    out.push(rd_where(T_PAYMENTS, seq_range(PAYMENT_ID, *from, end)));
                }
                out.push(rd_where(T_COUNTER, h5i_app_sql::Pred::All));
            }
        }
        Command::RefundRange { from, limit } => {
            // Refused pages read nothing.
            if *limit != 0 && *limit <= MAX_RANGE && is_operator(actor) {
                let end = page_end(*from, *limit);
                if *from < end {
                    out.push(rd_where(T_REFUNDS, seq_range(REFUND_ID, *from, end)));
                    // The payments of those refunds (not `OR`ed with an id
                    // range: PostgreSQL scans the whole index for that).
                    let parents = h5i_app_sql::Pred::In {
                        col: PAYMENT_ID,
                        table: T_REFUNDS,
                        pred: Box::new(seq_range(REFUND_ID, *from, end)),
                        of: REFUND_PAYMENT,
                        window: h5i_app_sql::whole(),
                    };
                    out.push(rd_where(T_PAYMENTS, parents));
                }
                out.push(rd_where(T_COUNTER, h5i_app_sql::Pred::All));
            }
        }
    }
    Some(out)
}

/// What one write does to the state; `schema!` builds `apply` from it.
fn apply_write(s: &mut Snapshot, w: Write) {
    match w {
        Write::PutPayment(p) => Payment::put(&mut s.payments, p),
        Write::PutRefund(r) => Refund::put(&mut s.refunds, r),
        Write::PutEvent(e) => Event::put(&mut s.events, e),
        Write::PutRef(r) => PspRef::put(&mut s.refs, r),
        Write::SetCounter(c) => s.counter = c,
        Write::PutMerchant(c) => MerchantCounter::put(&mut s.merchants, c),
        Write::PutHistory(h) => HistoryEntry::put(&mut s.history, h),
    }
}

/// The table writes one write makes.
fn sql_write(w: &Write, out: &mut Vec<h5i_app_sql::Write>) {
    match w {
        Write::PutPayment(p) => out.push(p.sql_put()),
        Write::PutRefund(r) => out.push(r.sql_put()),
        Write::PutEvent(e) => out.push(e.sql_put()),
        Write::PutRef(r) => out.push(r.sql_put()),
        Write::SetCounter(c) => out.push(c.sql_put()),
        Write::PutMerchant(c) => out.push(c.sql_put()),
        Write::PutHistory(h) => out.push(h.sql_put()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OP: Principal = Principal { org: 1, actor: Actor::Operator(9), at: 5 };
    const M: Principal = Principal { org: 1, actor: Actor::User(7), at: 3 };
    const OTHER: Principal = Principal { org: 1, actor: Actor::User(8), at: 0 };

    fn run(s: &Snapshot, a: Principal, c: Command) -> (Snapshot, Result<Reply, Error>) {
        match transition(&a, s, &c) {
            Ok((ws, r)) => (apply(s, &ws), Ok(r)),
            Err(e) => (s.clone(), Err(e)),
        }
    }

    fn pay(s: &Snapshot, id: u64) -> Payment {
        find_payment(&s.payments, id).unwrap()
    }

    fn captured(amount: u64) -> (Snapshot, u64) {
        let s = Snapshot::default();
        let (s, r) = run(&s, M, Command::Create { amount, currency: 392, manual_capture: true });
        let Ok(Reply::Created(id)) = r else { panic!("{r:?}") };
        let (s, _) = run(&s, M, Command::BeginAuthorize { id });
        let (s, r) = run(&s, OP, Command::Record { payment: id, result: PspResult::Authorized { amount } });
        assert_eq!(r, Ok(Reply::Status(AUTHORIZED)));
        let (s, r) = run(&s, OP, Command::Record { payment: id, result: PspResult::Captured { amount } });
        assert_eq!(r, Ok(Reply::Status(CAPTURED)));
        (s, id)
    }

    #[test]
    fn lifecycle_and_invalid_transitions() {
        let (s, id) = captured(1000);
        assert_eq!(pay(&s, id).status, CAPTURED);
        // Recording the same capture again changes nothing.
        let (s2, r) = run(&s, OP, Command::Record { payment: id, result: PspResult::Captured { amount: 1000 } });
        assert_eq!(r, Ok(Reply::Unchanged));
        assert_eq!(s2, s);
        for bad in [PspResult::Authorized { amount: 1000 }, PspResult::Declined, PspResult::Canceled] {
            assert_eq!(run(&s, OP, Command::Record { payment: id, result: bad }).1, Err(Error::InvalidTransition));
        }
        assert_eq!(run(&s, M, Command::Cancel { id }).1, Err(Error::InvalidTransition));
        assert_eq!(run(&s, M, Command::Record { payment: id, result: PspResult::Declined }).1, Err(Error::Forbidden));
        assert_eq!(run(&s, OTHER, Command::Get { id }).1, Err(Error::NotFound));
    }

    #[test]
    fn refunds_never_exceed_the_capture() {
        let (s, id) = captured(1000);
        let (s, r) = run(&s, M, Command::RequestRefund { payment: id, amount: 600 });
        let Ok(Reply::Created(r1)) = r else { panic!("{r:?}") };
        // 600 is in flight, so only 400 more may be requested.
        assert_eq!(run(&s, M, Command::RequestRefund { payment: id, amount: 401 }).1, Err(Error::RefundExceedsCaptured));
        let (s, r) = run(&s, M, Command::RequestRefund { payment: id, amount: 400 });
        let Ok(Reply::Created(r2)) = r else { panic!("{r:?}") };
        let (s, _) = run(&s, OP, Command::Record { payment: id, result: PspResult::RefundSucceeded { refund: r1 } });
        let (s, _) = run(&s, OP, Command::Record { payment: id, result: PspResult::RefundFailed { refund: r2 } });
        let p = pay(&s, id);
        assert_eq!((p.refunded, p.refund_pending), (600, 0));
        // The failed refund's 400 can be requested again.
        let (s, r) = run(&s, M, Command::RequestRefund { payment: id, amount: 400 });
        assert!(r.is_ok());
        assert_eq!(run(&s, M, Command::RequestRefund { payment: id, amount: 1 }).1, Err(Error::RefundExceedsCaptured));
        assert_eq!(
            run(&s, OP, Command::Record { payment: id, result: PspResult::RefundSucceeded { refund: r2 } }).1,
            Err(Error::InvalidTransition)
        );
    }

    #[test]
    fn webhooks_dedup_and_tolerate_reordering() {
        let s = Snapshot::default();
        let (s, _) = run(&s, M, Command::Create { amount: 500, currency: 840, manual_capture: false });
        let (s, _) = run(&s, M, Command::BeginAuthorize { id: 0 });
        // `captured` arrives before `authorized`.
        let (s, r) = run(&s, OP, Command::Event { event: 11, payment: 0, result: PspResult::Captured { amount: 500 } });
        assert_eq!(r, Ok(Reply::Status(CAPTURED)));
        let (s, r) = run(&s, OP, Command::Event { event: 10, payment: 0, result: PspResult::Authorized { amount: 500 } });
        assert_eq!(r, Ok(Reply::Stale));
        let (s2, r) = run(&s, OP, Command::Event { event: 11, payment: 0, result: PspResult::Captured { amount: 500 } });
        assert_eq!(r, Ok(Reply::Duplicate));
        assert_eq!(s2, s);
        assert_eq!(pay(&s, 0).status, CAPTURED);
        assert_eq!(s.events.len(), 2);
    }

    #[test]
    fn psp_refs_are_write_once() {
        let s = Snapshot::default();
        let (s, _) = run(&s, M, Command::Create { amount: 5, currency: 392, manual_capture: false });
        assert_eq!(run(&s, M, Command::AttachRef { payment: 0, reference: b"pi_1".to_vec() }).1, Err(Error::Forbidden));
        let (s, _) = run(&s, OP, Command::AttachRef { payment: 0, reference: b"pi_1".to_vec() });
        let (s2, r) = run(&s, OP, Command::AttachRef { payment: 0, reference: b"pi_1".to_vec() });
        assert_eq!((r, &s2), (Ok(Reply::Unchanged), &s));
        assert_eq!(run(&s, OP, Command::AttachRef { payment: 0, reference: b"pi_2".to_vec() }).1, Err(Error::RefConflict));
        assert_eq!(run(&s, M, Command::GetRef { payment: 0 }).1, Ok(Reply::Ref(b"pi_1".to_vec())));
        assert_eq!(run(&s, OTHER, Command::GetRef { payment: 0 }).1, Err(Error::NotFound));
    }

    #[test]
    fn rows_name_the_operator_who_wrote_them() {
        let op2 = Principal { org: 1, actor: Actor::Operator(2), at: 0 };
        let (s, id) = captured(100);
        // `captured` recorded as operator 9; the merchant's refund request as a user.
        assert_eq!(pay(&s, id).operator, 9);
        let (s, r) = run(&s, M, Command::RequestRefund { payment: id, amount: 10 });
        let Ok(Reply::Created(rid)) = r else { panic!("{r:?}") };
        assert_eq!((pay(&s, id).operator, find_refund(&s.refunds, rid).unwrap().operator), (0, 0));
        let (s, _) = run(&s, op2, Command::Event { event: 5, payment: id, result: PspResult::RefundSucceeded { refund: rid } });
        assert_eq!((pay(&s, id).operator, find_refund(&s.refunds, rid).unwrap().operator), (2, 2));
        assert_eq!(find_event(&s.events, 5).unwrap().operator, 2);
        // A user is never an operator, whatever its id.
        let u9 = Principal { org: 1, actor: Actor::User(9), at: 0 };
        assert_eq!(run(&s, u9, Command::Record { payment: id, result: PspResult::Canceled }).1, Err(Error::Forbidden));
    }

    #[test]
    fn ledger_done_is_set_only_on_the_state_it_names_and_cleared_by_changes() {
        let (s, id) = captured(100);
        let p = pay(&s, id);
        assert!(!p.ledger_done);
        // A payment still in flight cannot be marked; a failed one can.
        let (s1, r) = run(&s, M, Command::Create { amount: 5, currency: 392, manual_capture: false });
        let Ok(Reply::Created(other)) = r else { panic!("{r:?}") };
        let in_flight = Command::MarkPosted { payment: other, captured: 0, refunds: vec![] };
        assert_eq!(run(&s1, OP, in_flight.clone()).1, Err(Error::InvalidTransition));
        let (s1, _) = run(&s1, M, Command::BeginAuthorize { id: other });
        let (s1, _) = run(&s1, OP, Command::Record { payment: other, result: PspResult::Declined });
        let (s1, r) = run(&s1, OP, in_flight);
        assert_eq!((r, pay(&s1, other).ledger_done), (Ok(Reply::Status(FAILED)), true));
        let mark = |captured, refunds: Vec<Refund>| Command::MarkPosted { payment: id, captured, refunds };
        assert_eq!(run(&s, M, mark(100, vec![])).1, Err(Error::Forbidden));
        assert_eq!(run(&s, OP, mark(99, vec![])).1, Ok(Reply::Stale));
        let (s, r) = run(&s, OP, mark(100, vec![]));
        assert_eq!((r, pay(&s, id).ledger_done), (Ok(Reply::Status(CAPTURED)), true));
        assert_eq!(run(&s, OP, mark(100, vec![])).1, Ok(Reply::Unchanged));
        // A refund needs new postings: the mark goes.
        let (s, r) = run(&s, M, Command::RequestRefund { payment: id, amount: 10 });
        let Ok(Reply::Created(rid)) = r else { panic!("{r:?}") };
        assert!(!pay(&s, id).ledger_done);
        let pending = find_refund(&s.refunds, rid).unwrap();
        // The orchestrator saw no refund: stale. A refund it did not see: stale.
        assert_eq!(run(&s, OP, mark(100, vec![])).1, Ok(Reply::Stale));
        let other = Refund { id: rid + 1, ..pending };
        assert_eq!(run(&s, OP, mark(100, vec![pending, other])).1, Ok(Reply::Stale));
        let (s, _) = run(&s, OP, mark(100, vec![pending]));
        assert!(pay(&s, id).ledger_done);
        let (s, _) = run(&s, OP, Command::Record { payment: id, result: PspResult::RefundSucceeded { refund: rid } });
        assert!(!pay(&s, id).ledger_done);
        // The pending refund it saw has since succeeded: stale.
        assert_eq!(run(&s, OP, mark(100, vec![pending])).1, Ok(Reply::Stale));
        let done = find_refund(&s.refunds, rid).unwrap();
        assert_eq!(run(&s, OP, mark(100, vec![done])).1, Ok(Reply::Status(CAPTURED)));
    }

    #[test]
    fn pages_of_a_merchants_payments() {
        let mut s = Snapshot::default();
        for i in 0..5 {
            let who = if i == 2 { OTHER } else { M };
            s = run(&s, who, Command::Create { amount: 10, currency: 392, manual_capture: false }).0;
        }
        let page = |s: &Snapshot, a, merchant, from, limit| match run(s, a, Command::List { merchant, from, limit }).1 {
            Ok(Reply::Payments(ps, next)) => Ok((ps.iter().map(|p| (p.id, p.seq)).collect::<Vec<_>>(), next)),
            Ok(r) => panic!("{r:?}"),
            Err(e) => Err(e),
        };
        // M has ids 0, 1, 3, 4, numbered 0..4; OTHER has id 2, numbered 0.
        assert_eq!(page(&s, M, 7, 0, 3), Ok((vec![(0, 0), (1, 1), (3, 2)], Some(3))));
        assert_eq!(page(&s, M, 7, 3, 3), Ok((vec![(4, 3)], None)));
        assert_eq!(page(&s, OP, 8, 0, MAX_PAGE), Ok((vec![(2, 0)], None)));
        assert_eq!(page(&s, M, 8, 0, 1), Err(Error::Forbidden));
        assert_eq!(page(&s, M, 7, 0, 0), Err(Error::BadPage));
        assert_eq!(page(&s, M, 7, 0, MAX_PAGE + 1), Err(Error::BadPage));
        assert_eq!(page(&s, M, 7, u64::MAX, 2), Ok((vec![], None)));
    }

    #[test]
    fn every_change_has_one_history_entry() {
        let (s, id) = captured(100);
        let entries = |s: &Snapshot| -> Vec<_> {
            s.history
                .iter()
                .filter(|h| h.payment == id)
                .map(|h| (h.n, h.command, h.result, h.by_operator, h.actor, h.at, h.status_before, h.status_after))
                .collect()
        };
        assert_eq!(pay(&s, id).changes, 4);
        assert_eq!(
            entries(&s),
            vec![
                (0, CMD_CREATE, RES_NONE, false, 7, 3, NO_STATUS, CREATED),
                (1, CMD_BEGIN_AUTHORIZE, RES_NONE, false, 7, 3, CREATED, PROCESSING),
                (2, CMD_RECORD, RES_AUTHORIZED, true, 9, 5, PROCESSING, AUTHORIZED),
                (3, CMD_RECORD, RES_CAPTURED, true, 9, 5, AUTHORIZED, CAPTURED),
            ]
        );
        // A no-op writes nothing, so it records nothing.
        let (s2, r) = run(&s, OP, Command::Record { payment: id, result: PspResult::Captured { amount: 100 } });
        assert_eq!((r, &s2), (Ok(Reply::Unchanged), &s));
        // A stale webhook, a PSP reference and the ledger mark are changes too.
        let (s, r) = run(&s, OP, Command::Event { event: 3, payment: id, result: PspResult::Declined });
        assert_eq!(r, Ok(Reply::Stale));
        let (s, _) = run(&s, OP, Command::AttachRef { payment: id, reference: b"pi".to_vec() });
        let (s, _) = run(&s, OP, Command::MarkPosted { payment: id, captured: 100, refunds: vec![] });
        let (s, r) = run(&s, M, Command::RequestRefund { payment: id, amount: 30 });
        let Ok(Reply::Created(rid)) = r else { panic!("{r:?}") };
        let tail: Vec<_> = s.history.iter().filter(|h| h.payment == id && h.n >= 4).map(|h| (h.n, h.command, h.event, h.refund)).collect();
        assert_eq!(tail, vec![(4, CMD_EVENT, 3, 0), (5, CMD_ATTACH_REF, 0, 0), (6, CMD_MARK_POSTED, 0, 0), (7, CMD_REQUEST_REFUND, 0, rid)]);
        let last = s.history.iter().find(|h| h.payment == id && h.n == 7).unwrap();
        assert_eq!((last.pending_before, last.pending_after, last.captured_after), (0, 30, 100));
        assert_eq!(s.history.len(), 8);
        // Paged, operators only.
        let page = run(&s, OP, Command::History { payment: id, from: 2, limit: 3 }).1;
        let Ok(Reply::History(hs, next)) = page else { panic!("{page:?}") };
        assert_eq!((hs.iter().map(|h| h.n).collect::<Vec<_>>(), next), (vec![2, 3, 4], Some(5)));
        let page = run(&s, OP, Command::History { payment: id, from: 5, limit: 3 }).1;
        assert!(matches!(page, Ok(Reply::History(ref hs, None)) if hs.len() == 3));
        assert_eq!(run(&s, M, Command::History { payment: id, from: 0, limit: 3 }).1, Err(Error::Forbidden));
        assert_eq!(run(&s, OP, Command::History { payment: id, from: 0, limit: 0 }).1, Err(Error::BadPage));
        assert_eq!(run(&s, OP, Command::History { payment: 99, from: 0, limit: 1 }).1, Err(Error::NotFound));
    }

    #[test]
    fn declines_and_cancels() {
        let s = Snapshot::default();
        let (s, _) = run(&s, M, Command::Create { amount: 5, currency: 392, manual_capture: false });
        let (s, _) = run(&s, M, Command::Create { amount: 6, currency: 392, manual_capture: false });
        let (s, r) = run(&s, M, Command::Cancel { id: 1 });
        assert_eq!(r, Ok(Reply::Status(CANCELED)));
        assert_eq!(run(&s, M, Command::BeginAuthorize { id: 1 }).1, Err(Error::InvalidTransition));
        let (s, _) = run(&s, M, Command::BeginAuthorize { id: 0 });
        assert_eq!(run(&s, M, Command::Cancel { id: 0 }).1, Err(Error::InvalidTransition));
        let (s, r) = run(&s, OP, Command::Record { payment: 0, result: PspResult::Declined });
        assert_eq!(r, Ok(Reply::Status(FAILED)));
        assert_eq!(run(&s, OP, Command::Record { payment: 0, result: PspResult::Captured { amount: 5 } }).1, Err(Error::InvalidTransition));
        assert_eq!(run(&s, M, Command::Create { amount: 0, currency: 392, manual_capture: false }).1, Err(Error::InvalidAmount));
    }
}
