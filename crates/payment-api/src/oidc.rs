//! OIDC/JWT access tokens from a configured identity provider (G3).
//!
//! Next to the HMAC tokens, the API accepts JWT access tokens signed by one
//! configured issuer. Only `RS256` and `ES256` are accepted (no `none`, no
//! HMAC algorithms, so a public key can never be used as an HMAC secret);
//! the key is the issuer's JWKS entry with the token's `kid`, and its type
//! must match the algorithm. The claims checked: `iss` equal to the issuer,
//! `aud` containing the audience (or, without `aud`, `client_id` equal to it,
//! as in Amazon Cognito access tokens), `exp` in the future, `nbf` (if any)
//! and `iat` not in the future (60 s of skew each way), and a `jti`. They
//! map to the caller:
//!
//! * the tenant: claim `H5I_PAY_OIDC_TENANT_CLAIM` (default `h5i_tenant`),
//!   a number or a decimal string;
//! * the actor: `H5I_PAY_OIDC_OPERATOR_CLAIM` (default `h5i_operator`) makes
//!   an operator with that id (1 to 2^62 − 1, living at most 12 h like
//!   every operator token), else `H5I_PAY_OIDC_USER_CLAIM` (default
//!   `h5i_user`) a user (id ≥ 1);
//! * the scopes: `scope` (space-separated) or `scp` (a list), each with
//!   `H5I_PAY_OIDC_SCOPE_PREFIX` removed (e.g. Cognito's
//!   `<resource server>/payments:read`); names this API does not know are
//!   ignored, and a user keeps only user scopes.
//!
//! The JWKS (`H5I_PAY_OIDC_JWKS_URL`, or the `jwks_uri` of
//! `<issuer>/.well-known/openid-configuration`) is fetched at startup and
//! cached; it is fetched again when it is older than ten minutes, or when a
//! token names an unknown `kid` (at most every 30 s), so the issuer can
//! rotate its keys. A failed fetch keeps the keys already known.

use crate::auth::{self, Caller, Scopes, TokenKind, Who, CLOCK_SKEW, MAX_OPERATOR_TTL, USER_SCOPES};
use h5i_app_http::AuthError;
use base64::Engine as _;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const JWKS_MAX_AGE: Duration = Duration::from_secs(600);
const JWKS_MIN_REFETCH: Duration = Duration::from_secs(30);

/// Where tokens come from and how their claims map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcConfig {
    pub issuer: String,
    pub audience: String,
    /// `None`: discovered from the issuer.
    pub jwks_url: Option<String>,
    pub tenant_claim: String,
    pub user_claim: String,
    pub operator_claim: String,
    pub scope_prefix: String,
}

impl OidcConfig {
    /// Defaults for everything but the issuer and the audience.
    pub fn new(issuer: &str, audience: &str) -> Self {
        OidcConfig {
            issuer: issuer.into(),
            audience: audience.into(),
            jwks_url: None,
            tenant_claim: "h5i_tenant".into(),
            user_claim: "h5i_user".into(),
            operator_claim: "h5i_operator".into(),
            scope_prefix: String::new(),
        }
    }

    /// `H5I_PAY_OIDC_ISSUER` set: the configuration (`H5I_PAY_OIDC_AUDIENCE`
    /// required with it). Unset: no identity provider.
    pub fn from_env() -> Result<Option<Self>, String> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let Some(issuer) = env("H5I_PAY_OIDC_ISSUER") else { return Ok(None) };
        let audience = env("H5I_PAY_OIDC_AUDIENCE").ok_or("H5I_PAY_OIDC_AUDIENCE is required with H5I_PAY_OIDC_ISSUER")?;
        let mut c = OidcConfig::new(&issuer, &audience);
        c.jwks_url = env("H5I_PAY_OIDC_JWKS_URL");
        if let Some(v) = env("H5I_PAY_OIDC_TENANT_CLAIM") {
            c.tenant_claim = v;
        }
        if let Some(v) = env("H5I_PAY_OIDC_USER_CLAIM") {
            c.user_claim = v;
        }
        if let Some(v) = env("H5I_PAY_OIDC_OPERATOR_CLAIM") {
            c.operator_claim = v;
        }
        if let Some(v) = env("H5I_PAY_OIDC_SCOPE_PREFIX") {
            c.scope_prefix = v;
        }
        Ok(Some(c))
    }
}

