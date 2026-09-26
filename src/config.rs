use crate::db::RecordDTO;
use crate::services::BootMode;
use config::{Config as ConfigLoader, ConfigError, Environment, File};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::collections::HashMap;

/// A config-level mailbox entry. Can be a bare address string or a table that
/// additionally sets a forward target.
///
/// ```toml
/// # store-only:
/// mailboxes = ["user@example.com"]
///
/// # with forward target:
/// mailboxes = [
///     "user@example.com",
///     { id = "user2@example.com", forward_to = "user2@gmail.com" }
/// ]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MailboxEntry {
     /// `"user@example.com"` — store-only, no forward target.
     Simple(String),
     /// `{ id = "user@example.com", forward_to = "user@gmail.com" }` — with an
     /// optional external forward target. `forward_to` defaults to `None`.
     Detailed { id: String, #[serde(default)] forward_to: Option<String> },
}

impl MailboxEntry {
      /// The bare address of this mailbox entry.
    pub fn address(&self) -> &str {
        match self {
            MailboxEntry::Simple(s) => s,
            MailboxEntry::Detailed { id, .. } => id,
          }
      }
      /// The forward target if set, or `None` for store-only entries.
    pub fn forward_to(&self) -> Option<&str> {
        match self {
            MailboxEntry::Simple(_) => None,
            MailboxEntry::Detailed { forward_to, .. } => forward_to.as_deref(),
          }
     }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub host: String,
    pub dns: u16,
    pub smtp: u16,
    pub api: u16,
    pub database_url: String,
    pub domains: Vec<String>,
    pub records: Vec<RecordDTO>,
    pub mailboxes: Vec<MailboxEntry>,
      /// Which controllable listeners to bring up at boot
      /// (`full`/`api`/`smtp`/`dns`/`off`). `None` means "use the default",
      /// which is `full` (API + inbound SMTP + DNS) — the historical always-on
      /// behaviour. The HTTP API can override this at runtime.
    #[serde(default)]
    pub mode: Option<BootMode>,
    #[serde(default)]
    pub auth: AuthConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: "0.0.0.0".to_string(),
            dns: 53,
            smtp: 25,
            api: 8081,
            database_url: "turso:zonemail.db".to_string(),
            domains: Vec::new(),
            records: Vec::new(),
            mailboxes: Vec::new(),   // Vec<MailboxEntry> is empty by default
            mode: None,            // default BootMode (full) applied via resolved_mode()
            auth: AuthConfig::default(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        let builder = ConfigLoader::builder()
            // 1. Optional config file (if it doesn't exist, it's ignored)
            .add_source(File::with_name("zonemail").required(false))
            // 2. Environment variable overrides (e.g., ZONEMAIL_SMTP=25)
            .add_source(Environment::with_prefix("ZONEMAIL").separator("__"));

        // Build and deserialize. If fields are missing in the file/env,
        // Serde falls back cleanly to values defined in `Config::default()`.
        let settings = builder.build()?;
        settings.try_deserialize()
    }
    pub fn api_addr(&self) -> Result<SocketAddr, std::net::AddrParseError> {
        format!("{}:{}", self.host, self.api).parse()
    }

    pub fn smtp_addr(&self) -> Result<SocketAddr, std::net::AddrParseError> {
        format!("{}:{}", self.host, self.smtp).parse()
    }

    pub fn dns_addr(&self) -> Result<SocketAddr, std::net::AddrParseError> {
        format!("{}:{}", self.host, self.dns).parse()
    }

    /// The boot mode to start with: an explicit `mode` if set, else the default
    /// (`full` = API + SMTP + DNS). This is the historical always-on behaviour.
    pub fn resolved_mode(&self) -> BootMode {
        self.mode.unwrap_or_default()
    }
}

// ===========================================================================
// API authentication & authorization
//
// When `auth.enabled` is `true`, every request to the HTTP API must carry a JWT
// whose signature verifies against the configured key material AND whose role
// claim is allowed by the configured role->permission policy. When `enabled` is
// `false` (the default) the API is open, exactly as before this feature.
// ===========================================================================

