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

    /// The transaction's clock, for the history (G1).
    fn stamp(actor: &mut k::Principal, now: h5i_app_core::Timestamp) {
        actor.at = now.0;
    }
}

/// User `user` (a merchant) of tenant `org`.
pub fn user(org: u64, user: u64) -> k::Principal {
    k::Principal { org, actor: k::Actor::User(user), at: 0 }
}

/// Operator `id` of tenant `org`.
pub fn operator(org: u64, id: u64) -> k::Principal {
    k::Principal { org, actor: k::Actor::Operator(id), at: 0 }
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
        // F3: payments numbered per merchant, and the merchants' counters.
        ddl.extend(crate::numbering("payments", "merchant", "merchant_counters"));
        // F2: the ledger-complete marker. Older rows start unmarked; the
        // first recovery pass syncs and marks them.
        ddl.push(crate::add_column("payments", "ledger_done", "BOOLEAN NOT NULL DEFAULT false"));
        ddl.push("DROP INDEX IF EXISTS payments_merchant".into());
        ddl.push("CREATE INDEX IF NOT EXISTS payments_merchant_seq ON payments (tenant_id, merchant, seq)".into());
        ddl.push("CREATE INDEX IF NOT EXISTS refunds_payment ON refunds (tenant_id, payment)".into());
        // Recovery's work (F4): payments not marked done, and payments with
        // a refund in flight. The status is not in any index, so the status
        // changes on the hot path (BeginAuthorize, Record) are HOT updates:
        // they add no index entries, and concurrent commands on neighbouring
        // payments stop conflicting on shared primary-key index pages under
        // SERIALIZABLE. These replace `payments_unfinished` and
        // `payments_unposted` (status in the predicate made every status
        // change a non-HOT update).
        ddl.push("DROP INDEX IF EXISTS payments_unfinished".into());
        ddl.push("DROP INDEX IF EXISTS payments_unposted".into());
        ddl.push("DROP INDEX IF EXISTS payments_captured".into());
        // G5: `ledger_done` is not indexed either, so `MarkPosted` is a HOT
        // update too. (As the partial index `payments_open … WHERE NOT
        // ledger_done` it made every mark add primary-key index entries on
        // the page concurrent `Create`s read and insert into: the last
        // SERIALIZABLE aborts of the payment flow.) The unmarked payments
        // are kept in `open_payments` instead, by a trigger.
        ddl.push("DROP INDEX IF EXISTS payments_open".into());
        ddl.extend(crate::open_payments());
        ddl.push("CREATE INDEX IF NOT EXISTS payments_refunding ON payments (tenant_id, id) WHERE refund_pending > 0".into());
        // Room in each heap page for HOT updates of its rows.
        ddl.push(crate::fillfactor("payments", 70));
        // G1: each payment's count of history entries; older payments start
        // at 0 (their history begins with their next change). The history
        // table itself is `schema_ddl`'s, append-only.
        ddl.push(crate::add_column("payments", "changes", "BIGINT NOT NULL DEFAULT 0"));
        ddl.extend(crate::append_only_history());
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

    /// The rows `k::reads` names, one declarative read per table.
    /// `proofs/payments/Payments/Frame.lean` (`Frame.load_for_sound`):
    /// `transition` gives the same result on them as on `load`.
    async fn load_for(tx: &Tx<'_>, t: TenantId, actor: &k::Principal, cmd: &k::Command) -> Result<k::Snapshot, DbError> {
        match k::reads(actor, cmd) {
            None => schema_load(tx, t).await,
            Some(plan) => schema_load_plan(tx, t, &plan).await,
        }
    }

    /// Writes queue on the payment they change. Only a hint for fewer
    /// retries; SERIALIZABLE makes any interleaving serial.
    ///
    /// F4: `AttachRef` also takes a tenant-wide key. It reads `psp_refs`
    /// by key and inserts there; PostgreSQL's SERIALIZABLE tracks key reads
    /// per B-tree page, and recent payments share a page, so two concurrent
    /// `AttachRef`s on neighbouring payments each read what the other
    /// writes and one is aborted. (The payments table avoids the same
    /// conflict without a lock: its status changes are HOT updates, see
    /// `ddl`.)
    fn lock_for(_actor: &k::Principal, cmd: &k::Command) -> Lock {
        let pay = |id: &u64| Lock::key("payment", &id.to_be_bytes());
        let counter = || Lock::key("counter", &[]);
        match cmd {
            k::Command::Create { .. } => Lock::Keys(vec![counter()]),
            k::Command::RequestRefund { payment, .. } => Lock::Keys(vec![pay(payment), counter()]),
            k::Command::AttachRef { payment, .. } => Lock::Keys(vec![pay(payment), Lock::key("psp-refs", &[])]),
            k::Command::BeginAuthorize { id } | k::Command::Cancel { id } => Lock::Keys(vec![pay(id)]),
            k::Command::Record { payment, .. } | k::Command::Event { payment, .. } | k::Command::MarkPosted { payment, .. } => {
                Lock::Keys(vec![pay(payment)])
            }
            k::Command::Get { .. }
            | k::Command::List { .. }
            | k::Command::GetRefund { .. }
            | k::Command::Refunds { .. }
            | k::Command::GetRef { .. }
            | k::Command::View { .. }
            | k::Command::History { .. } => Lock::None,
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
        ("seq", p.seq.into()),
        ("changes", p.changes.into()),
    ])
}

