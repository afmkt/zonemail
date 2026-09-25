//! JWT bearer authentication with config-based role authorization.
//!
//! When `auth.enabled = false` the [`guard::AuthGuard`] is a no-op; the API
//! behaves exactly as before. When enabled, a `Bearer` token presented on
//! `Authorization` is verified — signature, algorithm allow-list, expiry, and
//! optional issuer/audience — and its roles are matched against the configured
//! policy.
//!
//! Two concerns are kept separate:
//!
//! * **Authentication** (`keys.rs`) — is the token valid? The token is the source
//!   of truth for *who* is calling; no user is written to the database and no key
//!   material is changed through the API.
//! * **Authorization** (`policy.rs`) — with a fixed, config-based role model, does
//!   the authenticated principal's `role` claim(s) permit this `(verb, resource)`?
//!   Unknown roles are denied (default-deny).
//!
//! The [`AuthService`] is cheap to clone (`Arc`-backed key material) and is held
//! on [`crate::AppState`].

pub mod guard;
pub mod keys;
pub mod policy;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use salvo::prelude::*;
use salvo::Request;

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::Value as JsonValue;

use crate::api::ApiProblem;
use crate::auth::policy::RouteTarget;
use crate::config::{AuthConfig, KeysKind};

pub use guard::AuthGuard;

// ===========================================================================
// Errors
// ===========================================================================

/// A failure producing an `Authentication` rejection (HTTP 401).
#[derive(thiserror::Error, Debug)]
pub enum AuthError {
        /// The token's declared algorithm is not in the configured allow-list, or it
        /// is the confusing `alg=none`.
      #[error("algorithm {0} is not permitted")]
    AlgorithmDisallowed(String),
        /// The `kid` in the token matches no key in the key set.
      #[error("token key id matched no verification key")]
    UnknownKid,
        /// A remote key source could not be reached and no cached key was available.
      #[error("verification key source is unavailable")]
    SourceUnavailable,
        /// A token-header parse, `decode`, signature, issuer, audience or expiry
      /// failure. All `jsonwebtoken` parse/verify errors funnel through here.
      #[error("token verification failed: {0}")]
    Verify(#[from] jsonwebtoken::errors::Error),
}



/// A build-time configuration error (surfaced when the service would start).
#[derive(thiserror::Error, Debug)]
pub enum AuthBuildError {
     #[error("`auth.enabled = true` but no verification key material is configured")]
    NoKeyMaterial,
     #[error("a configured algorithm `{0}` is unknown (expected e.g. `HS256`, `RS256`, `ES256`)")]
    UnknownAlgorithm(String),
     #[error("could not parse the inline JWKS set: {0}")]
    JwksParse(String),
     #[error(
         "the `secret` source did not resolve to bytes (check `secret`/`secret_b64`/`secret_env` and the environment)"
         )]
    NoSecret,
}

// ===========================================================================
// Verified principal
// ===========================================================================

/// The authenticated principal, extracted from a successfully verified token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifiedClaims {
     /// `sub` (or the configured `subject_claim`).
    pub subject: Option<String>,
     /// Collected `role` claim values (possibly from several configured fields).
    pub roles: Vec<String>,
}

impl VerifiedClaims {
     /// Extract `subject`/`roles` from the decoded claims using the configured
     /// field names. A role claim may be a string, an array of strings, or an
     /// object; dotted field names (e.g. `metadata.roles`) are walked by split on
     /// `.`.
    fn extract(raw: &JsonValue, subject_claim: &str, role_claims: &[String]) -> Self {
        let subject = field_str(raw, subject_claim);
        let mut roles: BTreeSet<String> = BTreeSet::new();
        for field in role_claims {
            collect_roles(raw, field, &mut roles);
             }
        VerifiedClaims {
            subject,
            roles: roles.into_iter().collect(),
             }
         }
}

/// Read `path` (dotted) as a single string, if present.
fn field_str(value: &JsonValue, path: &str) -> Option<String> {
    walk(value, path).and_then(|v| v.as_str().map(str::to_string))
     }

