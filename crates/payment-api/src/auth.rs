//! Who may call the API (SEC-02, G3).
//!
//! Three kinds of bearer token:
//!
//! * **v2 tokens** (G3, what this server issues): `v2.<payload>.<mac>`,
//!   the payload base64url JSON `{"t": tenant, "k": "u"|"o", "i": id,
//!   "iat", "exp", "jti", "scp"}`, the MAC HMAC-SHA256 over `v2.<payload>`
//!   with the user key (`k: "u"`) or the operator key (`k: "o"`). They carry
//!   a token id (`jti`, for revocation), when they were issued (`iat`, for
//!   revocation by date) and their scopes ([`Scopes`]);
//! * **v1 tokens**, h5i-app's format `v1.<tenant>.<user>.<expiry>.<hmac>`,
//!   still accepted: user tokens get [`USER_SCOPES`], operator tokens every
//!   scope; they have no token id or issue time, so only revocation by
//!   actor or by date applies to them, and it revokes them whatever the
//!   date;
//! * **OIDC/JWT access tokens** from a configured issuer ([`crate::oidc`]).
//!
//! Users and operators are signed with different keys:
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

use base64::Engine as _;
use h5i_app_core::Kernel;
use h5i_app_http::{AuthError, Authenticator, HmacAuth};
use hmac::Mac;
use payment_storage::{ledger::LedgerApp, payments::PaymentsApp};
pub use payment_storage::SYSTEM_OPERATORS;

/// What a token may do (G3). Every route checks one before a kernel runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Scopes(pub u16);

/// The scopes, by name.
pub const SCOPE_NAMES: [(&str, Scopes); 8] = [
    ("payments:read", Scopes(1)),
    ("payments:write", Scopes(1 << 1)),
    ("refunds:write", Scopes(1 << 2)),
    ("ledger:read", Scopes(1 << 3)),
    ("ledger:write", Scopes(1 << 4)),
    ("reconcile:read", Scopes(1 << 5)),
    ("recover", Scopes(1 << 6)),
    ("tokens:revoke", Scopes(1 << 7)),
];

pub mod scope {
    use super::Scopes;
    pub const PAYMENTS_READ: Scopes = Scopes(1);
    pub const PAYMENTS_WRITE: Scopes = Scopes(1 << 1);
    pub const REFUNDS_WRITE: Scopes = Scopes(1 << 2);
    pub const LEDGER_READ: Scopes = Scopes(1 << 3);
    pub const LEDGER_WRITE: Scopes = Scopes(1 << 4);
    pub const RECONCILE_READ: Scopes = Scopes(1 << 5);
    pub const RECOVER: Scopes = Scopes(1 << 6);
    pub const TOKENS_REVOKE: Scopes = Scopes(1 << 7);
}

/// Every scope (`operator:*`).
pub const ALL_SCOPES: Scopes = Scopes(0xff);
/// The scopes a user (merchant, account holder) may hold: its payments,
/// refunds and accounts. The others are operators' only.
pub const USER_SCOPES: Scopes = Scopes(0b1_1111);

impl Scopes {
    pub fn contains(self, s: Scopes) -> bool {
        self.0 & s.0 == s.0
    }

    pub fn union(self, s: Scopes) -> Scopes {
        Scopes(self.0 | s.0)
    }

    /// One scope's name, or `operator:*` for all.
    pub fn of_name(name: &str) -> Option<Scopes> {
        if name == "operator:*" {
            return Some(ALL_SCOPES);
        }
        SCOPE_NAMES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
    }

    /// Names separated by spaces or commas; an unknown name is an error.
    pub fn parse(list: &str) -> Result<Scopes, String> {
        let mut out = Scopes::default();
        for n in list.split([' ', ',']).filter(|n| !n.is_empty()) {
            out = out.union(Scopes::of_name(n).ok_or_else(|| format!("unknown scope {n:?}"))?);
        }
        Ok(out)
    }

    pub fn names(self) -> Vec<&'static str> {
        SCOPE_NAMES.iter().filter(|(_, s)| self.contains(*s)).map(|(n, _)| *n).collect()
    }

    /// The names, space-separated (the `scp` of a v2 token).
    pub fn to_list(self) -> String {
        self.names().join(" ")
    }
}

/// Who a token names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Who {
    User(u64),
    Operator(u64),
}

/// Which format a token had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    V1,
    V2,
    Oidc,
}

/// What a valid token says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub tenant: u64,
    pub who: Who,
    pub scopes: Scopes,
    /// The token's id (v2, OIDC); `None` for v1 tokens.
    pub jti: Option<String>,
    /// When it was issued, in seconds since the Unix epoch; 0 (unknown) for
    /// v1 tokens, which revocation by date therefore always covers.
    pub iat: u64,
    pub exp: u64,
    pub kind: TokenKind,
}

