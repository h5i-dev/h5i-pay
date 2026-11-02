//! Token revocation (G3).
//!
//! Revocations are rows of `ops.revocations` in PostgreSQL
//! ([`payment_storage::ops::Revocation`]): one token (`jti`), one actor's
//! tokens issued at or before a time, or all of a tenant's tokens issued at
//! or before a time. Each API process keeps the whole list in memory and
//! checks every request against it, without a database round trip; a task
//! reads the rows added since its last read every `refresh` (default 2 s,
//! `H5I_PAY_REVOCATION_REFRESH_MS`).
//!
//! Staleness, stated: a revocation committed through any server is
//! enforced by every server within `refresh` plus one query's time (by the
//! server that recorded it, at once). If a server cannot read the list for
//! more than five `refresh` intervals (10 s by default), it refuses every
//! token (503) until it can again, so no server acts on a list older than
//! that.

use crate::auth::{Caller, Who, CLOCK_SKEW};
use h5i_app_pg::DbError;
use payment_storage::ops::{self, Revocation};
use payment_storage::DbTls;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Why a token is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Revoked,
    /// The list could not be read for too long.
    Stale,
}

#[derive(Default)]
struct List {
    last_id: i64,
    /// `(tenant, jti)` → when the token expires anyway.
    tokens: HashMap<(u64, String), Option<u64>>,
    /// `(tenant, is operator, id)` → tokens issued at or before this time.
    actors: HashMap<(u64, bool, u64), u64>,
    /// tenant → tokens issued at or before this time.
    tenants: HashMap<u64, u64>,
}

impl List {
    fn add(&mut self, r: &Revocation) {
        self.last_id = self.last_id.max(r.id);
        match (&r.jti, &r.actor, r.before) {
            (Some(j), _, _) => {
                self.tokens.insert((r.tenant, j.clone()), r.expires_at);
            }
            (None, Some((kind, id)), Some(b)) => {
                let e = self.actors.entry((r.tenant, kind == "operator", *id)).or_insert(0);
                *e = (*e).max(b);
            }
            (None, None, Some(b)) => {
                let e = self.tenants.entry(r.tenant).or_insert(0);
                *e = (*e).max(b);
            }
            (None, _, None) => {}
        }
    }

    fn revoked(&self, c: &Caller) -> bool {
        let (op, id) = match c.who {
            Who::User(u) => (false, u),
            Who::Operator(o) => (true, o),
        };
        c.jti.as_ref().is_some_and(|j| self.tokens.contains_key(&(c.tenant, j.clone())))
            || self.actors.get(&(c.tenant, op, id)).is_some_and(|b| c.iat <= *b)
            || self.tenants.get(&c.tenant).is_some_and(|b| c.iat <= *b)
    }

    /// Forget token entries of tokens that have expired anyway.
    fn prune(&mut self, now: u64) {
        self.tokens.retain(|_, exp| exp.is_none_or(|e| e + CLOCK_SKEW > now));
    }
}

/// The cached list, kept fresh by a background task.
pub struct Revocations {
    list: RwLock<(List, Instant)>,
    url: String,
    tls: DbTls,
    refresh: Duration,
    client: tokio::sync::Mutex<Option<tokio_postgres::Client>>,
}

impl Revocations {
    /// Load the whole list, then refresh it every `refresh` in the background.
    pub async fn start(url: &str, tls: &DbTls, refresh: Duration) -> Result<Arc<Revocations>, DbError> {
        ops::install(url, tls).await?;
        let r = Arc::new(Revocations {
            list: RwLock::new((List::default(), Instant::now())),
            url: url.to_string(),
            tls: tls.clone(),
            refresh,
            client: tokio::sync::Mutex::new(None),
        });
        r.refresh_now().await?;
        let weak = Arc::downgrade(&r);
        tokio::spawn(async move {
            loop {
                let Some(r) = weak.upgrade() else { return };
                let every = r.refresh;
                if let Err(e) = r.refresh_now().await {
                    tracing::warn!(error = %e, "revocation list refresh failed");
                }
                drop(r);
                tokio::time::sleep(every).await;
            }
        });
        Ok(r)
    }

