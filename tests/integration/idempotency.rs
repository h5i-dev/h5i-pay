//! G2: idempotency keys are per principal (`u<id>`, `o<id>`), and the keys
//! every operator shared before (`u0/…`) are migrated or still replay.

use h5i_app_pg::{EngineConfig, ReplyCodec};
use h5i_pay_tests::{database_url, fresh_tenant, keys, operator_token_for, user_token, ScratchDb};
use payment_ledger as k;
use payment_storage::ledger::{LedgerApp, LedgerStore};
use serde_json::{json, Value};

struct Server {
    base: String,
    http: reqwest::Client,
}

async fn start(url: &str) -> (Server, std::sync::Arc<payment_storage::LedgerEngine>) {
    let (ledger, _) = payment_storage::engines(url, 4, EngineConfig::default()).await.unwrap();
    let state = payment_api::AppState::new(ledger.clone(), &keys()).with_ops(url, &payment_storage::DbTls::Off);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });
    (Server { base: format!("http://{addr}"), http: reqwest::Client::new() }, ledger)
}

impl Server {
    async fn ledger(&self, tok: &str, body: Value, key: &str) -> (u16, Value) {
        let resp = self.http.post(format!("{}/v1/ledger", self.base)).bearer_auth(tok).header("Idempotency-Key", key).json(&body).send().await.unwrap();
        let st = resp.status().as_u16();
        (st, resp.json().await.unwrap_or(Value::Null))
    }
}

async fn keys_in_db(url: &str, org: u64) -> Vec<String> {
    let (c, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(conn);
    let rows = c.query("SELECT key FROM ledger.h5i_idempotency WHERE tenant_id = $1 ORDER BY key", &[&(org as i64)]).await.unwrap();
    rows.iter().map(|r| r.get(0)).collect()
}

fn open(code: u64) -> Value {
    json!({"cmd": "open", "owner": 0, "currency": 392, "code": code})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_are_per_principal() {
    let Some(url) = database_url() else { return };
    let (s, _) = start(&url).await;
    let org = fresh_tenant();
    let (a, b) = (operator_token_for(org, 3), operator_token_for(org, 4));
    let (st, r) = s.ledger(&a, open(5), "k1").await;
    assert_eq!(st, 200, "{r}");
    let a1 = r["id"].as_u64().unwrap();
    // Operator 3's retry replays its reply; another command under its key is refused.
    assert_eq!(s.ledger(&a, open(5), "k1").await, (200, json!({"id": a1})));
    assert_eq!(s.ledger(&a, open(6), "k1").await.0, 422);
    // Operator 4 with the same key and command runs its own request: a new
    // account, not operator 3's reply (before G2 both shared `u0`: a replay).
    let (st, r) = s.ledger(&b, open(5), "k1").await;
    assert_eq!(st, 200, "{r}");
    let b1 = r["id"].as_u64().unwrap();
    assert_ne!(a1, b1);
    // And a different command under the same key collides with nothing.
    let (st, _) = s.ledger(&b, open(6), "k2").await;
    assert_eq!(st, 200);
    assert_eq!(s.ledger(&a, open(7), "k2").await.0, 200);
    // A user's keys are its own too.
    let u = user_token(org, 7);
    let (st, r) = s.ledger(&u, json!({"cmd": "open", "currency": 392}), "k1").await;
    assert_eq!(st, 200, "{r}");
    assert!(r["id"].as_u64().unwrap() > b1);
    assert_eq!(keys_in_db(&url, org).await, vec!["o3/k1", "o3/k2", "o4/k1", "o4/k2", "u7/k1"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_from_before_are_migrated_or_still_replay() {
    let Some(base) = database_url() else { return };
    // Its own database: the test re-runs the startup migrations.
    let db = ScratchDb::create(&base).await;
    let url = db.url.clone();
    // The schema exists (an engine installed it); write rows as before G2.
    let _ = payment_storage::engines(&url, 1, EngineConfig::default()).await.unwrap();
    let org = fresh_tenant();
    let fp = |c: &k::Command| <LedgerStore as ReplyCodec<LedgerApp>>::fingerprint(c);
    let enc = |r: &k::Reply| <LedgerStore as ReplyCodec<LedgerApp>>::encode(r);
    let system = k::Command::Open { owner: 7, currency: 392, code: 1001, no_overdraft: true, no_overcredit: false };
    let manual = k::Command::Open { owner: 0, currency: 392, code: 5, no_overdraft: false, no_overcredit: false };
    {
        let (c, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(conn);
        let ins = "INSERT INTO ledger.h5i_idempotency (tenant_id, key, fingerprint, reply) VALUES ($1, $2, $3, $4)";
        c.execute(ins, &[&(org as i64), &"u0/system-account-7-392-1001", &fp(&system), &enc(&k::Reply::Created(42))]).await.unwrap();
        c.execute(ins, &[&(org as i64), &"u0/manual-1", &fp(&manual), &enc(&k::Reply::Created(43))]).await.unwrap();
        // A database from before G2 has not recorded this schema version.
        c.execute("DELETE FROM h5i_pay_meta.installed", &[]).await.unwrap();
    }
    // Startup migrates: the system's key moves to the orchestrator's scope.
    let (s, ledger) = start(&url).await;
    let orch = format!("o{}/system-account-7-392-1001", payment_storage::ORCHESTRATOR_OPERATOR);
    assert_eq!(keys_in_db(&url, org).await, vec![orch.clone(), "u0/manual-1".to_string()]);
    // The orchestrator, which now always opens the system's accounts as
    // itself, finds its earlier reply: no second account.
    let op = payment_storage::ledger::operator(org, payment_storage::ORCHESTRATOR_OPERATOR);
    let r = ledger.execute_idempotent(&op, "system-account-7-392-1001", &system).await.unwrap();
    assert_eq!(r, Ok(k::Reply::Created(42)));
    // An operator's retry of a request from before still replays (whichever
    // operator sent it then); another command under that key is refused.
    let (o3, o5) = (operator_token_for(org, 3), operator_token_for(org, 5));
    assert_eq!(s.ledger(&o3, open(5), "manual-1").await, (200, json!({"id": 43})));
    assert_eq!(s.ledger(&o5, open(6), "manual-1").await.0, 422);
    // Users never had scope `u0`: theirs is unaffected.
    let (st, r) = s.ledger(&user_token(org, 7), json!({"cmd": "open", "currency": 392}), "manual-1").await;
    assert_eq!(st, 200, "{r}");
    assert_ne!(r["id"], json!(43));
    // Nothing was written under `u0`.
    assert!(keys_in_db(&url, org).await.iter().all(|k| k == "u0/manual-1" || !k.starts_with("u0/")));
}
