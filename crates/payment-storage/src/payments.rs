//! The payment kernel's shell: PostgreSQL tables, idempotent replies, JSON.
//! It decides nothing; every decision is `payment_domain::transition`.
// The `schema!` mapping generates loaders over the kernel's `&Vec` types.
#![allow(clippy::ptr_arg)]

use axum::http::StatusCode;
use h5i_app_core::{Kernel, TenantId};
use h5i_app_json::Value as Out;
use h5i_app_pg::{DbError, Lock, ReplyCodec, Store, Tx};
use payment_domain as k;
use serde::{Deserialize, Serialize};

pub struct PaymentsApp;

impl Kernel for PaymentsApp {
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

/// User `user` (a merchant) of tenant `org`.
pub fn user(org: u64, user: u64) -> k::Principal {
    k::Principal { org, actor: k::Actor::User(user) }
}

/// Operator `id` of tenant `org`.
pub fn operator(org: u64, id: u64) -> k::Principal {
    k::Principal { org, actor: k::Actor::Operator(id) }
}

payment_domain::payment_tables!(PaymentsApp);

pub struct PaymentsStore;

impl Store<PaymentsApp> for PaymentsStore {
    /// The kernel's tables, and indexes for the read plans that are not by
    /// key (`List` for a merchant, `Refunds` of a payment) and for the
    /// scheduled recovery's search for unfinished payments.
    fn ddl() -> Vec<String> {
        let mut ddl = schema_ddl();
        // SEC-02 (F1): which operator last wrote a row. Older rows say 0 (unknown).
        for t in ["payments", "refunds", "psp_events", "psp_refs"] {
            ddl.push(crate::add_column(t, "operator", "BIGINT NOT NULL DEFAULT 0"));
        }
        // F2: the ledger-complete marker. Older rows start unmarked; the
        // first recovery pass syncs and marks them.
        ddl.push(crate::add_column("payments", "ledger_done", "BOOLEAN NOT NULL DEFAULT false"));
        ddl.push("CREATE INDEX IF NOT EXISTS payments_merchant ON payments (tenant_id, merchant)".into());
        ddl.push("CREATE INDEX IF NOT EXISTS refunds_payment ON refunds (tenant_id, payment)".into());
        ddl.push(format!(
            "CREATE INDEX IF NOT EXISTS payments_unfinished ON payments (tenant_id, id) \
             WHERE status = {} OR status = {} OR refund_pending > 0",
            k::PROCESSING,
            k::AUTHORIZED
        ));
        // Recovery's ledger work: captured and not marked complete (F2).
        // It replaces the index on every captured payment.
        ddl.push("DROP INDEX IF EXISTS payments_captured".into());
        ddl.push(format!(
            "CREATE INDEX IF NOT EXISTS payments_unposted ON payments (tenant_id, id) WHERE status = {} AND NOT ledger_done",
            k::CAPTURED
        ));
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

    /// The rows `k::reads` names, one declarative read per table.
    /// `proofs/payments/Payments/Frame.lean` (`Frame.load_for_sound`):
    /// `transition` gives the same result on them as on `load`.
    async fn load_for(tx: &Tx<'_>, t: TenantId, actor: &k::Principal, cmd: &k::Command) -> Result<k::Snapshot, DbError> {
        match k::reads(actor, cmd) {
            None => schema_load(tx, t).await,
            Some(plan) => schema_load_plan(tx, t, &plan).await,
        }
    }

    /// Writes queue on the payment they change (and the id counter when
    /// they allocate an id). Only a hint for fewer retries; SERIALIZABLE
    /// makes any interleaving serial.
    fn lock_for(_actor: &k::Principal, cmd: &k::Command) -> Lock {
        let pay = |id: &u64| Lock::key("payment", &id.to_be_bytes());
        let counter = || Lock::key("counter", &[]);
        match cmd {
            k::Command::Create { .. } => Lock::Keys(vec![counter()]),
            k::Command::RequestRefund { payment, .. } => Lock::Keys(vec![pay(payment), counter()]),
            k::Command::BeginAuthorize { id } | k::Command::Cancel { id } => Lock::Keys(vec![pay(id)]),
            k::Command::Record { payment, .. }
            | k::Command::Event { payment, .. }
            | k::Command::AttachRef { payment, .. }
            | k::Command::MarkPosted { payment, .. } => Lock::Keys(vec![pay(payment)]),
            k::Command::Get { .. }
            | k::Command::List
            | k::Command::GetRefund { .. }
            | k::Command::Refunds { .. }
            | k::Command::GetRef { .. }
            | k::Command::View { .. } => Lock::None,
        }
    }
}

pub fn status_name(s: u32) -> &'static str {
    match s {
        k::CREATED => "created",
        k::PROCESSING => "processing",
        k::AUTHORIZED => "authorized",
        k::CAPTURED => "captured",
        k::CANCELED => "canceled",
        k::FAILED => "failed",
        _ => "unknown",
    }
}

pub fn refund_status_name(s: u32) -> &'static str {
    match s {
        k::REFUND_PENDING => "pending",
        k::REFUND_SUCCEEDED => "succeeded",
        k::REFUND_FAILED => "failed",
        _ => "unknown",
    }
}

