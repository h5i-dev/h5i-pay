//! Payment orchestration: drives a payment through the PSP and records every
//! outcome through the payment kernel, then posts the matching ledger
//! transfers.
//!
//! The orchestrator holds no state of its own. Each step is one kernel
//! command in its own SERIALIZABLE transaction, and every external effect is
//! made idempotent by a key derived from our ids:
//!
//! * PSP calls carry `h5i-pay-<tenant>-<object>-<id>-<op>` idempotency keys,
//!   so a retry after a timeout cannot charge, capture or refund twice;
//! * ledger postings use deterministic transfer ids (top bit set, see
//!   [`system_transfer_id`]), so posting again is a replay (`Exists`);
//! * webhooks are deduplicated by the payment kernel (`Command::Event`).
//!
//! A crash between steps leaves a payment in a state [`Orchestrator::recover`]
//! finishes: `PROCESSING` payments are looked up at the PSP, pending refunds
//! too, and [`Orchestrator::sync_ledger`] re-posts whatever the ledger lacks.
//! The ledger is posted *after* the payment kernel records an outcome, so the
//! payment state is the source of truth and the ledger converges to it;
//! reconciliation (`/v1/reconcile`) reports any gap.

use h5i_app_core::TenantId;
use h5i_app_pg::DbError;
use payment_connectors::{self as psp, AuthorizeRequest, CheckoutRequest, CheckoutSession, EventResult, Outcome, PaymentKey, Psp, PspError, RefundOutcome};
use payment_domain as d;
use payment_ledger as l;
use payment_storage::ledger::{SYSTEM_ACCOUNT_CODES, SYSTEM_TRANSFER_BIT};
use payment_storage::{ledger, payments, LedgerEngine, PaymentsEngine};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Ledger account codes the orchestrator owns; client-opened accounts use
/// codes below `SYSTEM_ACCOUNT_CODES` (enforced when decoding).
pub const SYSTEM_CODES: u32 = SYSTEM_ACCOUNT_CODES;
/// Owed to us by the PSP (debit-normal, unconstrained).
pub const PSP_CLEARING: u32 = 1001;
/// Owed by us to the merchant (`no_overdraft`: refunds need a balance).
pub const MERCHANT_PAYABLE: u32 = 1002;
/// Refunds reserved but not settled by the PSP yet.
pub const REFUND_HOLDING: u32 = 1003;

/// Ledger transfer kinds the orchestrator posts, and their transfer ids
/// (`payment_storage::ledger`, shared with the recovery query).
pub use payment_storage::ledger::{system_transfer_id, POST_CAPTURE, POST_REFUND_RELEASE, POST_REFUND_RESERVE, POST_REFUND_SETTLE};

#[derive(Debug)]
pub enum OrchError {
    Db(DbError),
    Payment(d::Error),
    Ledger(l::Error),
    Psp(PspError),
    /// The merchant's ledger balance cannot cover the refund.
    MerchantBalance,
    BadRequest(String),
}

impl std::fmt::Display for OrchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl From<DbError> for OrchError {
    fn from(e: DbError) -> Self {
        OrchError::Db(e)
    }
}

pub type Result<T> = std::result::Result<T, OrchError>;

/// What a payment request ended in, for the HTTP reply.
#[derive(Debug, Clone)]
pub struct View {
    pub payment: d::Payment,
    pub refunds: Vec<d::Refund>,
    pub reference: Option<String>,
    /// The PSP did not answer in time; the outcome will come from a webhook
    /// or recovery.
    pub pending_psp: bool,
}

pub struct Orchestrator {
    pub ledger: Arc<LedgerEngine>,
    pub payments: Arc<PaymentsEngine>,
    pub psp: Arc<dyn Psp>,
    accounts: Mutex<HashMap<(u64, u64, u32, u32), u64>>,
}

fn op_l(tenant: u64) -> l::Principal {
    ledger::principal(tenant, 0)
}

fn op_p(tenant: u64) -> d::Principal {
    payments::principal(tenant, 0)
}

