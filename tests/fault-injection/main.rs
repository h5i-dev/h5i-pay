//! Database failures: connections killed mid-transaction, a COMMIT whose
//! outcome is unknown, and a database that is not there at all.
//!
//! `FaultyStore` wraps the real ledger store. When armed, it terminates its
//! own PostgreSQL backend (`pg_terminate_backend`) before or after writing,
//! so the transaction dies at exactly that point. The ledger must never end
//! up half-written, and a retried request must take effect exactly once.

use h5i_app_core::TenantId;
use h5i_app_pg::{DbError, Engine, EngineConfig, ReplyCodec, Store, Tx};
use h5i_pay_tests::ledger::{balance, op, open, transfer, user};
use h5i_pay_tests::{database_url, fresh_tenant};
use payment_ledger as k;
use payment_storage::ledger::{audit, LedgerApp, LedgerStore};
use payment_storage::{schema_pool, LEDGER_SCHEMA};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Where the next armed write kills its connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    BeforeWrite,
    AfterWrite,
}

static ARMED: AtomicU32 = AtomicU32::new(0);
static MODE: Mutex<Option<Fault>> = Mutex::new(None);
static ADMIN_URL: Mutex<String> = Mutex::new(String::new());

fn arm(n: u32, f: Fault) {
    *MODE.lock().unwrap() = Some(f);
    ARMED.store(n, Ordering::SeqCst);
}

async fn kill_self(tx: &Tx<'_>) {
    let pid = tx.backend_pid().await.unwrap();
    let url = ADMIN_URL.lock().unwrap().clone();
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(conn);
    client.execute("SELECT pg_terminate_backend($1)", &[&pid]).await.unwrap();
    // Wait until the backend is really gone.
    for _ in 0..200 {
        let n: i64 = client.query_one("SELECT count(*) FROM pg_stat_activity WHERE pid = $1", &[&pid]).await.unwrap().get(0);
        if n == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("backend {pid} did not exit");
}

struct FaultyStore;

impl Store<LedgerApp> for FaultyStore {
    fn ddl() -> Vec<String> {
        <LedgerStore as Store<LedgerApp>>::ddl()
    }

    fn tables() -> Vec<&'static str> {
        <LedgerStore as Store<LedgerApp>>::tables()
    }

    async fn load(tx: &Tx<'_>, t: TenantId) -> Result<k::Snapshot, DbError> {
        <LedgerStore as Store<LedgerApp>>::load(tx, t).await
    }

    async fn write(tx: &Tx<'_>, t: TenantId, ws: &Vec<k::Write>) -> Result<(), DbError> {
        let fire = !ws.is_empty() && ARMED.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
        let mode = *MODE.lock().unwrap();
        if fire && mode == Some(Fault::BeforeWrite) {
            kill_self(tx).await;
        }
        let r = <LedgerStore as Store<LedgerApp>>::write(tx, t, ws).await;
        if fire && mode == Some(Fault::AfterWrite) {
            kill_self(tx).await;
        }
        r
    }
}

impl ReplyCodec<LedgerApp> for FaultyStore {
    fn fingerprint(cmd: &k::Command) -> Vec<u8> {
        <LedgerStore as ReplyCodec<LedgerApp>>::fingerprint(cmd)
    }
    fn scope(actor: &k::Principal) -> String {
        <LedgerStore as ReplyCodec<LedgerApp>>::scope(actor)
    }
    fn encode(r: &k::Reply) -> Vec<u8> {
        <LedgerStore as ReplyCodec<LedgerApp>>::encode(r)
    }
    fn decode(b: &[u8]) -> Result<k::Reply, String> {
        <LedgerStore as ReplyCodec<LedgerApp>>::decode(b)
    }
}

/// Faults are process-global, so these tests run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn faulty(url: &str) -> Engine<LedgerApp, FaultyStore> {
    *ADMIN_URL.lock().unwrap() = url.to_string();
    // Tables are the same as the real store's, in the same schema.
    let e = Engine::new(schema_pool(url, LEDGER_SCHEMA, 4).unwrap(), EngineConfig::default());
    e.install_schema().await.unwrap();
    e
}

async fn funded(e: &Engine<LedgerApp, FaultyStore>, org: u64) {
    e.execute(&op(org), &open(0, 392, false)).await.unwrap().unwrap();
    e.execute(&user(org, 1), &open(1, 392, true)).await.unwrap().unwrap();
    e.execute(&user(org, 2), &open(2, 392, true)).await.unwrap().unwrap();
    e.execute(&op(org), &transfer(1, 0, 1, 1_000)).await.unwrap().unwrap();
}

async fn wallet(e: &Engine<LedgerApp, FaultyStore>, org: u64, id: u64) -> i128 {
    let s = e.snapshot(TenantId(org)).await.unwrap();
    audit(&s).unwrap();
    balance(s.accounts.iter().find(|a| a.id == id).unwrap())
}

