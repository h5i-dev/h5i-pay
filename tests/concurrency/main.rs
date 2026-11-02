//! Concurrent transfers against PostgreSQL.
//!
//! Many tasks post random transfers between `no_overdraft` wallets at once.
//! With the advisory tenant lock off, every conflict is left to PostgreSQL's
//! SERIALIZABLE isolation, so the engine has to retry serialization failures.
//! Afterwards the committed state must satisfy what the Lean proofs claim:
//! per-currency debits = credits = journal, no wallet overdrawn, money
//! conserved, and exactly the successful transfers in the journal.

use h5i_app_core::{MemoryEngine, TenantId};
use h5i_app_pg::{Engine, EngineConfig};
use h5i_pay_tests::ledger::{balance, op, open, transfer, user};
use h5i_pay_tests::{database_url, fresh_tenant, Rng};
use payment_ledger as k;
use payment_storage::ledger::{audit, audit_paged as paged, LedgerApp, LedgerStore};
use payment_storage::{schema_pool, LEDGER_SCHEMA};
use std::collections::HashSet;
use std::sync::Arc;

const JPY: u32 = 392;

async fn engine(url: &str, tenant_lock: bool, pool: usize) -> Arc<Engine<LedgerApp, LedgerStore>> {
    let cfg = EngineConfig { tenant_lock, max_attempts: 200, ..EngineConfig::default() };
    let e = Arc::new(Engine::new(schema_pool(url, LEDGER_SCHEMA, pool).unwrap(), cfg));
    e.install_schema().await.unwrap();
    e
}

/// A funding account (0) and `n` wallets (1..=n), each funded with `each`.
async fn setup(e: &Engine<LedgerApp, LedgerStore>, org: u64, n: u64, each: u64) {
    assert_eq!(e.execute(&op(org), &open(0, JPY, false)).await.unwrap(), Ok(k::Reply::Created(0)));
    for w in 1..=n {
        assert_eq!(e.execute(&user(org, w), &open(w, JPY, true)).await.unwrap(), Ok(k::Reply::Created(w)));
        let r = e.execute(&op(org), &transfer(1_000_000 + w, 0, w, each)).await.unwrap();
        assert_eq!(r, Ok(k::Reply::Posted(1_000_000 + w)));
    }
}

