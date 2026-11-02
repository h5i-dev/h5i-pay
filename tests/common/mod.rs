//! Shared helpers for the h5i-pay test suites.
//!
//! Tests that need PostgreSQL read `H5I_APP_TEST_DATABASE_URL` (as the h5i-app
//! examples do) and skip when it is unset. Start one with
//! `scripts/dev-db.sh`. Every test uses its own tenant id, so tests can share
//! a database and run in parallel.

use std::sync::atomic::{AtomicU64, Ordering};

/// The token keys every test server uses (SEC-02: users and operators are
/// signed with different keys).
pub const USER_SECRET: &str = "test-user-secret-0123456789abcdef";
pub const OPERATOR_SECRET: &str = "test-operator-secret-0123456789ab";

pub fn keys() -> payment_api::Keys {
    payment_api::Keys::new(USER_SECRET, OPERATOR_SECRET).unwrap()
}

/// A token for user `user` (≥ 1) of tenant `org`.
pub fn user_token(org: u64, user: u64) -> String {
    keys().issue_user(org, user, 600).unwrap()
}

/// An operator token for tenant `org` (operator id 1).
pub fn operator_token(org: u64) -> String {
    operator_token_for(org, 1)
}

/// An operator token for operator `id` of tenant `org`.
pub fn operator_token_for(org: u64, id: u64) -> String {
    keys().issue_operator(org, id, 600).unwrap()
}

pub fn database_url() -> Option<String> {
    match std::env::var("H5I_APP_TEST_DATABASE_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            eprintln!("H5I_APP_TEST_DATABASE_URL not set; skipping");
            None
        }
    }
}

/// A tenant id no other test (or earlier run) uses.
pub fn fresh_tenant() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    ((t >> 8) % (1 << 40)) * 1024 + N.fetch_add(1, Ordering::Relaxed) % 1024
}

/// A small deterministic PRNG (PCG step), so failures reproduce.
pub struct Rng(pub u64);

impl Rng {
    pub fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

pub mod ledger {
    use payment_ledger as k;

    pub fn op(org: u64) -> k::Principal {
        k::Principal { org, actor: k::Actor::Operator(1), at: 0 }
    }

    pub fn user(org: u64, user: u64) -> k::Principal {
        k::Principal { org, actor: k::Actor::User(user), at: 0 }
    }

    pub fn open(owner: u64, currency: u32, no_overdraft: bool) -> k::Command {
        k::Command::Open { owner, currency, code: 0, no_overdraft, no_overcredit: false }
    }

    pub fn transfer(id: u64, debit: u64, credit: u64, amount: u64) -> k::Command {
        k::Command::Transfer { t: k::TransferReq { id, debit, credit, amount, code: 0 } }
    }

    /// Available balance of a `no_overdraft` account.
    pub fn balance(a: &k::Account) -> i128 {
        i128::from(a.credits) - i128::from(a.debits)
    }
}

/// A new empty database next to the test database; dropped by `Drop`.
pub struct ScratchDb {
    admin: String,
    name: String,
    pub url: String,
}

impl ScratchDb {
    pub async fn create(base: &str) -> ScratchDb {
        let (prefix, _) = base.rsplit_once('/').expect("database URL with a path");
        let name = format!("h5i_pay_scratch_{}_{}", std::process::id(), fresh_tenant());
        let (c, conn) = tokio_postgres::connect(base, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(conn);
        c.batch_execute(&format!("CREATE DATABASE {name}")).await.unwrap();
        ScratchDb { admin: base.to_string(), url: format!("{prefix}/{name}"), name }
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        let (admin, name) = (self.admin.clone(), self.name.clone());
        // Best effort, on a thread of its own (Drop cannot await).
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                if let Ok((c, conn)) = tokio_postgres::connect(&admin, tokio_postgres::NoTls).await {
                    tokio::spawn(conn);
                    let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).await;
                }
            })
        })
        .join();
    }
}

