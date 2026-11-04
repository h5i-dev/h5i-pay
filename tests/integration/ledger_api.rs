//! The ledger over HTTP: JSON commands, tokens, idempotency keys, tenant
//! isolation and the audit endpoint.

use h5i_app_http::HmacAuth;
use h5i_pay_tests::{keys, operator_token, user_token, USER_SECRET};
use h5i_app_pg::EngineConfig;
use h5i_pay_tests::{database_url, fresh_tenant};
use payment_storage::ledger::{user as principal, LedgerApp};
use serde_json::{json, Value};


pub struct Server {
    pub base: String,
    pub http: reqwest::Client,
}

pub async fn start(url: &str) -> Server {
    let (ledger, _) = payment_storage::engines(url, 8, EngineConfig::default()).await.unwrap();
    let state = payment_api::AppState::new(ledger, &keys());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });
    Server { base: format!("http://{addr}"), http: reqwest::Client::new() }
}

pub fn token(org: u64, user: u64) -> String {
    // User 0 is the tenant's operator, which only an operator token is.
    if user == 0 { operator_token(org) } else { user_token(org, user) }
}

impl Server {
    pub async fn ledger(&self, tok: &str, body: Value, key: Option<&str>) -> (u16, Value) {
        let mut req = self.http.post(format!("{}/v1/ledger", self.base)).bearer_auth(tok).json(&body);
        if let Some(k) = key {
            req = req.header("Idempotency-Key", k);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ledger_over_http() {
    let Some(url) = database_url() else { return };
    let s = start(&url).await;
    let org = fresh_tenant();
    let (op, alice, bob) = (token(org, 0), token(org, 1), token(org, 2));

    assert_eq!(s.ledger(&op, json!({"cmd": "open", "currency": 392}), None).await, (200, json!({"id": 0})));
    assert_eq!(s.ledger(&alice, json!({"cmd": "open", "currency": 392, "no_overdraft": true}), None).await, (200, json!({"id": 1})));
    assert_eq!(s.ledger(&bob, json!({"cmd": "open", "currency": 392, "no_overdraft": true}), None).await, (200, json!({"id": 2})));
    // Bob cannot open an account for Alice.
    assert_eq!(s.ledger(&bob, json!({"cmd": "open", "owner": 1, "currency": 392}), None).await.0, 403);

    let fund = json!({"cmd": "transfer", "id": 1, "debit": 0, "credit": 1, "amount": 1000});
    assert_eq!(s.ledger(&op, fund.clone(), None).await, (200, json!({"id": 1, "status": "posted"})));
    // The same transfer again: a replay, nothing changes.
    assert_eq!(s.ledger(&op, fund, None).await, (200, json!({"id": 1, "status": "exists"})));
    // A different transfer under the same id is refused.
    let (st, body) = s.ledger(&op, json!({"cmd": "transfer", "id": 1, "debit": 0, "credit": 1, "amount": 5}), None).await;
    assert_eq!((st, body["error"].as_str()), (409, Some("transfer_id_conflict")));

    // Bob cannot move Alice's money; Alice cannot overdraw.
    let (st, body) = s.ledger(&bob, json!({"cmd": "transfer", "id": 2, "debit": 1, "credit": 2, "amount": 10}), None).await;
    assert_eq!((st, body["error"].as_str()), (403, Some("forbidden")));
    let (st, body) = s.ledger(&alice, json!({"cmd": "transfer", "id": 2, "debit": 1, "credit": 2, "amount": 1001}), None).await;
    assert_eq!((st, body["error"].as_str()), (409, Some("insufficient_funds")));

    // An idempotency key: the second request replays the first reply.
    let pay = json!({"cmd": "transfer", "id": 3, "debit": 1, "credit": 2, "amount": 300});
    assert_eq!(s.ledger(&alice, pay.clone(), Some("k-3")).await, (200, json!({"id": 3, "status": "posted"})));
    assert_eq!(s.ledger(&alice, pay, Some("k-3")).await, (200, json!({"id": 3, "status": "posted"})));
    let other = json!({"cmd": "transfer", "id": 4, "debit": 1, "credit": 2, "amount": 1});
    assert_eq!(s.ledger(&alice, other, Some("k-3")).await.0, 422);

    // System transfer ids (top bit) are reserved for the orchestrator.
    let reserved = json!({"cmd": "transfer", "id": 9_223_372_036_854_775_809u64, "debit": 1, "credit": 2, "amount": 1});
    assert_eq!(s.ledger(&alice, reserved, None).await.0, 400);

    // A linked batch is all-or-nothing.
    let batch = json!({"cmd": "batch", "transfers": [
        {"id": 10, "debit": 1, "credit": 2, "amount": 100},
        {"id": 11, "debit": 1, "credit": 2, "amount": 10_000}]});
    assert_eq!(s.ledger(&alice, batch, None).await.0, 409);

    let (st, accounts) = s.ledger(&alice, json!({"cmd": "accounts"}), None).await;
    assert_eq!(st, 200);
    assert_eq!(accounts, json!({"accounts": [{"id": 1, "owner": 1, "currency": 392, "code": 0, "no_overdraft": true,
        "no_overcredit": false, "debits": 300, "credits": 1000, "seq": 0, "changes": 3}], "next": null}));
    // G1: the account's history, operators only, paged; who, when, what, before and after.
    let (st, h) = s.ledger(&op, json!({"cmd": "history", "account": 1, "limit": 2}), None).await;
    assert_eq!(st, 200, "{h}");
    let hs = h["history"].as_array().unwrap();
    assert_eq!((hs.len(), &h["next"]), (2, &json!(2)));
    assert_eq!((&hs[0]["n"], &hs[0]["command"], &hs[0]["actor"]["kind"]), (&json!(0), &json!("open"), &json!("user")));
    assert_eq!((&hs[1]["command"], &hs[1]["credits_before"], &hs[1]["credits_after"]), (&json!("post"), &json!(0), &json!(1000)));
    let t0 = hs[0]["at"].as_u64().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64;
    assert!(t0 > now - 600_000_000 && t0 <= now + 600_000_000, "at {t0} is the database clock");
    let (_, h2) = s.ledger(&op, json!({"cmd": "history", "account": 1, "from": 2}), None).await;
    assert_eq!((h2["history"][0]["transfer"].clone(), h2["history"][0]["actor"]["id"].clone(), h2["next"].clone()), (json!(3), json!(1), Value::Null));
    assert_eq!(s.ledger(&alice, json!({"cmd": "history", "account": 1}), None).await.0, 403);
    // Another owner's accounts are not listed for a user.
    assert_eq!(s.ledger(&alice, json!({"cmd": "accounts", "owner": 2}), None).await.0, 403);
    assert_eq!(s.ledger(&alice, json!({"cmd": "accounts", "limit": 65}), None).await.0, 400);
    // Bob sees his account only, and cannot read Alice's.
    assert_eq!(s.ledger(&bob, json!({"cmd": "account", "id": 1}), None).await.0, 404);
    assert_eq!(s.ledger(&bob, json!({"cmd": "transfers", "account": 2}), None).await.0, 403);
    // G4: the journal is read per account, paged over its history entries
    // (the opening names no transfer, so a page can hold fewer than `limit`).
    let (st, j) = s.ledger(&op, json!({"cmd": "transfers", "account": 1, "limit": 2}), None).await;
    assert_eq!(st, 200, "{j}");
    assert_eq!((j["transfers"].as_array().map(Vec::len), &j["next"]), (Some(1), &json!(2)));
    assert_eq!(j["transfers"][0]["id"], json!(1));
    let (_, j) = s.ledger(&op, json!({"cmd": "transfers", "account": 1, "from": 2}), None).await;
    assert_eq!((j["transfers"][0]["id"].clone(), j["next"].clone()), (json!(3), Value::Null));
    // I2: the kernel takes journal and account-id pages up to MAX_RANGE for
    // the system's own readers; over HTTP they stay at most MAX_PAGE.
    assert_eq!(s.ledger(&op, json!({"cmd": "transfers", "account": 1, "limit": 65}), None).await.0, 400);
    assert_eq!(s.ledger(&op, json!({"cmd": "transfers", "account": 1, "limit": 4096}), None).await.0, 400);
    assert_eq!(s.ledger(&op, json!({"cmd": "accounts_by_id", "limit": 65}), None).await.0, 400);
    assert_eq!(s.ledger(&op, json!({"cmd": "transfers", "account": 9}), None).await.0, 404);
    assert_eq!(s.ledger(&op, json!({"cmd": "transfers"}), None).await.0, 400);
    let (st, ids) = s.ledger(&op, json!({"cmd": "accounts_by_id", "from": 1, "limit": 1}), None).await;
    assert_eq!((st, ids["accounts"][0]["id"].clone(), ids["next"].clone()), (200, json!(1), json!(2)));
    assert_eq!(s.ledger(&alice, json!({"cmd": "accounts_by_id"}), None).await.0, 403);

    let audit = s.http.get(format!("{}/v1/ledger/audit", s.base)).bearer_auth(&op).send().await.unwrap();
    assert_eq!(audit.status().as_u16(), 200);
    let audit: Value = audit.json().await.unwrap();
    assert_eq!(audit["ok"], json!(true));
    assert_eq!((&audit["accounts"], &audit["transfers"], &audit["in_flight"]), (&json!(3), &json!(2), &json!(0)));
    let audit = s.http.get(format!("{}/v1/ledger/audit", s.base)).bearer_auth(&alice).send().await.unwrap();
    assert_eq!(audit.status().as_u16(), 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenants_are_isolated() {
    let Some(url) = database_url() else { return };
    let s = start(&url).await;
    let (a, b) = (fresh_tenant(), fresh_tenant());
    let (op_a, op_b) = (token(a, 0), token(b, 0));
    s.ledger(&op_a, json!({"cmd": "open", "currency": 392}), None).await;
    s.ledger(&op_a, json!({"cmd": "open", "currency": 392}), None).await;
    s.ledger(&op_a, json!({"cmd": "transfer", "id": 1, "debit": 0, "credit": 1, "amount": 50}), None).await;
    // Tenant B's operator names the same account ids: they are B's ids, which do not exist.
    let (st, _) = s.ledger(&op_b, json!({"cmd": "transfer", "id": 2, "debit": 1, "credit": 0, "amount": 50}), None).await;
    assert_eq!(st, 404);
    assert_eq!(s.ledger(&op_b, json!({"cmd": "accounts"}), None).await, (200, json!({"accounts": [], "next": null})));
    assert_eq!(s.ledger(&op_b, json!({"cmd": "transfers", "account": 0}), None).await.0, 404);
    // A forged token for tenant A signed with another secret is refused.
    let forged = HmacAuth::<LedgerApp>::new("other-secret", principal).issue(a, 0, 600);
    assert_eq!(s.ledger(&forged, json!({"cmd": "accounts"}), None).await.0, 401);
    let (_, ts) = s.ledger(&op_a, json!({"cmd": "transfers", "account": 1}), None).await;
    assert_eq!(ts["transfers"].as_array().unwrap().len(), 1);
}

/// SEC-02: holding the user key does not make an operator.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_user_key_cannot_mint_an_operator() {
    let Some(url) = database_url() else { return };
    let s = start(&url).await;
    let a = fresh_tenant();
    let op = operator_token(a);
    assert_eq!(s.ledger(&op, json!({"cmd": "open", "currency": 392}), None).await.0, 200);
    // What anyone holding H5I_APP_SECRET could sign: tenant a, user 0.
    let forged_op = HmacAuth::<LedgerApp>::new(USER_SECRET, principal).issue(a, 0, 600);
    let (st, body) = s.ledger(&forged_op, json!({"cmd": "transfers", "account": 0}), None).await;
    assert_eq!(st, 401, "{body}");
    let audit = s.http.get(format!("{}/v1/ledger/audit", s.base)).bearer_auth(&forged_op).send().await.unwrap();
    assert_eq!(audit.status().as_u16(), 401);
    // A user token is a user: it cannot read the journal or debit the
    // operator's account into its own.
    let user = user_token(a, 5);
    assert_eq!(s.ledger(&user, json!({"cmd": "open", "currency": 392}), None).await, (200, json!({"id": 1})));
    assert_eq!(s.ledger(&user, json!({"cmd": "transfers", "account": 1}), None).await.0, 403);
    let (st, body) = s.ledger(&user, json!({"cmd": "transfer", "id": 9, "debit": 0, "credit": 1, "amount": 1}), None).await;
    assert_eq!((st, body["error"].as_str()), (403, Some("forbidden")), "{body}");
    let audit = s.http.get(format!("{}/v1/ledger/audit", s.base)).bearer_auth(&user).send().await.unwrap();
    assert_eq!(audit.status().as_u16(), 403);
    // The operator token works for its own tenant only.
    assert_eq!(s.ledger(&operator_token(fresh_tenant()), json!({"cmd": "accounts"}), None).await, (200, json!({"accounts": [], "next": null})));
    // One account each for owner 0 (the operator's) and user 5.
    let (st, accts) = s.ledger(&op, json!({"cmd": "accounts", "owner": 5}), None).await;
    assert_eq!((st, accts["accounts"].as_array().map(Vec::len)), (200, Some(1)));
    let (st, accts) = s.ledger(&op, json!({"cmd": "accounts", "owner": 0}), None).await;
    assert_eq!((st, accts["accounts"].as_array().map(Vec::len)), (200, Some(1)));
}

/// SEC-02: operators are distinct inside the ledger kernel, and the journal
/// records which operator posted each transfer (0 for a user).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_journal_names_the_operator() {
    let Some(url) = database_url() else { return };
    let s = start(&url).await;
    let a = fresh_tenant();
    let (op3, op4, user) = (h5i_pay_tests::operator_token_for(a, 3), h5i_pay_tests::operator_token_for(a, 4), user_token(a, 5));
    assert_eq!(s.ledger(&op3, json!({"cmd": "open", "currency": 392}), None).await, (200, json!({"id": 0})));
    assert_eq!(s.ledger(&user, json!({"cmd": "open", "currency": 392}), None).await, (200, json!({"id": 1})));
    assert_eq!(s.ledger(&op3, json!({"cmd": "transfer", "id": 1, "debit": 0, "credit": 1, "amount": 10}), None).await.0, 200);
    assert_eq!(s.ledger(&op4, json!({"cmd": "transfer", "id": 2, "debit": 0, "credit": 1, "amount": 10}), None).await.0, 200);
    assert_eq!(s.ledger(&user, json!({"cmd": "transfer", "id": 3, "debit": 1, "credit": 0, "amount": 5}), None).await.0, 200);
    // Operator 4 replaying operator 3's transfer gets the replay; the row keeps 3.
    let replay = json!({"cmd": "transfer", "id": 1, "debit": 0, "credit": 1, "amount": 10});
    assert_eq!(s.ledger(&op4, replay, None).await, (200, json!({"id": 1, "status": "exists"})));
    let (st, ts) = s.ledger(&op4, json!({"cmd": "transfers", "account": 0}), None).await;
    assert_eq!(st, 200);
    let mut by: Vec<(u64, u64)> = ts["transfers"].as_array().unwrap().iter().map(|t| (t["id"].as_u64().unwrap(), t["operator"].as_u64().unwrap())).collect();
    by.sort();
    assert_eq!(by, vec![(1, 3), (2, 4), (3, 0)]);
}

async fn account_states(l: &payment_storage::LedgerEngine, op: &payment_ledger::Principal) -> Vec<(u64, u64, u64)> {
    let mut out = Vec::new();
    for id in 0..3 {
        match l.execute(op, &payment_ledger::Command::Account { id }).await.unwrap().unwrap() {
            payment_ledger::Reply::Account(a) => out.push((a.debits, a.credits, a.changes)),
            r => panic!("{r:?}"),
        }
    }
    out
}

/// G4: transfers posted before the history existed (G1) have no history
/// entries, so the paged journal would miss them; installing the schema
/// gives each of their legs one entry, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transfers_from_before_the_history_are_backfilled() {
    use h5i_pay_tests::ledger::{open, transfer};
    use h5i_pay_tests::ScratchDb;
    use payment_ledger as k;
    use payment_storage::ledger::{audit_paged, journal, operator};
    let Some(url) = database_url() else { return };
    let db = ScratchDb::create(&url).await;
    let org = fresh_tenant();
    let op = operator(org, 2);
    let ids = |ts: Vec<k::Transfer>| ts.iter().map(|t| t.id).collect::<Vec<_>>();
    {
        let (l, _) = payment_storage::engines(&db.url, 2, EngineConfig::default()).await.unwrap();
        for c in [open(0, 392, false), open(5, 392, true), open(6, 392, false)] {
            l.execute(&op, &c).await.unwrap().unwrap();
        }
        l.execute(&op, &transfer(1, 0, 1, 100)).await.unwrap().unwrap();
        l.execute(&principal(org, 5), &transfer(2, 1, 2, 30)).await.unwrap().unwrap();
        // Two transfers as they were written before G1: rows and balances,
        // no history entries.
        let (c, conn) = tokio_postgres::connect(&db.url, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(conn);
        let t = org as i64;
        c.batch_execute(&format!(
            "INSERT INTO ledger.transfers (tenant_id, id, debit, credit, amount, currency, code, operator) VALUES
               ({t}, 4, 1, 0, 5, 392, 8, 0), ({t}, 3, 0, 2, 7, 392, 9, 9);
             UPDATE ledger.accounts SET debits = debits + 7, credits = credits + 5 WHERE tenant_id = {t} AND id = 0;
             UPDATE ledger.accounts SET debits = debits + 5 WHERE tenant_id = {t} AND id = 1;
             UPDATE ledger.accounts SET credits = credits + 7 WHERE tenant_id = {t} AND id = 2;
             DELETE FROM ledger.h5i_pay_migrations WHERE name = 'g4-journal-history';
             DELETE FROM h5i_pay_meta.installed;"
        ))
        .await
        .unwrap();
        // Before the backfill the paged journal misses them and the audit says so.
        assert_eq!(ids(journal(&l, &op, 0).await.unwrap()), vec![1]);
        assert!(audit_paged(&l, &op).await.unwrap().is_err());
    }
    // Installing again (a restart) backfills: entries after the existing
    // ones, by transfer id, debit leg first.
    let (l, _) = payment_storage::engines(&db.url, 2, EngineConfig::default()).await.unwrap();
    assert_eq!(ids(journal(&l, &op, 0).await.unwrap()), vec![1, 3, 4]);
    assert_eq!(ids(journal(&l, &op, 1).await.unwrap()), vec![1, 2, 4]);
    assert_eq!(ids(journal(&l, &op, 2).await.unwrap()), vec![2, 3]);
    let after = account_states(&l, &op).await;
    assert_eq!(after, vec![(107, 5, 4), (35, 100, 4), (0, 37, 3)]);
    let r = audit_paged(&l, &op).await.unwrap().unwrap();
    assert_eq!((r.accounts, r.transfers, r.in_flight), (3, 4, 0));
    // The migrated entries say what is known: the leg, the actor, no time.
    let h = match l.execute(&op, &k::Command::History { account: 0, from: 2, limit: 2 }).await.unwrap().unwrap() {
        k::Reply::History(h, _) => h,
        r => panic!("{r:?}"),
    };
    let e: Vec<_> = h.iter().map(|e| (e.command, e.transfer, e.by_operator, e.actor, e.at, e.debits_after, e.credits_after)).collect();
    assert_eq!(e, vec![(k::CMD_MIGRATED, 3, true, 9, 0, 7, 0), (k::CMD_MIGRATED, 4, false, 5, 0, 0, 5)]);
    // Once: a further install changes nothing, and new postings follow on.
    drop(l);
    let (l, _) = payment_storage::engines(&db.url, 2, EngineConfig::default()).await.unwrap();
    assert_eq!(account_states(&l, &op).await, after);
    l.execute(&op, &transfer(5, 0, 2, 1)).await.unwrap().unwrap();
    assert_eq!(ids(journal(&l, &op, 2).await.unwrap()), vec![2, 3, 5]);
    assert!(audit_paged(&l, &op).await.unwrap().is_ok());
}