/// Append every string under `path` (a scalar or an array of scalars) to `out`.
fn collect_roles(value: &JsonValue, path: &str, out: &mut BTreeSet<String>) {
    match walk(value, path) {
        None => {}
          Some(s) => {
          if let Some(s) = s.as_str() {
              out.insert(s.to_string());
                  }
          if let Some(arr) = s.as_array() {
              for item in arr {
                  if let Some(x) = item.as_str() {
                      out.insert(x.to_string());
                       }
                 }
                  }
               }
          }
      }

/// Walk a dotted path through a JSON object (`a.b.c`); returns `None` at the first
/// non-object hop or an absent key.
fn walk<'a>(root: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    path.split('.').try_fold(root, |cur, seg| cur.get(seg))
     }

// ===========================================================================
// Service
// ===========================================================================

/// A decision the guard takes after running an inbound request through the
/// authenticator + authorizer.
#[derive(Debug)]
pub enum Decision {
     /// Continue handling. `Some(claims)` means the request was authenticated and
     /// its principal should be injected for downstream handlers; `None` means the
     /// route is public or authentication is disabled.
    Continue(Option<VerifiedClaims>),
     /// Reject with HTTP 401 (no / unreadable / unverified token).
    Unauthenticated(&'static str),
     /// Reject with HTTP 403 (authenticated but a role does not permit the target).
    Forbidden(&'static str),
}

/// The authenticator + authorizer. Cheap to clone (`Arc`-backed key material);
/// held on [`crate::AppState`].
#[derive(Clone)]
pub struct AuthService {
    enabled: bool,
        // Optional issuer/audience constraints applied to every decoded token.
    issuer: Option<String>,
    audience: Option<String>,
        // Grace (seconds) for token clock skew (`leeway`).
    leeway: u64,
        // The algorithm allow-list; the token's `alg` must be a member.
    algorithms: Vec<Algorithm>,
        // Configurable claim field names.
    subject_claim: String,
    role_claims: Vec<String>,
        // The fixed, config-based role→route authorization policy.
    policy: policy::AuthPolicy,
        // How a token's signature is checked.
    key_material: keys::KeyMaterial,
        // When a remote key source is unreachable, deny (`true`) or open (`false`).
    fail_closed: bool,
}

impl AuthService {
     /// An always-passing no-op service used when authentication is not configured.
     /// It carries a dummy key and `enabled = false` so the guard early-returns.
    pub fn disabled() -> Arc<Self> {
        Arc::new(AuthService {
            enabled: false,
            issuer: None,
            audience: None,
            leeway: 60,
            algorithms: vec![Algorithm::HS256],
            subject_claim: "sub".to_string(),
            role_claims: vec!["role".to_string()],
            policy: policy::AuthPolicy::build(
                   &std::collections::HashMap::new(),
                   &crate::config::AuthConfig::default_public()),
            key_material: keys::KeyMaterial::Secret(Arc::new(empty_key())),
            fail_closed: true,
             })
         }

      /// Build an enabled service from a resolved [`AuthConfig`]. Returns an
       /// [`AuthBuildError`] if the config could not yield a usable key material.
    pub fn build(cfg: &AuthConfig) -> Result<Arc<Self>, AuthBuildError> {        let algorithms = build_algorithms(&cfg.keys)?;
        let key_material = build_key_material(cfg, &algorithms)?;

           // Spawn a background refresh for remote/issuer key sets. For an inline
           // JWKS set this is a no-op.
        if let keys::KeyMaterial::Set(ks) = &key_material {
            if ks.is_remote() {
                ks.spawn_refresh();
                 }
            }

        let service = AuthService {
            enabled: cfg.enabled,
            issuer: cfg.issuer.clone(),
            audience: cfg.audience.clone(),
            leeway: cfg.clock_skew_secs,
            algorithms,
            subject_claim: cfg.subject_claim.clone(),
            role_claims: cfg.role_claims.clone(),
            policy: policy::AuthPolicy::build(&cfg.roles, &cfg.public),
            key_material,
            fail_closed: cfg.keys.fail_closed,
             };

        Ok(Arc::new(service))
         }

      /// Is authentication actually enforced? `false` ⇒ no-op.
    pub fn is_enabled(&self) -> bool {
        self.enabled
         }

      /// The decision for one inbound request. Called by the guard.
    pub async fn enforce(&self, req: &Request) -> Decision {
         // Fast path: authentication is off ⇒ pass everything through.
        if !self.enabled {
            return Decision::Continue(None);
            }

          // Resolve the target. A route the policy never names (a static file, or a
          // path no handler owns) is not governed here: let the router handle it.
        let Some(target) = policy::route_target(req.uri().path(), req.method()) else {
            return Decision::Continue(None);
             };

           // Public routes are exempt from authentication entirely.
        if self.policy.is_public(&target.resource) {
            return Decision::Continue(None);
             }

         // Governed, non-public route: a Bearer token is required.
        let token = match extract_bearer(req) {
            None => return Decision::Unauthenticated("missing bearer token on `Authorization`"),
             Some(t) => t,
            };

        match self.verify(&token).await {
            Ok(claims) => {
                if self.policy.allows(&claims.roles, &target) {
                    Decision::Continue(Some(claims))
                     } else {
                     Decision::Forbidden("authenticated roles do not permit this route")
                     }
                 }
              Err(AuthError::SourceUnavailable) =>
                   // Fail-open vs. fail-closed on a source outage.
                   if self.fail_closed {
                      Decision::Unauthenticated("verification key source is unavailable")
                       } else {
                      tracing::warn!("auth key source unavailable; failing open");
                      Decision::Continue(None)
                       },
                Err(_) => Decision::Unauthenticated("invalid or expired token"),
                }
            }

      /// Parse the token header and reject an out-of-allow-list algorithm before
       /// touching any key (guards against `alg`-confusion attacks), then decode
       /// the token against the resolved key.
    pub async fn verify(&self, token: &str) -> Result<VerifiedClaims, AuthError> {
        let header = jsonwebtoken::decode_header(token)?;
        if !self.algorithms.contains(&header.alg) {
            return Err(AuthError::AlgorithmDisallowed(format!("{:?}", header.alg)));
             }

             // Resolve the signing key and decode the (already header-checked) token
             // against it into a generic claims map, then read the configured fields.
        let key: DecodingKey = match &self.key_material {
            keys::KeyMaterial::Secret(k) => DecodingKey::clone(k),
              keys::KeyMaterial::Set(ks) => ks.resolve(&header).await?,
                };

        let mut validation = Validation::new(header.alg);
        validation.algorithms = self.algorithms.clone();
        validation.leeway = self.leeway;
        if let Some(iss) = &self.issuer {
            let iss_owned = iss.to_string();
            validation.set_issuer(&[iss_owned]);
             }
        if let Some(aud) = &self.audience {
            let aud_owned = aud.to_string();
            validation.set_audience(&[aud_owned]);
             }

        let data = jsonwebtoken::decode(token, &key, &validation)?;
        Ok(VerifiedClaims::extract(
             &data.claims,
             &self.subject_claim,
             &self.role_claims,
             ))
         }

       /// Does a set of roles permit the given `(verb, resource)` target? Public
       /// resource paths are always allowed; this is the authorization step that
       /// runs after authentication succeeds.
    pub fn authorize(&self, roles: &[String], target: &RouteTarget) -> bool {
        self.policy.allows(roles, target)
          }

        /// Inject the authenticated principal into the depot so downstream
        /// handlers may read it. No-op when there are no claims to inject.
    pub fn inject_principal(&self, depot: &mut Depot, claims: VerifiedClaims) {
        #[allow(deprecated)]
        depot.inject(claims);
          }
}

// ===========================================================================
// Construction helpers
// ===========================================================================

fn empty_key() -> DecodingKey {
    DecodingKey::from_secret("z".as_bytes())
      }

/// Resolve the algorithm allow-list from config, falling back to the default
/// algorithm for the selected key source.
fn build_algorithms(keys: &crate::config::KeyConfig) -> Result<Vec<Algorithm>, AuthBuildError> {
    // Materialize the names first (owned) so both branches share one lifetime.
    let names: Vec<String> = if keys.algorithms.is_empty() {
        keys.default_algorithms()
        } else {
        keys.algorithms.clone()
             };
    names
          .iter()
          .map(|n| parse_algorithm(n).ok_or_else(|| AuthBuildError::UnknownAlgorithm(n.clone())))
          .collect()
      }

/// Parse an algorithm name (`"HS256"`, `"RS256"`, ...) case-insensitively.
fn parse_algorithm(name: &str) -> Option<Algorithm> {
    let alg = match name.to_ascii_uppercase().as_str() {
        "HS256" => Algorithm::HS256,
           "HS384" => Algorithm::HS384,
           "HS512" => Algorithm::HS512,
           "ES256" => Algorithm::ES256,
           "ES384" => Algorithm::ES384,
           "RS256" => Algorithm::RS256,
           "RS384" => Algorithm::RS384,
            "RS512" => Algorithm::RS512,
            "PS256" => Algorithm::PS256,
            "PS384" => Algorithm::PS384,
             "PS512" => Algorithm::PS512,
               "EDDSA" => Algorithm::EdDSA,
                   _ => return None,
               };
    Some(alg)
     }

/// Resolve the configured key material into a [`keys::KeyMaterial`].
fn build_key_material(
    cfg: &AuthConfig,
    _algs: &[Algorithm],
) -> Result<keys::KeyMaterial, AuthBuildError> {
    use keys::{HttpKeysFetcher, KeySet};
    let tc = &cfg.keys;
    match &tc.kind {
        KeysKind::Secret => {
            let bytes = resolve_secret_bytes(tc)?;
            Ok(keys::KeyMaterial::Secret(Arc::new(DecodingKey::from_secret(
                 &bytes,
                 ))))
               }
            KeysKind::Jwks => {
          let value = tc.jwks.as_ref().ok_or(AuthBuildError::NoKeyMaterial)?;
          let set: jsonwebtoken::jwk::JwkSet =
              serde_json::from_value(value.clone())
                   .map_err(|e| AuthBuildError::JwksParse(e.to_string()))?;
          if set.keys.is_empty() {
             return Err(AuthBuildError::NoKeyMaterial);
                   }
          Ok(keys::KeyMaterial::Set(KeySet::static_set(set)))
                  }
         KeysKind::JwksRemote => {
          let url = tc.jwks_url.clone().ok_or(AuthBuildError::NoKeyMaterial)?;
          let fetcher = HttpKeysFetcher::jwks(
               url,
               tc.janux_token.as_deref(),
               tc.janux_auth_header.as_deref(),
               tc.janux_bearer,
                );
          let ttl = Duration::from_secs(tc.cache_ttl_secs);
          Ok(keys::KeyMaterial::Set(KeySet::remote(fetcher, ttl)))
                  }
        KeysKind::Issuer => {
            let issuer = tc.issuer.clone().ok_or(AuthBuildError::NoKeyMaterial)?;
            let fetcher = HttpKeysFetcher::issuer(
                 issuer,
                 tc.discovery_path.as_deref(),
                 tc.jwks_path.as_deref(),
                 tc.janux_token.as_deref(),
                 tc.janux_auth_header.as_deref(),
                 tc.janux_bearer,
                  );
            let ttl = Duration::from_secs(tc.cache_ttl_secs);
            Ok(keys::KeyMaterial::Set(KeySet::remote(fetcher, ttl)))
              }
          }
}

/// Resolve the symmetric secret bytes from `secret_env` → `secret_b64` → `secret`.
/// The first non-empty wins; `secret_b64` is base64-decoded.
fn resolve_secret_bytes(keys: &crate::config::KeyConfig) -> Result<Vec<u8>, AuthBuildError> {
     // `secret_env` may be an environment variable name or an inline value.
    if let Some(env) = &keys.secret_env {
        let raw = std::env::var(env).unwrap_or_else(|_| env.clone());
        let v = raw.trim();
        if !v.is_empty() {
            return Ok(v.as_bytes().to_vec());
              }
          }

        // `secret_b64` is decoded from base64 (standard, then URL-safe).
    if let Some(b64) = &keys.secret_b64 {
        use base64::engine::general_purpose::{STANDARD, URL_SAFE};
        use base64::Engine as _;
        if let Ok(bytes) = STANDARD.decode(b64.trim()) {
            return Ok(bytes);
              }
        if let Ok(bytes) = URL_SAFE.decode(b64.trim()) {
            return Ok(bytes);
              }
        return Err(AuthBuildError::NoSecret);
          }

       // Plain `secret`.
    if let Some(s) = &keys.secret {
        return Ok(s.as_bytes().to_vec());
           }

    Err(AuthBuildError::NoSecret)
     }

// ===========================================================================
// Bearer extraction
// ===========================================================================

/// Extract a `Bearer <token>` value from the `Authorization` header (case-insensitive
/// scheme). Returns `None` if the header is absent, malformed, or carries no token.
fn extract_bearer(req: &Request) -> Option<String> {
    let header = req.headers().get("authorization")?;
    let value = header.to_str().ok()?;
      // A compact JWT never contains whitespace, so a 2-part split with a
      // case-insensitive `bearer` scheme is exactly what we expect.
    let parts: Vec<&str> = value.trim().split_whitespace().collect();
    match parts.as_slice() {
         [scheme, token] if scheme.eq_ignore_ascii_case("bearer") => Some((*token).to_string()),
           _ => None,
         }
    }

// ===========================================================================
// Rendering (used by the guard)
// ===========================================================================

/// Write a problem document as the response body with the given status. The
/// guard calls this on rejection; on `Continue` it returns without rendering so
/// the downstream handler runs.
/// Render an RFC-7807 problem document. Mirrors the fire-and-forget `res.render(..)`
         /// used by the `api.rs` error handlers (`render` returns `&mut Self`, no `.await`).
 pub(crate) fn write_rejection(res: &mut Response, status: StatusCode, detail: &str) {
       let problem = ApiProblem {
           status: status.as_u16(),
            r#type: "authentication_error".to_string(),
            detail: Some(detail.to_string()),
         };
       res.status_code(status);
       res.render(Json(problem));
    }

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

        /// A minimal, always-passing policy for verification-only tests.
    fn permissive_config() -> AuthConfig {
        let mut cfg = AuthConfig::default();
        cfg.enabled = true;
        cfg.issuer = Some("https://janux.example/auth".to_string());
        cfg.audience = Some("zonemail-api".to_string());
        cfg.clock_skew_secs = 30;
        cfg.subject_claim = "sub".to_string();
        cfg.role_claims = vec!["role".to_string(), "roles".to_string()];
        cfg.public = vec!["health".to_string(), "doc".to_string()];
        cfg.keys.kind = KeysKind::Secret;
        cfg.roles.insert(
             "admin".into(),
              crate::config::RoleConfig { grants: vec!["*".to_string()], extends: vec![] },
              );
        cfg.roles.insert(
             "read".into(),
              crate::config::RoleConfig { grants: vec!["mailboxes:read".to_string()], extends: vec![] },
              );
        cfg
         }

        /// A real HS256 token signed with `secret`, carrying the given JSON claims
         /// (optional `iss`/`aud`/`exp`/`kid`).
     fn hs256(secret: &[u8], claims: &str, iss: Option<&str>, aud: Option<&str>, kid: Option<&str>) -> String {
        use jsonwebtoken::{Algorithm, EncodingKey, Header as JwtHeader};
        let mut header = JwtHeader::new(Algorithm::HS256);
        if let Some(kid) = kid {
            header.kid = Some(kid.to_string());
              }
        let mut map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(claims).unwrap();
        if let Some(iss) = iss {
            map.insert("iss".into(), serde_json::Value::String(iss.into()));
              }
        if let Some(aud) = aud {
            map.insert("aud".into(), serde_json::Value::String(aud.into()));
          }
        jsonwebtoken::encode(&header, &map, &EncodingKey::from_secret(secret)).unwrap()
          }

        #[test]
    fn parse_algorithm_is_case_insensitive_including_eddsa() {
              // Regression guard: the match is over `to_ascii_uppercase()`, so
                // every arm (incl. the formerly broken "EdDSA") must be uppercase.
        use jsonwebtoken::Algorithm;
        assert_eq!(super::parse_algorithm("hs256"), Some(Algorithm::HS256));
        assert_eq!(super::parse_algorithm("RS256"), Some(Algorithm::RS256));
        assert_eq!(super::parse_algorithm("eddsa"), Some(Algorithm::EdDSA));
        assert_eq!(super::parse_algorithm("EDDSA"), Some(Algorithm::EdDSA));
        assert_eq!(super::parse_algorithm("nonsense"), None);
        }

      #[tokio::test]
     async fn verification_roundtrip_with_subject_and_roles() {
        let mut cfg = permissive_config();
        cfg.keys.secret = Some("0123456789abcdefsecretstuff!".to_string());
        let svc = AuthService::build(&cfg).unwrap();

               // A token matching the configured `iss`/`aud`, with two role fields.
         let claims = r#"{"sub":"alice","role":"admin","roles":["ops"],
                       "exp":9999999999}"#;
        let token = hs256(b"0123456789abcdefsecretstuff!", claims,
                 Some("https://janux.example/auth"),
                 Some("zonemail-api"),
                 None);
        let verified = svc.verify(&token).await.expect("verify must succeed");
        assert_eq!(verified.subject.as_deref(), Some("alice"));
        assert_eq!(verified.roles, vec!["admin", "ops"]);
          }