impl Orchestrator {
    pub fn new(ledger: Arc<LedgerEngine>, payments: Arc<PaymentsEngine>, psp: Arc<dyn Psp>) -> Self {
        Orchestrator { ledger, payments, psp, accounts: Mutex::new(HashMap::new()) }
    }

    async fn pay(&self, actor: &d::Principal, cmd: d::Command) -> Result<std::result::Result<d::Reply, d::Error>> {
        Ok(self.payments.execute(actor, &cmd).await?)
    }

    async fn pay_ok(&self, actor: &d::Principal, cmd: d::Command) -> Result<d::Reply> {
        self.pay(actor, cmd).await?.map_err(OrchError::Payment)
    }

    /// The payment as `actor` may see it: one `View` command (payment,
    /// refunds and PSP reference together).
    pub async fn view(&self, actor: &d::Principal, id: u64) -> Result<View> {
        match self.pay_ok(actor, d::Command::View { id }).await? {
            d::Reply::View(payment, refunds, reference) => {
                Ok(View { payment, refunds, reference: reference.map(|b| String::from_utf8_lossy(&b).into_owned()), pending_psp: false })
            }
            r => unreachable!("view replied {r:?}"),
        }
    }

    async fn reference(&self, tenant: u64, id: u64) -> Result<Option<String>> {
        match self.pay(&op_p(tenant), d::Command::GetRef { payment: id }).await? {
            Ok(d::Reply::Ref(b)) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
            Ok(r) => unreachable!("getref replied {r:?}"),
            Err(d::Error::NotFound) => Ok(None),
            Err(e) => Err(OrchError::Payment(e)),
        }
    }

    async fn attach(&self, tenant: u64, id: u64, reference: &str) -> Result<()> {
        let cmd = d::Command::AttachRef { payment: id, reference: reference.as_bytes().to_vec() };
        match self.pay(&op_p(tenant), cmd).await? {
            Ok(_) => Ok(()),
            Err(e) => Err(OrchError::Payment(e)),
        }
    }

    /// Record a PSP result. A result the kernel finds out of date
    /// (`InvalidTransition`) means another path recorded a later one first.
    async fn record(&self, tenant: u64, id: u64, result: d::PspResult) -> Result<()> {
        match self.pay(&op_p(tenant), d::Command::Record { payment: id, result }).await? {
            Ok(_) | Err(d::Error::InvalidTransition) => Ok(()),
            Err(e) => Err(OrchError::Payment(e)),
        }
    }

    async fn apply_outcome(&self, tenant: u64, id: u64, outcome: &Outcome) -> Result<()> {
        let result = match outcome {
            Outcome::Authorized { amount } => d::PspResult::Authorized { amount: *amount },
            Outcome::Captured { amount } => d::PspResult::Captured { amount: *amount },
            Outcome::Declined { .. } => d::PspResult::Declined,
            Outcome::Canceled => d::PspResult::Canceled,
            Outcome::Pending => return Ok(()),
        };
        self.record(tenant, id, result).await?;
        if matches!(outcome, Outcome::Captured { .. }) {
            self.sync_ledger(tenant, id).await?;
        }
        Ok(())
    }

    /// A merchant creates a payment and the orchestrator authorizes it
    /// (and captures it, for automatic capture) at the PSP.
    pub async fn create(
        &self,
        merchant: &d::Principal,
        idempotency_key: Option<&str>,
        amount: u64,
        currency: u32,
        manual_capture: bool,
        payment_method: &str,
    ) -> Result<View> {
        psp::check_payment_method(payment_method).map_err(OrchError::Psp)?;
        if psp::currency_alpha(currency).is_none() {
            return Err(OrchError::BadRequest(format!("unsupported currency {currency}")));
        }
        let cmd = d::Command::Create { amount, currency, manual_capture };
        let reply = match idempotency_key {
            Some(k) => self.payments.execute_idempotent(merchant, k, &cmd).await?,
            None => self.payments.execute(merchant, &cmd).await?,
        };
        let id = match reply.map_err(OrchError::Payment)? {
            d::Reply::Created(id) => id,
            r => unreachable!("create replied {r:?}"),
        };
        let pending = self.authorize(merchant.org, id, payment_method).await?;
        let mut v = self.view(merchant, id).await?;
        v.pending_psp = pending;
        Ok(v)
    }

