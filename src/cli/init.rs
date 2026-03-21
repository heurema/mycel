use anyhow::Result;
use nostr_sdk::ToBech32;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::config::IdentityStorage;
use crate::error::MycelError;
use crate::{config, crypto, store};

/// Test TCP connectivity to a relay URL (wss://host[:port]).
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
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))?;
    let host_part = rest.split('/').next()?;
    if let Some(pos) = host_part.rfind(':') {
        let host = &host_part[..pos];
        let port: u16 = host_part[pos + 1..].parse().ok()?;
        Some((host.to_string(), port))
    } else {
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
        return Err(MycelError::AlreadyInitialized.into());
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
    let db_path_owned = db_path.to_path_buf();
    tokio::task::spawn_blocking(move || store::open(&db_path_owned).map(|_| ()))
        .await
        .map_err(|e| anyhow::anyhow!("db init task panicked: {e}"))??;

    // AC4: write default config.toml (before key — recoverable if fails)
    let config_path = cfg_dir.join("config.toml");

    // AC1+AC2: generate keypair and store (last step — if this succeeds, init is complete)
    let (keys, backend) = crypto::generate_and_store(&enc_path)?;

    // Verify persistence: spawn a subprocess to check keychain read-back.
    let verified_backend = if matches!(backend, crypto::StorageBackend::Keychain) {
        let verify_ok = std::process::Command::new(std::env::current_exe()?)
            .args(["id"])
            .env("MYCEL_KEY_PASSPHRASE", "unused-keychain-path")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if verify_ok {
            backend
        } else {
            eprintln!("Warning: keychain did not persist key. Falling back to encrypted file.");
            let secret_hex = zeroize::Zeroizing::new(keys.secret_key().to_secret_hex());
            let passphrase = if let Ok(p) = std::env::var("MYCEL_KEY_PASSPHRASE") {
                zeroize::Zeroizing::new(p)
            } else {
                zeroize::Zeroizing::new(rpassword::prompt_password("Enter passphrase to protect your key: ")?)
            };
            crypto::store_key_file(&enc_path, &passphrase, &secret_hex)?;
            crypto::StorageBackend::EncryptedFile
        }
    } else {
        backend
    };

    let npub = keys.public_key().to_bech32()?;

    // Write config with actual backend used
    let mut cfg = config::Config::default();
    if matches!(verified_backend, crypto::StorageBackend::EncryptedFile) {
        cfg.identity.storage = IdentityStorage::File;
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
        let result = check_relay("wss://nos.lol").await;
        let _ = result;

        assert!(!check_relay("not-a-url").await);
        assert!(!check_relay("").await);
    }

    #[tokio::test]
    async fn test_db_creation() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("mycel.db");
        let conn = store::open(&db_path).expect("db open");

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
        assert_eq!(loaded.identity.storage, IdentityStorage::Keychain);
    }

    #[tokio::test]
    async fn test_init_no_overwrite() {
        let dir = TempDir::new().unwrap();
        let cfg_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        unsafe { std::env::set_var("MYCEL_KEY_PASSPHRASE", "test-passphrase-ci"); }

        let enc_path = cfg_dir.join("key.enc");
        let keys = nostr_sdk::Keys::generate();
        let hex = keys.secret_key().to_secret_hex();
        crate::crypto::store_key_file(&enc_path, "test-passphrase-ci", &hex)
            .expect("store key file");

        assert!(crate::crypto::is_initialized(&enc_path));

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