pub fn payment_json(p: &k::Payment) -> Out {
    Out::obj([
        ("id", p.id.into()),
        ("merchant", p.merchant.into()),
        ("amount", p.amount.into()),
        ("currency", u64::from(p.currency).into()),
        ("status", status_name(p.status).into()),
        ("manual_capture", p.manual_capture.into()),
        ("attempts", p.attempts.into()),
        ("authorized", p.authorized.into()),
        ("captured", p.captured.into()),
        ("refunded", p.refunded.into()),
        ("refund_pending", p.refund_pending.into()),
        ("operator", p.operator.into()),
        ("ledger_done", p.ledger_done.into()),
    ])
}

pub fn refund_json(r: &k::Refund) -> Out {
    Out::obj([
        ("id", r.id.into()),
        ("payment", r.payment.into()),
        ("amount", r.amount.into()),
        ("status", refund_status_name(r.status).into()),
        ("operator", r.operator.into()),
    ])
}

pub fn error_code(e: &k::Error) -> (StatusCode, &'static str) {
    match e {
        k::Error::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        k::Error::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
        k::Error::InvalidAmount => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_amount"),
        k::Error::InvalidTransition => (StatusCode::CONFLICT, "invalid_transition"),
        k::Error::AmountMismatch => (StatusCode::CONFLICT, "amount_mismatch"),
        k::Error::RefundExceedsCaptured => (StatusCode::CONFLICT, "refund_exceeds_captured"),
        k::Error::Overflow => (StatusCode::UNPROCESSABLE_ENTITY, "overflow"),
        k::Error::Corrupt => (StatusCode::INTERNAL_SERVER_ERROR, "corrupt"),
        k::Error::RefConflict => (StatusCode::CONFLICT, "psp_reference_conflict"),
    }
}

/// The kernel commands a merchant may send directly to `POST /v1/payments/rpc`.
/// PSP results are recorded only by the orchestrator, never decoded from clients.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
enum CommandJson {
    Get { id: u64 },
    List,
    GetRefund { id: u64 },
    Refunds { payment: u64 },
    View { id: u64 },
}

impl h5i_app_http::Api<PaymentsApp> for PaymentsStore {
    fn decode_command(body: serde_json::Value) -> Result<k::Command, String> {
        Ok(match serde_json::from_value::<CommandJson>(body).map_err(|e| e.to_string())? {
            CommandJson::Get { id } => k::Command::Get { id },
            CommandJson::List => k::Command::List,
            CommandJson::GetRefund { id } => k::Command::GetRefund { id },
            CommandJson::Refunds { payment } => k::Command::Refunds { payment },
            CommandJson::View { id } => k::Command::View { id },
        })
    }

