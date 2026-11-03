//! I3: `POST /v1/recover`'s `find_work` reads the tenant by id pages
//! (`Orchestrator::find_work_paged`) instead of one whole-tenant load. On
//! random states, with several page sizes (so pages split payments from
//! their refunds and from each other), it finds exactly what the
//! whole-tenant version (`find_work_whole`) finds: payments to ask the PSP
//! about, pending refunds, and ledger work, including captured payments
//! marked done whose postings the ledger lacks (as after a restore of an
//! older ledger backup).

use h5i_app_pg::EngineConfig;
use h5i_pay_tests::{database_url, fresh_tenant, Rng};
use payment_connectors::mock::MockPsp;
use payment_domain::{Actor, Command as C, Principal, PspResult as R, Reply};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_work_matches_the_whole_tenant() {
    let Some(url) = database_url() else { return };
    let (ledger, payments) = payment_storage::engines(&url, 8, EngineConfig::default()).await.unwrap();
    let mock = Arc::new(MockPsp::new("whsec_test"));
    let o = payment_api::orchestrator::Orchestrator::new(ledger, payments.clone(), mock);
    let mut kinds = [0u64; 4];
    for seed in 0..6u64 {
        let org = fresh_tenant();
        let mut rng = Rng(1000 + seed);
        let user = Principal { org, actor: Actor::User(1 + rng.below(2)), at: 0 };
        let op = Principal { org, actor: Actor::Operator(1), at: 0 };
        let run = |actor: Principal, cmd: C| {
            let payments = payments.clone();
            async move { payments.execute(&actor, &cmd).await.unwrap() }
        };
        let n = 10 + rng.below(10);
        for _ in 0..n {
            let amount = 100 * (1 + rng.below(5));
            let Ok(Reply::Created(id)) = run(user, C::Create { amount, currency: 392, manual_capture: rng.below(3) == 0 }).await else {
                panic!("create")
            };
            // How far the payment got: created, processing, authorized,
            // captured, or declined / canceled.
            let stage = rng.below(6);
            if stage >= 1 {
                run(op, C::BeginAuthorize { id }).await.unwrap();
            }
            if stage >= 2 && stage != 5 {
                run(op, C::Record { payment: id, result: R::Authorized { amount } }).await.unwrap();
            }
            if stage == 5 {
                let r = if rng.below(2) == 0 { R::Declined } else { R::Canceled };
                run(op, C::Record { payment: id, result: r }).await.ok();
            }
            if stage >= 3 && stage != 5 {
                run(op, C::Record { payment: id, result: R::Captured { amount } }).await.unwrap();
                // Refunds, some settled either way.
                for _ in 0..rng.below(3) {
                    if let Ok(Reply::Created(rid)) = run(user, C::RequestRefund { payment: id, amount: 1 + rng.below(amount / 4) }).await {
                        match rng.below(3) {
                            0 => drop(run(op, C::Record { payment: id, result: R::RefundSucceeded { refund: rid } }).await),
                            1 => drop(run(op, C::Record { payment: id, result: R::RefundFailed { refund: rid } }).await),
                            _ => {}
                        }
                    }
                }
            }
            // The ledger: nothing, synced (posted and marked), marked without
            // its postings (lost: an older ledger restored), or synced and
            // changed afterwards (a refund after the sync).
            match rng.below(4) {
                0 => {}
                1 => o.sync_ledger(org, id).await.unwrap(),
                2 => {
                    let Ok(Reply::View(p, refunds, _)) = run(op, C::View { id }).await else { panic!("view") };
                    run(op, C::MarkPosted { payment: id, captured: p.captured, refunds }).await.ok();
                }
                _ => {
                    o.sync_ledger(org, id).await.unwrap();
                    if let Ok(Reply::Created(rid)) = run(user, C::RequestRefund { payment: id, amount: 1 }).await {
                        if rng.below(2) == 0 {
                            run(op, C::Record { payment: id, result: R::RefundSucceeded { refund: rid } }).await.ok();
                        }
                        // Marked again without the new refund's postings.
                        if rng.below(2) == 0 {
                            let Ok(Reply::View(p, refunds, _)) = run(op, C::View { id }).await else { panic!("view") };
                            run(op, C::MarkPosted { payment: id, captured: p.captured, refunds }).await.ok();
                        }
                    }
                }
            }
        }
        let whole = o.find_work_whole(org).await.unwrap();
        for page in [1, 2, 3, 7, payment_domain::MAX_RANGE] {
            assert_eq!(o.find_work_paged(org, page).await.unwrap(), whole, "seed {seed}, pages of {page}");
        }
        assert_eq!(o.find_work(org).await.unwrap(), whole, "seed {seed}");
        // The ledger work the marker alone misses: marked, postings missing.
        let snap = payments.snapshot(h5i_app_core::TenantId(org)).await.unwrap();
        let lost = whole.ledger.iter().filter(|id| snap.payments.iter().any(|p| p.id == **id && p.ledger_done)).count();
        kinds[0] += whole.psp.len() as u64;
        kinds[1] += whole.refunds.len() as u64;
        kinds[2] += whole.ledger.len() as u64;
        kinds[3] += lost as u64;
    }
    // The states cover every kind of work.
    assert!(kinds.iter().all(|k| *k > 0), "psp, refunds, ledger, lost postings: {kinds:?}");
}
