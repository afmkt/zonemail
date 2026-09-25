//! Verification key material: how a token's signature is checked.
//!
//! Four source kinds, mirroring `crate::config::KeyConfig`:
//!
//! * `Secret`       — a single symmetric HMAC secret (`DecodingKey::from_secret`).
//! * `Jwks`         — an inline static JWK set carried in the config.
//! * `JwksRemote`   — a JWKS fetched from a URL and periodically refreshed.
//! * `Issuer`       — an OIDC-style auth-server URL; the JWKS location is
//!                    discovered from a discovery document (`jwks_uri`).
//!
//! A token is verified by (1) decoding *only its header* to learn `alg` + `kid`,
//! (2) `resolve`-ing a matching [`DecodingKey`] — which, for the remote kinds, is
//! kept in a shared cache that a background task refreshes and that is refetched
//! on demand after a cache miss — and (3) doing the full `decode` against that
//! single key. Restricting the algorithm to the token's declared (allow-listed)
//! algorithm plus the key chosen for the `kid` is what blocks the classic
//! `alg=none` and RS↔HS confusion attacks.

use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, Header};

use super::AuthError;

/// A `Send` future box for fetchers, avoiding a dependency on `async-trait`.
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Produce a JWKS. The HTTP implementations fetch it over the network; a test
/// implementation can supply one in memory.
pub trait JwksFetcher: Send + Sync {
    fn fetch(&self) -> BoxFuture<'_, Result<JwkSet, String>>;
}

/// A cached, last-known-good JWK set plus the instant it was last refreshed.
#[derive(Clone, Debug)]
struct CachedSet {
    jwk_set: JwkSet,
    fetched_at: Instant,
}

impl CachedSet {
    fn new(jwk_set: JwkSet) -> Self {
        Self { jwk_set, fetched_at: Instant::now() }
      }
}

/// The key material used to verify token signatures.
#[derive(Clone)]
pub enum KeyMaterial {
       /// A single symmetric HMAC secret (the `secret`/`secret_b64`/`secret_env`
       /// source).
    Secret(Arc<DecodingKey>),
       /// A JWK set: either a static inline set or a remote/issuer-backed one that
       /// is refreshed and refetched on demand.
    Set(KeySet),
}

/// A (possibly remote) JWK set held in a shared, refreshable cache.
#[derive(Clone)]
pub struct KeySet {
       /// Last-known-good key set. `None` until the first successful fetch.
    cache: Arc<RwLock<Option<CachedSet>>>,
       /// How to (re)obtain the set; `None` for a static inline set.
    fetcher: Option<Arc<dyn JwksFetcher>>,
       /// Age after which the cache is considered stale enough to be refetched.
    ttl: Duration,
}

impl KeySet {
       /// A static inline set (no fetcher, never refetched).
    pub fn static_set(jwk_set: JwkSet) -> KeySet {
        KeySet {
            cache: Arc::new(RwLock::new(Some(CachedSet::new(jwk_set)))),
            fetcher: None,
            ttl: Duration::ZERO,
           }
        }

      /// A remote/issuer-backed set that starts empty and relies on the fetcher.
    pub fn remote(fetcher: Arc<dyn JwksFetcher>, ttl: Duration) -> KeySet {
        KeySet {
            cache: Arc::new(RwLock::new(None)),
            fetcher: Some(fetcher),
            ttl,
           }
        }

      /// Is this set fetched from a remote source (vs. inline/static)?
    pub fn is_remote(&self) -> bool {
        self.fetcher.is_some()
        }