/// Top-level authentication/authorization configuration. Every field is
/// `#[serde(default)]` so an existing `zonemail.toml` (with no `[auth]` table)
/// keeps working: auth is simply disabled and the API stays open.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
      /// Master switch. `false` (default) => the API is open; the guard is a pure
      /// pass-through that never touches the request.
    pub enabled: bool,

      /// Expected `iss` claim. When set, tokens whose issuer differs are rejected.
    pub issuer: Option<String>,
       /// Expected `aud` claim(s). When non-empty a token not in this set is rejected.
      #[serde(default, deserialize_with = "deserialize_audience")]
    pub audience: Vec<String>,
      /// Clock skew (seconds) tolerated for `exp`/`nbf`. Default 30.
    pub clock_skew_secs: u64,
      /// Claim name carrying the caller's principal identity (informational; not
      /// used for authorization). Default `"sub"`.
    pub subject_claim: String,
      /// Claim name(s) from which the caller's role(s) are read. Each may name a
      /// bare string or an array of strings; all are unioned into the role set.
      /// Default `["role"]`. The field name is configurable per the design.
    pub role_claims: Vec<String>,
      /// Resource names that stay open regardless of `enabled`, matched against the
      /// governed resource of a route. `"health"` and `"doc"` are public by default.
    pub public: Vec<String>,
      /// Where the verification key material comes from.
    pub keys: KeyConfig,
      /// Named roles and the permissions each is granted. This is the *only*
      /// role-authoring surface; zonemail is a verifier, **not** an authz server,
      /// so roles are declared here and the upstream auth server mints JWTs that
      /// name one of these roles.
    pub roles: HashMap<String, RoleConfig>,
}

/// Serde helper: accept both `audience = "foo"` (bare string) and
/// `audience = ["foo", "bar"]` in TOML/JSON, normalising to `Vec<String>`.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum OneOrManyStrings {
    Single(String),
    Many(Vec<String>),
}

fn deserialize_audience<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
      where
        D: serde::Deserializer<'de>,
   {
        match OneOrManyStrings::deserialize(deserializer)? {
             OneOrManyStrings::Single(s) => Ok(vec![s]),
             OneOrManyStrings::Many(v) => Ok(v),
        }
   }

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            enabled: false,
            issuer: None,
            audience: Vec::new(),
            clock_skew_secs: 30,
            subject_claim: "sub".to_string(),
            role_claims: vec!["role".to_string()],
            public: AuthConfig::default_public(),
            keys: KeyConfig::default(),
            roles: HashMap::new(),
        }
     }
}

impl AuthConfig {
      /// The resources that stay public by default. Exposed for testing and as the
      /// fallback the guard uses when config omits `public`.
    pub fn default_public() -> Vec<String> {
      ["health", "doc"].into_iter().map(String::from).collect()
     }
}

/// A named role and the permissions it is granted.
///
/// ```toml
/// [auth.roles.viewer]
/// grants   = ["domains:read", "mailboxes:read", "records:read", "messages:read"]
///
/// [auth.roles.editor]
/// extends = ["viewer"]                          # inherit viewer's grants
/// grants   = ["domains:write", "mailboxes:*", "records:*", "mail:write"]
///
/// [auth.roles.admin]
/// grants   = ["*"]                               # everything
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleConfig {
      /// Grant tokens for this role. Each is `"*"`, `"res:*"`, `"*:verb"`, or
      /// `"res:verb"`. See `crate::auth::policy`.
    pub grants: Vec<String>,
      /// Other role names whose grants this role inherits (union).
    pub extends: Vec<String>,
}

impl Default for RoleConfig {
    fn default() -> Self {
        Self { grants: Vec::new(), extends: Vec::new() }
     }
}

