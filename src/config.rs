use anyhow::Result;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.mycel.run",
    "wss://nos.lol",
    "wss://relay.damus.io",
    "wss://relay.nostr.band",
    "wss://relay.primal.net",
    "wss://relay.snort.social",
    "wss://nostr.mutinywallet.com",
    "wss://nostr.wine",
];

/// A local agent entry in [local.agents] config section.
/// Inline table format: `alias = { pubkey = "...", db = "path/to/mycel.db" }`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAgentEntry {
    /// Recipient's secp256k1 public key (hex).
    pub pubkey: String,
    /// Path to recipient's mycel.db (~ is expanded to home directory).
    pub db: String,
}

/// Local transport configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalConfig {
    /// Named agents reachable via local transport.
    /// Key is the alias used on the command line (e.g. `codex`).
    #[serde(default)]
    pub agents: HashMap<String, LocalAgentEntry>,
}

/// Configuration for application-level ACK protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckConfig {
    /// Whether to automatically send ACKs after receiving messages.
    #[serde(default = "default_ack_enabled")]
    pub enabled: bool,
    /// Minimum interval in seconds between ACKs for the same message (anti-storm).
    #[serde(default = "default_ack_min_interval_secs")]
    pub min_interval_secs: u64,
}

fn default_ack_enabled() -> bool {
    false
}

fn default_ack_min_interval_secs() -> u64 {
    60
}

impl Default for AckConfig {
    fn default() -> Self {
        Self {
            enabled: default_ack_enabled(),
            min_interval_secs: default_ack_min_interval_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    #[default]
    Nostr,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransportConfig {
    #[serde(rename = "type", default)]
    pub kind: TransportKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub relays: RelayConfig,
    pub identity: IdentityConfig,
    /// Optional local transport section.
    #[serde(default)]
    pub local: LocalConfig,
    /// Optional ACK protocol configuration.
    #[serde(default)]
    pub ack: AckConfig,
    /// Optional transport configuration.
    #[serde(default)]
    pub transport: TransportConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfig {
    pub urls: Vec<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    10
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdentityStorage {
    Keychain,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityConfig {
    pub storage: IdentityStorage,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relays: RelayConfig {
                urls: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
                timeout_secs: default_timeout_secs(),
            },
            identity: IdentityConfig {
                storage: IdentityStorage::Keychain,
            },
            local: LocalConfig::default(),
            ack: AckConfig::default(),
            transport: TransportConfig::default(),
        }
    }
}

/// Expand a leading `~` in `path` to the user's home directory.
/// Returns the original string if no `~` prefix or if home cannot be resolved.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = dirs_home() {
            return home;
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn dirs_home() -> Option<PathBuf> {
    ProjectDirs::from("run", "mycel", "mycel")
        .map(|_| ()) // ensure ProjectDirs is available
        .and_then(|_| std::env::var("HOME").ok().map(PathBuf::from))
}

/// ~/.config/mycel/
pub fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("run", "mycel", "mycel")
        .ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;
    Ok(dirs.config_dir().to_path_buf())
}

/// ~/.local/share/mycel/
pub fn data_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("run", "mycel", "mycel")
        .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Load config from disk or return defaults
pub fn load() -> Result<Config> {
    let path = config_dir()?.join("config.toml");
    if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        Ok(toml::from_str(&content)?)
    } else {
        Ok(Config::default())
    }
}

/// Write config to config_dir/config.toml
#[allow(dead_code)]
pub fn save(cfg: &Config) -> Result<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");
    let content = toml::to_string_pretty(cfg)?;
    std::fs::write(&path, content)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Build a Config and serialise/deserialise it to/from a temp directory.
    #[test]
    fn test_config_creation() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let cfg = Config::default();
        let content = toml::to_string_pretty(&cfg).expect("toml serialise");
        std::fs::write(&config_path, &content).expect("write config");

        // File must exist
        assert!(config_path.exists(), "config.toml should be created");

        // Must round-trip
        let loaded: Config =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).expect("toml parse");

        // Relay URLs must contain all seven defaults
        assert!(loaded.relays.urls.contains(&"wss://nos.lol".to_string()));
        assert!(
            loaded
                .relays
                .urls
                .contains(&"wss://relay.damus.io".to_string())
        );
        assert!(
            loaded
                .relays
                .urls
                .contains(&"wss://relay.nostr.band".to_string())
        );
        assert!(
            loaded
                .relays
                .urls
                .contains(&"wss://relay.primal.net".to_string())
        );
        assert!(
            loaded
                .relays
                .urls
                .contains(&"wss://relay.snort.social".to_string())
        );
        assert!(
            loaded
                .relays
                .urls
                .contains(&"wss://nostr.mutinywallet.com".to_string())
        );
        assert!(loaded.relays.urls.contains(&"wss://nostr.wine".to_string()));

        // Identity storage default
        assert_eq!(loaded.identity.storage, IdentityStorage::Keychain);
    }

    #[test]
    fn default_relay_urls_correct() {
        let cfg = Config::default();
        let urls = &cfg.relays.urls;
        assert_eq!(urls.len(), 8);
        assert!(urls.iter().any(|u| u == "wss://relay.mycel.run"));
        assert!(urls.iter().any(|u| u == "wss://nos.lol"));
        assert!(urls.iter().any(|u| u == "wss://relay.damus.io"));
        assert!(urls.iter().any(|u| u == "wss://relay.nostr.band"));
        assert!(urls.iter().any(|u| u == "wss://relay.primal.net"));
        assert!(urls.iter().any(|u| u == "wss://relay.snort.social"));
        assert!(urls.iter().any(|u| u == "wss://nostr.mutinywallet.com"));
        assert!(urls.iter().any(|u| u == "wss://nostr.wine"));
    }

    #[test]
    fn identity_storage_serde() {
        let cfg = Config::default();
        let content = toml::to_string_pretty(&cfg).unwrap();
        assert!(content.contains("storage = \"keychain\""));

        let mut cfg2 = Config::default();
        cfg2.identity.storage = IdentityStorage::File;
        let content2 = toml::to_string_pretty(&cfg2).unwrap();
        assert!(content2.contains("storage = \"file\""));

        let loaded: Config = toml::from_str(&content2).unwrap();
        assert_eq!(loaded.identity.storage, IdentityStorage::File);
    }
}
