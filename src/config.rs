use crate::db::RecordDTO;
use config::{Config as ConfigLoader, ConfigError, Environment, File};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

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
    pub mailboxes: Vec<String>,
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
            mailboxes: Vec::new(),
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