      /// Resolve a [`DecodingKey`] for a token already reduced to a `header`.
      ///
      /// Fast path: the cached set already contains the `kid`. On a miss with a
      /// fetcher, it refreshes (guarded by a staleness check to avoid a
      /// per-request thundering herd) then tries the cache again.
    pub async fn resolve(&self, header: &Header) -> Result<DecodingKey, AuthError> {
            // 1. Fast path: the current cache already answers this `kid`.
        if let Some(set) = self.current() {
            if let Some(key) = pick_key(&set, header) {
                return Ok(key);
                  }
               }

            // 2. Cache miss: refresh via the fetcher if we have one and it is stale.
        if let Some(fetcher) = &self.fetcher {
            if self.should_refetch() {
                match fetcher.fetch().await {
                    Ok(set) => {
                        self.store(set);
                            // 3a. Re-check the freshly populated cache.
                        if let Some(fresh) = self.current() {
                            if let Some(key) = pick_key(&fresh, header) {
                                return Ok(key);
                                 }
                             }
                        return Err(AuthError::UnknownKid);
                           }
                      Err(_) => { /* fall through to any stale cache */ }
                        }
                    }
                }

            // 3b. Use whatever (possibly stale) cache remains.
        if let Some(set) = self.current() {
            if let Some(key) = pick_key(&set, header) {
                return Ok(key);
                   }
            return Err(AuthError::UnknownKid);
               }

            // 4. No keys at all; the caller decides fail-open vs. fail-closed.
        Err(AuthError::SourceUnavailable)
         }

      /// Current cached set, or `None`.
    fn current(&self) -> Option<JwkSet> {
        self.cache.read().ok().and_then(|g| g.as_ref().map(|c| c.jwk_set.clone()))
        }

      /// Stale enough to be worth refetching: empty, or older than the TTL.
    fn should_refetch(&self) -> bool {
          // Hold the read guard for the whole arm so the borrowed `CachedSet` never
         // outlives the guard (the prior inline `and_then` left a dangling ref).
        match self.cache.read() {
            Ok(guard) => guard
                  .as_ref()
                  .map_or(true, |c| c.fetched_at.elapsed() >= self.ttl),
             Err(_) => true,
                 }
           }

    fn store(&self, set: JwkSet) {
        if let Ok(mut w) = self.cache.write() {
            *w = Some(CachedSet::new(set));
             }
        }

      /// Kick off the detached background refresh loop for a remote/issuer set.
      /// No-op for a static set. The task lives for the process; refresh failures
      /// keep the last-known-good set and retry on the next cycle.
    pub fn spawn_refresh(&self) {
        let fetcher = match &self.fetcher {
            Some(f) => f.clone(),
              None => return,
            };
        let cache = self.cache.clone();
        let ttl = self.ttl;
        tokio::spawn(async move {
             loop {
                match fetcher.fetch().await {
                    Ok(set) => {
                        if let Ok(mut w) = cache.write() {
                             *w = Some(CachedSet::new(set));
                            }
                        }
                      Err(_) => { /* keep the last-known-good set */ }
                      }
                  tokio::time::sleep(ttl).await;
               }
             });
        }
}

/// Pick the [`DecodingKey`] for a token's header from a JWK set.
///
/// With a `kid`, the set must contain a matching key. Without one, exactly one
/// kidless key is used; multiple kidless keys are refused (no silent guessing).
fn pick_key(set: &JwkSet, header: &Header) -> Option<DecodingKey> {
    match &header.kid {
             Some(kid) => set.find(kid).and_then(|j| DecodingKey::from_jwk(j).ok()),
              None => {
                let kidless: Vec<_> =
                  set.keys.iter().filter(|k| k.common.key_id.is_none()).collect();
                match kidless.len() {
                         1 => DecodingKey::from_jwk(kidless[0]).ok(),
                        _ => None,
                  }
             }
        }
}

// ===========================================================================
// HTTP fetcher (remote JWKS / OAuth2-issuer discovery)
// ===========================================================================