pub fn command_name(c: u32) -> &'static str {
    match c {
        k::CMD_CREATE => "create",
        k::CMD_BEGIN_AUTHORIZE => "begin_authorize",
        k::CMD_CANCEL => "cancel",
        k::CMD_REQUEST_REFUND => "request_refund",
        k::CMD_RECORD => "record",
        k::CMD_EVENT => "event",
        k::CMD_ATTACH_REF => "attach_ref",
        k::CMD_MARK_POSTED => "mark_posted",
        _ => "unknown",
    }
}

pub fn result_name(r: u32) -> Option<&'static str> {
    match r {
        k::RES_AUTHORIZED => Some("authorized"),
        k::RES_DECLINED => Some("declined"),
        k::RES_CAPTURED => Some("captured"),
        k::RES_CANCELED => Some("canceled"),
        k::RES_REFUND_SUCCEEDED => Some("refund_succeeded"),
        k::RES_REFUND_FAILED => Some("refund_failed"),
        _ => None,
    }
}

/// One history entry (G1). `at` is microseconds since the Unix epoch, the
/// transaction's clock.
pub fn history_json(h: &k::HistoryEntry) -> Out {
    let status = |s: u32| if s == k::NO_STATUS { Out::Null } else { status_name(s).into() };
    Out::obj([
        ("payment", h.payment.into()),
        ("n", h.n.into()),
        ("actor", Out::obj([("kind", if h.by_operator { "operator" } else { "user" }.into()), ("id", h.actor.into())])),
        ("at", h.at.into()),
        ("command", command_name(h.command).into()),
        ("result", result_name(h.result).map_or(Out::Null, Out::from)),
        ("event", if h.command == k::CMD_EVENT { h.event.into() } else { Out::Null }),
        ("refund", if h.command == k::CMD_REQUEST_REFUND || h.result >= k::RES_REFUND_SUCCEEDED { h.refund.into() } else { Out::Null }),
        ("status_before", status(h.status_before)),
        ("status_after", status(h.status_after)),
        ("captured_before", h.captured_before.into()),
        ("captured_after", h.captured_after.into()),
        ("refunded_before", h.refunded_before.into()),
        ("refunded_after", h.refunded_after.into()),
        ("refund_pending_before", h.pending_before.into()),
        ("refund_pending_after", h.pending_after.into()),
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
        k::Error::BadPage => (StatusCode::BAD_REQUEST, "bad_page"),
    }
}

/// The kernel commands a merchant may send directly to `POST /v1/payments/rpc`.
/// PSP results are recorded only by the orchestrator, never decoded from clients.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
enum CommandJson {
    Get { id: u64 },
    /// A page of a merchant's payments (default: the caller's), from
    /// sequence number `from` (default 0), at most `limit` (default and
    /// maximum `k::MAX_PAGE`).
    List {
        #[serde(default)]
        merchant: Option<u64>,
        #[serde(default)]
        from: u64,
        #[serde(default)]
        limit: Option<u32>,
    },
    GetRefund { id: u64 },
    Refunds { payment: u64 },
    View { id: u64 },
    /// A page of a payment's history (operators only, G1), from entry
    /// `from` (default 0), at most `limit` (default and maximum `k::MAX_PAGE`).
    History {
        payment: u64,
        #[serde(default)]
        from: u64,
        #[serde(default)]
        limit: Option<u32>,
    },
}

/// Decode a client's payment command, with defaults from the caller.
pub fn decode_for(actor: &k::Principal, body: serde_json::Value) -> Result<k::Command, String> {
    Ok(match serde_json::from_value::<CommandJson>(body).map_err(|e| e.to_string())? {
        CommandJson::Get { id } => k::Command::Get { id },
        CommandJson::List { merchant, from, limit } => {
            k::Command::List { merchant: merchant.unwrap_or(k::user_of(actor)), from, limit: limit.unwrap_or(k::MAX_PAGE) }
        }
        CommandJson::GetRefund { id } => k::Command::GetRefund { id },
        CommandJson::Refunds { payment } => k::Command::Refunds { payment },
        CommandJson::View { id } => k::Command::View { id },
        CommandJson::History { payment, from, limit } => k::Command::History { payment, from, limit: limit.unwrap_or(k::MAX_PAGE) },
    })
}

impl h5i_app_http::Api<PaymentsApp> for PaymentsStore {
    /// Without the caller (`List` defaults to merchant 0); the server
    /// routes requests through [`decode_for`] instead.
    fn decode_command(body: serde_json::Value) -> Result<k::Command, String> {
        decode_for(&operator(0, crate::ORCHESTRATOR_OPERATOR), body)
    }

    fn encode_reply(r: &k::Reply) -> Out {
        match r {
            k::Reply::Created(id) => Out::obj([("id", (*id).into())]),
            k::Reply::Status(s) => Out::obj([("status", status_name(*s).into())]),
            k::Reply::Unchanged => Out::obj([("status", "unchanged".into())]),
            k::Reply::Duplicate => Out::obj([("status", "duplicate".into())]),
            k::Reply::Stale => Out::obj([("status", "stale".into())]),
            k::Reply::Payment(p) => payment_json(p),
            k::Reply::Payments(ps, next) => Out::obj([
                ("payments", Out::Arr(ps.iter().map(payment_json).collect())),
                ("next", next.map_or(Out::Null, Out::from)),
            ]),
            k::Reply::Refund(r) => refund_json(r),
            k::Reply::Refunds(rs) => Out::Arr(rs.iter().map(refund_json).collect()),
            k::Reply::Ref(b) => Out::obj([("psp_reference", Out::str(b))]),
            k::Reply::View(p, rs, r) => Out::obj([
                ("payment", payment_json(p)),
                ("refunds", Out::Arr(rs.iter().map(refund_json).collect())),
                ("psp_reference", r.as_ref().map_or(Out::Null, Out::str)),
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
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    changes: u64,
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
        seq: p.seq,
        changes: p.changes,
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
        seq: p.seq,
        changes: p.changes,
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
    /// Before F3 (unpaged); decoded as the last page.
    Payments(Vec<StoredPayment>),
    PaymentPage(Vec<StoredPayment>, Option<u64>),
    Refund(StoredRefund),
    Refunds(Vec<StoredRefund>),
    Ref(Vec<u8>),
    View(StoredPayment, Vec<StoredRefund>, Option<Vec<u8>>),
    /// G1: a history page, entries as tuples.
    History(Vec<StoredEntry>, Option<u64>),
}

/// A history entry in a stored reply.
type StoredEntry = (u64, u64, bool, u64, u64, u32, u32, u64, u64, [u32; 2], [u64; 6]);

fn store_entry(h: &k::HistoryEntry) -> StoredEntry {
    (
        h.payment,
        h.n,
        h.by_operator,
        h.actor,
        h.at,
        h.command,
        h.result,
        h.event,
        h.refund,
        [h.status_before, h.status_after],
        [h.captured_before, h.captured_after, h.refunded_before, h.refunded_after, h.pending_before, h.pending_after],
    )
}

fn load_entry(e: StoredEntry) -> k::HistoryEntry {
    let (payment, n, by_operator, actor, at, command, result, event, refund, [status_before, status_after], a) = e;
    k::HistoryEntry {
        payment,
        n,
        by_operator,
        actor,
        at,
        command,
        result,
        event,
        refund,
        status_before,
        status_after,
        captured_before: a[0],
        captured_after: a[1],
        refunded_before: a[2],
        refunded_after: a[3],
        pending_before: a[4],
        pending_after: a[5],
    }
}

impl ReplyCodec<PaymentsApp> for PaymentsStore {
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
            k::Reply::Status(s) => StoredReply::Status(*s),
            k::Reply::Unchanged => StoredReply::Unchanged,
            k::Reply::Duplicate => StoredReply::Duplicate,
            k::Reply::Stale => StoredReply::Stale,
            k::Reply::Payment(p) => StoredReply::Payment(store_payment(p)),
            k::Reply::Payments(ps, next) => StoredReply::PaymentPage(ps.iter().map(store_payment).collect(), *next),
            k::Reply::Refund(r) => StoredReply::Refund(store_refund(r)),
            k::Reply::Refunds(rs) => StoredReply::Refunds(rs.iter().map(store_refund).collect()),
            k::Reply::Ref(b) => StoredReply::Ref(b.clone()),
            k::Reply::View(p, rs, r) => {
                StoredReply::View(store_payment(p), rs.iter().map(store_refund).collect(), r.clone())
            }
            k::Reply::History(hs, next) => StoredReply::History(hs.iter().map(store_entry).collect(), *next),
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
            StoredReply::Payments(ps) => k::Reply::Payments(ps.into_iter().map(load_payment).collect(), None),
            StoredReply::PaymentPage(ps, next) => k::Reply::Payments(ps.into_iter().map(load_payment).collect(), next),
            StoredReply::Refund(r) => k::Reply::Refund(load_refund(r)),
            StoredReply::Refunds(rs) => k::Reply::Refunds(rs.into_iter().map(load_refund).collect()),
            StoredReply::Ref(b) => k::Reply::Ref(b),
            StoredReply::View(p, rs, r) => k::Reply::View(load_payment(p), rs.into_iter().map(load_refund).collect(), r),
            StoredReply::History(hs, next) => k::Reply::History(hs.into_iter().map(load_entry).collect(), next),
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