    /// Move a payment to `PROCESSING` and ask the PSP. Returns whether the
    /// outcome is still unknown.
    pub async fn authorize(&self, tenant: u64, id: u64, payment_method: &str) -> Result<bool> {
        let p = match self.pay(&op_p(tenant), d::Command::BeginAuthorize { id }).await? {
            // The kernel replies with the payment it moved to PROCESSING.
            Ok(d::Reply::Payment(p)) => p,
            Ok(r) => unreachable!("begin_authorize replied {r:?}"),
            // Already past CREATED: a retry of this request; ask the PSP again,
            // under the same idempotency key, only if still processing.
            Err(d::Error::InvalidTransition) => self.payment(tenant, id).await?,
            Err(e) => return Err(OrchError::Payment(e)),
        };
        if p.status != d::PROCESSING {
            return Ok(false);
        }
        let req = AuthorizeRequest {
            key: PaymentKey { tenant, payment: id },
            amount: p.amount,
            currency: p.currency,
            manual_capture: p.manual_capture,
            payment_method: payment_method.to_string(),
        };
        match self.psp.authorize(&req, &psp::idempotency_key(tenant, "payment", id, "authorize")).await {
            Ok(r) => {
                self.attach(tenant, id, &r.reference).await?;
                self.apply_outcome(tenant, id, &r.outcome).await?;
                Ok(matches!(r.outcome, Outcome::Pending))
            }
            Err(PspError::Timeout | PspError::Unavailable(_)) => Ok(true),
            Err(PspError::Rejected { .. }) => {
                self.record(tenant, id, d::PspResult::Declined).await?;
                Ok(false)
            }
            Err(e) => Err(OrchError::Psp(e)),
        }
    }

    async fn payment(&self, tenant: u64, id: u64) -> Result<d::Payment> {
        match self.pay_ok(&op_p(tenant), d::Command::Get { id }).await? {
            d::Reply::Payment(p) => Ok(p),
            r => unreachable!("get replied {r:?}"),
        }
    }

    /// Capture an authorized payment (all of it, or `amount`).
    pub async fn capture(&self, merchant: &d::Principal, id: u64, amount: Option<u64>) -> Result<View> {
        let mut v = self.view(merchant, id).await?;
        let tenant = merchant.org;
        if v.payment.status == d::AUTHORIZED {
            let amount = amount.unwrap_or(v.payment.authorized);
            let reference = v.reference.clone().ok_or(OrchError::Payment(d::Error::InvalidTransition))?;
            match self.psp.capture(&reference, amount, &psp::idempotency_key(tenant, "payment", id, "capture")).await {
                Ok(o) => self.apply_outcome(tenant, id, &o).await?,
                Err(PspError::Timeout | PspError::Unavailable(_)) => v.pending_psp = true,
                Err(e) => return Err(OrchError::Psp(e)),
            }
        } else if v.payment.status != d::CAPTURED {
            return Err(OrchError::Payment(d::Error::InvalidTransition));
        }
        let pending = v.pending_psp;
        v = self.view(merchant, id).await?;
        v.pending_psp = pending;
        Ok(v)
    }

    /// Cancel: in the kernel alone before the PSP saw the payment, else void
    /// it at the PSP first.
    pub async fn cancel(&self, merchant: &d::Principal, id: u64) -> Result<View> {
        let v = self.view(merchant, id).await?;
        let tenant = merchant.org;
        let mut pending = false;
        match v.payment.status {
            d::CREATED | d::CANCELED => {
                self.pay_ok(merchant, d::Command::Cancel { id }).await?;
            }
            d::AUTHORIZED | d::PROCESSING => {
                let reference = v.reference.clone().ok_or(OrchError::Payment(d::Error::InvalidTransition))?;
                match self.psp.cancel(&reference, &psp::idempotency_key(tenant, "payment", id, "cancel")).await {
                    Ok(o) => self.apply_outcome(tenant, id, &o).await?,
                    Err(PspError::Timeout | PspError::Unavailable(_)) => pending = true,
                    Err(e) => return Err(OrchError::Psp(e)),
                }
            }
            _ => return Err(OrchError::Payment(d::Error::InvalidTransition)),
        }
        let mut v = self.view(merchant, id).await?;
        v.pending_psp = pending;
        Ok(v)
    }

