//! Who may call the API (SEC-02).
//!
//! Two kinds of bearer token, both in h5i-app's verified format
//! `v1.<tenant>.<user>.<expiry>.<hmac>`, signed with different keys:
//!
//! * **User tokens** (merchants, account holders), signed with the user key
//!   (`H5I_APP_SECRET`). The user id must be at least 1: a user-key token for
//!   user 0 is refused, so no user token can act as the tenant operator.
//! * **Operator tokens** (support staff, reconciliation, recovery by hand),
//!   signed with the operator key (`H5I_PAY_OPERATOR_SECRET`). The token's
//!   user field is the operator's own id (1 to [`SYSTEM_OPERATORS`] − 1),
//!   logged on every request (`operator request`) and passed to the kernels
//!   as `Actor::Operator(id)`: they record it on every row the operator
//!   writes (ledger transfers, payments, refunds, events, PSP references).
//!   They are short-lived: at most [`MAX_OPERATOR_TTL`].
//!
//! Holding the user key no longer mints operators, and the operator key can
//! be kept from everything that only issues user tokens. Each key may have a
//! previous value (`…_PREVIOUS`) that still verifies, for rotation without
//! downtime (`docs/runbooks/secret-rotation.md`); new tokens always use the
//! current value. The orchestrator, the scheduled recovery job and the
//! export act in process as named system operators
//! ([`payment_storage::ORCHESTRATOR_OPERATOR`], …, ids from
//! [`SYSTEM_OPERATORS`] up) and need no token; no token can carry those ids.

use h5i_app_core::Kernel;
use h5i_app_http::{AuthError, Authenticator, HmacAuth};
use payment_storage::{ledger::LedgerApp, payments::PaymentsApp};
pub use payment_storage::SYSTEM_OPERATORS;

/// Operator tokens live at most this long (12 hours).
pub const MAX_OPERATOR_TTL: u64 = 12 * 3600;
/// Operator tokens default to one hour.
pub const DEFAULT_OPERATOR_TTL: u64 = 3600;
/// Secrets shorter than this are refused when the API runs deployed.
pub const MIN_DEPLOYED_SECRET_LEN: usize = 32;

/// The signing keys: current first, then the previous one if rotating.
#[derive(Clone)]
pub struct Keys {
    user: Vec<Vec<u8>>,
    operator: Vec<Vec<u8>>,
}

impl Keys {
    /// Refuses empty keys and a user key equal to an operator key (a token
    /// must say by its key which kind it is).
    pub fn new(user: &str, operator: &str) -> Result<Keys, String> {
        Keys { user: vec![user.as_bytes().to_vec()], operator: vec![operator.as_bytes().to_vec()] }.checked()
    }

    /// Also accept tokens signed with these previous values (verification only).
    pub fn with_previous(mut self, user: Option<&str>, operator: Option<&str>) -> Result<Keys, String> {
        if let Some(u) = user.filter(|s| !s.is_empty()) {
            self.user.push(u.as_bytes().to_vec());
        }
        if let Some(o) = operator.filter(|s| !s.is_empty()) {
            self.operator.push(o.as_bytes().to_vec());
        }
        self.checked()
    }

    fn checked(self) -> Result<Keys, String> {
        if self.user.iter().chain(&self.operator).any(Vec::is_empty) {
            return Err("token secrets must not be empty".into());
        }
        if self.user.iter().any(|u| self.operator.contains(u)) {
            return Err("H5I_APP_SECRET and H5I_PAY_OPERATOR_SECRET (and their _PREVIOUS values) must all differ".into());
        }
        Ok(self)
    }

    /// Every key at least `n` bytes long.
    pub fn require_len(&self, n: usize) -> Result<(), String> {
        if self.user.iter().chain(&self.operator).any(|k| k.len() < n) {
            return Err(format!("token secrets must be at least {n} bytes"));
        }
        Ok(())
    }

    fn signer(key: &[u8]) -> HmacAuth<LedgerApp> {
        // Tokens do not depend on the kernel; the principal is not used to issue.
        HmacAuth::new(key.to_vec(), payment_storage::ledger::user)
    }

    /// A user token for `user` (≥ 1) of `tenant`, with the current user key.
    pub fn issue_user(&self, tenant: u64, user: u64, ttl_secs: u64) -> Result<String, String> {
        issue_user(&self.user[0], tenant, user, ttl_secs)
    }

    /// An operator token with the current operator key; see [`issue_operator`].
    pub fn issue_operator(&self, tenant: u64, operator_id: u64, ttl_secs: u64) -> Result<String, String> {
        issue_operator(&self.operator[0], tenant, operator_id, ttl_secs)
    }

