//! Id ranges across 2^63 (H2). h5i-app stores a `u64` as its 64 bits in a
//! `BIGINT`, so ids from 2^63 up (system transfers set that bit) are
//! negative in PostgreSQL, and a range read compares in signed order. The
//! kernels' plans read `[from, end)` as `id >= from AND id < end`, or as
//! `id >= from OR id < end` when the range crosses 2^63; here the reads run
//! against PostgreSQL on ids on both sides of 2^63 and of `u64::MAX`.

use h5i_app_pg::EngineConfig;
use h5i_pay_tests::{database_url, fresh_tenant};
use payment_domain as d;
use payment_ledger as l;

const SIGN: u64 = 1 << 63;

fn ids_of(ts: &[l::Transfer]) -> Vec<u64> {
    let mut v: Vec<u64> = ts.iter().map(|t| t.id).collect();
    v.sort_unstable();
    v
}

#[tokio::test]
async fn transfer_ranges_across_the_sign_bit() {
    let Some(url) = database_url() else { return };
    let (ledger, _) = payment_storage::engines(&url, 2, EngineConfig::default()).await.unwrap();
    let org = fresh_tenant();
    let op = l::Principal { org, actor: l::Actor::Operator(1), at: 0 };
    let open = l::Command::Open { owner: 1, currency: 392, code: 0, no_overdraft: false, no_overcredit: false };
    let Ok(l::Reply::Created(a)) = ledger.execute(&op, &open).await.unwrap() else { panic!() };
    let Ok(l::Reply::Created(b)) = ledger.execute(&op, &open).await.unwrap() else { panic!() };
    let ids = [5, SIGN - 2, SIGN - 1, SIGN, SIGN + 1, u64::MAX - 1];
    for id in ids {
        let t = l::Command::Transfer { t: l::TransferReq { id, debit: a, credit: b, amount: 1, code: 0 } };
        assert!(matches!(ledger.execute(&op, &t).await.unwrap(), Ok(l::Reply::Posted(_))), "{id}");
    }
    let range = |from: u64, limit: u32| {
        let ledger = ledger.clone();
        async move {
            match ledger.execute(&op, &l::Command::TransferRange { from, limit }).await.unwrap() {
                Ok(l::Reply::Transfers(ts, _)) => ids_of(&ts),
                r => panic!("{r:?}"),
            }
        }
    };
    // Crossing 2^63: both signed halves.
    assert_eq!(range(SIGN - 2, 4).await, vec![SIGN - 2, SIGN - 1, SIGN, SIGN + 1]);
    assert_eq!(range(SIGN - 1, 2).await, vec![SIGN - 1, SIGN]);
    assert_eq!(range(0, 4096).await, vec![5]);
    assert_eq!(range(SIGN - 4096, 4096).await, vec![SIGN - 2, SIGN - 1]);
    // Ending exactly at 2^63 (its BIGINT is the smallest): the low half only.
    assert_eq!(range(SIGN - 3, 3).await, vec![SIGN - 2, SIGN - 1]);
    // Within the high half (negative BIGINTs), and up to u64::MAX.
    assert_eq!(range(SIGN, 2).await, vec![SIGN, SIGN + 1]);
    assert_eq!(range(SIGN + 1, 4096).await, vec![SIGN + 1]);
    assert_eq!(range(u64::MAX - 3, 10).await, vec![u64::MAX - 1]);
}

#[tokio::test]
async fn payment_ranges_across_the_sign_bit() {
    let Some(url) = database_url() else { return };
    let (_, pay) = payment_storage::engines(&url, 2, EngineConfig::default()).await.unwrap();
    let org = fresh_tenant();
    // The tenant's counter just below 2^63, so new payments get ids across it.
    let (c, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(conn);
    c.execute(
        "INSERT INTO payments.payment_counter (tenant_id, next_id) VALUES ($1, $2)",
        &[&(org as i64), &((SIGN - 2) as i64)],
    )
    .await
    .unwrap();
    let merchant = d::Principal { org, actor: d::Actor::User(7), at: 0 };
    let op = d::Principal { org, actor: d::Actor::Operator(1), at: 0 };
    let mut made = Vec::new();
    for _ in 0..4 {
        let cmd = d::Command::Create { amount: 100, currency: 392, manual_capture: true };
        let Ok(d::Reply::Created(id)) = pay.execute(&merchant, &cmd).await.unwrap() else { panic!() };
        made.push(id);
    }
    assert_eq!(made, vec![SIGN - 2, SIGN - 1, SIGN, SIGN + 1]);
    let range = |from: u64, limit: u32| {
        let pay = pay.clone();
        async move {
            match pay.execute(&op, &d::Command::Range { from, limit }).await.unwrap() {
                Ok(d::Reply::Range(ps, _, _, _)) => {
                    let mut v: Vec<u64> = ps.iter().map(|p| p.id).collect();
                    v.sort_unstable();
                    v
                }
                r => panic!("{r:?}"),
            }
        }
    };
    assert_eq!(range(SIGN - 2, 4).await, made);
    assert_eq!(range(SIGN - 4096, 4096).await, vec![SIGN - 2, SIGN - 1]);
    assert_eq!(range(SIGN - 1, 2).await, vec![SIGN - 1, SIGN]);
    assert_eq!(range(SIGN, 4096).await, vec![SIGN, SIGN + 1]);
    assert_eq!(range(0, 4096).await, Vec::<u64>::new());
}
