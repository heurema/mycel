// Key management: generation, encryption at rest, loading
//
// Storage priority:
//   1. OS keychain (keyring crate) – macOS/Linux
//   2. Passphrase-encrypted file (~/.config/mycel/key.enc) – argon2id + AES-256-GCM
//
// MYCEL_KEY_PASSPHRASE env var for headless/CI (skips rpassword prompt).

use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng as AesOsRng},
    Aes256Gcm, Nonce,
};
use anyhow::{Context, Result};
use argon2::{
    password_hash::SaltString,
    Argon2, PasswordHasher,
};
use nostr_sdk::Keys;
use std::path::Path;
use zeroize::Zeroizing;

use crate::config::IdentityStorage;
use crate::error::MycelError;

const SERVICE: &str = "mycel";
const ACCOUNT: &str = "mycel-private-key";
const NONCE_LEN: usize = 12;
const SALT_LEN: usize = 22; // SaltString base64 len (16 bytes raw)

// Argon2id parameters — explicit, not default()
const ARGON2_M_COST: u32 = 19456; // KiB (~19 MiB)
const ARGON2_T_COST: u32 = 2;     // iterations
const ARGON2_P_COST: u32 = 1;     // parallelism
const ARGON2_OUTPUT_LEN: usize = 32;

/// Attempt to store secret key hex in OS keychain.
/// Returns Ok(true) on success, Ok(false) if keychain unavailable.
/// Note: in-process read-back cannot detect silent keychain failures
/// (keyring crate caches in-process). Cross-process verification is done in init.
fn try_store_keychain(secret_hex: &str) -> Result<bool> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT)?;
    match entry.set_password(secret_hex) {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoStorageAccess(_)) | Err(keyring::Error::PlatformFailure(_)) => {
            Ok(false)
        }
        Err(e) => Err(anyhow::anyhow!("keychain store error: {e}")),
    }
}

/// Attempt to load secret key hex from OS keychain.
/// Returns Ok(None) if not stored there.
fn try_load_keychain() -> Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT)?;
    match entry.get_password() {
        Ok(hex) => Ok(Some(hex)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(keyring::Error::NoStorageAccess(_)) | Err(keyring::Error::PlatformFailure(_)) => {
            Ok(None)
        }
        Err(e) => Err(anyhow::anyhow!("keychain load error: {e}")),
    }
}

/// Build Argon2id hasher with explicit parameters.
fn argon2_hasher() -> Result<Argon2<'static>> {
    let params = argon2::Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, Some(ARGON2_OUTPUT_LEN))
        .map_err(|e| anyhow::anyhow!("argon2 params: {e}"))?;
    Ok(Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params))
}

/// Derive 32-byte key from passphrase+salt using argon2id.
fn derive_key(passphrase: &str, salt: &SaltString) -> Result<[u8; 32]> {
    let argon2 = argon2_hasher()?;
    let hash = argon2
        .hash_password(passphrase.as_bytes(), salt)
        .map_err(|e| anyhow::anyhow!("argon2 error: {e}"))?;
    let hash_bytes = hash.hash.ok_or_else(|| anyhow::anyhow!("argon2 hash missing"))?;
    let hash_slice = hash_bytes.as_bytes();
    if hash_slice.len() < 32 {
        return Err(anyhow::anyhow!("argon2 hash too short: {} bytes (need 32)", hash_slice.len()));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&hash_slice[..32]);
    Ok(key)
}

