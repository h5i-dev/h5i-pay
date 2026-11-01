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
#![allow(clippy::ptr_arg, clippy::vec_init_then_push, clippy::if_same_then_else, clippy::len_zero, clippy::manual_range_contains)]

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

/// Who is calling: the tenant and who acts in it.
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
}

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
    /// The caller's payments (every payment, for an operator).
    List,
    /// One refund.
    GetRefund { id: u64 },
    /// A payment's refunds.
    Refunds { payment: u64 },
    /// A payment's PSP reference.
    GetRef { payment: u64 },
    /// A payment with its refunds and PSP reference: what `Get`, `Refunds`
    /// and `GetRef` return, in one command.
    View { id: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    PutPayment(Payment),
    PutRefund(Refund),
    PutEvent(Event),
    PutRef(PspRef),
    SetCounter(Counter),
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
    Payments(Vec<Payment>),
    Refund(Refund),
    Refunds(Vec<Refund>),
    Ref(Vec<u8>),
    /// A payment, its refunds, and its PSP reference if it has one.
    View(Payment, Vec<Refund>, Option<Vec<u8>>),
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
    let p = Payment {
        id: c.next_id,
        merchant: user_of(actor),
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
    };
    let c2 = Counter { next_id: c.next_id + 1 };
    Ok((two(Write::PutPayment(p), Write::SetCounter(c2)), Reply::Created(p.id)))
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

fn visible(actor: &Principal, s: &Snapshot) -> Vec<Payment> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.payments.len() {
        if may_manage(actor, &s.payments[i]) {
            out.push(s.payments[i]);
        }
        i += 1;
    }
    out
}

/// The payment logic: a command's writes and reply, or a refusal that
/// commits nothing.
pub fn transition(actor: &Principal, s: &Snapshot, cmd: &Command) -> Outcome {
    match cmd {
        Command::Create { amount, currency, manual_capture } => create(actor, s, *amount, *currency, *manual_capture),
        Command::BeginAuthorize { id } => begin_authorize(actor, s, *id),
        Command::Cancel { id } => cancel(actor, s, *id),
        Command::RequestRefund { payment, amount } => request_refund(actor, s, *payment, *amount),
        Command::Record { payment, result } => record(actor, s, *payment, result),
        Command::Event { event: ev, payment, result } => event(actor, s, *ev, *payment, result),
        Command::AttachRef { payment, reference } => attach_ref(actor, s, *payment, reference),
        Command::Get { id } => get(actor, s, *id),
        Command::List => Ok((Vec::new(), Reply::Payments(visible(actor, s)))),
        Command::GetRefund { id } => get_refund(actor, s, *id),
        Command::Refunds { payment } => refunds_of(actor, s, *payment),
        Command::GetRef { payment } => get_ref(actor, s, *payment),
        Command::View { id } => view(actor, s, *id),
    }
}

// ---------------------------------------------------------------- read plans

/// Table numbers, in `schema!` declaration order (`Payment::TABLE`, ...).
pub const T_PAYMENTS: u32 = 0;
pub const T_REFUNDS: u32 = 1;
pub const T_EVENTS: u32 = 2;
pub const T_REFS: u32 = 3;
pub const T_COUNTER: u32 = 4;
/// Column numbers: key columns first, then fields in declaration order.
pub const PAYMENT_ID: u32 = 0;
pub const PAYMENT_MERCHANT: u32 = 1;
pub const REFUND_ID: u32 = 0;
pub const REFUND_PAYMENT: u32 = 1;
pub const EVENT_ID: u32 = 0;
pub const REF_PAYMENT: u32 = 0;

fn v_int(x: u64) -> h5i_app_sql::Val {
    h5i_app_sql::Column::to_val(&x)
}

fn p_eq(col: u32, v: h5i_app_sql::Val) -> h5i_app_sql::Pred {
    h5i_app_sql::Pred::Eq { col, val: v }
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

/// The rows `transition(actor, _, cmd)` reads, one declarative read per
/// table (`Store::load_for`); a table without a read loads empty.
/// `proofs/payments/Payments/Frame.lean` proves that `transition` gives the
/// same result on what the plan loads as on the whole tenant.
pub fn reads(actor: &Principal, cmd: &Command) -> Option<Vec<h5i_app_sql::Read>> {
    let mut out = Vec::new();
    match cmd {
        Command::Create { .. } => out.push(rd_where(T_COUNTER, h5i_app_sql::Pred::All)),
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
        Command::List => {
            if is_operator(actor) {
                out.push(rd_where(T_PAYMENTS, h5i_app_sql::Pred::All));
            } else {
                out.push(rd_where(T_PAYMENTS, p_eq(PAYMENT_MERCHANT, v_int(user_of(actor)))));
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OP: Principal = Principal { org: 1, actor: Actor::Operator(9) };
    const M: Principal = Principal { org: 1, actor: Actor::User(7) };
    const OTHER: Principal = Principal { org: 1, actor: Actor::User(8) };

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
        let op2 = Principal { org: 1, actor: Actor::Operator(2) };
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
        let u9 = Principal { org: 1, actor: Actor::User(9) };
        assert_eq!(run(&s, u9, Command::Record { payment: id, result: PspResult::Canceled }).1, Err(Error::Forbidden));
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