async fn run_random_transfers(tenant_lock: bool) {
    let Some(url) = database_url() else { return };
    let e = engine(&url, tenant_lock, 16).await;
    let org = fresh_tenant();
    let (wallets, each) = (8u64, 1_000u64);
    setup(&e, org, wallets, each).await;
    let retries_before = e.stats.retries.load(std::sync::atomic::Ordering::Relaxed);

    // G4: the paged audit, run over and over while the transfers commit:
    // each run reads every account and journal page by page, and must hold.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let auditor = {
        let (e, done) = (e.clone(), done.clone());
        tokio::spawn(async move {
            let (mut runs, mut in_flight) = (0u64, 0u64);
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let r = paged(&e, &op(org)).await.unwrap().expect("the paged audit holds during concurrent transfers");
                runs += 1;
                in_flight += r.in_flight;
            }
            (runs, in_flight)
        })
    };

    let mut tasks = Vec::new();
    for task in 0..16u64 {
        let e = e.clone();
        tasks.push(tokio::spawn(async move {
            let mut rng = Rng(task * 7919 + 1);
            let mut posted = Vec::new();
            let mut refused = Vec::new();
            for i in 0..40u64 {
                let from = 1 + rng.below(wallets);
                let to = 1 + rng.below(wallets);
                let amount = 1 + rng.below(400);
                let id = task * 10_000 + i + 1;
                // The owner of the source wallet sends.
                match e.execute(&user(org, from), &transfer(id, from, to, amount)).await.unwrap() {
                    Ok(k::Reply::Posted(x)) => posted.push((x, from, to, amount)),
                    Ok(other) => panic!("unexpected reply {other:?}"),
                    Err(k::Error::Insufficient | k::Error::SameAccount) => refused.push(id),
                    Err(other) => panic!("unexpected refusal {other:?}"),
                }
            }
            (posted, refused)
        }));
    }
    let mut posted = Vec::new();
    let mut refused = HashSet::new();
    for t in tasks {
        let (p, r) = t.await.unwrap();
        posted.extend(p);
        refused.extend(r);
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let (runs, in_flight) = auditor.await.unwrap();
    eprintln!("paged audit: {runs} runs during the transfers, {in_flight} transfers seen in flight");
    let snap = e.snapshot(TenantId(org)).await.unwrap();
    audit(&snap).expect("ledger invariants hold after concurrent transfers");
    let r = paged(&e, &op(org)).await.unwrap().unwrap();
    assert_eq!((r.accounts, r.transfers, r.in_flight), (snap.accounts.len() as u64, snap.transfers.len() as u64, 0));
    assert!(r.pages > r.accounts, "journals span several pages");
    // Money is conserved: the wallets still hold what was funded.
    let total: i128 = snap.accounts.iter().filter(|a| a.no_overdraft).map(balance).sum();
    assert_eq!(total, i128::from(wallets * each));
    for a in snap.accounts.iter().filter(|a| a.no_overdraft) {
        assert!(balance(a) >= 0, "wallet {} overdrawn", a.id);
    }
    // Exactly the posted transfers are in the journal, each once.
    let journal: HashSet<u64> = snap.transfers.iter().map(|t| t.id).collect();
    assert_eq!(journal.len(), snap.transfers.len());
    for (id, from, to, amount) in &posted {
        let t = snap.transfers.iter().find(|t| t.id == *id).expect("posted transfer is in the journal");
        assert_eq!((t.debit, t.credit, t.amount), (*from, *to, *amount));
    }
    for id in &refused {
        assert!(!journal.contains(id), "refused transfer {id} was committed");
    }
    assert_eq!(snap.transfers.len(), posted.len() + wallets as usize);
    let retries = e.stats.retries.load(std::sync::atomic::Ordering::Relaxed) - retries_before;
    eprintln!("tenant_lock={tenant_lock}: {} posted, {} refused, {retries} retries", posted.len(), refused.len());
    if !tenant_lock {
        assert!(retries > 0, "expected SERIALIZABLE conflicts to force retries");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_transfers_serializable_retries() {
    run_random_transfers(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_transfers_with_tenant_lock() {
    run_random_transfers(true).await;
}

/// G4: with more accounts than one page, the paged audit reads the account
/// rows in several transactions, so a transfer between accounts on
/// different pages can commit in between and be in one side's journal
/// only. The audit must find it past the other side's bound and still
/// balance. Transfers go on until some audit has seen one in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn paged_audit_across_account_pages() {
    let Some(url) = database_url() else { return };
    let e = engine(&url, true, 16).await;
    let org = fresh_tenant();
    let wallets = u64::from(k::MAX_PAGE) + 16;
    setup(&e, org, wallets, 1_000_000).await;
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tasks = Vec::new();
    for task in 0..8u64 {
        let (e, done) = (e.clone(), done.clone());
        tasks.push(tokio::spawn(async move {
            let mut rng = Rng(task * 31 + 7);
            let mut i = 0;
            while !done.load(std::sync::atomic::Ordering::Relaxed) && i < 20_000 {
                // One side on the first page of accounts, the other on the second.
                let a = 1 + rng.below(u64::from(k::MAX_PAGE) - 1);
                let b = u64::from(k::MAX_PAGE) + rng.below(16);
                let (from, to) = if rng.below(2) == 0 { (a, b) } else { (b, a) };
                i += 1;
                let r = e.execute(&user(org, from), &transfer(task * 100_000 + i, from, to, 1 + rng.below(50))).await.unwrap();
                assert!(matches!(r, Ok(k::Reply::Posted(_)) | Err(k::Error::Insufficient)), "{r:?}");
            }
        }));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let (mut runs, mut in_flight) = (0u64, 0u64);
    while in_flight == 0 && std::time::Instant::now() < deadline {
        let r = paged(&e, &op(org)).await.unwrap().expect("the paged audit holds during concurrent transfers");
        assert_eq!(r.accounts, wallets + 1);
        runs += 1;
        in_flight += r.in_flight;
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    for t in tasks {
        t.await.unwrap();
    }
    eprintln!("paged audit across pages: {runs} runs, {in_flight} transfers seen in flight");
    assert!(in_flight > 0, "no audit run saw a transfer in flight in {runs} runs");
    let snap = e.snapshot(TenantId(org)).await.unwrap();
    audit(&snap).unwrap();
    let r = paged(&e, &op(org)).await.unwrap().unwrap();
    assert_eq!((r.transfers, r.in_flight), (snap.transfers.len() as u64, 0));
}

/// The same transfer submitted concurrently many times commits once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_duplicates_commit_once() {
    let Some(url) = database_url() else { return };
    let e = engine(&url, false, 16).await;
    let org = fresh_tenant();
    setup(&e, org, 2, 500).await;
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let e = e.clone();
        tasks.push(tokio::spawn(async move { e.execute(&user(org, 1), &transfer(77, 1, 2, 300)).await.unwrap() }));
    }
    let mut posted = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(k::Reply::Posted(77)) => posted += 1,
            Ok(k::Reply::Exists(77)) => {}
            r => panic!("{r:?}"),
        }
    }
    assert_eq!(posted, 1);
    let snap = e.snapshot(TenantId(org)).await.unwrap();
    assert_eq!(snap.transfers.iter().filter(|t| t.id == 77).count(), 1);
    let w1 = snap.accounts.iter().find(|a| a.id == 1).unwrap();
    assert_eq!(balance(w1), 200);
    audit(&snap).unwrap();
}

/// Concurrent linked batches that would overdraw a wallet together: each
/// batch is all-or-nothing, and the wallet never goes negative.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_batches_are_atomic() {
    let Some(url) = database_url() else { return };
    let e = engine(&url, false, 16).await;
    let org = fresh_tenant();
    setup(&e, org, 3, 1_000).await;
    let mut tasks = Vec::new();
    for b in 0..10u64 {
        let e = e.clone();
        tasks.push(tokio::spawn(async move {
            // 1 → 2 → 3 in one batch: the second leg needs the first.
            let ts = vec![
                k::TransferReq { id: 500 + b * 2, debit: 1, credit: 2, amount: 300, code: 0 },
                k::TransferReq { id: 501 + b * 2, debit: 2, credit: 3, amount: 1_100, code: 0 },
            ];
            e.execute(&op(org), &k::Command::Batch { transfers: ts }).await.unwrap()
        }));
    }
    let mut ok = 0u64;
    for t in tasks {
        match t.await.unwrap() {
            Ok(k::Reply::Batch(2)) => ok += 1,
            Err(k::Error::Insufficient) => {}
            r => panic!("{r:?}"),
        }
    }
    let snap = e.snapshot(TenantId(org)).await.unwrap();
    audit(&snap).unwrap();
    // Wallet 1 has 1000: at most three batches of 300 fit.
    assert!((1..=3).contains(&ok), "{ok} batches committed");
    let legs = snap.transfers.iter().filter(|t| t.id >= 500 && t.id < 600).count() as u64;
    assert_eq!(legs, 2 * ok, "a batch committed partially");
    let w1 = snap.accounts.iter().find(|a| a.id == 1).unwrap();
    assert_eq!(balance(w1), 1_000 - 300 * ok as i128);
}

