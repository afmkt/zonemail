use crate::db::RecordDTO;
use crate::services::BootMode;
use config::{Config as ConfigLoader, ConfigError, Environment, File};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

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
      /// Which optional servers to bring up at boot (`full`/`smtp`/`dns`/`api-only`).
      /// `None` means "use the default", which is `full` (SMTP + DNS) -- the
      /// historical always-on behaviour. The HTTP API can override this at runtime.
    #[serde(default)]
    pub mode: Option<BootMode>,
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
    /// (`full` = SMTP + DNS), preserving the historical always-on behaviour.
    pub fn resolved_mode(&self) -> BootMode {
        self.mode.unwrap_or_default()
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
               // No `mode` configured -> default is `full` (SMTP + DNS on), matching the
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

        let cfg: Config = serde_json::from_str(r#"{ "mode": "api-only" }"#).expect("parse");
        assert_eq!(cfg.resolved_mode(), BootMode::ApiOnly);
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
}