/// Encrypt secret_hex with passphrase, write to enc_path.
/// File format: [22-byte salt base64][12-byte nonce][ciphertext]
pub fn store_key_file(enc_path: &Path, passphrase: &str, secret_hex: &str) -> Result<()> {
    let salt = SaltString::generate(&mut AesOsRng);
    let key_bytes = derive_key(passphrase, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| anyhow::anyhow!("aes-gcm init: {e}"))?;

    let nonce = Aes256Gcm::generate_nonce(&mut AesOsRng);
    let ciphertext = cipher
        .encrypt(&nonce, secret_hex.as_bytes())
        .map_err(|e| anyhow::anyhow!("aes-gcm encrypt: {e}"))?;

    let salt_str = salt.as_str();
    if salt_str.len() != SALT_LEN {
        return Err(anyhow::anyhow!(
            "unexpected salt length: {} (expected {})",
            salt_str.len(),
            SALT_LEN
        ));
    }

    if let Some(parent) = enc_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut data = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    data.extend_from_slice(salt_str.as_bytes());
    data.extend_from_slice(nonce.as_slice());
    data.extend_from_slice(&ciphertext);

    // Write to temp file then rename (atomic) + set 0600 permissions
    let tmp_path = enc_path.with_extension("enc.tmp");
    std::fs::write(&tmp_path, &data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp_path, enc_path)?;
    Ok(())
}

/// Decrypt key.enc file with passphrase, return secret hex.
pub fn load_key_file(enc_path: &Path, passphrase: &str) -> Result<Zeroizing<String>> {
    let data = std::fs::read(enc_path).context("reading key.enc")?;
    if data.len() < SALT_LEN + NONCE_LEN {
        return Err(anyhow::anyhow!("key.enc too short"));
    }
    let salt_str = std::str::from_utf8(&data[..SALT_LEN])?;
    let salt = SaltString::from_b64(salt_str).map_err(|e| anyhow::anyhow!("invalid salt: {e}"))?;
    let key_bytes = derive_key(passphrase, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| anyhow::anyhow!("aes-gcm init: {e}"))?;
    let nonce = Nonce::from_slice(&data[SALT_LEN..SALT_LEN + NONCE_LEN]);
    let plaintext = cipher
        .decrypt(nonce, &data[SALT_LEN + NONCE_LEN..])
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong passphrase?"))?;
    let secret = String::from_utf8(plaintext).map_err(|e| {
        // Ensure the secret bytes are dropped — FromUtf8Error holds the original Vec
        let _ = e.into_bytes();
        anyhow::anyhow!("decrypted key is not valid UTF-8")
    })?;
    Ok(Zeroizing::new(secret))
}

/// Get passphrase: env var first, then prompt.
fn get_passphrase(prompt: &str) -> Result<Zeroizing<String>> {
    if let Ok(p) = std::env::var("MYCEL_KEY_PASSPHRASE") {
        return Ok(Zeroizing::new(p));
    }
    let p = rpassword::prompt_password(prompt)?;
    Ok(Zeroizing::new(p))
}

/// Storage backend reported after successful store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageBackend {
    Keychain,
    EncryptedFile,
}

/// Generate a new keypair, persist it, and return (Keys, StorageBackend).
/// Caller must check `is_initialized()` first — this function does not enforce uniqueness.
pub(crate) fn generate_and_store(enc_path: &Path) -> Result<(Keys, StorageBackend)> {
    let keys = Keys::generate();
    let secret_hex = Zeroizing::new(keys.secret_key().to_secret_hex());

    // Try keychain first
    match try_store_keychain(&secret_hex) {
        Ok(true) => return Ok((keys, StorageBackend::Keychain)),
        Ok(false) => {
            // Keychain backend genuinely unavailable (no Secret Service, no Keychain)
            eprintln!("Note: OS keychain not available. Using encrypted file instead.");
        }
        Err(e) => {
            // Keychain present but denied/locked — warn explicitly, don't silently degrade
            eprintln!("Warning: keychain error: {e}");
            eprintln!("Falling back to encrypted file. Use `mycel init --file` to skip keychain.");
        }
    }

    // Fall back to encrypted file
    let passphrase = get_passphrase("Enter passphrase to protect your key: ")?;
    store_key_file(enc_path, &passphrase, &secret_hex)?;
    Ok((keys, StorageBackend::EncryptedFile))
}