    /// Refund part or all of a captured payment: reserve it in the payment
    /// kernel, reserve the merchant's funds in the ledger, then ask the PSP.
    pub async fn refund(&self, merchant: &d::Principal, idempotency_key: Option<&str>, id: u64, amount: u64) -> Result<(u64, View)> {
        let tenant = merchant.org;
        let cmd = d::Command::RequestRefund { payment: id, amount };
        let reply = match idempotency_key {
            Some(k) => self.payments.execute_idempotent(merchant, k, &cmd).await?,
            None => self.payments.execute(merchant, &cmd).await?,
        };
        let rid = match reply.map_err(OrchError::Payment)? {
            d::Reply::Created(rid) => rid,
            r => unreachable!("refund replied {r:?}"),
        };
        let v = self.view(&op_p(tenant), id).await?;
        let p = v.payment;
        let refund = *v.refunds.iter().find(|r| r.id == rid).expect("the refund just created");
        if refund.status == d::REFUND_PENDING {
            // The ledger reservation decides whether the merchant can pay it back.
            match self.reserve(tenant, &p, &refund).await? {
                Ok(()) => {}
                Err(l::Error::Insufficient) => {
                    self.record(tenant, id, d::PspResult::RefundFailed { refund: rid }).await?;
                    return Err(OrchError::MerchantBalance);
                }
                Err(e) => return Err(OrchError::Ledger(e)),
            }
            let reference = v.reference.clone().ok_or(OrchError::Payment(d::Error::InvalidTransition))?;
            let key = PaymentKey { tenant, payment: id };
            match self.psp.refund(key, &reference, rid, amount, &psp::idempotency_key(tenant, "refund", rid, "refund")).await {
                Ok(RefundOutcome::Succeeded) => self.record_refund(tenant, id, rid, true).await?,
                Ok(RefundOutcome::Failed) => self.record_refund(tenant, id, rid, false).await?,
                Ok(RefundOutcome::Pending) | Err(PspError::Timeout | PspError::Unavailable(_)) => {}
                Err(e) => return Err(OrchError::Psp(e)),
            }
        }
        Ok((rid, self.view(merchant, id).await?))
    }

    async fn record_refund(&self, tenant: u64, id: u64, rid: u64, ok: bool) -> Result<()> {
        let result = if ok { d::PspResult::RefundSucceeded { refund: rid } } else { d::PspResult::RefundFailed { refund: rid } };
        self.record(tenant, id, result).await?;
        self.sync_ledger(tenant, id).await
    }

    /// A hosted payment page: create the payment, move it to `PROCESSING`
    /// and return the PSP's checkout URL; the outcome arrives by webhook.
    pub async fn checkout(&self, merchant: &d::Principal, idempotency_key: Option<&str>, req: CheckoutParams) -> Result<(View, CheckoutSession)> {
        let cmd = d::Command::Create { amount: req.amount, currency: req.currency, manual_capture: req.manual_capture };
        let reply = match idempotency_key {
            Some(k) => self.payments.execute_idempotent(merchant, k, &cmd).await?,
            None => self.payments.execute(merchant, &cmd).await?,
        };
        let id = match reply.map_err(OrchError::Payment)? {
            d::Reply::Created(id) => id,
            r => unreachable!("create replied {r:?}"),
        };
        match self.pay(&op_p(merchant.org), d::Command::BeginAuthorize { id }).await? {
            Ok(_) | Err(d::Error::InvalidTransition) => {}
            Err(e) => return Err(OrchError::Payment(e)),
        }
        let session = self
            .psp
            .checkout(&CheckoutRequest {
                key: PaymentKey { tenant: merchant.org, payment: id },
                amount: req.amount,
                currency: req.currency,
                manual_capture: req.manual_capture,
                description: req.description,
                success_url: req.success_url,
                cancel_url: req.cancel_url,
            })
            .await
            .map_err(OrchError::Psp)?;
        Ok((self.view(merchant, id).await?, session))
    }