/// A connection killed in the middle of a transaction: the attempt rolls
/// back, the engine retries from BEGIN on a new connection, and the transfer
/// commits exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_mid_transaction_is_retried_once() {
    let Some(url) = database_url() else { return };
    let _g = SERIAL.lock().await;
    let e = faulty(&url).await;
    let org = fresh_tenant();
    funded(&e, org).await;
    let retries = e.stats.retries.load(Ordering::Relaxed);
    arm(1, Fault::BeforeWrite);
    let r = e.execute(&user(org, 1), &transfer(10, 1, 2, 250)).await.unwrap();
    assert_eq!(r, Ok(k::Reply::Posted(10)));
    assert_eq!(ARMED.load(Ordering::SeqCst), 0, "the fault fired");
    assert!(e.stats.retries.load(Ordering::Relaxed) > retries, "the engine retried");
    assert_eq!(wallet(&e, org, 1).await, 750);
    assert_eq!(wallet(&e, org, 2).await, 250);
}

/// The connection dies after the writes, before COMMIT. Without an
/// idempotency key the engine cannot know whether COMMIT happened and says
/// so (`CommitUnknown`); here it did not, nothing is half-applied, and the
/// client's retry with the same transfer id posts it once. A second retry
/// is recognised as a replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_commit_then_client_retry() {
    let Some(url) = database_url() else { return };
    let _g = SERIAL.lock().await;
    let e = faulty(&url).await;
    let org = fresh_tenant();
    funded(&e, org).await;
    arm(1, Fault::AfterWrite);
    match e.execute(&user(org, 1), &transfer(20, 1, 2, 400)).await {
        Err(DbError::CommitUnknown(_)) => {}
        other => panic!("expected CommitUnknown, got {other:?}"),
    }
    // Nothing committed: not a debit without a credit, not a journal row.
    assert_eq!(wallet(&e, org, 1).await, 1_000);
    assert_eq!(wallet(&e, org, 2).await, 0);
    assert_eq!(e.execute(&user(org, 1), &transfer(20, 1, 2, 400)).await.unwrap(), Ok(k::Reply::Posted(20)));
    assert_eq!(e.execute(&user(org, 1), &transfer(20, 1, 2, 400)).await.unwrap(), Ok(k::Reply::Exists(20)));
    assert_eq!(wallet(&e, org, 1).await, 600);
}

/// With an idempotency key the engine retries an unknown COMMIT by itself:
/// the retry either finds the stored reply or runs again, never twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_commit_with_idempotency_key_is_retried_once() {
    let Some(url) = database_url() else { return };
    let _g = SERIAL.lock().await;
    let e = faulty(&url).await;
    let org = fresh_tenant();
    funded(&e, org).await;
    arm(2, Fault::AfterWrite);
    let r = e.execute_idempotent(&user(org, 1), "pay-30", &transfer(30, 1, 2, 100)).await.unwrap();
    assert_eq!(r, Ok(k::Reply::Posted(30)));
    assert_eq!(ARMED.load(Ordering::SeqCst), 0, "both faults fired");
    // The client's own retry replays the stored reply.
    let r = e.execute_idempotent(&user(org, 1), "pay-30", &transfer(30, 1, 2, 100)).await.unwrap();
    assert_eq!(r, Ok(k::Reply::Posted(30)));
    assert_eq!(wallet(&e, org, 1).await, 900);
    // The same key for a different command is refused, and changes nothing.
    match e.execute_idempotent(&user(org, 1), "pay-30", &transfer(31, 1, 2, 100)).await {
        Err(DbError::IdempotencyConflict) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(wallet(&e, org, 1).await, 900);
}

/// Every attempt killed: the engine gives up with an error after its retry
/// budget, and the ledger is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_failure_leaves_the_ledger_untouched() {
    let Some(url) = database_url() else { return };
    let _g = SERIAL.lock().await;
    *ADMIN_URL.lock().unwrap() = url.clone();
    let e: Engine<LedgerApp, FaultyStore> = Engine::new(
        schema_pool(&url, LEDGER_SCHEMA, 4).unwrap(),
        EngineConfig { max_attempts: 3, ..EngineConfig::default() },
    );
    e.install_schema().await.unwrap();
    let org = fresh_tenant();
    funded(&e, org).await;
    arm(10, Fault::BeforeWrite);
    match e.execute(&user(org, 1), &transfer(40, 1, 2, 1)).await {
        Err(DbError::RetriesExhausted) => {}
        other => panic!("{other:?}"),
    }
    arm(0, Fault::BeforeWrite);
    assert_eq!(wallet(&e, org, 1).await, 1_000);
    let s = e.snapshot(TenantId(org)).await.unwrap();
    assert!(s.transfers.iter().all(|t| t.id != 40));
}

/// No database: an infrastructure error, never a kernel decision, and the
/// HTTP API answers 503 rather than inventing a reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_database() {
    let e: Arc<Engine<LedgerApp, LedgerStore>> = Arc::new(Engine::new(
        schema_pool("postgres://h5i:h5i@127.0.0.1:1/h5i", LEDGER_SCHEMA, 2).unwrap(),
        EngineConfig { max_attempts: 2, ..EngineConfig::default() },
    ));
    assert!(e.execute(&op(1), &open(0, 392, false)).await.is_err());
    let state = payment_api::AppState::new(e, "test-secret");
    let token = h5i_app_http::HmacAuth::<LedgerApp>::new("test-secret", payment_storage::ledger::principal).issue(1, 0, 60);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/ledger"))
        .bearer_auth(token)
        .json(&serde_json::json!({"cmd": "accounts"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
}
