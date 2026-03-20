use anyhow::Result;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const DEFAULT_RELAYS: &[&str] = &[
    "wss://nos.lol",
    "wss://relay.damus.io",
    "wss://relay.nostr.band",
];

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub relays: RelayConfig,
    pub identity: IdentityConfig,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RelayConfig {
    pub urls: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IdentityConfig {
    /// "keychain" or "file"
    pub storage: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relays: RelayConfig {
                urls: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            },
            identity: IdentityConfig {
                storage: "keychain".to_string(),
            },
        }
    }
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
    use std::path::PathBuf;

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
        let loaded: Config = toml::from_str(&std::fs::read_to_string(&config_path).unwrap())
            .expect("toml parse");

        // Relay URLs must contain all three defaults
        assert!(loaded.relays.urls.contains(&"wss://nos.lol".to_string()));
        assert!(loaded
            .relays
            .urls
            .contains(&"wss://relay.damus.io".to_string()));
        assert!(loaded
            .relays
            .urls
            .contains(&"wss://relay.nostr.band".to_string()));

        // Identity storage default
        assert_eq!(loaded.identity.storage, "keychain");
    }

    #[test]
    fn default_relay_urls_correct() {
        let cfg = Config::default();
        let urls = &cfg.relays.urls;
        assert_eq!(urls.len(), 3);
        assert!(urls.iter().any(|u| u == "wss://nos.lol"));
        assert!(urls.iter().any(|u| u == "wss://relay.damus.io"));
        assert!(urls.iter().any(|u| u == "wss://relay.nostr.band"));
    }
}