/// Random commands through PostgreSQL and through the in-memory reference
/// engine give the same replies and the same final state: the store holds
/// what the kernel's `apply` computes (the property `Ledger/Storage.lean`
/// proves for the SQL model).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agrees_with_the_reference_engine() {
    let Some(url) = database_url() else { return };
    let pg = engine(&url, true, 4).await;
    let mem = MemoryEngine::<LedgerApp>::default();
    let org = fresh_tenant();
    let mut rng = Rng(42);
    for i in 0..300u64 {
        let u = rng.below(4);
        let actor = if u == 0 { op(org) } else { user(org, u) };
        let acct = |r: &mut Rng| r.below(7);
        let amount = |r: &mut Rng| if r.below(25) == 0 { u64::MAX - r.below(3) } else { r.below(200) };
        let cmd = match rng.below(8) {
            0 => open(if u == 0 { rng.below(4) } else { u }, if rng.below(3) == 0 { 840 } else { JPY }, rng.below(2) == 0),
            1..=4 => transfer(1 + rng.below(40), acct(&mut rng), acct(&mut rng), amount(&mut rng)),
            5 => k::Command::Batch {
                transfers: (0..1 + rng.below(4))
                    .map(|_| k::TransferReq { id: 1 + rng.below(60), debit: acct(&mut rng), credit: acct(&mut rng), amount: amount(&mut rng), code: 1 })
                    .collect(),
            },
            // The caller's first page (no extra random draws: the sequence
            // of the other commands stays what it was).
            6 => k::Command::Accounts { owner: u, from: 0, limit: k::MAX_PAGE },
            _ => k::Command::Account { id: acct(&mut rng) },
        };
        let got = pg.execute(&actor, &cmd).await.unwrap();
        let want = mem.execute(&actor, &cmd);
        let norm = |r: Result<k::Reply, k::Error>| match r {
            Ok(k::Reply::Accounts(mut v, next)) => {
                v.sort_by_key(|a| a.id);
                Ok(k::Reply::Accounts(v, next))
            }
            other => other,
        };
        assert_eq!(norm(got), norm(want), "step {i}: {cmd:?}");
    }
    let mut a = pg.snapshot(TenantId(org)).await.unwrap();
    let mut b = mem.snapshot(TenantId(org));
    // G1: PostgreSQL's history carries the transaction's clock; the
    // in-memory engine does not stamp one.
    assert!(!a.history.is_empty() && a.history.iter().all(|h| h.at > 0));
    for s in [&mut a, &mut b] {
        s.accounts.sort_by_key(|x| x.id);
        s.transfers.sort_by_key(|x| x.id);
        s.owners.sort_by_key(|x| x.owner);
        s.history.sort_by_key(|h| (h.account, h.n));
        s.history.iter_mut().for_each(|h| h.at = 0);
    }
    assert_eq!(a, b);
    audit(&a).unwrap();
    assert!(!a.transfers.is_empty());
}

