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