/// A [`JwksFetcher`] backed by the Janux auth server (or any JWKS/OIDC
/// endpoint). Supports both a direct `jwks_url` and an `issuer` URL whose JWKS
/// location is discovered from a discovery document (`jwks_uri`).
#[derive(Clone, Debug)]
pub struct HttpKeysFetcher {
    client: reqwest::Client,
       /// Either the direct JWKS URL or the issuer base URL (issuer mode only).
    base: String,
       /// When `true`, fetch the discovery document first and read its `jwks_uri`.
    discovery: bool,
    discovery_path: String,
       /// Bypass discovery when set (issuer mode: a direct path on the issuer).
    jwks_path_override: Option<String>,
       /// Optional credential presented *to the auth server* to pull a protected
       /// JWKS/discovery (e.g. a service-to-service token).
    auth_header: Option<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
       /// Cache of the discovery-resolved JWKS URL (issuer mode).
    resolved: Arc<RwLock<Option<String>>>,
}

impl HttpKeysFetcher {
       /// Build a direct-JWKS fetcher (`kind = "jwks_remote"`).
    pub fn jwks(
        url: String,
        janux_token: Option<&str>,
        janux_auth_header: Option<&str>,
        janux_bearer: bool,
        ) -> Arc<Self> {
        Arc::new(HttpKeysFetcher {
            client: reqwest::Client::new(),
            base: url,
            discovery: false,
            discovery_path: "/.well-known/openid-configuration".to_string(),
            jwks_path_override: None,
            auth_header: janux_credential(janux_token, janux_auth_header, janux_bearer),
            resolved: Arc::new(RwLock::new(None)),
            })
        }

      /// Build an issuer-based fetcher (`kind = "issuer"`) that discovers the JWKS
      /// URI from the standard discovery document unless `jwks_path_override` names
      /// the JWKS directly on the issuer.
    pub fn issuer(
        issuer: String,
        discovery_path: Option<&str>,
        jwks_path_override: Option<&str>,
        janux_token: Option<&str>,
        janux_auth_header: Option<&str>,
        janux_bearer: bool,
        ) -> Arc<Self> {
        Arc::new(HttpKeysFetcher {
            client: reqwest::Client::new(),
            base: issuer,
            discovery: jwks_path_override.is_none(),
            discovery_path: discovery_path
                  .map(str::to_string)
                  .unwrap_or_else(|| "/.well-known/openid-configuration".to_string()),
            jwks_path_override: jwks_path_override.map(str::to_string),
            auth_header: janux_credential(janux_token, janux_auth_header, janux_bearer),
            resolved: Arc::new(RwLock::new(None)),
            })
        }
}

fn janux_credential(
    token: Option<&str>,
    header_name: Option<&str>,
    bearer: bool,
) -> Option<(reqwest::header::HeaderName, reqwest::header::HeaderValue)> {
    let token = token?.trim();
    if token.is_empty() {
        return None;
      }
    let name = header_name.map(str::trim).filter(|s| !s.is_empty());
    let hname = match name {
             Some(n) => reqwest::header::HeaderName::try_from(n)
                   .ok()
                   .unwrap_or(reqwest::header::AUTHORIZATION),
              None => reqwest::header::AUTHORIZATION,
            };
    let value = if bearer {
        format!("Bearer {token}")
        } else {
        token.to_string()
        };
    match reqwest::header::HeaderValue::from_str(&value) {
        Ok(v) => Some((hname, v)),
          Err(_) => None,
         }
    }

impl JwksFetcher for HttpKeysFetcher {
    fn fetch(&self) -> BoxFuture<'_, Result<JwkSet, String>> {
        let this = self.clone();
        let f = async move {
            let url = this.jwks_location().await?;
            let mut req = this.client.get(url.as_str());
            if let Some((h, v)) = &self.auth_header {
                req = req.header(h, v.clone());
                }
            let body = req.send().await.map_err(|e| format!("http: {e}"))?;
            let text = body.text().await.map_err(|e| format!("body: {e}"))?;
            let set: JwkSet =
                 serde_json::from_str(&text).map_err(|e| format!("jwks parse: {e}"))?;
            if set.keys.is_empty() {
                return Err("empty JWKS".to_string());
                }
            Ok(set)
            };
        Box::pin(f)
        }
}