impl Caller {
    pub fn is_operator(&self) -> bool {
        matches!(self.who, Who::Operator(_))
    }

    pub fn ledger_principal(&self) -> payment_ledger::Principal {
        match self.who {
            Who::User(u) => payment_storage::ledger::user(self.tenant, u),
            Who::Operator(o) => payment_storage::ledger::operator(self.tenant, o),
        }
    }

    pub fn payments_principal(&self) -> payment_domain::Principal {
        match self.who {
            Who::User(u) => payment_storage::payments::user(self.tenant, u),
            Who::Operator(o) => payment_storage::payments::operator(self.tenant, o),
        }
    }
}

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

fn mac(key: &[u8], msg: &[u8]) -> HmacSha256 {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(msg);
    m
}

/// 128 random bits, hex: a token id.
pub fn new_jti() -> String {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut b).expect("system randomness");
    hex::encode(b)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct V2Payload {
    t: u64,
    k: String,
    i: u64,
    iat: u64,
    exp: u64,
    jti: String,
    scp: String,
}

fn issue_v2(key: &[u8], kind: &str, tenant: u64, id: u64, ttl_secs: u64, scopes: Scopes) -> String {
    let iat = now();
    let p = V2Payload { t: tenant, k: kind.into(), i: id, iat, exp: iat.saturating_add(ttl_secs), jti: new_jti(), scp: scopes.to_list() };
    let body = format!("v2.{}", b64().encode(serde_json::to_vec(&p).expect("payload serializes")));
    let sig = b64().encode(mac(key, body.as_bytes()).finalize().into_bytes());
    format!("{body}.{sig}")
}

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

    /// A v2 user token for `user` (≥ 1) of `tenant` with every user scope,
    /// with the current user key.
    pub fn issue_user(&self, tenant: u64, user: u64, ttl_secs: u64) -> Result<String, String> {
        issue_user(&self.user[0], tenant, user, ttl_secs)
    }

    /// A v2 user token limited to `scopes` (a subset of [`USER_SCOPES`]).
    pub fn issue_user_scoped(&self, tenant: u64, user: u64, ttl_secs: u64, scopes: Scopes) -> Result<String, String> {
        issue_user_scoped(&self.user[0], tenant, user, ttl_secs, scopes)
    }

    /// A v2 operator token with every scope; see [`issue_operator`].
    pub fn issue_operator(&self, tenant: u64, operator_id: u64, ttl_secs: u64) -> Result<String, String> {
        issue_operator(&self.operator[0], tenant, operator_id, ttl_secs)
    }

    /// A v2 operator token limited to `scopes`.
    pub fn issue_operator_scoped(&self, tenant: u64, operator_id: u64, ttl_secs: u64, scopes: Scopes) -> Result<String, String> {
        issue_operator_scoped(&self.operator[0], tenant, operator_id, ttl_secs, scopes)
    }

    /// What a valid HMAC token (v2 or v1) says. Revocation is checked
    /// separately ([`crate::revocation`]).
    pub fn verify(&self, token: &str) -> Result<Caller, AuthError> {
        if let Some(rest) = token.strip_prefix("v2.") {
            return self.verify_v2(rest);
        }
        self.verify_v1(token)
    }

    fn verify_v2(&self, rest: &str) -> Result<Caller, AuthError> {
        let bad = || AuthError("malformed token".into());
        let (payload, sig) = rest.split_once('.').ok_or_else(bad)?;
        let p: V2Payload = serde_json::from_slice(&b64().decode(payload).map_err(|_| bad())?).map_err(|_| bad())?;
        let sig = b64().decode(sig).map_err(|_| bad())?;
        let keys = match p.k.as_str() {
            "u" => &self.user,
            "o" => &self.operator,
            _ => return Err(bad()),
        };
        let msg = format!("v2.{payload}");
        // Constant-time comparison (`verify_slice`), against each key in turn.
        if !keys.iter().any(|k| mac(k, msg.as_bytes()).verify_slice(&sig).is_ok()) {
            return Err(AuthError("bad signature".into()));
        }
        let t = now();
        if p.exp <= t {
            return Err(AuthError("token expired".into()));
        }
        if p.iat > t + CLOCK_SKEW {
            return Err(AuthError("token issued in the future".into()));
        }
        if p.jti.is_empty() || p.jti.len() > 128 {
            return Err(bad());
        }
        let scopes = Scopes::parse(&p.scp).map_err(AuthError)?;
        let who = if p.k == "o" {
            if p.exp > t.saturating_add(MAX_OPERATOR_TTL + CLOCK_SKEW) || p.exp.saturating_sub(p.iat) > MAX_OPERATOR_TTL {
                return Err(AuthError("operator token lives too long".into()));
            }
            if p.i == 0 {
                return Err(AuthError("operator ids start at 1".into()));
            }
            if p.i >= SYSTEM_OPERATORS {
                return Err(AuthError("operator id is reserved for the system".into()));
            }
            Who::Operator(p.i)
        } else {
            if p.i == 0 {
                return Err(AuthError("user 0 is not a user; use an operator token".into()));
            }
            if !USER_SCOPES.contains(scopes) {
                return Err(AuthError("a user token cannot carry operator scopes".into()));
            }
            Who::User(p.i)
        };
        Ok(Caller { tenant: p.t, who, scopes, jti: Some(p.jti), iat: p.iat, exp: p.exp, kind: TokenKind::V2 })
    }

    fn verify_v1(&self, token: &str) -> Result<Caller, AuthError> {
        let v1 = |tenant, who, scopes| {
            let exp = token_expiry(token).unwrap_or(0);
            Caller { tenant, who, scopes, jti: None, iat: 0, exp, kind: TokenKind::V1 }
        };
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
            return Ok(v1(tenant, Who::Operator(op), ALL_SCOPES));
        }
        if let Some((tenant, user)) = first_valid(&self.user, token)? {
            if user == 0 {
                return Err(AuthError("user 0 is not a user; use an operator token".into()));
            }
            return Ok(v1(tenant, Who::User(user), USER_SCOPES));
        }
        Err(AuthError("bad signature".into()))
    }

    /// The authenticator for kernel `K`: users map with `user`, operators
    /// with `operator` (tenant, operator id).
    pub fn authenticator<K: Kernel>(&self, user: fn(u64, u64) -> K::Principal, operator: fn(u64, u64) -> K::Principal) -> PayAuth<K> {
        PayAuth { keys: self.clone(), user, operator }
    }
}

