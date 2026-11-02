//! G3: scoped tokens, revocation, and access tokens from an OIDC issuer
//! (a local one here, with keys generated for the test).

use base64::Engine as _;
use h5i_app_pg::EngineConfig;
use h5i_pay_tests::{database_url, fresh_tenant, keys, OPERATOR_SECRET, USER_SECRET};
use payment_api::auth::{scope, Scopes, ALL_SCOPES, USER_SCOPES};
use payment_api::revocation::Revocations;
use payment_connectors::mock::MockPsp;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Api {
    base: String,
    http: reqwest::Client,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, tok: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(tok);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let st = resp.status().as_u16();
        (st, resp.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, tok: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, tok, Some(body)).await
    }

    async fn get(&self, path: &str, tok: &str) -> (u16, Value) {
        self.call(reqwest::Method::GET, path, tok, None).await
    }
}

async fn serve(state: payment_api::AppState) -> Api {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, payment_api::router(state)).await.unwrap() });
    Api { base: format!("http://{addr}"), http: reqwest::Client::new() }
}

async fn state(url: &str) -> payment_api::AppState {
    let (ledger, payments) = payment_storage::engines(url, 4, EngineConfig::default()).await.unwrap();
    let mock = Arc::new(MockPsp::new("whsec_test"));
    mock.set_now(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    let off = payment_storage::DbTls::Off;
    let rev = Revocations::start(url, &off, Duration::from_millis(200)).await.unwrap();
    payment_api::AppState::with_payments(ledger, payments, mock, &keys()).with_ops(url, &off).with_revocations(rev)
}

fn pay() -> Value {
    json!({"amount": 500, "currency": 392, "payment_method": "tok_visa"})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_route_checks_its_scope() {
    let Some(url) = database_url() else { return };
    let api = serve(state(&url).await).await;
    let org = fresh_tenant();
    let k = keys();
    let merchant = k.issue_user(org, 7, 600).unwrap();
    let (st, p) = api.post("/v1/payments", &merchant, pay()).await;
    assert_eq!(st, 200, "{p}");
    let id = p["id"].as_u64().unwrap();
    // A read-only merchant token reads, and cannot pay, refund or post.
    let ro = k.issue_user_scoped(org, 7, 600, scope::PAYMENTS_READ).unwrap();
    assert_eq!(api.get(&format!("/v1/payments/{id}"), &ro).await.0, 200);
    let (st, e) = api.post("/v1/payments", &ro, pay()).await;
    assert_eq!((st, e), (403, json!({"error": "insufficient_scope", "scope": "payments:write"})));
    assert_eq!(api.post(&format!("/v1/payments/{id}/refunds"), &ro, json!({"amount": 1})).await.0, 403);
    assert_eq!(api.post("/v1/ledger", &ro, json!({"cmd": "accounts"})).await.0, 403);
    // Ledger reads and writes are separate scopes.
    let lr = k.issue_user_scoped(org, 7, 600, scope::LEDGER_READ).unwrap();
    assert_eq!(api.post("/v1/ledger", &lr, json!({"cmd": "accounts"})).await.0, 200);
    let (st, e) = api.post("/v1/ledger", &lr, json!({"cmd": "open", "currency": 392})).await;
    assert_eq!((st, e["scope"].clone()), (403, json!("ledger:write")));
    // Refunds need their own scope.
    let rf = k.issue_user_scoped(org, 7, 600, scope::REFUNDS_WRITE).unwrap();
    let (st, r) = api.post(&format!("/v1/payments/{id}/refunds"), &rf, json!({"amount": 100})).await;
    assert!(st == 200 || st == 202, "{st} {r}");
    // An operator limited to reconciliation reads it, and cannot recover,
    // audit, post, or revoke.
    let recon = k.issue_operator_scoped(org, 3, 600, scope::RECONCILE_READ).unwrap();
    assert_eq!(api.post("/v1/reconcile", &recon, json!({})).await.0, 200);
    assert_eq!(api.post("/v1/recover", &recon, json!({})).await.0, 403);
    assert_eq!(api.get("/v1/ledger/audit", &recon).await.0, 403);
    assert_eq!(api.post("/v1/ledger", &recon, json!({"cmd": "transfers", "account": 0})).await.0, 403);
    assert_eq!(api.post("/v1/tokens/revoke", &recon, json!({"before": 1})).await.0, 403);
    let rec = k.issue_operator_scoped(org, 3, 600, scope::RECOVER).unwrap();
    assert_eq!(api.post("/v1/recover", &rec, json!({})).await.0, 200);
    // v1 tokens, as issued before G3, keep working: users with the user
    // scopes, operators with all.
    let v1_user = payment_api::auth::issue_v1(USER_SECRET.as_bytes(), org, 7, 600);
    assert_eq!(api.get(&format!("/v1/payments/{id}"), &v1_user).await.0, 200);
    assert_eq!(api.post("/v1/recover", &v1_user, json!({})).await.0, 403);
    let v1_op = payment_api::auth::issue_v1(OPERATOR_SECRET.as_bytes(), org, 2, 600);
    assert_eq!(api.post("/v1/recover", &v1_op, json!({})).await.0, 200);
    // Without a token, or with garbage: 401.
    assert_eq!(api.get(&format!("/v1/payments/{id}"), "nope").await.0, 401);
    assert_eq!(USER_SCOPES.union(scope::RECOVER).0 & !ALL_SCOPES.0, 0);
}

async fn until(api: &Api, path: &str, tok: &str, want: u16) -> u16 {
    let mut st = 0;
    for _ in 0..50 {
        st = api.get(path, tok).await.0;
        if st == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    st
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revocation_by_token_by_actor_and_by_date() {
    let Some(url) = database_url() else { return };
    let api = serve(state(&url).await).await;
    // A second API process with its own cached list, as in a deployment.
    let other = serve(state(&url).await).await;
    let org = fresh_tenant();
    let k = keys();
    let admin = k.issue_operator(org, 1, 3600).unwrap();
    let a = k.issue_user(org, 7, 600).unwrap();
    let b = k.issue_user(org, 7, 600).unwrap();
    let path = "/v1/payments/999999";
    assert_eq!(api.get(path, &a).await.0, 404);
    // One token: this server refuses it at once, the other within its refresh.
    let jti = k.verify(&a).unwrap().jti.unwrap();
    let (st, r) = api.post("/v1/tokens/revoke", &admin, json!({"jti": jti, "reason": "leaked"})).await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["revoked"]["revoked_by"], json!(1));
    assert_eq!(api.get(path, &a).await, (401, json!({"error": "token revoked"})));
    assert_eq!(until(&other, path, &a, 401).await, 401);
    assert_eq!(api.get(path, &b).await.0, 404);
    // A user's tokens issued so far, the v1 kind included; one issued later works.
    let v1 = payment_api::auth::issue_v1(USER_SECRET.as_bytes(), org, 7, 600);
    assert_eq!(api.get(path, &v1).await.0, 404);
    let (st, _) = api.post("/v1/tokens/revoke", &admin, json!({"actor": {"kind": "user", "id": 7}})).await;
    assert_eq!(st, 200);
    assert_eq!(api.get(path, &b).await.0, 401);
    assert_eq!(api.get(path, &v1).await.0, 401);
    // Another user, and an operator with the same id, are not affected.
    assert_eq!(api.get(path, &k.issue_user(org, 8, 600).unwrap()).await.0, 404);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let c = k.issue_user(org, 7, 600).unwrap();
    assert_eq!(api.get(path, &c).await.0, 404);
    // Everything of the tenant issued before a date, the revoking operator's own token too.
    let before = payment_api::auth::now();
    let (st, _) = api.post("/v1/tokens/revoke", &admin, json!({"before": before, "reason": "incident"})).await;
    assert_eq!(st, 200);
    assert_eq!(api.get(path, &c).await.0, 401);
    assert_eq!(api.get("/v1/tokens/revocations", &admin).await.0, 401);
    assert_eq!(until(&other, path, &c, 401).await, 401);
    // Other tenants keep working.
    assert_eq!(api.get(path, &k.issue_user(fresh_tenant(), 7, 600).unwrap()).await.0, 404);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let admin2 = k.issue_operator(org, 1, 3600).unwrap();
    let (st, list) = api.get("/v1/tokens/revocations", &admin2).await;
    assert_eq!(st, 200, "{list}");
    let rs = list["revocations"].as_array().unwrap();
    assert_eq!(rs.len(), 3);
    assert_eq!((rs[0]["before"].clone(), rs[1]["actor"].clone(), rs[2]["reason"].clone()), (json!(before), json!({"kind": "user", "id": 7}), json!("leaked")));
    // Users may not revoke.
    assert_eq!(api.post("/v1/tokens/revoke", &k.issue_user(org, 9, 600).unwrap(), json!({"before": 1})).await.0, 403);
    assert_eq!(api.post("/v1/tokens/revoke", &admin2, json!({})).await.0, 400);
}

/// A local OIDC issuer: discovery, and a JWKS the test can change.
struct Issuer {
    base: String,
    jwks: Arc<Mutex<Value>>,
}

async fn issuer() -> Issuer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let jwks = Arc::new(Mutex::new(json!({"keys": []})));
    let (b, j) = (base.clone(), jwks.clone());
    let app = axum::Router::new()
        .route(
            "/.well-known/openid-configuration",
            axum::routing::get(move || {
                let b = b.clone();
                async move { axum::Json(json!({"issuer": b, "jwks_uri": format!("{b}/jwks")})) }
            }),
        )
        .route(
            "/jwks",
            axum::routing::get(move || {
                let j = j.clone();
                async move { axum::Json(j.lock().unwrap().clone()) }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Issuer { base, jwks }
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// An RSA key from `openssl genpkey` (DER: PKCS#1 `RSAPrivateKey`), for RS256.
fn rsa_key() -> ring::rsa::KeyPair {
    let out = std::process::Command::new("openssl")
        .args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-outform", "DER"])
        .output()
        .expect("openssl generates the test's RSA key");
    assert!(out.status.success());
    ring::rsa::KeyPair::from_der(&out.stdout).unwrap()
}

fn rsa_jwk(kid: &str, k: &ring::rsa::KeyPair) -> Value {
    let c = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(k.public());
    json!({"kid": kid, "kty": "RSA", "alg": "RS256", "use": "sig", "n": b64(&c.n), "e": b64(&c.e)})
}

fn ec_key() -> ring::signature::EcdsaKeyPair {
    let rng = ring::rand::SystemRandom::new();
    let alg = &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
    let pk = ring::signature::EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
    ring::signature::EcdsaKeyPair::from_pkcs8(alg, pk.as_ref(), &rng).unwrap()
}

fn ec_jwk(kid: &str, k: &ring::signature::EcdsaKeyPair) -> Value {
    use ring::signature::KeyPair;
    let p = k.public_key().as_ref();
    json!({"kid": kid, "kty": "EC", "crv": "P-256", "x": b64(&p[1..33]), "y": b64(&p[33..65])})
}

fn jwt(header: Value, claims: &Value, sign: impl Fn(&[u8]) -> Vec<u8>) -> String {
    let msg = format!("{}.{}", b64(&serde_json::to_vec(&header).unwrap()), b64(&serde_json::to_vec(claims).unwrap()));
    let sig = sign(msg.as_bytes());
    format!("{msg}.{}", b64(&sig))
}

fn rs256(k: &ring::rsa::KeyPair, kid: &str, claims: &Value) -> String {
    jwt(json!({"alg": "RS256", "kid": kid, "typ": "JWT"}), claims, |m| {
        let mut sig = vec![0; k.public().modulus_len()];
        k.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), m, &mut sig).unwrap();
        sig
    })
}

fn es256(k: &ring::signature::EcdsaKeyPair, kid: &str, claims: &Value) -> String {
    jwt(json!({"alg": "ES256", "kid": kid}), claims, |m| k.sign(&ring::rand::SystemRandom::new(), m).unwrap().as_ref().to_vec())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn access_tokens_from_an_oidc_issuer() {
    let Some(url) = database_url() else { return };
    let idp = issuer().await;
    let (rsa, ec) = (rsa_key(), ec_key());
    *idp.jwks.lock().unwrap() = json!({"keys": [rsa_jwk("r1", &rsa), ec_jwk("e1", &ec)]});
    let mut cfg = payment_api::oidc::OidcConfig::new(&idp.base, "h5i-pay");
    cfg.scope_prefix = "h5i-pay/".into();
    let oidc = payment_api::oidc::Oidc::connect(cfg).await.unwrap().with_min_refetch(Duration::from_millis(100));
    let api = serve(state(&url).await.with_oidc(Arc::new(oidc))).await;
    let org = fresh_tenant();
    let now = payment_api::auth::now();
    let claims = |who: (&str, u64), scopes: &str| {
        let mut c = json!({"iss": idp.base, "aud": "h5i-pay", "iat": now, "exp": now + 600, "jti": payment_api::auth::new_jti(),
               "h5i_tenant": org, "scope": scopes});
        c[who.0] = who.1.into();
        c
    };
    // An operator from the IdP, RS256, limited to reconciliation.
    let op = rs256(&rsa, "r1", &claims(("h5i_operator", 5), "openid h5i-pay/reconcile:read"));
    assert_eq!(api.post("/v1/reconcile", &op, json!({})).await.0, 200);
    assert_eq!(api.post("/v1/recover", &op, json!({})).await.0, 403);
    // A merchant, ES256, with payment scopes; operator scopes are dropped.
    let m = es256(&ec, "e1", &claims(("h5i_user", 7), "h5i-pay/payments:read h5i-pay/payments:write h5i-pay/recover"));
    let (st, p) = api.post("/v1/payments", &m, pay()).await;
    assert_eq!(st, 200, "{p}");
    assert_eq!(p["merchant"], json!(7));
    assert_eq!(api.post("/v1/recover", &m, json!({})).await.0, 403);
    assert_eq!(api.post("/v1/ledger", &m, json!({"cmd": "accounts"})).await.0, 403);
    // Refused: wrong issuer, audience, expired, not yet valid, unknown key,
    // a payload changed after signing, and algorithms other than RS256/ES256.
    let refused = |c: Value| rs256(&rsa, "r1", &c);
    let mut bad = claims(("h5i_operator", 5), "h5i-pay/reconcile:read");
    bad["iss"] = "http://elsewhere".into();
    let mut aud = claims(("h5i_operator", 5), "h5i-pay/reconcile:read");
    aud["aud"] = "other".into();
    let mut old = claims(("h5i_operator", 5), "h5i-pay/reconcile:read");
    old["exp"] = (now - 120).into();
    let mut nbf = claims(("h5i_operator", 5), "h5i-pay/reconcile:read");
    nbf["nbf"] = (now + 600).into();
    for t in [refused(bad), refused(aud), refused(old), refused(nbf), rs256(&rsa, "nope", &claims(("h5i_operator", 5), ""))] {
        assert_eq!(api.post("/v1/reconcile", &t, json!({})).await.0, 401, "{t}");
    }
    let (h, _, s) = {
        let mut it = op.split('.');
        (it.next().unwrap().to_string(), it.next().unwrap().to_string(), it.next().unwrap().to_string())
    };
    let wider = b64(&serde_json::to_vec(&claims(("h5i_operator", 5), "operator:*")).unwrap());
    assert_eq!(api.post("/v1/recover", &format!("{h}.{wider}.{s}"), json!({})).await.0, 401);
    let hs = jwt(json!({"alg": "HS256", "kid": "r1"}), &claims(("h5i_operator", 5), "operator:*"), |m| {
        // HMAC with the RSA public key as the "secret": the classic confusion attack.
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, rsa.public().as_ref());
        ring::hmac::sign(&key, m).as_ref().to_vec()
    });
    let none = jwt(json!({"alg": "none", "kid": "r1"}), &claims(("h5i_operator", 5), "operator:*"), |_| Vec::new());
    for t in [hs, none] {
        assert_eq!(api.post("/v1/recover", &t, json!({})).await.0, 401);
    }
    // Key rotation: the IdP publishes a new key; tokens signed with it work
    // once the JWKS is fetched again for the unknown kid.
    let rsa2 = rsa_key();
    let t2 = rs256(&rsa2, "r2", &claims(("h5i_operator", 5), "h5i-pay/reconcile:read"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(api.post("/v1/reconcile", &t2, json!({})).await.0, 401);
    idp.jwks.lock().unwrap()["keys"].as_array_mut().unwrap().push(rsa_jwk("r2", &rsa2));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(api.post("/v1/reconcile", &t2, json!({})).await.0, 200);
    // IdP tokens are revoked like the others: by jti, by actor.
    let admin = keys().issue_operator(org, 1, 600).unwrap();
    let jti = claims(("h5i_operator", 5), "")["jti"].clone();
    let t3 = rs256(&rsa, "r1", &{
        let mut c = claims(("h5i_operator", 5), "h5i-pay/reconcile:read");
        c["jti"] = jti.clone();
        c
    });
    assert_eq!(api.post("/v1/reconcile", &t3, json!({})).await.0, 200);
    assert_eq!(api.post("/v1/tokens/revoke", &admin, json!({"jti": jti})).await.0, 200);
    assert_eq!(api.post("/v1/reconcile", &t3, json!({})).await.0, 401);
    assert_eq!(api.post("/v1/reconcile", &op, json!({})).await.0, 200);
    assert_eq!(api.post("/v1/tokens/revoke", &admin, json!({"actor": {"kind": "operator", "id": 5}})).await.0, 200);
    assert_eq!(api.post("/v1/reconcile", &op, json!({})).await.0, 401);
    let _ = Scopes::default();
}
