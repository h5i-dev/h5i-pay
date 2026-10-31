//! Shared helpers for the h5i-pay test suites.
//!
//! Tests that need PostgreSQL read `H5I_APP_TEST_DATABASE_URL` (as the h5i-app
//! examples do) and skip when it is unset. Start one with
//! `scripts/dev-db.sh`. Every test uses its own tenant id, so tests can share
//! a database and run in parallel.

use std::sync::atomic::{AtomicU64, Ordering};

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
        k::Principal { org, user: 0, operator: true }
    }

    pub fn user(org: u64, user: u64) -> k::Principal {
        k::Principal { org, user, operator: false }
    }

    pub fn open(owner: u64, currency: u64, no_overdraft: bool) -> k::Command {
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