    /// A verified webhook event: record it (deduplicated by the kernel),
    /// remember the PSP reference, and bring the ledger up to date.
    pub async fn webhook(&self, ev: &psp::WebhookEvent) -> Result<d::Reply> {
        let tenant = ev.key.tenant;
        let id = ev.key.payment;
        let result = match ev.result {
            EventResult::Authorized { amount } => d::PspResult::Authorized { amount },
            EventResult::Captured { amount } => d::PspResult::Captured { amount },
            EventResult::Declined => d::PspResult::Declined,
            EventResult::Canceled => d::PspResult::Canceled,
            EventResult::RefundSucceeded { refund } => d::PspResult::RefundSucceeded { refund },
            EventResult::RefundFailed { refund } => d::PspResult::RefundFailed { refund },
        };
        let event = psp::webhook::event_id(self.psp.name(), &ev.id);
        let reply = self.pay_ok(&op_p(tenant), d::Command::Event { event, payment: id, result }).await?;
        if let Some(r) = &ev.reference
            && self.reference(tenant, id).await?.is_none()
        {
            self.attach(tenant, id, r).await?;
        }
        if !matches!(reply, d::Reply::Duplicate) {
            self.sync_ledger(tenant, id).await?;
        }
        Ok(reply)
    }

    /// The ledger account `(owner, currency, code)` of a tenant, opened once
    /// (the idempotency key makes concurrent first uses open one account).
    async fn account(&self, tenant: u64, owner: u64, currency: u32, code: u32, no_overdraft: bool) -> Result<u64> {
        let k = (tenant, owner, currency, code);
        if let Some(id) = self.accounts.lock().unwrap().get(&k) {
            return Ok(*id);
        }
        let key = format!("system-account-{owner}-{currency}-{code}");
        let cmd = l::Command::Open { owner, currency, code, no_overdraft, no_overcredit: false };
        let id = match self.ledger.execute_idempotent(&op_l(tenant), &key, &cmd).await?.map_err(OrchError::Ledger)? {
            l::Reply::Created(id) => id,
            r => unreachable!("open replied {r:?}"),
        };
        self.accounts.lock().unwrap().insert(k, id);
        Ok(id)
    }

    async fn post(&self, tenant: u64, ts: Vec<l::TransferReq>) -> Result<std::result::Result<(), l::Error>> {
        let cmd = if ts.len() == 1 { l::Command::Transfer { t: ts[0] } } else { l::Command::Batch { transfers: ts } };
        Ok(self.ledger.execute(&op_l(tenant), &cmd).await?.map(|_| ()))
    }

    async fn reserve(&self, tenant: u64, p: &d::Payment, r: &d::Refund) -> Result<std::result::Result<(), l::Error>> {
        let payable = self.account(tenant, p.merchant, p.currency, MERCHANT_PAYABLE, true).await?;
        let holding = self.account(tenant, 0, p.currency, REFUND_HOLDING, true).await?;
        let t = l::TransferReq { id: system_transfer_id(POST_REFUND_RESERVE, r.id), debit: payable, credit: holding, amount: r.amount, code: POST_REFUND_RESERVE };
        self.post(tenant, vec![t]).await
    }