/// Where verification key material comes from and how it is kept fresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyConfig {
      /// The key source. Exactly one applies, selected by this field.
    pub kind: KeysKind,

      /// - `kind = "secret"`: a literal HMAC secret. Prefer `secret_b64` or, to
      ///   keep the secret out of the config file, `secret_env` (the *name* of an
      ///   environment variable that holds the secret).
    pub secret: Option<String>,
     /// Base64-encoded HMAC secret, alternative to `secret`.
    pub secret_b64: Option<String>,
      /// Name of the environment variable holding the HMAC secret.
    pub secret_env: Option<String>,

      /// - `kind = "jwks"`: an inline JWKS (`JWK` set object) written directly in
      ///   the config. A key *set* with `kid`s, evaluated locally.
    pub jwks: Option<serde_json::Value>,

      /// - `kind = "jwks_remote"`: a URL serving a JWKS to fetch & refresh, e.g.
      ///   `"https://janux.example.com/.well-known/jwks.json"`.
    pub jwks_url: Option<String>,
      /// - `kind = "issuer"`: an auth-server issuer base URL; zonemail discovers
      ///   the JWKS via the standard OIDC discovery document (or a direct
      ///   `jwks_path`), e.g. `"https://janux.example.com"`.
    pub issuer: Option<String>,
      /// Override of the discovery document path. Default
      /// `/.well-known/openid-configuration`.
    pub discovery_path: Option<String>,
      /// A direct JWKS path on the issuer, bypassing discovery when set, e.g.
      /// `".well-known/jwks.json"`.
    pub jwks_path: Option<String>,

      /// Credential presented *to the auth server* (not to zonemail callers) so
      /// zonemail can pull a protected JWKS/discovery. Sent in `janux_auth_header`
      /// (`Authorization` by default); wrapped as `"Bearer ..."` when `janux_bearer`.
    pub janux_token: Option<String>,
      /// Header used to present `janux_token`. Default `"Authorization"`.
    pub janux_auth_header: Option<String>,
      /// Wrap `janux_token` as a `Bearer` credential. Default `true`.
#[serde(default = "default_janux_bearer")]
    pub janux_bearer: bool,

      /// Background refresh cadence for remote key material. Default 300s.
#[serde(default = "default_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
      /// How long a fetched key set is trusted via an on-demand refetch after a
      /// cache miss. Default 300s.
#[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
      /// When `true` (default), a remote fetch failure with an empty key cache
      /// denies the request (fail-closed). When `false`, it stays open
      /// (fail-open) until a key set is available.
#[serde(default = "default_fail_closed")]
    pub fail_closed: bool,

      /// Allowed signature algorithms, e.g. `["RS256"]`, `["HS256"]`,
      /// `["ES256", "ES384"]`. A token whose declared algorithm is not in this set
      /// is rejected -- this blocks the `alg=none` and HS<->RS confusion attacks.
    /// When empty the default for the chosen `kind` is used.
    pub algorithms: Vec<String>,
}

impl KeyConfig {
      /// Default per-key-source algorithm allow-list. Symmetric `secret` => HS256;
      /// every key-*set* source => the common RS/ES asymmetric set.
    pub fn default_algorithms(&self) -> Vec<String> {
        match self.kind {
            KeysKind::Secret => vec!["HS256".to_string()],
             _ => vec!["RS256".to_string(), "ES256".to_string()],
        }
     }
}

/// Default per-field values for the `KeyConfig` serde-missing fields; mirrored
/// by the per-field `#[serde(default = ...)]` so a partial `[keys]` table yields
/// the same secure values as an absent one.
const DEFAULT_JANUX_BEARER: bool = true;
const DEFAULT_CACHE_TTL_SECS: u64 = 300;
const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 300;
const DEFAULT_FAIL_CLOSED: bool = true;

fn default_janux_bearer() -> bool { DEFAULT_JANUX_BEARER }
fn default_cache_ttl_secs() -> u64 { DEFAULT_CACHE_TTL_SECS }
fn default_refresh_interval_secs() -> u64 { DEFAULT_REFRESH_INTERVAL_SECS }
fn default_fail_closed() -> bool { DEFAULT_FAIL_CLOSED }

impl Default for KeyConfig {
    fn default() -> Self {
        KeyConfig {
            kind: KeysKind::default(),
            secret: None,
            secret_b64: None,
            secret_env: None,
            jwks: None,
            jwks_url: None,
            issuer: None,
            discovery_path: None,
            jwks_path: None,
            janux_token: None,
            janux_auth_header: None,
            janux_bearer: DEFAULT_JANUX_BEARER,
            refresh_interval_secs: DEFAULT_REFRESH_INTERVAL_SECS,
            cache_ttl_secs: DEFAULT_CACHE_TTL_SECS,
            fail_closed: DEFAULT_FAIL_CLOSED,
            algorithms: Vec::new(),
          }
      }
}

