use anyhow::Result;
use nostr_sdk::ToBech32;

use crate::{config, crypto};

pub async fn run() -> Result<()> {
    let enc_path = config::config_dir()?.join("key.enc");
    run_with_enc_path(&enc_path)
}

pub fn run_with_enc_path(enc_path: &std::path::Path) -> Result<()> {
    if !crypto::is_initialized(enc_path) {
        anyhow::bail!("not initialized — run `mycel init` first");
    }
    let keys = crypto::load_keys(enc_path)?;
    let npub = keys.public_key().to_bech32()?;
    println!("{}", npub);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use tempfile::TempDir;

    #[test]
    fn test_id_command() {
        let dir = TempDir::new().unwrap();
        let enc_path = dir.path().join("key.enc");

        // Not initialized: should error
        let result = run_with_enc_path(&enc_path);
        assert!(result.is_err(), "id should fail when not initialized");
        assert!(
            result.unwrap_err().to_string().contains("not initialized"),
            "error must mention 'not initialized'"
        );

        // Store a key via file backend
        unsafe { std::env::set_var("MYCEL_KEY_PASSPHRASE", "test-passphrase-ci"); }
        let keys_orig = nostr_sdk::Keys::generate();
        let secret_hex =
            zeroize::Zeroizing::new(keys_orig.secret_key().to_secret_hex());
        crypto::store_key_file(&enc_path, "test-passphrase-ci", &secret_hex)
            .expect("store_key_file");

        // Now id should succeed
        let result = run_with_enc_path(&enc_path);
        assert!(result.is_ok(), "id should succeed when initialized: {:?}", result);

        unsafe { std::env::remove_var("MYCEL_KEY_PASSPHRASE"); }
    }
}