    /// What a valid token says: its tenant, and the user or operator id.
    pub fn verify(&self, token: &str) -> Result<Caller, AuthError> {
        if let Some((tenant, op)) = first_valid(&self.operator, token)? {
            // The lifetime limit holds for every operator token, not only the
            // ones `issue_operator` made: a token signed with the operator key
            // by other means and expiring later than the limit is refused.
            let exp = token_expiry(token).ok_or_else(|| AuthError("malformed token".into()))?;
            if exp > now().saturating_add(MAX_OPERATOR_TTL + CLOCK_SKEW) {
                return Err(AuthError("operator token lives too long".into()));
            }
            if op == 0 {
                return Err(AuthError("operator ids start at 1".into()));
            }
            if op >= SYSTEM_OPERATORS {
                return Err(AuthError("operator id is reserved for the system".into()));
            }
            return Ok(Caller::Operator { tenant, operator_id: op });
        }
        if let Some((tenant, user)) = first_valid(&self.user, token)? {
            if user == 0 {
                return Err(AuthError("user 0 is not a user; use an operator token".into()));
            }
            return Ok(Caller::User { tenant, user });
        }
        Err(AuthError("bad signature".into()))
    }

    /// The authenticator for kernel `K`: users map with `user`, operators
    /// with `operator` (tenant, operator id).
    pub fn authenticator<K: Kernel>(&self, user: fn(u64, u64) -> K::Principal, operator: fn(u64, u64) -> K::Principal) -> PayAuth<K> {
        PayAuth { keys: self.clone(), user, operator }
    }
}

/// A user token for `user` (≥ 1) of `tenant`, signed with `user_key` alone:
/// whatever issues user tokens needs only the user key.
pub fn issue_user(user_key: &[u8], tenant: u64, user: u64, ttl_secs: u64) -> Result<String, String> {
    if user == 0 {
        return Err("user 0 is not a user; operators get operator tokens".into());
    }
    Ok(Keys::signer(user_key).issue(tenant, user, ttl_secs))
}

/// An operator token for operator `operator_id` (≥ 1, for the audit log)
/// acting on `tenant`, valid at most [`MAX_OPERATOR_TTL`].
pub fn issue_operator(operator_key: &[u8], tenant: u64, operator_id: u64, ttl_secs: u64) -> Result<String, String> {
    if operator_id == 0 {
        return Err("operator ids start at 1".into());
    }
    if operator_id >= SYSTEM_OPERATORS {
        return Err(format!("operator ids from {SYSTEM_OPERATORS} up are the system's own"));
    }
    if ttl_secs > MAX_OPERATOR_TTL {
        return Err(format!("operator tokens live at most {MAX_OPERATOR_TTL} s"));
    }
    Ok(Keys::signer(operator_key).issue(tenant, operator_id, ttl_secs))
}

/// Slack for clocks of the issuer and the API differing (60 s).
const CLOCK_SKEW: u64 = 60;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The expiry field of a `v1.<tenant>.<user>.<exp>.<mac>` token. Called only
/// after the signature verified, so the format is known to be well formed.
fn token_expiry(token: &str) -> Option<u64> {
    token.split('.').nth(3)?.parse().ok()
}