/// `LedgerStore::load_for` (the read plans of `payment_ledger::reads`) gives
/// every command what it needs: `transition` on the rows PostgreSQL returns
/// for the plan equals `transition` on the whole tenant, for random commands
/// and callers on a growing ledger (what `Ledger/Frame.lean` proves, here
/// checked against the real SQL of the reads).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_for_matches_the_whole_tenant() {
    let Some(url) = database_url() else { return };
    let pg = engine(&url, true, 4).await;
    let org = fresh_tenant();
    let mut rng = Rng(7);
    let mut planned = 0;
    for i in 0..400u64 {
        let u = rng.below(4);
        let actor = if u == 0 { op(org) } else { user(org, u) };
        let acct = |r: &mut Rng| r.below(9);
        let req = |r: &mut Rng| k::TransferReq { id: 1 + r.below(80), debit: acct(r), credit: acct(r), amount: r.below(300), code: 1 };
        let cmd = match rng.below(9) {
            0 | 1 => open(if u == 0 { rng.below(4) } else { u }, if rng.below(3) == 0 { 840 } else { JPY }, rng.below(2) == 0),
            2..=4 => k::Command::Transfer { t: req(&mut rng) },
            // Up to 70 legs: past `MAX_BATCH` (64) refused, reading nothing.
            5 => k::Command::Batch { transfers: (0..rng.below(71)).map(|_| req(&mut rng)).collect() },
            // Pages, some refused (limit 0, another owner's for a user).
            6 => k::Command::Accounts { owner: rng.below(4), from: rng.below(4), limit: rng.below(4) as u32 },
            // G1/G4: history and journal pages, accounts by id (some refused).
            7 => match rng.below(3) {
                0 => k::Command::Transfers { account: acct(&mut rng), from: rng.below(5), limit: rng.below(4) as u32 },
                1 => k::Command::History { account: acct(&mut rng), from: rng.below(5), limit: rng.below(4) as u32 },
                _ => k::Command::AccountRange { from: rng.below(6), limit: rng.below(5) as u32 },
            },
            _ => k::Command::Account { id: acct(&mut rng) },
        };
        if k::reads(&actor, &cmd).is_some() {
            planned += 1;
        }
        let whole = pg.snapshot(TenantId(org)).await.unwrap();
        let part = pg.snapshot_for(&actor, &cmd).await.unwrap();
        assert_eq!(k::transition(&actor, &part, &cmd), k::transition(&actor, &whole, &cmd), "step {i}: {cmd:?}");
        pg.execute(&actor, &cmd).await.unwrap().ok();
    }
    assert!(planned > 300, "{planned} planned commands");
    let snap = pg.snapshot(TenantId(org)).await.unwrap();
    audit(&snap).unwrap();
    assert!(snap.transfers.len() > 10, "{} transfers", snap.transfers.len());
}