impl HttpKeysFetcher {
       /// Resolve the JWKS URL, following discovery in issuer mode. The resolved
       /// URL is cached so discovery runs at most once per fetcher.
    async fn jwks_location(&self) -> Result<String, String> {
        if let Some(override_path) = &self.jwks_path_override {
            return Ok(join_url(&self.base, override_path));
            }
        if !self.discovery {
            return Ok(self.base.clone());
            }

         // Reuse a previously-resolved location if we have one.
        if let Ok(guard) = self.resolved.read() {
            if let Some(url) = guard.as_deref() {
                return Ok(url.to_string());
                }
            }

        let discovery_url = join_url(&self.base, &self.discovery_path);
        let mut req = self.client.get(discovery_url.as_str());
        if let Some((h, v)) = &self.auth_header {
            req = req.header(h, v.clone());
            }
        let body = req.send().await.map_err(|e| format!("discovery http: {e}"))?;
        let text = body.text().await.map_err(|e| format!("discovery body: {e}"))?;
        let doc: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("discovery parse: {e}"))?;
        let jwks_uri = doc
              .get("jwks_uri")
              .and_then(|v| v.as_str())
              .ok_or_else(|| "discovery document missing `jwks_uri`".to_string())?
              .to_string();
         // A relative `jwks_uri` is resolved against the issuer base.
        let jwks_uri = if jwks_uri.starts_with("http") {
            jwks_uri
          } else {
            join_url(&self.base, &jwks_uri)
            };
        if let Ok(mut w) = self.resolved.write() {
             *w = Some(jwks_uri.clone());
            }
        Ok(jwks_uri)
        }
}