/// `Some` for the first key whose signature matches; an expired or malformed
/// token is an error at once (its signature matched, or no key can help).
fn first_valid(keys: &[Vec<u8>], token: &str) -> Result<Option<(u64, u64)>, AuthError> {
    for k in keys {
        match Keys::signer(k).verify(token) {
            Ok(v) => return Ok(Some(v)),
            Err(AuthError(m)) if m == "bad signature" => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    User { tenant: u64, user: u64 },
    Operator { tenant: u64, operator_id: u64 },
}

/// An h5i-app [`Authenticator`] over [`Keys`].
pub struct PayAuth<K: Kernel> {
    keys: Keys,
    user: fn(u64, u64) -> K::Principal,
    operator: fn(u64, u64) -> K::Principal,
}

impl<K: Kernel> Authenticator<K> for PayAuth<K> {
    fn authenticate(&self, headers: &axum::http::HeaderMap) -> Result<K::Principal, AuthError> {
        let h = headers.get("authorization").ok_or_else(|| AuthError("missing authorization".into()))?;
        let h = h.to_str().map_err(|_| AuthError("bad authorization header".into()))?;
        let token = h.strip_prefix("Bearer ").ok_or_else(|| AuthError("expected Bearer token".into()))?;
        match self.keys.verify(token)? {
            Caller::User { tenant, user } => Ok((self.user)(tenant, user)),
            Caller::Operator { tenant, operator_id } => {
                tracing::info!(tenant, operator_id, "operator request");
                // `Actor::Operator(operator_id)`: the kernels record the id.
                Ok((self.operator)(tenant, operator_id))
            }
        }
    }
}

/// Both kernels' authenticators.
pub fn ledger_auth(keys: &Keys) -> PayAuth<LedgerApp> {
    keys.authenticator::<LedgerApp>(payment_storage::ledger::user, payment_storage::ledger::operator)
}

pub fn payments_auth(keys: &Keys) -> PayAuth<PaymentsApp> {
    keys.authenticator::<PaymentsApp>(payment_storage::payments::user, payment_storage::payments::operator)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys::new("user-key-0123456789abcdef0123456789", "operator-key-0123456789abcdef012345").unwrap()
    }

    #[test]
    fn user_and_operator_tokens() {
        let k = keys();
        let u = k.issue_user(5, 7, 60).unwrap();
        assert_eq!(k.verify(&u).unwrap(), Caller::User { tenant: 5, user: 7 });
        let o = k.issue_operator(5, 3, 60).unwrap();
        assert_eq!(k.verify(&o).unwrap(), Caller::Operator { tenant: 5, operator_id: 3 });
    }

    #[test]
    fn the_user_key_cannot_make_an_operator() {
        let k = keys();
        assert!(k.issue_user(5, 0, 60).is_err());
        // A user-key token for user 0, as anyone with H5I_APP_SECRET could make it.
        let forged = Keys::signer(b"user-key-0123456789abcdef0123456789").issue(5, 0, 60);
        assert!(k.verify(&forged).unwrap_err().0.contains("user 0"));
        // Signed with neither key.
        let other = Keys::signer(b"some-other-key-0123456789abcdef01").issue(5, 7, 60);
        assert_eq!(k.verify(&other).unwrap_err().0, "bad signature");
    }

    #[test]
    fn operator_tokens_are_short_lived_and_named() {
        let k = keys();
        assert!(k.issue_operator(5, 0, 60).is_err());
        assert!(k.issue_operator(5, 1, MAX_OPERATOR_TTL + 1).is_err());
        assert!(k.issue_operator(5, 1, MAX_OPERATOR_TTL).is_ok());
        // Signed with the operator key without going through issue_operator:
        // too long-lived, or for operator id 0. Both refused on use.
        let op_key = b"operator-key-0123456789abcdef012345";
        let long = Keys::signer(op_key).issue(5, 1, MAX_OPERATOR_TTL + 3600);
        assert_eq!(k.verify(&long).unwrap_err().0, "operator token lives too long");
        let zero = Keys::signer(op_key).issue(5, 0, 60);
        assert!(k.verify(&zero).is_err());
        let ok = Keys::signer(op_key).issue(5, 1, MAX_OPERATOR_TTL);
        assert!(k.verify(&ok).is_ok());
        // The system operators' ids cannot be carried by a token.
        assert!(k.issue_operator(5, SYSTEM_OPERATORS, 60).is_err());
        let system = Keys::signer(op_key).issue(5, payment_storage::ORCHESTRATOR_OPERATOR, 60);
        assert_eq!(k.verify(&system).unwrap_err().0, "operator id is reserved for the system");
    }

    #[test]
    fn keys_must_differ_and_be_long_when_deployed() {
        assert!(Keys::new("same", "same").is_err());
        assert!(Keys::new("", "x").is_err());
        assert!(keys().with_previous(Some("operator-key-0123456789abcdef012345"), None).is_err());
        assert!(Keys::new("short", "also-short").unwrap().require_len(MIN_DEPLOYED_SECRET_LEN).is_err());
        assert!(keys().require_len(MIN_DEPLOYED_SECRET_LEN).is_ok());
    }

    #[test]
    fn rotation_accepts_the_previous_key_and_issues_with_the_new_one() {
        let old = keys();
        let new = Keys::new("user-key-NEW-456789abcdef0123456789", "operator-key-NEW-789abcdef012345")
            .unwrap()
            .with_previous(Some("user-key-0123456789abcdef0123456789"), Some("operator-key-0123456789abcdef012345"))
            .unwrap();
        let u_old = old.issue_user(5, 7, 60).unwrap();
        let o_old = old.issue_operator(5, 2, 60).unwrap();
        assert_eq!(new.verify(&u_old).unwrap(), Caller::User { tenant: 5, user: 7 });
        assert_eq!(new.verify(&o_old).unwrap(), Caller::Operator { tenant: 5, operator_id: 2 });
        // New tokens use the new key: the old configuration rejects them.
        let u_new = new.issue_user(5, 7, 60).unwrap();
        assert!(old.verify(&u_new).is_err());
        // After rotation completes (previous dropped), old tokens stop working.
        let done = Keys::new("user-key-NEW-456789abcdef0123456789", "operator-key-NEW-789abcdef012345").unwrap();
        assert!(done.verify(&u_old).is_err() && done.verify(&o_old).is_err());
    }

    #[test]
    fn expired_tokens_are_refused_with_either_key() {
        let k = keys();
        // ttl 0 → expiry = now; wait past it.
        let o = k.issue_operator(5, 1, 0).unwrap();
        let u = k.issue_user(5, 7, 0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(k.verify(&o).unwrap_err().0, "token expired");
        assert_eq!(k.verify(&u).unwrap_err().0, "token expired");
    }
}