    fn encode_reply(r: &k::Reply) -> Out {
        match r {
            k::Reply::Created(id) => Out::obj([("id", (*id).into())]),
            k::Reply::Status(s) => Out::obj([("status", status_name(*s).into())]),
            k::Reply::Unchanged => Out::obj([("status", "unchanged".into())]),
            k::Reply::Duplicate => Out::obj([("status", "duplicate".into())]),
            k::Reply::Stale => Out::obj([("status", "stale".into())]),
            k::Reply::Payment(p) => payment_json(p),
            k::Reply::Payments(ps) => Out::Arr(ps.iter().map(payment_json).collect()),
            k::Reply::Refund(r) => refund_json(r),
            k::Reply::Refunds(rs) => Out::Arr(rs.iter().map(refund_json).collect()),
            k::Reply::Ref(b) => Out::obj([("psp_reference", Out::str(b))]),
            k::Reply::View(p, rs, r) => Out::obj([
                ("payment", payment_json(p)),
                ("refunds", Out::Arr(rs.iter().map(refund_json).collect())),
                ("psp_reference", r.as_ref().map_or(Out::Null, Out::str)),
            ]),
        }
    }

    fn encode_error(e: &k::Error) -> (StatusCode, Out) {
        let (status, code) = error_code(e);
        (status, h5i_app_http::error_body(code))
    }
}

/// A payment in a stored reply. Replies stored before SEC-02 are arrays
/// without `operator`; serde reads them positionally, `operator` 0.
#[derive(Serialize, Deserialize)]
struct StoredPayment {
    id: u64,
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
    #[serde(default)]
    operator: u64,
    #[serde(default)]
    ledger_done: bool,
}

fn store_payment(p: &k::Payment) -> StoredPayment {
    StoredPayment {
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
        operator: p.operator,
        ledger_done: p.ledger_done,
    }
}

fn load_payment(p: StoredPayment) -> k::Payment {
    k::Payment {
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
        operator: p.operator,
        ledger_done: p.ledger_done,
    }
}

/// A refund in a stored reply (older replies: arrays without `operator`).
#[derive(Serialize, Deserialize)]
struct StoredRefund {
    id: u64,
    payment: u64,
    amount: u64,
    status: u32,
    #[serde(default)]
    operator: u64,
}

fn store_refund(r: &k::Refund) -> StoredRefund {
    StoredRefund { id: r.id, payment: r.payment, amount: r.amount, status: r.status, operator: r.operator }
}

fn load_refund(r: StoredRefund) -> k::Refund {
    k::Refund { id: r.id, payment: r.payment, amount: r.amount, status: r.status, operator: r.operator }
}

#[derive(Serialize, Deserialize)]
enum StoredReply {
    Created(u64),
    Status(u32),
    Unchanged,
    Duplicate,
    Stale,
    Payment(StoredPayment),
    Payments(Vec<StoredPayment>),
    Refund(StoredRefund),
    Refunds(Vec<StoredRefund>),
    Ref(Vec<u8>),
    View(StoredPayment, Vec<StoredRefund>, Option<Vec<u8>>),
}

impl ReplyCodec<PaymentsApp> for PaymentsStore {
    fn fingerprint(cmd: &k::Command) -> Vec<u8> {
        format!("{cmd:?}").into_bytes()
    }

    /// Idempotency keys are per user; operators share the tenant's operator
    /// namespace (`u0`, as before SEC-02).
    fn scope(actor: &k::Principal) -> String {
        format!("u{}", k::user_of(actor))
    }

