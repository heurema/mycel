use anyhow::Result;
use nostr_sdk::ToBech32;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::{config, crypto, store};

/// Test TCP connectivity to a relay URL (wss://host[:port]).
/// Returns true if TCP handshake completes within 5 seconds.
async fn check_relay(url: &str) -> bool {
    let host_port = extract_host_port(url);
    let Some((host, port)) = host_port else {
        return false;
    };
    let addr = format!("{host}:{port}");
    matches!(
        timeout(Duration::from_secs(5), TcpStream::connect(&addr)).await,
        Ok(Ok(_))
    )
}

fn extract_host_port(url: &str) -> Option<(String, u16)> {
    // Strip wss:// or ws://
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))?;
    // Remove path
    let host_part = rest.split('/').next()?;
    if let Some(pos) = host_part.rfind(':') {
        let host = &host_part[..pos];
        let port: u16 = host_part[pos + 1..].parse().ok()?;
        Some((host.to_string(), port))
    } else {
        // Default port for wss is 443
        let port = if url.starts_with("wss://") { 443 } else { 80 };
        Some((host_part.to_string(), port))
    }
}

pub async fn run() -> Result<()> {
    run_with_dirs(
        &config::config_dir()?,
        &config::data_dir()?,
    )
    .await
}

/// Inner implementation allowing test overrides for config/data directories.
pub async fn run_with_dirs(
    cfg_dir: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<()> {
    let enc_path = cfg_dir.join("key.enc");

    // AC8: refuse to overwrite existing key
    if crypto::is_initialized(&enc_path) {
        anyhow::bail!(
            "already initialized. Use `mycel id` to view your address. \
             Delete ~/.config/mycel/ to start over (this will LOSE your key)."
        );
    }

    // Create dirs first (no key material yet — safe partial state)
    std::fs::create_dir_all(cfg_dir)?;
    std::fs::create_dir_all(data_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cfg_dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))?;
    }

    // AC3: create SQLite database (before key — recoverable if fails)
    let db_path = data_dir.join("mycel.db");
    let _conn = store::open(&db_path)?;

    // AC4: write default config.toml (before key — recoverable if fails)
    let config_path = cfg_dir.join("config.toml");

    // AC1+AC2: generate keypair and store (last step — if this succeeds, init is complete)
    let (keys, backend) = crypto::generate_and_store(&enc_path)?;
    let npub = keys.public_key().to_bech32()?;

    // Write config with actual backend used
    let mut cfg = config::Config::default();
    if matches!(backend, crypto::StorageBackend::EncryptedFile) {
        cfg.identity.storage = "file".to_string();
    }
    let content = toml::to_string_pretty(&cfg)?;
    std::fs::write(&config_path, content)?;

    // AC5: test relay connectivity
    println!("Testing relay connectivity...");
    let mut reachable = Vec::new();
    let mut unreachable = Vec::new();
    for url in &cfg.relays.urls {
        if check_relay(url).await {
            println!("  {} reachable", url);
            reachable.push(url.clone());
        } else {
            println!("  {} unreachable", url);
            unreachable.push(url.clone());
        }
    }

    if reachable.is_empty() {
        println!(
            "Warning: no relays reachable. Check your network connection."
        );
    } else {
        println!("{}/{} relays reachable.", reachable.len(), cfg.relays.urls.len());
    }

    // AC6: print npub
    println!("Your address: {}", npub);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_extract_host_port() {
        let (h, p) = extract_host_port("wss://nos.lol").unwrap();
        assert_eq!(h, "nos.lol");
        assert_eq!(p, 443);

        let (h, p) = extract_host_port("wss://relay.damus.io").unwrap();
        assert_eq!(h, "relay.damus.io");
        assert_eq!(p, 443);
    }

    #[tokio::test]
    async fn test_relay_connectivity() {
        // We simply verify the function doesn't panic and returns a bool.
        // In CI without network, all will be unreachable — that's fine.
        let result = check_relay("wss://nos.lol").await;
        // result can be true or false depending on network; we just assert it's a bool
        let _ = result;

        // Invalid URL returns false
        assert!(!check_relay("not-a-url").await);
        assert!(!check_relay("").await);
    }

    #[tokio::test]
    async fn test_db_creation() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("mycel.db");
        let conn = store::open(&db_path).expect("db open");

        // Verify all expected tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"messages".to_string()));
        assert!(tables.contains(&"contacts".to_string()));
        assert!(tables.contains(&"sync_state".to_string()));
        assert!(tables.contains(&"relays".to_string()));
    }

    #[tokio::test]
    async fn test_config_creation() {
        let dir = TempDir::new().unwrap();
        let cfg_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");

        // Directly write a config
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg = crate::config::Config::default();
        let content = toml::to_string_pretty(&cfg).unwrap();
        let config_path = cfg_dir.join("config.toml");
        std::fs::write(&config_path, content).unwrap();

        assert!(config_path.exists());
        let loaded: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(loaded.relays.urls.contains(&"wss://nos.lol".to_string()));
        assert!(loaded
            .relays
            .urls
            .contains(&"wss://relay.damus.io".to_string()));
        assert!(loaded
            .relays
            .urls
            .contains(&"wss://relay.nostr.band".to_string()));
        assert_eq!(loaded.identity.storage, "keychain");
    }

    #[tokio::test]
    async fn test_init_no_overwrite() {
        let dir = TempDir::new().unwrap();
        let cfg_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        unsafe { std::env::set_var("MYCEL_KEY_PASSPHRASE", "test-passphrase-ci"); }

        // Create key.enc directly (bypasses keychain, ensures file-based detection)
        let enc_path = cfg_dir.join("key.enc");
        let keys = nostr_sdk::Keys::generate();
        let hex = keys.secret_key().to_secret_hex();
        crate::crypto::store_key_file(&enc_path, "test-passphrase-ci", &hex)
            .expect("store key file");

        // is_initialized should detect the file
        assert!(crate::crypto::is_initialized(&enc_path));

        // Init should refuse because key.enc exists
        let r = run_with_dirs(&cfg_dir, &data_dir).await;
        assert!(r.is_err(), "init should refuse when key already exists");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("already initialized"),
            "error should mention 'already initialized', got: {msg}"
        );

        unsafe { std::env::remove_var("MYCEL_KEY_PASSPHRASE"); }
    }
}