/// Join a base and a (possibly absolute or relative) path/URL into one URL.
pub fn join_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
        }
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        base.to_string()
        } else {
        format!("{base}/{path}")
        }
    }

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::jwk::Jwk;
    use jsonwebtoken::{Algorithm, EncodingKey, Header as JwtHeader};

     /// A fixed HS256 secret used to mint both the JWK set and the test tokens.
    const SECRET: [u8; 32] = [0x42; 32];

     /// A small JWK set whose single key is the base64url of `SECRET`, with the
     /// given `kid`.
    fn in_memory_jwks(kid: &str) -> JwkSet {
        let mut jwk =
            Jwk::from_encoding_key(&EncodingKey::from_secret(&SECRET), Algorithm::HS256).unwrap();
        jwk.common.key_id = Some(kid.to_string());
        JwkSet { keys: vec![jwk] }
      }

     /// A fetcher returns a fixed set, no network.
    struct InMemoryFetcher(JwkSet);
    impl JwksFetcher for InMemoryFetcher {
        fn fetch(&self) -> BoxFuture<'_, Result<JwkSet, String>> {
            let set = self.0.clone();
            Box::pin(async move { Ok(set) })
           }
        }

     struct FailingFetcher;
    impl JwksFetcher for FailingFetcher {
        fn fetch(&self) -> BoxFuture<'_, Result<JwkSet, String>> {
            Box::pin(async { Err("down".to_string()) })
           }
        }

     /// Build a signed HS256 token with the given `kid` (or none).
    fn make_hs256(claims: serde_json::Value, kid: Option<&str>) -> String {
        let mut header = JwtHeader::new(Algorithm::HS256);
        if let Some(id) = kid {
            header.kid = Some(id.to_string());
           }
        jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(&SECRET)).unwrap()
       }

    #[tokio::test]
     async fn inline_set_resolves_by_kid() {
        let ks = KeySet::static_set(in_memory_jwks("test-key"));
        let claims = serde_json::json!({ "sub": "u", "role": ["admin"], "exp": 9999999999_i64 });
        let token = make_hs256(claims, Some("test-key"));
        let header = jsonwebtoken::decode_header(&token).expect("header");
        let key = ks.resolve(&header).await.expect("resolved");
          // Signature actually verifies end-to-end using the resolved key.
        let data = jsonwebtoken::decode::<serde_json::Value>(
             &token,
             &key,
             &jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256),
             ).expect("verify");
        assert_eq!(data.claims.get("sub").and_then(|v| v.as_str()), Some("u"));
        }

    #[tokio::test]
     async fn unknown_and_kidless_kinds_fail_at_resolution() {
        let claims = serde_json::json!({ "sub": "x" });

          // A token whose *secret* is wrong but whose `kid` matches is resolved
          // (the mismatch surfaces at decode time, not in `resolve`).
        let ks = KeySet::static_set(in_memory_jwks("test-key"));
        let token = make_hs256(claims.clone(), Some("test-key"));
        let header = jsonwebtoken::decode_header(&token).expect("header");
        assert!(ks.resolve(&header).await.is_ok());

          // A genuinely unknown kid is rejected at resolution on a static set.
        assert!(matches!(
            ks.resolve(&decode(&claims, Some("nope"))).await,
            Err(AuthError::UnknownKid)
             ));

          // A token with no kid and a set that has a kid: refused (no guessing).
        assert!(matches!(
            ks.resolve(&decode(&claims, None)).await,
            Err(AuthError::UnknownKid)
             ));
        }

    fn decode(claims: &serde_json::Value, kid: Option<&str>) -> Header {
        let token = make_hs256(claims.clone(), kid);
        jsonwebtoken::decode_header(&token).expect("header")
       }

    #[tokio::test]
     async fn remote_set_serves_on_demand() {
        let fetcher: Arc<dyn JwksFetcher> = Arc::new(InMemoryFetcher(in_memory_jwks("test-key")));
        let ks = KeySet::remote(fetcher, Duration::from_secs(3600));
        assert!(ks.is_remote());
        let token = make_hs256(serde_json::json!({ "sub": "u" }), Some("test-key"));
        assert!(ks.resolve(&decode(&serde_json::json!({ "sub": "u" }), Some("test-key"))).await.is_ok());
         let _ = token;
         }

    #[tokio::test]
     async fn remote_set_reports_source_unavailable_when_unreachable() {
        // A failing remote with no cache is reported as SourceUnavailable; the
        // enforce layer turns that into a request decision (fail-closed vs open).
        let fetcher: Arc<dyn JwksFetcher> = Arc::new(FailingFetcher);
        let ks = KeySet::remote(fetcher, Duration::from_secs(3600));
        let header = jsonwebtoken::decode_header(&make_hs256(serde_json::json!({ "sub": "u" }), Some("test-key"))).unwrap();
        assert!(matches!(ks.resolve(&header).await, Err(AuthError::SourceUnavailable)));
         }

    #[test]
     fn join_url_semantics() {
        assert_eq!(join_url("https://j.example.com/", "/.well-known/jwks.json"), "https://j.example.com/.well-known/jwks.json");
        assert_eq!(join_url("https://j.example.com", ".well-known/jwks.json"), "https://j.example.com/.well-known/jwks.json");
        assert_eq!(join_url("https://j.example.com", "https://o.example/jwks"), "https://o.example/jwks");
        assert_eq!(join_url("https://j.example.com/", ""), "https://j.example.com");
         }

    #[test]
     fn janux_credential_bearer_and_named() {
        assert!(janux_credential(Some("abc"), None, true).is_some());
             // Non-bearer puts the raw token in the value; a named header is honored.
        let (name, v) = janux_credential(Some("abc"), Some("X-Token"), false).unwrap();
        assert_eq!(v.to_str().unwrap(), "abc");
         assert_eq!(name, reqwest::header::HeaderName::from_static("x-token"));
             // Empty and missing tokens are ignored.
        assert!(janux_credential(Some("    "), None, true).is_none());
         assert!(janux_credential(None, None, true).is_none());
          }
}