    fn encode(r: &k::Reply) -> Vec<u8> {
        let stored = match r {
            k::Reply::Created(id) => StoredReply::Created(*id),
            k::Reply::Status(s) => StoredReply::Status(*s),
            k::Reply::Unchanged => StoredReply::Unchanged,
            k::Reply::Duplicate => StoredReply::Duplicate,
            k::Reply::Stale => StoredReply::Stale,
            k::Reply::Payment(p) => StoredReply::Payment(store_payment(p)),
            k::Reply::Payments(ps) => StoredReply::Payments(ps.iter().map(store_payment).collect()),
            k::Reply::Refund(r) => StoredReply::Refund(store_refund(r)),
            k::Reply::Refunds(rs) => StoredReply::Refunds(rs.iter().map(store_refund).collect()),
            k::Reply::Ref(b) => StoredReply::Ref(b.clone()),
            k::Reply::View(p, rs, r) => {
                StoredReply::View(store_payment(p), rs.iter().map(store_refund).collect(), r.clone())
            }
        };
        serde_json::to_vec(&stored).expect("stored replies serialize")
    }

    fn decode(b: &[u8]) -> Result<k::Reply, String> {
        Ok(match serde_json::from_slice(b).map_err(|e| e.to_string())? {
            StoredReply::Created(id) => k::Reply::Created(id),
            StoredReply::Status(s) => k::Reply::Status(s),
            StoredReply::Unchanged => k::Reply::Unchanged,
            StoredReply::Duplicate => k::Reply::Duplicate,
            StoredReply::Stale => k::Reply::Stale,
            StoredReply::Payment(p) => k::Reply::Payment(load_payment(p)),
            StoredReply::Payments(ps) => k::Reply::Payments(ps.into_iter().map(load_payment).collect()),
            StoredReply::Refund(r) => k::Reply::Refund(load_refund(r)),
            StoredReply::Refunds(rs) => k::Reply::Refunds(rs.into_iter().map(load_refund).collect()),
            StoredReply::Ref(b) => k::Reply::Ref(b),
            StoredReply::View(p, rs, r) => k::Reply::View(load_payment(p), rs.into_iter().map(load_refund).collect(), r),
        })
    }
}

/// The refund-safety and amount invariants the Lean proofs establish,
/// checked on a loaded snapshot.
pub fn audit(s: &k::Snapshot) -> Result<(), String> {
    for p in &s.payments {
        let refunds = u128::from(p.refunded) + u128::from(p.refund_pending);
        if refunds > u128::from(p.captured) || p.captured > p.authorized || p.authorized > p.amount {
            return Err(format!("payment {} amounts out of order: {p:?}", p.id));
        }
        let pending: u128 = s
            .refunds
            .iter()
            .filter(|r| r.payment == p.id && r.status == k::REFUND_PENDING)
            .map(|r| u128::from(r.amount))
            .sum();
        let done: u128 = s
            .refunds
            .iter()
            .filter(|r| r.payment == p.id && r.status == k::REFUND_SUCCEEDED)
            .map(|r| u128::from(r.amount))
            .sum();
        if pending != u128::from(p.refund_pending) || done != u128::from(p.refunded) {
            return Err(format!("payment {}: refund rows {pending}/{done} disagree with counters {p:?}", p.id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_stored_before_the_operator_column_still_decode() {
        let old = br#"{"View":[[3,7,500,392,3,false,1,500,500,0,0],[[4,3,100,0]],null]}"#;
        let k::Reply::View(p, rs, None) = <PaymentsStore as ReplyCodec<PaymentsApp>>::decode(old).unwrap() else { panic!() };
        assert_eq!((p.id, p.captured, p.operator), (3, 500, 0));
        assert_eq!((rs[0].id, rs[0].amount, rs[0].operator), (4, 100, 0));
        let p2 = k::Payment { operator: 9, ..p };
        let back = <PaymentsStore as ReplyCodec<PaymentsApp>>::encode(&k::Reply::Payment(p2));
        assert_eq!(<PaymentsStore as ReplyCodec<PaymentsApp>>::decode(&back).unwrap(), k::Reply::Payment(p2));
    }
}
