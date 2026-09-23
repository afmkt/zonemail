use crate::db::RecordDTO;
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
}