/// A v2 user token for `user` (≥ 1) of `tenant` with every user scope,
/// signed with `user_key` alone: whatever issues user tokens needs only
/// the user key.
pub fn issue_user(user_key: &[u8], tenant: u64, user: u64, ttl_secs: u64) -> Result<String, String> {
    issue_user_scoped(user_key, tenant, user, ttl_secs, USER_SCOPES)
}

/// A v2 user token limited to `scopes`, which must be user scopes.
pub fn issue_user_scoped(user_key: &[u8], tenant: u64, user: u64, ttl_secs: u64, scopes: Scopes) -> Result<String, String> {
    if user == 0 {
        return Err("user 0 is not a user; operators get operator tokens".into());
    }
    if !USER_SCOPES.contains(scopes) {
        return Err(format!("users may hold only {}", USER_SCOPES.to_list()));
    }
    Ok(issue_v2(user_key, "u", tenant, user, ttl_secs, scopes))
}

/// A v2 operator token for operator `operator_id` (≥ 1, for the audit log)
/// acting on `tenant` with every scope, valid at most [`MAX_OPERATOR_TTL`].
pub fn issue_operator(operator_key: &[u8], tenant: u64, operator_id: u64, ttl_secs: u64) -> Result<String, String> {
    issue_operator_scoped(operator_key, tenant, operator_id, ttl_secs, ALL_SCOPES)
}

/// A v2 operator token limited to `scopes`.
pub fn issue_operator_scoped(operator_key: &[u8], tenant: u64, operator_id: u64, ttl_secs: u64, scopes: Scopes) -> Result<String, String> {
    if operator_id == 0 {
        return Err("operator ids start at 1".into());
    }
    if operator_id >= SYSTEM_OPERATORS {
        return Err(format!("operator ids from {SYSTEM_OPERATORS} up are the system's own"));
    }
    if ttl_secs > MAX_OPERATOR_TTL {
        return Err(format!("operator tokens live at most {MAX_OPERATOR_TTL} s"));
    }
    Ok(issue_v2(operator_key, "o", tenant, operator_id, ttl_secs, scopes))
}

/// A v1 (h5i-app format) token, as before G3. For tests of compatibility.
pub fn issue_v1(key: &[u8], tenant: u64, id: u64, ttl_secs: u64) -> String {
    Keys::signer(key).issue(tenant, id, ttl_secs)
}

/// Slack for clocks of the issuer and the API differing (60 s).
pub const CLOCK_SKEW: u64 = 60;