/// Generate and store using ONLY file backend (skip keychain).
#[allow(dead_code)]
pub(crate) fn generate_and_store_file(enc_path: &Path) -> Result<(Keys, StorageBackend)> {
    let keys = Keys::generate();
    let secret_hex = Zeroizing::new(keys.secret_key().to_secret_hex());
    let passphrase = get_passphrase("Enter passphrase to protect your key: ")?;
    store_key_file(enc_path, &passphrase, &secret_hex)?;
    Ok((keys, StorageBackend::EncryptedFile))
}

/// Load existing keypair from storage. Respects the configured storage backend.
pub fn load_keys(enc_path: &Path, storage: IdentityStorage) -> Result<Keys> {
    match storage {
        IdentityStorage::Keychain => {
            if let Some(hex) = try_load_keychain()? {
                let secret_hex = Zeroizing::new(hex);
                let keys = Keys::parse(&secret_hex)
                    .map_err(|e| anyhow::anyhow!("invalid key from keychain: {e}"))?;
                return Ok(keys);
            }
            // Keychain configured but key not found there — check file as fallback
            if enc_path.exists() {
                let passphrase = get_passphrase("Enter passphrase to unlock your key: ")?;
                let hex = load_key_file(enc_path, &passphrase)
                    .map_err(|e| anyhow::anyhow!("{e} — wrong passphrase or corrupted key file"))?;
                let keys = Keys::parse(&hex)
                    .map_err(|e| anyhow::anyhow!("invalid key from file: {e}"))?;
                return Ok(keys);
            }
            Err(MycelError::NotInitialized.into())
        }
        IdentityStorage::File => {
            if enc_path.exists() {
                let passphrase = get_passphrase("Enter passphrase to unlock your key: ")?;
                let hex = load_key_file(enc_path, &passphrase)
                    .map_err(|e| anyhow::anyhow!("{e} — wrong passphrase or corrupted key file"))?;
                let keys = Keys::parse(&hex)
                    .map_err(|e| anyhow::anyhow!("invalid key from file: {e}"))?;
                return Ok(keys);
            }
            Err(MycelError::NotInitialized.into())
        }
    }
}

/// Check whether a key is already stored (without decrypting).
pub fn is_initialized(enc_path: &Path) -> bool {
    if enc_path.exists() {
        return true;
    }
    // Light keychain check: try to read, true if NoEntry means it could exist with diff result,
    // but realistically we just check the file path as primary indicator.
    // Also do a quick keychain probe.
    if let Ok(entry) = keyring::Entry::new(SERVICE, ACCOUNT) {
        match entry.get_password() {
            Ok(_) => return true,
            Err(keyring::Error::NoEntry) => {}
            Err(_) => {} // Keychain error — treat as not found
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::ToBech32;
    use tempfile::TempDir;

    #[test]
    fn test_keygen() {
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32().expect("bech32 encoding");
        assert!(npub.starts_with("npub1"), "npub should start with npub1");
        // Verify secret key is 32 bytes
        assert_eq!(keys.secret_key().to_secret_bytes().len(), 32);
    }

    #[test]
    fn test_key_storage() {
        let dir = TempDir::new().unwrap();
        let enc_path = dir.path().join("key.enc");
        let passphrase = "test-passphrase-for-ci";

        // Generate a key
        let keys = Keys::generate();
        let secret_hex = Zeroizing::new(keys.secret_key().to_secret_hex());

        // Store to file
        store_key_file(&enc_path, passphrase, &secret_hex).expect("store_key_file");
        assert!(enc_path.exists(), "key.enc should be created");

        // Load back
        let loaded = load_key_file(&enc_path, passphrase).expect("load_key_file");
        assert_eq!(*loaded, *secret_hex, "round-trip secret hex must match");

        // Wrong passphrase must fail
        let result = load_key_file(&enc_path, "wrong-passphrase");
        assert!(result.is_err(), "wrong passphrase should fail decryption");

        // Secret is zeroized on drop — we just verify the type
        drop(loaded);
    }

    #[test]
    fn test_argon2_hasher_builds() {
        // Verify explicit params construct without error
        let hasher = argon2_hasher();
        assert!(hasher.is_ok(), "argon2 hasher should build with explicit params");
    }
}