       #[tokio::test]
     async fn wrong_alg_is_rejected() {
        let mut cfg = permissive_config();
        cfg.keys.secret = Some("secret-value!!".to_string());
        cfg.keys.algorithms = vec!["HS512".to_string()];
        let svc = AuthService::build(&cfg).unwrap();

               // An HS256 token is not in an `HS512`-only allow-list.
         let token = hs256(b"secret-value!!", r#"{"sub":"x"}"#, None, None, None);
        let err = svc.verify(&token).await.unwrap_err();
        assert!(
            matches!(err, AuthError::AlgorithmDisallowed(_)),
              "got {err:?}"
             );
          }

       #[tokio::test]
     async fn mismatched_secret_is_rejected() {
        let mut cfg = permissive_config();
        cfg.keys.secret = Some("right-secret-value".to_string());
        let svc = AuthService::build(&cfg).unwrap();

            // A token signed with the wrong secret.
         let token = hs256(b"wrong-secret-value!!", r#"{"sub":"x"}"#, None, None, None);
        let err = svc.verify(&token).await.unwrap_err();
        assert!(matches!(err, AuthError::Verify(_)), "got {err:?}");
           }

       #[tokio::test]
     async fn expired_token_is_rejected() {
        let mut cfg = permissive_config();
        cfg.keys.secret = Some("s3cr3t-value!!".to_string());
        let svc = AuthService::build(&cfg).unwrap();

         // `exp` in the past by far more than the leeway.
         let claims = r#"{"sub":"x","role":"admin","exp":0}"#;
        let token = hs256(b"s3cr3t-value!!", claims, None, None, None);
        assert!(svc.verify(&token).await.is_err());
           }

       #[tokio::test]
     async fn role_policy_denies_unauthorized_target() {
        let mut cfg = permissive_config();
        cfg.keys.secret = Some("secret-value!!".to_string());
        let svc = AuthService::build(&cfg).unwrap();

               // A `read` role may read `mailboxes` but not `mail`.
            let ok_target = policy::route_target("/mailboxes", &salvo::http::Method::GET).unwrap();
        assert!(svc.authorize(&vec!["read".to_string()], &ok_target));
        let denied_target = policy::route_target("/mail", &salvo::http::Method::GET).unwrap();
        assert!(!svc.authorize(&vec!["read".to_string()], &denied_target));
          }

           #[test]
    fn bearer_extraction_and_disabled_pass_through() {
            // A request with no `Authorization` header yields no token; a disabled
        // service enforces nothing. These are exercised end-to-end via the guard in
        // the integration tests; here we just assert the disabled service builds.
        let _ = AuthService::disabled();
        let cfg = std::collections::HashMap::new();
        let _ = policy::AuthPolicy::build(&cfg, &[]);
        }

        /// Drives `AuthService::enforce` directly with hand-built requests — the
       /// authoritative decision matrix without any router reuse concerns.
      #[tokio::test]
    async fn enforce_decision_matrix() {
        use salvo::http::{HeaderName, HeaderValue, Method, Request};
        use http::Uri;
        use std::str::FromStr;
        use salvo::http::header::AUTHORIZATION;

        let mut cfg = permissive_config();
        cfg.keys.secret = Some("s3cr3t-value!!".to_string());
        let svc = AuthService::build(&cfg).unwrap();

        const ISS: &str = "https://janux.example/auth";
        const AUD: &str = "zonemail-api";

               // Build a GET request to `path`, optionally carrying a Bearer token.
        let req = |path: &str, token: Option<&str>| {
            let mut r = Request::default();
            r.set_uri(Uri::from_str(path).unwrap());
            if let Some(t) = token {
                r.headers_mut()
                        .insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {t}")).unwrap());
                  }
            r
                };

               // 1. No token on a governed route -> Unauthenticated (401).
        let d = svc.enforce(&req("/domains", None)).await;
        assert!(matches!(d, Decision::Unauthenticated(_)), "no token must 401");

               // 2. The public health probe with no token -> Continue(None).
        let d = svc.enforce(&req("/health", None)).await;
        assert!(matches!(d, Decision::Continue(None)), "public must bypass");

               // 3. A valid admin token (wildcard grant) -> Continue(claims).
        let token = hs256(b"s3cr3t-value!!", r#"{"sub":"a","role":"admin","exp":32503680000}"#,
                 Some(ISS), Some(AUD), None);
        let d = svc.enforce(&req("/domains", Some(&token))).await;
        assert!(matches!(d, Decision::Continue(Some(_))), "admin must pass");

               // 4. The `read` role can reach mailboxes but not domains -> Forbidden (403).
        let token = hs256(b"s3cr3t-value!!", r#"{"sub":"r","role":"read","exp":32503680000}"#,
                 Some(ISS), Some(AUD), None);
        let d = svc.enforce(&req("/domains", Some(&token))).await;
        assert!(matches!(d, Decision::Forbidden(_)), "reader must 403 on /domains");
        let d = svc.enforce(&req("/mailboxes", Some(&token))).await;
        assert!(matches!(d, Decision::Continue(_)), "reader may read mailboxes");

               // 5. An expired token is rejected.
        let token = hs256(b"s3cr3t-value!!", r#"{"sub":"x","role":"admin","exp":1}"#,
                 Some(ISS), Some(AUD), None);
        let d = svc.enforce(&req("/domains", Some(&token))).await;
        assert!(matches!(d, Decision::Unauthenticated(_)), "expired must 401");

               // 6. A disabled service enforces nothing, even with no token.
        let d = AuthService::disabled().enforce(&req("/domains", None)).await;
        assert!(matches!(d, Decision::Continue(_)), "disabled must pass-through");

               // 7. A wrong secret (invalid signature) is rejected -> Unauthenticated.
        let token = hs256(b"some-other-secret-value!!", r#"{"sub":"x","role":"admin","exp":32503680000}"#,
                 Some(ISS), Some(AUD), None);
        let d = svc.enforce(&req("/domains", Some(&token))).await;
        assert!(matches!(d, Decision::Unauthenticated(_)), "bad sig must 401");

        // Silence unused-import lints if a variant above omits one arm.
        let _ = (Method::GET, HeaderName::from_static("authorization"));
     }

      

}