/// A key of the issuer's JWKS.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Jwk {
    /// RSA, for `RS256`: modulus and exponent, big-endian.
    Rsa { n: Vec<u8>, e: Vec<u8> },
    /// P-256, for `ES256`: the uncompressed point `04 || x || y`.
    Ec { point: Vec<u8> },
}

#[derive(Default)]
struct Cache {
    keys: HashMap<String, Jwk>,
    fetched: Option<Instant>,
    attempted: Option<Instant>,
}

/// The verifier for one issuer.
pub struct Oidc {
    cfg: OidcConfig,
    jwks_url: String,
    http: reqwest::Client,
    cache: Mutex<Cache>,
    min_refetch: Duration,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

fn err(m: impl Into<String>) -> AuthError {
    AuthError(m.into())
}

/// Parse a JWKS document: the RS256 and ES256 signing keys with a `kid`;
/// other keys are skipped.
fn parse_jwks(doc: &serde_json::Value) -> HashMap<String, Jwk> {
    let mut out = HashMap::new();
    for k in doc["keys"].as_array().into_iter().flatten() {
        let (Some(kid), Some(kty)) = (k["kid"].as_str(), k["kty"].as_str()) else { continue };
        if k["use"].as_str().is_some_and(|u| u != "sig") {
            continue;
        }
        let field = |f: &str| k[f].as_str().and_then(|v| b64().decode(v).ok());
        let jwk = match (kty, k["alg"].as_str()) {
            ("RSA", None | Some("RS256")) => match (field("n"), field("e")) {
                (Some(n), Some(e)) => Jwk::Rsa { n, e },
                _ => continue,
            },
            ("EC", None | Some("ES256")) if k["crv"].as_str() == Some("P-256") => match (field("x"), field("y")) {
                (Some(x), Some(y)) if x.len() == 32 && y.len() == 32 => {
                    let mut point = vec![4u8];
                    point.extend(x);
                    point.extend(y);
                    Jwk::Ec { point }
                }
                _ => continue,
            },
            _ => continue,
        };
        out.insert(kid.to_string(), jwk);
    }
    out
}

/// A claim as a `u64`: a JSON number, or a decimal string.
fn claim_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

impl Oidc {
    /// Discover the JWKS (unless configured) and fetch it once.
    pub async fn connect(cfg: OidcConfig) -> Result<Oidc, String> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().map_err(|e| e.to_string())?;
        let jwks_url = match &cfg.jwks_url {
            Some(u) => u.clone(),
            None => {
                let url = format!("{}/.well-known/openid-configuration", cfg.issuer.trim_end_matches('/'));
                let doc: serde_json::Value =
                    http.get(&url).send().await.map_err(|e| format!("{url}: {e}"))?.json().await.map_err(|e| format!("{url}: {e}"))?;
                if doc["issuer"].as_str() != Some(cfg.issuer.as_str()) {
                    return Err(format!("{url} names issuer {:?}, not {:?}", doc["issuer"], cfg.issuer));
                }
                doc["jwks_uri"].as_str().ok_or(format!("{url} has no jwks_uri"))?.to_string()
            }
        };
        let o = Oidc { cfg, jwks_url, http, cache: Mutex::new(Cache::default()), min_refetch: JWKS_MIN_REFETCH };
        o.refresh().await.map_err(|AuthError(m)| m)?;
        Ok(o)
    }

    /// Refetch the JWKS for an unknown `kid` at most this often (default
    /// 30 s; tests use less).
    pub fn with_min_refetch(mut self, d: Duration) -> Self {
        self.min_refetch = d;
        self
    }

    pub fn config(&self) -> &OidcConfig {
        &self.cfg
    }

    async fn refresh(&self) -> Result<(), AuthError> {
        self.cache.lock().unwrap().attempted = Some(Instant::now());
        let resp = self.http.get(&self.jwks_url).send().await.map_err(|e| err(format!("JWKS fetch failed: {e}")))?;
        let doc: serde_json::Value = resp.json().await.map_err(|e| err(format!("JWKS is not JSON: {e}")))?;
        let keys = parse_jwks(&doc);
        if keys.is_empty() {
            return Err(err("JWKS has no RS256 or ES256 signing keys"));
        }
        let mut c = self.cache.lock().unwrap();
        c.keys = keys;
        c.fetched = Some(Instant::now());
        Ok(())
    }

