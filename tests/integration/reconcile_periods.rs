//! Reconciliation by period (G6): on random payments, refunds, lost PSP
//! answers, a tampered PSP report and stray ledger postings, the result
//! for every period equals the whole-tenant run restricted to it, row by
//! row; and the HTTP reply's counts over periods that tile the ids add up
//! to the whole run's.

use h5i_app_pg::EngineConfig;
use h5i_pay_tests::{database_url, fresh_tenant};
use payment_api::orchestrator::{system_transfer_id, Orchestrator, POST_CAPTURE, POST_REFUND_SETTLE, PSP_CLEARING, REFUND_HOLDING};
use payment_connectors::mock::MockPsp;
use payment_reconcile::{period, rules, Engine, Recon, Record};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Each row's code in a run, keyed by the row (rows with equal contents get
/// equal codes: they share a key, and the codes depend only on the key).
fn codes(r: &Recon) -> (HashMap<Record, u32>, HashMap<Record, u32>) {
    let rep = Engine::Reference.run(r).unwrap();
    let int = r.internal.iter().copied().zip(rep.internal.iter().copied()).collect();
    let ext = r.external.iter().copied().zip(rep.external.iter().copied()).collect();
    (int, ext)
}

/// `part` (a period's run, by every engine) gives each of its rows the
/// code `whole` gives it, and has exactly the whole run's rows in the period.
fn same_as_restricted(whole: &Recon, part: &Recon, from: u64, to: u64, what: &str) {
    let restricted = period::restrict(whole, from, to);
    let sorted = |v: &[Record]| {
        let mut v = v.to_vec();
        v.sort_by_key(|r| (r.key, r.amount, r.currency, r.status));
        v
    };
    assert_eq!(sorted(&part.internal), sorted(&restricted.internal), "{what} [{from},{to}): internal rows");
    assert_eq!(sorted(&part.external), sorted(&restricted.external), "{what} [{from},{to}): external rows");
    let (wi, we) = codes(whole);
    for e in [Engine::Reference, Engine::Single, Engine::Parallel, Engine::Simd] {
        let rep = e.run(part).unwrap();
        for (i, r) in part.internal.iter().enumerate() {
            assert_eq!(rep.internal[i], wi[r], "{what} [{from},{to}) {e:?}: internal {r:?}");
        }
        for (j, r) in part.external.iter().enumerate() {
            assert_eq!(rep.external[j], we[r], "{what} [{from},{to}) {e:?}: external {r:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_period_equals_the_whole_tenant_run_restricted_to_it() {
    let Some(url) = database_url() else { return };
    let (ledger, payments) = payment_storage::engines(&url, 8, EngineConfig::default()).await.unwrap();
    let mock = Arc::new(MockPsp::new("whsec_test"));
    mock.set_now(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    let state = payment_api::AppState::with_payments(ledger.clone(), payments.clone(), mock.clone(), &h5i_pay_tests::keys());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });
    let http = reqwest::Client::new();
    let org = fresh_tenant();
    let (merchant, op) = (h5i_pay_tests::user_token(org, 7), h5i_pay_tests::operator_token(org));
    let post = |path: String, tok: String, body: Value| {
        let req = http.post(format!("{base}{path}")).bearer_auth(tok).json(&body);
        async move {
            let r = req.send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };

    // About 150 ids (payments and refunds share the counter), so the
    // periods below cut through payments and refunds alike.
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    let mut captured = Vec::new();
    for i in 0..110u64 {
        if rng.below(12) == 0 {
            // The PSP's answer is lost: the payment stays processing.
            mock.timeout_next(1);
        }
        let (st, p) = post("/v1/payments".into(), merchant.clone(), json!({
            "amount": 100 + rng.below(5) * 50, "currency": "jpy", "capture": "automatic", "payment_method": "tok_visa"
        }))
        .await;
        assert!(st == 200 || st == 202, "{p}");
        if p["status"] == "captured" {
            captured.push(p["id"].as_u64().unwrap());
        }
        if i % 3 == 0 && !captured.is_empty() {
            let id = captured[rng.below(captured.len() as u64) as usize];
            let (st, r) = post(format!("/v1/payments/{id}/refunds"), merchant.clone(), json!({"amount": 10 + rng.below(3) * 10})).await;
            assert!(st == 200 || st == 202 || st == 409, "{r}");
        }
    }

    // The PSP's report, tampered: amounts changed, lines dropped, lines
    // doubled, lines with ids nobody has (also past the newest id).
    let mut t = Rng(77);
    mock.tamper_settlement(|rows| {
        let mine: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].tenant == org).collect();
        for &i in &mine {
            match t.below(10) {
                0 => rows[i].amount += 1,
                1 => rows[i].currency = 840,
                _ => {}
            }
        }
        let mut extra = Vec::new();
        for &i in &mine {
            if t.below(10) == 0 {
                extra.push(rows[i].clone());
            }
        }
        for k in 0..6 {
            let mut x = rows[mine[k]].clone();
            x.id = 140 + t.below(200);
            extra.push(x);
        }
        let drop: Vec<u64> = mine.iter().filter(|_| t.below(12) == 0).map(|&i| rows[i].id).collect();
        rows.retain(|r| !(r.tenant == org && drop.contains(&r.id) && r.kind == payment_connectors::SettlementKind::Capture));
        rows.extend(extra);
    });

    // Stray ledger postings with system ids: a second capture posting id
    // nobody captured, and one past the newest id.
    let o = Orchestrator::new(ledger.clone(), payments.clone(), mock.clone());
    let lop = payment_storage::ledger::operator(org, 1);
    let sys = payment_storage::ledger::accounts_of(&ledger, &lop, 0).await.unwrap();
    let acct = |code: u32| sys.iter().find(|a| a.code == code).unwrap().id;
    for (kind, id) in [(POST_CAPTURE, 9_000), (POST_CAPTURE, 3_000), (POST_REFUND_SETTLE, 200)] {
        let t = payment_ledger::TransferReq {
            id: system_transfer_id(kind, id % 4096),
            debit: acct(PSP_CLEARING),
            credit: acct(REFUND_HOLDING),
            amount: 5,
            code: kind,
        };
        let r = ledger.execute(&lop, &payment_ledger::Command::Transfer { t }).await.unwrap();
        assert!(r.is_ok(), "{r:?}");
    }

    // The whole-tenant run (one snapshot), and periods: random ones, and
    // ones that tile [0, 4096).
    let whole = o.reconcile_inputs(org).await.unwrap();
    assert!(whole.psp.internal.len() > 100, "{} internal rows", whole.psp.internal.len());
    let (_, all) = codes(&whole.psp);
    assert!(all.values().any(|c| *c != rules::MATCHED), "the report has exceptions");
    let mut periods: Vec<(u64, u64)> = (0..25)
        .map(|_| {
            let from = rng.below(400);
            (from, from + rng.below(300))
        })
        .collect();
    periods.extend((0..4096).step_by(512).map(|f| (f, f + 512)));
    periods.extend([(0, 0), (0, 1), (63, 65), (0, 4096), (4000, 4096 + 4000)]);
    for (from, to) in periods {
        let part = o.reconcile_period(org, from, to).await.unwrap();
        same_as_restricted(&whole.psp, &part.psp, from, to, "psp");
        same_as_restricted(&whole.ledger, &part.ledger, from, to, "ledger");
    }
    // A period past MAX_PERIOD is refused.
    assert!(o.reconcile_period(org, 0, 4097).await.is_err());

    // Over HTTP: the counts of periods that tile the ids add up to the
    // whole run's, and the exceptions come in pages.
    let mut sum: HashMap<String, u64> = HashMap::new();
    let mut listed = 0;
    for from in (0..8192u64).step_by(1024) {
        let (st, r) = post(format!("/v1/reconcile?from={from}&to={}", from + 1024), op.clone(), json!({})).await;
        assert_eq!(st, 200, "{r}");
        for side in ["internal_codes", "external_codes"] {
            for (k, v) in r["psp"][side].as_object().unwrap() {
                *sum.entry(format!("{side}.{k}")).or_default() += v.as_u64().unwrap();
            }
        }
        listed += r["psp"]["listed_exceptions"].as_u64().unwrap();
    }
    let rep = Engine::Single.run(&whole.psp).unwrap();
    let (ci, ce) = rep.counts();
    const NAMES: [&str; rules::CODES] =
        ["matched", "amount_mismatch", "currency_mismatch", "status_mismatch", "missing_external", "duplicate_external", "duplicate_internal", "extra_external"];
    for (side, c) in [("internal_codes", ci), ("external_codes", ce)] {
        for (k, n) in NAMES.iter().zip(c) {
            assert_eq!(sum.get(&format!("{side}.{k}")).copied().unwrap_or(0), n, "{side}.{k}");
        }
    }
    // Exceptions paged three at a time in the first period list them all, once.
    let (_, r) = post("/v1/reconcile?from=0&to=4096&exceptions_limit=1000".into(), op.clone(), json!({})).await;
    let everything = r["psp"]["first_exceptions"].as_array().unwrap().clone();
    assert_eq!(everything.len() as u64, r["psp"]["listed_exceptions"].as_u64().unwrap());
    assert!(everything.len() > 3 && (everything.len() as u64) <= listed);
    let mut paged = Vec::new();
    let mut at = Some(0u64);
    while let Some(f) = at {
        let (st, r) = post(format!("/v1/reconcile?from=0&to=4096&exceptions_from={f}&exceptions_limit=3"), op.clone(), json!({})).await;
        assert_eq!(st, 200, "{r}");
        let page = r["psp"]["first_exceptions"].as_array().unwrap();
        assert!(page.len() <= 3);
        paged.extend(page.iter().cloned());
        at = r["psp"]["exceptions_next"].as_u64();
    }
    assert_eq!(paged, everything);
    // The first period is still open (ids below 4096 can still be made);
    // limits are enforced.
    assert_eq!((r["period"]["closed"].as_bool(), r["period"]["next"].is_null()), (Some(false), true), "{r}");
    assert_eq!(post("/v1/reconcile?from=0&to=4097".into(), op.clone(), json!({})).await.0, 400);
    assert_eq!(post("/v1/reconcile?exceptions_limit=1001".into(), op.clone(), json!({})).await.0, 400);
    // A closed period: ids past it exist.
    let (_, r) = post("/v1/reconcile?from=0&to=64".into(), op.clone(), json!({})).await;
    assert_eq!((r["period"]["closed"].as_bool(), r["period"]["next"].as_u64()), (Some(true), Some(64)), "{r}");
}