    /// Post every ledger transfer the payment's state implies. Idempotent:
    /// postings that exist are replays. One `View` reads the payment and
    /// its refunds; the legs go to the ledger as linked batches of at most
    /// `MAX_PLANNED_BATCH` transfers (one command for a capture with up to
    /// 31 refunds).
    pub async fn sync_ledger(&self, tenant: u64, id: u64) -> Result<()> {
        let v = self.view(&op_p(tenant), id).await?;
        let p = v.payment;
        if p.status != d::CAPTURED {
            return Ok(());
        }
        let clearing = self.account(tenant, 0, p.currency, PSP_CLEARING, false).await?;
        let payable = self.account(tenant, p.merchant, p.currency, MERCHANT_PAYABLE, true).await?;
        let holding = self.account(tenant, 0, p.currency, REFUND_HOLDING, true).await?;
        let capture = l::TransferReq { id: system_transfer_id(POST_CAPTURE, p.id), debit: clearing, credit: payable, amount: p.captured, code: POST_CAPTURE };
        // Each group: the legs of one object (the capture, or one refund),
        // and whether a refusal is expected (a failed refund whose
        // reservation never happened has nothing to release).
        let mut groups: Vec<(Vec<l::TransferReq>, Option<u64>)> = vec![(vec![capture], None)];
        for r in &v.refunds {
            let reserve = l::TransferReq { id: system_transfer_id(POST_REFUND_RESERVE, r.id), debit: payable, credit: holding, amount: r.amount, code: POST_REFUND_RESERVE };
            let legs = match r.status {
                d::REFUND_SUCCEEDED => vec![
                    reserve,
                    l::TransferReq { id: system_transfer_id(POST_REFUND_SETTLE, r.id), debit: holding, credit: clearing, amount: r.amount, code: POST_REFUND_SETTLE },
                ],
                // Net zero, and a no-op if the reservation never happened.
                d::REFUND_FAILED => vec![
                    reserve,
                    l::TransferReq { id: system_transfer_id(POST_REFUND_RELEASE, r.id), debit: holding, credit: payable, amount: r.amount, code: POST_REFUND_RELEASE },
                ],
                _ => vec![reserve],
            };
            groups.push((legs, (r.status != d::REFUND_SUCCEEDED).then_some(r.id)));
        }
        // Whole groups per batch, so a batch never splits one object's legs.
        let mut batches: Vec<Vec<(Vec<l::TransferReq>, Option<u64>)>> = vec![Vec::new()];
        for g in groups {
            let size: usize = batches.last().expect("one batch").iter().map(|(legs, _)| legs.len()).sum();
            if size + g.0.len() > l::MAX_PLANNED_BATCH {
                batches.push(Vec::new());
            }
            batches.last_mut().expect("one batch").push(g);
        }
        for batch in batches {
            let legs: Vec<l::TransferReq> = batch.iter().flat_map(|(legs, _)| legs.iter().copied()).collect();
            match self.post(tenant, legs).await? {
                Ok(()) => continue,
                // A batch is all or nothing: if a leg may be refused, post
                // its groups one by one so the others still go in.
                Err(l::Error::Insufficient) if batch.iter().any(|(_, may_fail)| may_fail.is_some()) => {}
                Err(e) => {
                    tracing::error!(tenant, payment = id, error = ?e, "ledger posting refused");
                    return Err(OrchError::Ledger(e));
                }
            }
            for (legs, may_fail) in batch {
                match self.post(tenant, legs).await? {
                    Ok(()) => {}
                    Err(l::Error::Insufficient) if may_fail.is_some() => {}
                    Err(e) => {
                        tracing::error!(tenant, payment = id, refund = ?may_fail, error = ?e, "ledger posting refused");
                        return Err(OrchError::Ledger(e));
                    }
                }
            }
        }
        Ok(())
    }

    /// Finish what crashes and timeouts left half done, for one tenant
    /// (`POST /v1/recover`). Finds the work from the kernels' state
    /// ([`Orchestrator::find_work`]), then [`Orchestrator::recover_work`].
    pub async fn recover(&self, tenant: u64, fail_unsubmitted: bool) -> Result<RecoveryReport> {
        let work = self.find_work(tenant).await?;
        self.recover_work(&work, fail_unsubmitted).await
    }