/// Which kind of key material the verifier uses. Exactly one applies, selected by
/// this field on [`KeyConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KeysKind {
      /// A single symmetric HMAC secret (`secret`/`secret_b64`/`secret_env`).
      #[default]
    Secret,
      /// An inline JWKS key set carried in the config file.
    Jwks,
      /// A JWKS fetched at runtime from `jwks_url` and periodically refreshed.
    JwksRemote,
      /// An OIDC-style auth-server issuer URL; JWKS is discovered at runtime.
    Issuer,
}

impl std::str::FromStr for KeysKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
              "secret" => Ok(KeysKind::Secret),
              "jwks" => Ok(KeysKind::Jwks),
              "jwks_remote" | "remote" => Ok(KeysKind::JwksRemote),
              "issuer" | "oidc" => Ok(KeysKind::Issuer),
            other => Err(format!(
                  "unknown auth keys.kind '{other}'; expected one of secret|jwks|jwks_remote|issuer"
              )),
       }
     }
}

// ===========================================================================
// Tests
// ===========================================================================
//
// Pure tests for configuration parsing and the untagged `MailboxEntry` enum,
// which is loaded verbatim from `zonemail.toml`.
#[cfg(test)]
mod tests {
    use super::*;

    use crate::services::Service;

         #[test]
    fn mailbox_entry_simple_has_no_forward() {
        let entry = MailboxEntry::Simple("user@example.com".to_string());
        assert_eq!(entry.address(), "user@example.com");
        assert_eq!(entry.forward_to(), None);
        }

         #[test]
    fn mailbox_entry_detailed_carries_forward() {
        let entry =
             MailboxEntry::Detailed {
                id: "user@example.com".to_string(),
                forward_to: Some("user@gmail.com".to_string()),
             };
        assert_eq!(entry.address(), "user@example.com");
        assert_eq!(entry.forward_to(), Some("user@gmail.com"));
        }

         #[test]
    fn mailbox_entry_detailed_defaults_to_no_forward() {
         // `forward_to` is `#[serde(default)]`, so a bare `id` deserializes to None.
        let json = r#"{"id": "user@example.com"}"#;
        let entry: MailboxEntry = serde_json::from_str(json).expect("deserialize");
        match entry {
            MailboxEntry::Detailed { forward_to, .. } => assert!(forward_to.is_none()),
            MailboxEntry::Simple(_) => panic!("expected a Detailed entry"),
            }
        }