pub fn now() -> u64 {
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
        // Only h5i-app's own extractor uses this; the routes authenticate
        // with `routes::Authed` (scopes, OIDC, revocation).
        let c = self.keys.verify(token)?;
        match c.who {
            Who::User(user) => Ok((self.user)(c.tenant, user)),
            Who::Operator(operator_id) => Ok((self.operator)(c.tenant, operator_id)),
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

    fn who(k: &Keys, t: &str) -> (u64, Who) {
        let c = k.verify(t).unwrap();
        (c.tenant, c.who)
    }

    #[test]
    fn user_and_operator_tokens() {
        let k = keys();
        let u = k.issue_user(5, 7, 60).unwrap();
        assert!(u.starts_with("v2."));
        let c = k.verify(&u).unwrap();
        assert_eq!((c.tenant, c.who, c.scopes, c.kind), (5, Who::User(7), USER_SCOPES, TokenKind::V2));
        assert!(c.jti.as_ref().is_some_and(|j| j.len() == 32) && c.iat > 0 && c.exp == c.iat + 60);
        let o = k.issue_operator(5, 3, 60).unwrap();
        let c = k.verify(&o).unwrap();
        assert_eq!((c.tenant, c.who, c.scopes), (5, Who::Operator(3), ALL_SCOPES));
        // Every token gets its own id.
        assert_ne!(k.verify(&k.issue_operator(5, 3, 60).unwrap()).unwrap().jti, c.jti);
    }

    #[test]
    fn scopes_are_named_limited_and_signed() {
        let k = keys();
        assert_eq!(Scopes::parse("payments:read ledger:read").unwrap(), scope::PAYMENTS_READ.union(scope::LEDGER_READ));
        assert_eq!(Scopes::parse("operator:*").unwrap(), ALL_SCOPES);
        assert!(Scopes::parse("payments:admin").is_err());
        // A user cannot be issued an operator scope.
        assert!(k.issue_user_scoped(5, 7, 60, scope::RECOVER).is_err());
        let ro = k.issue_operator_scoped(5, 3, 60, scope::RECONCILE_READ).unwrap();
        let c = k.verify(&ro).unwrap();
        assert!(c.scopes.contains(scope::RECONCILE_READ) && !c.scopes.contains(scope::LEDGER_WRITE));
        // Changing the scopes breaks the signature.
        let (body, sig) = ro.rsplit_once('.').unwrap();
        let mut p: serde_json::Value =
            serde_json::from_slice(&b64().decode(body.strip_prefix("v2.").unwrap()).unwrap()).unwrap();
        p["scp"] = "operator:*".into();
        let forged = format!("v2.{}.{sig}", b64().encode(serde_json::to_vec(&p).unwrap()));
        assert_eq!(k.verify(&forged).unwrap_err().0, "bad signature");
        // So does claiming to be an operator with a user-key MAC.
        let u = k.issue_user(5, 7, 60).unwrap();
        let (body, sig) = u.rsplit_once('.').unwrap();
        let mut p: serde_json::Value =
            serde_json::from_slice(&b64().decode(body.strip_prefix("v2.").unwrap()).unwrap()).unwrap();
        p["k"] = "o".into();
        let forged = format!("v2.{}.{sig}", b64().encode(serde_json::to_vec(&p).unwrap()));
        assert_eq!(k.verify(&forged).unwrap_err().0, "bad signature");
    }

    #[test]
    fn v1_tokens_still_verify() {
        let k = keys();
        let u = issue_v1(b"user-key-0123456789abcdef0123456789", 5, 7, 60);
        let c = k.verify(&u).unwrap();
        assert_eq!((c.who, c.scopes, c.kind, c.jti, c.iat), (Who::User(7), USER_SCOPES, TokenKind::V1, None, 0));
        let o = issue_v1(b"operator-key-0123456789abcdef012345", 5, 3, 60);
        assert_eq!(k.verify(&o).unwrap().scopes, ALL_SCOPES);
    }

    #[test]
    fn the_user_key_cannot_make_an_operator() {
        let k = keys();
        assert!(k.issue_user(5, 0, 60).is_err());
        // A user-key token for user 0, as anyone with H5I_APP_SECRET could make it.
        let forged = Keys::signer(b"user-key-0123456789abcdef0123456789").issue(5, 0, 60);
        assert!(k.verify(&forged).unwrap_err().0.contains("user 0"));
        let forged = issue_v2(b"user-key-0123456789abcdef0123456789", "u", 5, 0, 60, USER_SCOPES);
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
        assert_eq!(who(&new, &u_old), (5, Who::User(7)));
        assert_eq!(who(&new, &o_old), (5, Who::Operator(2)));
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