    /// One tenant's unfinished work, read from both kernels (one snapshot
    /// each), by the rule the scheduled job's query applies in SQL
    /// (`payment_storage::recovery_work`).
    pub async fn find_work(&self, tenant: u64) -> Result<payment_storage::RecoveryWork> {
        let pay = self.payments.snapshot(TenantId(tenant)).await?;
        let journal = self.ledger.snapshot(TenantId(tenant)).await?;
        let ids: std::collections::HashSet<u64> = journal.transfers.iter().map(|t| t.id).collect();
        let mut w = payment_storage::RecoveryWork { tenant, ..Default::default() };
        let mut ps = pay.payments.clone();
        ps.sort_by_key(|p| p.id);
        for p in &ps {
            if p.status == d::PROCESSING || (p.status == d::AUTHORIZED && !p.manual_capture) {
                w.psp.push(p.id);
            }
            if p.refund_pending > 0 {
                w.refunds.push(p.id);
            }
            if payment_storage::needs_ledger_sync(p, &pay.refunds, |id| ids.contains(&id)) {
                w.ledger.push(p.id);
            }
        }
        Ok(w)
    }

    /// Finish one tenant's unfinished work: ask the PSP about each payment in
    /// `work.psp` and each pending refund of `work.refunds` (at most
    /// [`PSP_CONCURRENCY`] calls at once), then post the missing ledger
    /// transfers of `work.ledger`. Every step re-reads the payment through
    /// the kernel, so work that finished meanwhile is skipped.
    pub async fn recover_work(&self, work: &payment_storage::RecoveryWork, fail_unsubmitted: bool) -> Result<RecoveryReport> {
        use futures_util::stream::{self, StreamExt};
        let tenant = work.tenant;
        let mut rep = RecoveryReport::default();
        let mut synced = std::collections::HashSet::new();
        let psp: Vec<_> = stream::iter(work.psp.iter().copied())
            .map(|id| async move { (id, self.recover_payment(tenant, id, fail_unsubmitted).await) })
            .buffer_unordered(PSP_CONCURRENCY)
            .collect()
            .await;
        for (id, r) in psp {
            match r? {
                PaymentRecovery::Resolved => {
                    rep.payments_resolved += 1;
                    synced.insert(id);
                }
                PaymentRecovery::Failed => rep.payments_failed += 1,
                PaymentRecovery::Unresolved => rep.payments_unresolved += 1,
                PaymentRecovery::Done => {}
            }
        }
        let refunds: Vec<_> = stream::iter(work.refunds.iter().copied())
            .map(|id| async move { (id, self.recover_refunds(tenant, id).await) })
            .buffer_unordered(PSP_CONCURRENCY)
            .collect()
            .await;
        for (id, r) in refunds {
            let n = r?;
            rep.refunds_resolved += n;
            if n > 0 {
                synced.insert(id);
            }
        }
        let todo: Vec<u64> = work.ledger.iter().copied().filter(|id| !synced.contains(id)).collect();
        let ledger: Vec<_> = stream::iter(todo).map(|id| self.sync_ledger(tenant, id)).buffer_unordered(PSP_CONCURRENCY).collect().await;
        for r in ledger {
            r?;
            rep.ledger_synced += 1;
        }
        Ok(rep)
    }

    /// Ask the PSP what became of a payment it may have seen.
    async fn recover_payment(&self, tenant: u64, id: u64, fail_unsubmitted: bool) -> Result<PaymentRecovery> {
        let p = self.payment(tenant, id).await?;
        if !(p.status == d::PROCESSING || (p.status == d::AUTHORIZED && !p.manual_capture)) {
            return Ok(PaymentRecovery::Done);
        }
        let reference = self.reference(tenant, id).await?;
        let found = match &reference {
            Some(r) => Some(self.psp.retrieve(r).await.map_err(OrchError::Psp)?),
            None => self.psp.find(PaymentKey { tenant, payment: id }).await.map_err(OrchError::Psp)?,
        };
        match found {
            Some(f) => {
                if reference.is_none() {
                    self.attach(tenant, id, &f.reference).await?;
                }
                self.apply_outcome(tenant, id, &f.outcome).await?;
                Ok(PaymentRecovery::Resolved)
            }
            None if fail_unsubmitted && p.status == d::PROCESSING => {
                // The PSP never received it: no money moved.
                self.record(tenant, id, d::PspResult::Declined).await?;
                Ok(PaymentRecovery::Failed)
            }
            None => Ok(PaymentRecovery::Unresolved),
        }
    }