    async fn key(&self, kid: &str) -> Result<Jwk, AuthError> {
        let (found, stale, may_refetch) = {
            let c = self.cache.lock().unwrap();
            let stale = c.fetched.is_none_or(|t| t.elapsed() > JWKS_MAX_AGE);
            let may = c.attempted.is_none_or(|t| t.elapsed() > self.min_refetch);
            (c.keys.get(kid).cloned(), stale, may)
        };
        match found {
            Some(k) if !stale => Ok(k),
            Some(k) => {
                // Old but known: use it if the refresh fails.
                if may_refetch && self.refresh().await.is_err() {
                    tracing::warn!("JWKS refresh failed; using the cached keys");
                }
                Ok(self.cache.lock().unwrap().keys.get(kid).cloned().unwrap_or(k))
            }
            None if may_refetch => {
                if let Err(AuthError(m)) = self.refresh().await {
                    tracing::warn!(error = %m, "JWKS refresh failed");
                }
                self.cache.lock().unwrap().keys.get(kid).cloned().ok_or_else(|| err("unknown signing key"))
            }
            None => Err(err("unknown signing key")),
        }
    }

    /// The caller a valid access token names (revocation is checked
    /// separately).
    pub async fn verify(&self, token: &str) -> Result<Caller, AuthError> {
        let bad = || err("malformed token");
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Err(bad()) };
        let header: serde_json::Value = serde_json::from_slice(&b64().decode(h).map_err(|_| bad())?).map_err(|_| bad())?;
        if header.get("crit").is_some() {
            return Err(err("unsupported critical header"));
        }
        let alg = header["alg"].as_str().ok_or_else(bad)?;
        let kid = header["kid"].as_str().ok_or_else(|| err("token has no kid"))?;
        let sig = b64().decode(s).map_err(|_| bad())?;
        let msg = format!("{h}.{p}");
        let key = self.key(kid).await?;
        let ok = match (alg, &key) {
            ("RS256", Jwk::Rsa { n, e }) => ring::signature::RsaPublicKeyComponents { n, e }
                .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, msg.as_bytes(), &sig)
                .is_ok(),
            ("ES256", Jwk::Ec { point }) => ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, point)
                .verify(msg.as_bytes(), &sig)
                .is_ok(),
            ("RS256" | "ES256", _) => return Err(err("key type does not match the algorithm")),
            _ => return Err(err(format!("algorithm {alg} is not accepted (RS256, ES256 only)"))),
        };
        if !ok {
            return Err(err("bad signature"));
        }
        let c: serde_json::Value = serde_json::from_slice(&b64().decode(p).map_err(|_| bad())?).map_err(|_| bad())?;
        self.claims(&c)
    }

    fn claims(&self, c: &serde_json::Value) -> Result<Caller, AuthError> {
        let cfg = &self.cfg;
        if c["iss"].as_str() != Some(cfg.issuer.as_str()) {
            return Err(err("wrong issuer"));
        }
        let aud_ok = match &c["aud"] {
            serde_json::Value::String(a) => *a == cfg.audience,
            serde_json::Value::Array(a) => a.iter().any(|x| x.as_str() == Some(cfg.audience.as_str())),
            serde_json::Value::Null => c["client_id"].as_str() == Some(cfg.audience.as_str()),
            _ => false,
        };
        if !aud_ok {
            return Err(err("wrong audience"));
        }
        let now = auth::now();
        let exp = c["exp"].as_u64().ok_or_else(|| err("token has no exp"))?;
        if exp + CLOCK_SKEW <= now {
            return Err(err("token expired"));
        }
        if c["nbf"].as_u64().is_some_and(|nbf| nbf > now + CLOCK_SKEW) || (!c["nbf"].is_null() && c["nbf"].as_u64().is_none()) {
            return Err(err("token not valid yet"));
        }
        let iat = c["iat"].as_u64().ok_or_else(|| err("token has no iat"))?;
        if iat > now + CLOCK_SKEW {
            return Err(err("token issued in the future"));
        }
        let jti = c["jti"].as_str().filter(|j| !j.is_empty() && j.len() <= 256).ok_or_else(|| err("token has no jti"))?;
        let tenant = claim_u64(&c[cfg.tenant_claim.as_str()]).ok_or_else(|| err(format!("token has no {}", cfg.tenant_claim)))?;
        let mut scopes = Scopes::default();
        let names: Vec<&str> = match (&c["scope"], &c["scp"]) {
            (serde_json::Value::String(s), _) => s.split(' ').collect(),
            (_, serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect(),
            _ => Vec::new(),
        };
        for n in names {
            if let Some(s) = Scopes::of_name(n.strip_prefix(cfg.scope_prefix.as_str()).unwrap_or(n)) {
                scopes = scopes.union(s);
            }
        }
        let who = if let Some(o) = claim_u64(&c[cfg.operator_claim.as_str()]) {
            if o == 0 || o >= auth::SYSTEM_OPERATORS {
                return Err(err("operator id out of range"));
            }
            if exp.saturating_sub(iat) > MAX_OPERATOR_TTL {
                return Err(err("operator token lives too long"));
            }
            Who::Operator(o)
        } else if let Some(u) = claim_u64(&c[cfg.user_claim.as_str()]) {
            if u == 0 {
                return Err(err("user ids start at 1"));
            }
            scopes = Scopes(scopes.0 & USER_SCOPES.0);
            Who::User(u)
        } else {
            return Err(err(format!("token has neither {} nor {}", cfg.operator_claim, cfg.user_claim)));
        };
        Ok(Caller { tenant, who, scopes, jti: Some(jti.to_string()), iat, exp, kind: TokenKind::Oidc })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwks_keeps_only_signing_keys_it_can_use() {
        let x = b64().encode([1u8; 32]);
        let doc = serde_json::json!({"keys": [
            {"kid": "r", "kty": "RSA", "n": b64().encode([7u8; 256]), "e": "AQAB", "alg": "RS256"},
            {"kid": "e", "kty": "EC", "crv": "P-256", "x": x, "y": x},
            {"kid": "h", "kty": "oct", "k": "c2VjcmV0"},
            {"kid": "enc", "kty": "RSA", "n": "AQAB", "e": "AQAB", "use": "enc"},
            {"kid": "p384", "kty": "EC", "crv": "P-384", "x": x, "y": x},
            {"kty": "RSA", "n": "AQAB", "e": "AQAB"},
        ]});
        let keys = parse_jwks(&doc);
        let mut kids: Vec<_> = keys.keys().cloned().collect();
        kids.sort();
        assert_eq!(kids, vec!["e", "r"]);
        assert!(matches!(&keys["e"], Jwk::Ec { point } if point.len() == 65 && point[0] == 4));
    }

    #[test]
    fn claims_map_and_are_checked() {
        let o = Oidc {
            cfg: OidcConfig { scope_prefix: "pay/".into(), ..OidcConfig::new("https://idp", "h5i-pay") },
            jwks_url: String::new(),
            http: reqwest::Client::new(),
            cache: Mutex::new(Cache::default()),
            min_refetch: JWKS_MIN_REFETCH,
        };
        let now = auth::now();
        let base = serde_json::json!({"iss": "https://idp", "aud": "h5i-pay", "exp": now + 300, "iat": now, "jti": "j1",
            "h5i_tenant": "42", "h5i_operator": 3, "scope": "openid pay/payments:read pay/recover"});
        let c = o.claims(&base).unwrap();
        assert_eq!((c.tenant, c.who, c.kind), (42, Who::Operator(3), TokenKind::Oidc));
        assert_eq!(c.scopes, auth::scope::PAYMENTS_READ.union(auth::scope::RECOVER));
        let with = |k: &str, v: serde_json::Value| {
            let mut x = base.clone();
            x[k] = v;
            o.claims(&x).map(|c| c.who).map_err(|AuthError(m)| m)
        };
        assert_eq!(with("iss", "https://other".into()), Err("wrong issuer".into()));
        assert_eq!(with("aud", serde_json::json!(["x", "h5i-pay"])), Ok(Who::Operator(3)));
        assert_eq!(with("aud", "x".into()), Err("wrong audience".into()));
        assert_eq!(with("exp", (now - 120).into()), Err("token expired".into()));
        assert_eq!(with("nbf", (now + 600).into()), Err("token not valid yet".into()));
        assert_eq!(with("jti", serde_json::Value::Null), Err("token has no jti".into()));
        assert_eq!(with("exp", (now + 13 * 3600).into()), Err("operator token lives too long".into()));
        // Cognito access tokens: no `aud`, the audience is `client_id`.
        let mut cog = base.clone();
        cog.as_object_mut().unwrap().remove("aud");
        cog["client_id"] = "h5i-pay".into();
        assert!(o.claims(&cog).is_ok());
        // A user keeps only user scopes.
        let mut u = base.clone();
        u.as_object_mut().unwrap().remove("h5i_operator");
        u["h5i_user"] = 7.into();
        let c = o.claims(&u).unwrap();
        assert_eq!((c.who, c.scopes), (Who::User(7), auth::scope::PAYMENTS_READ));
    }
}