         #[test]
    fn mailbox_entry_untagged_json_picks_variant_by_shape() {
         // A bare string is `Simple`; a table is `Detailed`.
        let simple: Vec<MailboxEntry> =
             serde_json::from_str(r#"[ "a@example.com" ]"#).expect("array");
        assert!(matches!(simple[0], MailboxEntry::Simple(_)));
        assert_eq!(simple[0].address(), "a@example.com");

        let detailed: Vec<MailboxEntry> =
             serde_json::from_str(r#"[ {"id": "b@example.com", "forward_to": "b@ext.com"} ]"#)
                    .expect("array");
        match &detailed[0] {
            MailboxEntry::Detailed { id, forward_to } => {
                assert_eq!(id.as_str(), "b@example.com");
                assert_eq!(forward_to.as_deref(), Some("b@ext.com"));
               }
            MailboxEntry::Simple(_) => panic!("expected a Detailed entry"),
            }
        }

         #[test]
    fn config_default_ports_and_host() {
        let cfg = Config::default();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.dns, 53);
        assert_eq!(cfg.smtp, 25);
        assert_eq!(cfg.api, 8081);
        assert_eq!(cfg.database_url, "turso:zonemail.db");
        assert!(cfg.domains.is_empty());
        assert!(cfg.records.is_empty());
        assert!(cfg.mailboxes.is_empty());
        }

        #[test]
    fn resolved_mode_defaults_to_full() {
               // No `mode` configured -> default is `full` (API + SMTP + DNS on), matching the
               // historical always-on behaviour.
        let cfg = Config::default();
        assert_eq!(cfg.resolved_mode(), BootMode::Full);
        assert!(cfg.resolved_mode().services().contains(&Service::Smtp));
        assert!(cfg.resolved_mode().services().contains(&Service::Dns));
          }

          #[test]
    fn resolved_mode_honors_explicit_choice_via_json() {
        let cfg: Config = serde_json::from_str(r#"{ "mode": "dns" }"#).expect("parse");
        assert_eq!(cfg.resolved_mode(), BootMode::Dns);
        assert!(cfg.resolved_mode().services().contains(&Service::Dns));
        assert!(!cfg.resolved_mode().services().contains(&Service::Smtp));

        let cfg: Config = serde_json::from_str(r#"{ "mode": "off" }"#).expect("parse");
        assert_eq!(cfg.resolved_mode(), BootMode::Off);
        assert!(cfg.resolved_mode().services().is_empty());
          }

         #[test]
    fn config_addr_helpers_join_host_and_port() {
        let mut cfg = Config::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.dns = 5353;
        cfg.smtp = 1025;
        cfg.api = 8080;
        assert_eq!(cfg.dns_addr().unwrap(), "127.0.0.1:5353".parse().unwrap());
        assert_eq!(cfg.smtp_addr().unwrap(), "127.0.0.1:1025".parse().unwrap());
        assert_eq!(cfg.api_addr().unwrap(), "127.0.0.1:8080".parse().unwrap());
        }

         #[test]
    fn config_addr_helpers_reject_bad_host() {
        let mut cfg = Config::default();
        cfg.host = "not a valid host".to_string();
         // An invalid host makes the `SocketAddr` parse fail.
        assert!(cfg.api_addr().is_err());
        }

         #[test]
    fn config_deserializes_with_all_defaults() {
          // An empty JSON object fills every field from `#[serde(default)]`.
        let cfg: Config = serde_json::from_str("{}").expect("defaults");
           // `Config` has no `PartialEq`; check the key fields instead.
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.dns, 53);
        assert_eq!(cfg.smtp, 25);
        assert_eq!(cfg.api, 8081);
        assert!(cfg.domains.is_empty());
        assert!(cfg.records.is_empty());
        assert!(cfg.mailboxes.is_empty());
        }

        #[test]
    fn auth_section_defaults_to_disabled() {
           // A config with no `[auth]` table yields a disabled, no-op auth layer.
        let cfg: Config = serde_json::from_str("{}").expect("defaults");
        assert!(!cfg.auth.enabled, "auth must be disabled by default");
        assert!(cfg.auth.roles.is_empty());
          }

        #[test]
    fn auth_issuer_source_with_roles_deserializes() {
           // A full `[auth]` table deserializes into the verifier shape, including
             // the key-source `kind` and the named roles.
        let json = r#"{
             "auth": {
               "enabled": true,
               "issuer": "https://janux.example.com",
               "audience": ["zonemail-api"],
               "role_claims": ["role", "roles"],
               "public": ["health", "doc"],
               "keys": {
                 "kind": "issuer",
                 "issuer": "https://janux.example.com",
                 "algorithms": ["RS256"],
                 "fail_closed": true
               },
               "roles": {
                 "admin":  { "grants": ["*"] },
                 "viewer": { "grants": ["domains:read", "records:read"] },
                 "ops":    { "extends": ["viewer"], "grants": ["mailboxes:*"] }
               }
             }
           }"#;
        let cfg: Config = serde_json::from_str(json).expect("auth config");
        let a = &cfg.auth;
        assert!(a.enabled);
        assert_eq!(a.keys.kind, crate::config::KeysKind::Issuer);
        assert_eq!(a.keys.issuer.as_deref(), Some("https://janux.example.com"));
        assert_eq!(a.keys.algorithms, vec!["RS256".to_string()]);
        assert!(a.keys.fail_closed);
        assert_eq!(a.roles.len(), 3);
        assert_eq!(a.roles["admin"].grants, Vec::from(["*".to_string()]));
        assert_eq!(a.roles["ops"].extends, Vec::from(["viewer".to_string()]));
          }
}