    /// `H5I_PAY_REVOCATION_REFRESH_MS`, default 2000.
    pub fn refresh_from_env() -> Duration {
        let ms = std::env::var("H5I_PAY_REVOCATION_REFRESH_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000u64);
        Duration::from_millis(ms.clamp(100, 60_000))
    }

    /// How old the list may get before every token is refused.
    pub fn max_stale(&self) -> Duration {
        self.refresh * 5
    }

    /// Read the rows added since the last read.
    pub async fn refresh_now(&self) -> Result<(), DbError> {
        let mut client = self.client.lock().await;
        if client.as_ref().is_none_or(|c| c.is_closed()) {
            let (c, _conn) = ops::connect_ops(&self.url, &self.tls).await?;
            *client = Some(c);
        }
        let after = self.list.read().unwrap().0.last_id;
        let rows = match ops::revocations_since(client.as_ref().unwrap(), after).await {
            Ok(r) => r,
            Err(e) => {
                *client = None;
                return Err(e);
            }
        };
        let mut l = self.list.write().unwrap();
        for r in &rows {
            l.0.add(r);
        }
        l.0.prune(crate::auth::now());
        l.1 = Instant::now();
        Ok(())
    }

    /// Whether the caller's token may be used.
    pub fn check(&self, c: &Caller) -> Result<(), Refusal> {
        let l = self.list.read().unwrap();
        if l.1.elapsed() > self.max_stale() {
            return Err(Refusal::Stale);
        }
        if l.0.revoked(c) { Err(Refusal::Revoked) } else { Ok(()) }
    }

    /// Record a revocation and apply it here at once.
    pub async fn revoke(&self, r: &Revocation) -> Result<i64, DbError> {
        let id = ops::revoke(&self.url, &self.tls, r).await?;
        self.refresh_now().await?;
        Ok(id)
    }

    pub fn db(&self) -> (&str, &DbTls) {
        (&self.url, &self.tls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{TokenKind, ALL_SCOPES};

    fn caller(tenant: u64, who: Who, jti: Option<&str>, iat: u64) -> Caller {
        Caller { tenant, who, scopes: ALL_SCOPES, jti: jti.map(Into::into), iat, exp: iat + 600, kind: TokenKind::V2 }
    }

    fn rev(id: i64, tenant: u64, jti: Option<&str>, actor: Option<(&str, u64)>, before: Option<u64>) -> Revocation {
        Revocation {
            id,
            tenant,
            jti: jti.map(Into::into),
            actor: actor.map(|(k, i)| (k.into(), i)),
            before,
            expires_at: None,
            revoked_by: 1,
            reason: String::new(),
        }
    }

    #[test]
    fn by_token_by_actor_and_by_date() {
        let mut l = List::default();
        l.add(&rev(1, 5, Some("a"), None, None));
        l.add(&rev(2, 5, None, Some(("operator", 3)), Some(1000)));
        l.add(&rev(3, 6, None, None, Some(2000)));
        assert!(l.revoked(&caller(5, Who::Operator(9), Some("a"), 5000)));
        // The same jti in another tenant is another token.
        assert!(!l.revoked(&caller(6, Who::Operator(9), Some("a"), 3000)));
        assert!(l.revoked(&caller(5, Who::Operator(3), Some("b"), 1000)));
        assert!(!l.revoked(&caller(5, Who::Operator(3), Some("b"), 1001)));
        // A user with the same id is another actor.
        assert!(!l.revoked(&caller(5, Who::User(3), Some("c"), 10)));
        assert!(l.revoked(&caller(6, Who::User(1), Some("d"), 2000)));
        assert!(!l.revoked(&caller(6, Who::User(1), Some("d"), 2001)));
        // v1 tokens (no jti, issued "at 0") fall under every date cutoff.
        assert!(l.revoked(&caller(5, Who::Operator(3), None, 0)));
        assert_eq!(l.last_id, 3);
    }

    #[test]
    fn expired_token_entries_are_forgotten() {
        let mut l = List::default();
        l.add(&Revocation { expires_at: Some(100), ..rev(1, 5, Some("a"), None, None) });
        l.add(&rev(2, 5, Some("b"), None, None));
        l.prune(1000);
        assert_eq!(l.tokens.len(), 1);
    }
}