/// The payment kernel's read plans (`payment_domain::reads`, used by
/// `PaymentsStore::load_for`) against the real SQL: for random commands,
/// `transition` on what the plan loads equals `transition` on the whole
/// tenant (`Payments/Frame.lean`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payments_load_for_matches_the_whole_tenant() {
    use payment_domain as d;
    let Some(url) = database_url() else { return };
    let (_, pg) = payment_storage::engines(&url, 4, EngineConfig::default()).await.unwrap();
    let org = fresh_tenant();
    let op = d::Principal { org, actor: d::Actor::Operator(1), at: 0 };
    let mut rng = Rng(11);
    for i in 0..500u64 {
        let u = rng.below(4);
        let actor = if u == 0 { op } else { d::Principal { org, actor: d::Actor::User(u), at: 0 } };
        let id = |r: &mut Rng| r.below(8);
        // Few amounts, so PSP results often match and payments get captured.
        let amount = |r: &mut Rng| 100 * (1 + r.below(2));
        let result = |r: &mut Rng| match r.below(6) {
            0 => d::PspResult::Authorized { amount: amount(r) },
            1 => d::PspResult::Declined,
            2 => d::PspResult::Captured { amount: amount(r) },
            3 => d::PspResult::Canceled,
            4 => d::PspResult::RefundSucceeded { refund: id(r) },
            _ => d::PspResult::RefundFailed { refund: id(r) },
        };
        let whole = pg.snapshot(TenantId(org)).await.unwrap();
        let cmd = match rng.below(13) {
            0 | 1 => d::Command::Create { amount: amount(&mut rng), currency: 392, manual_capture: rng.below(2) == 0 },
            2 => d::Command::BeginAuthorize { id: id(&mut rng) },
            3 => d::Command::Cancel { id: id(&mut rng) },
            4 => d::Command::RequestRefund { payment: id(&mut rng), amount: 1 + rng.below(60) },
            5 | 6 => d::Command::Record { payment: id(&mut rng), result: result(&mut rng) },
            7 => d::Command::Event { event: rng.below(20), payment: id(&mut rng), result: result(&mut rng) },
            8 => d::Command::AttachRef { payment: id(&mut rng), reference: vec![b'p', rng.below(3) as u8] },
            9 => d::Command::Get { id: id(&mut rng) },
            // Pages derived from the step (no extra random draws, so the other
            // commands' sequence stays what it was): own and others', some refused.
            10 => d::Command::List { merchant: i % 4, from: i % 5, limit: (i % 7) as u32 },
            11 => d::Command::GetRefund { id: id(&mut rng) },
            _ => {
                match rng.below(4) {
                    0 => d::Command::Refunds { payment: id(&mut rng) },
                    1 => d::Command::GetRef { payment: id(&mut rng) },
                    2 => d::Command::View { id: id(&mut rng) },
                    _ => {
                        // Usually the payment's true refunds, so marks succeed.
                        let payment = id(&mut rng);
                        let mut refunds: Vec<d::Refund> = whole.refunds.iter().filter(|r| r.payment == payment).copied().collect();
                        if rng.below(4) == 0 {
                            refunds.pop();
                        }
                        d::Command::MarkPosted { payment, captured: amount(&mut rng), refunds }
                    }
                }
            }
        };
        let part = pg.snapshot_for(&actor, &cmd).await.unwrap();
        assert_eq!(d::transition(&actor, &part, &cmd), d::transition(&actor, &whole, &cmd), "step {i}: {cmd:?}");
        pg.execute(&actor, &cmd).await.unwrap().ok();
        if i % 50 == 49 {
            open_payments_agree(&url, &pg, org).await;
        }
    }
    let snap = pg.snapshot(TenantId(org)).await.unwrap();
    assert!(snap.payments.len() > 20 && !snap.refunds.is_empty(), "{} payments, {} refunds", snap.payments.len(), snap.refunds.len());
    // Mark every settled payment as the orchestrator would (its own captured
    // amount and refunds): the trigger takes them out of `open_payments`.
    let mut marked = 0;
    for p in &snap.payments {
        let refunds: Vec<d::Refund> = snap.refunds.iter().filter(|r| r.payment == p.id).copied().collect();
        let cmd = d::Command::MarkPosted { payment: p.id, captured: p.captured, refunds };
        if let Ok(d::Reply::Status(_)) = pg.execute(&op, &cmd).await.unwrap() {
            marked += 1;
        }
    }
    assert!(marked > 0, "some payments are settled");
    open_payments_agree(&url, &pg, org).await;
    // A later change clears the mark (a refund on a marked captured
    // payment): the payment is open again.
    let snap = pg.snapshot(TenantId(org)).await.unwrap();
    let p = snap.payments.iter().find(|p| p.ledger_done && p.status == d::CAPTURED && p.captured > p.refunded + p.refund_pending);
    let p = p.expect("a marked captured payment with room for a refund (the sequence is fixed)");
    let m = d::Principal { org, actor: d::Actor::User(p.merchant), at: 0 };
    pg.execute(&m, &d::Command::RequestRefund { payment: p.id, amount: 1 }).await.unwrap().unwrap();
    assert!(!pg.snapshot(TenantId(org)).await.unwrap().payments.iter().find(|q| q.id == p.id).unwrap().ledger_done);
    open_payments_agree(&url, &pg, org).await;
}

/// G5: `open_payments` (kept by triggers) holds exactly the tenant's
/// payments not marked `ledger_done`.
async fn open_payments_agree(url: &str, pg: &payment_storage::PaymentsEngine, org: u64) {
    let snap = pg.snapshot(TenantId(org)).await.unwrap();
    let mut want: Vec<i64> = snap.payments.iter().filter(|p| !p.ledger_done).map(|p| p.id as i64).collect();
    want.sort_unstable();
    let (c, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls).await.unwrap();
    let h = tokio::spawn(conn);
    let sql = format!("SELECT id FROM {}.open_payments WHERE tenant_id = $1 ORDER BY id", payment_storage::PAYMENTS_SCHEMA);
    let got: Vec<i64> = c.query(sql.as_str(), &[&(org as i64)]).await.unwrap().iter().map(|r| r.get(0)).collect();
    drop(c);
    let _ = h.await;
    assert_eq!(got, want, "open_payments vs payments not ledger_done");
}