    /// Ask the PSP about a payment's pending refunds; how many it settled.
    async fn recover_refunds(&self, tenant: u64, id: u64) -> Result<u64> {
        let v = self.view(&op_p(tenant), id).await?;
        let Some(reference) = v.reference else { return Ok(0) };
        let mut n = 0;
        for r in v.refunds {
            if r.status != d::REFUND_PENDING {
                continue;
            }
            match self.psp.retrieve_refund(&reference, r.id).await.map_err(OrchError::Psp)? {
                Some(RefundOutcome::Succeeded) => self.record_refund(tenant, id, r.id, true).await?,
                Some(RefundOutcome::Failed) => self.record_refund(tenant, id, r.id, false).await?,
                _ => continue,
            }
            n += 1;
        }
        Ok(n)
    }

    /// Internal records from the payment kernel and from the ledger journal,
    /// and the PSP's settlement report, as reconciliation inputs.
    pub async fn reconcile_inputs(&self, tenant: u64) -> Result<ReconInputs> {
        let snap = self.payments.snapshot(TenantId(tenant)).await?;
        let internal = payment_reconcile::inputs::internal_records(&snap);
        let report = self.psp.settlement_report(tenant).await.map_err(OrchError::Psp)?;
        let external = report
            .iter()
            .map(|r| payment_reconcile::Record {
                key: payment_reconcile::rules::key(
                    match r.kind {
                        psp::SettlementKind::Capture => payment_reconcile::rules::KIND_CAPTURE,
                        psp::SettlementKind::Refund => payment_reconcile::rules::KIND_REFUND,
                    },
                    r.id,
                ),
                amount: r.amount,
                currency: r.currency,
                status: payment_reconcile::inputs::SETTLED,
            })
            .collect();
        // The ledger's view: capture postings and settled refunds.
        let journal = self.ledger.snapshot(TenantId(tenant)).await?;
        let mut ledger_rows = Vec::new();
        for t in &journal.transfers {
            if t.id & SYSTEM_TRANSFER_BIT == 0 {
                continue;
            }
            let kind = ((t.id >> 56) & 0x7f) as u32;
            let id = t.id & ((1 << 56) - 1);
            let k = match kind {
                POST_CAPTURE => payment_reconcile::rules::key(payment_reconcile::rules::KIND_CAPTURE, id),
                POST_REFUND_SETTLE => payment_reconcile::rules::key(payment_reconcile::rules::KIND_REFUND, id),
                _ => continue,
            };
            ledger_rows.push(payment_reconcile::Record { key: k, amount: t.amount, currency: t.currency, status: payment_reconcile::inputs::SETTLED });
        }
        let settled_internal: Vec<_> = internal.iter().copied().filter(|r| r.status == payment_reconcile::inputs::SETTLED).collect();
        Ok(ReconInputs {
            psp: payment_reconcile::Recon { internal, external },
            ledger: payment_reconcile::Recon { internal: settled_internal, external: ledger_rows },
        })
    }
}

pub struct CheckoutParams {
    pub amount: u64,
    pub currency: u32,
    pub manual_capture: bool,
    pub description: String,
    pub success_url: String,
    pub cancel_url: String,
}

/// PSP calls (and ledger syncs) one tenant's recovery makes at once.
pub const PSP_CONCURRENCY: usize = 8;

enum PaymentRecovery {
    /// Finished meanwhile; nothing to do.
    Done,
    Resolved,
    Failed,
    Unresolved,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    pub payments_resolved: u64,
    pub payments_failed: u64,
    pub payments_unresolved: u64,
    pub refunds_resolved: u64,
    pub ledger_synced: u64,
}

/// Two reconciliations: payment kernel vs PSP settlement report, and payment
/// kernel vs ledger journal.
pub struct ReconInputs {
    pub psp: payment_reconcile::Recon,
    pub ledger: payment_reconcile::Recon,
}
