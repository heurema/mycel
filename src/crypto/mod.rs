// Key management: generation, encryption at rest, loading
//
// File format versions:
//   v1: [22-byte salt base64][12-byte nonce][ciphertext] — OWASP minimum params
//   v2: [MYKF magic][version][KDF params][16-byte raw salt][12-byte nonce][ciphertext]
//
// Storage backends:
//   1. OS keychain (keyring crate) – macOS/Linux
//   2. Passphrase-encrypted file (~/.config/mycel/key.enc) – argon2id + AES-256-GCM
//
// MYCEL_KEY_PASSPHRASE env var for headless/CI (skips rpassword prompt).
// Migration: v1 files auto-upgraded to v2 on successful unlock.

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng as AesOsRng},
};
use anyhow::{Context, Result};
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use nostr_sdk::Keys;
use std::path::Path;
use zeroize::Zeroizing;

use crate::config::IdentityStorage;
use crate::error::MycelError;

const SERVICE: &str = "mycel";
const ACCOUNT: &str = "mycel-private-key";

// ---------------------------------------------------------------------------
// v1 format constants (kept for backward-compatible reader)
// ---------------------------------------------------------------------------
const V1_SALT_LEN: usize = 22; // SaltString base64 len (16 bytes raw)
const V1_NONCE_LEN: usize = 12;
const V1_M_COST: u32 = 19456; // KiB (~19 MiB) — OWASP minimum
const V1_T_COST: u32 = 2;
const V1_P_COST: u32 = 1;
const V1_OUTPUT_LEN: usize = 32;

// ---------------------------------------------------------------------------
// v2 format constants (RFC 9106 / Bitwarden-level)
// ---------------------------------------------------------------------------
const V2_MAGIC: &[u8; 4] = b"MYKF";
const V2_VERSION: u8 = 0x02;
// Salt: 16 raw bytes at offset 12. Nonce: 12 bytes at offset 28.
const V2_HEADER_LEN: usize = 40; // 4 + 1 + 4 + 1 + 1 + 1 + 16 + 12
const V2_M_COST: u32 = 65536; // KiB (64 MiB)
const V2_T_COST: u8 = 3;
const V2_P_COST: u8 = 4;
const V2_OUTPUT_LEN: u8 = 32;

/// KDF parameters stored in v2 file header.
#[derive(Debug, Clone, Copy)]
struct KdfParams {
    m_cost: u32,
    t_cost: u8,
    p_cost: u8,
    output_len: u8,
}

impl KdfParams {
    const fn v2_default() -> Self {
        Self {
            m_cost: V2_M_COST,
            t_cost: V2_T_COST,
            p_cost: V2_P_COST,
            output_len: V2_OUTPUT_LEN,
        }
    }
}

// ---------------------------------------------------------------------------
// Keychain operations
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// v1 KDF (PasswordHasher trait — kept for backward compat)
// ---------------------------------------------------------------------------

fn argon2_v1() -> Result<Argon2<'static>> {
    let params = argon2::Params::new(V1_M_COST, V1_T_COST, V1_P_COST, Some(V1_OUTPUT_LEN))
        .map_err(|e| anyhow::anyhow!("argon2 v1 params: {e}"))?;
    Ok(Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        params,
    ))
}

fn derive_key_v1(passphrase: &str, salt: &SaltString) -> Result<[u8; 32]> {
    let argon2 = argon2_v1()?;
    let hash = argon2
        .hash_password(passphrase.as_bytes(), salt)
        .map_err(|e| anyhow::anyhow!("argon2 v1 error: {e}"))?;
    let hash_bytes = hash
        .hash
        .ok_or_else(|| anyhow::anyhow!("argon2 hash missing"))?;
    let hash_slice = hash_bytes.as_bytes();
    if hash_slice.len() < 32 {
        return Err(anyhow::anyhow!(
            "argon2 hash too short: {} bytes",
            hash_slice.len()
        ));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&hash_slice[..32]);
    Ok(key)
}

// ---------------------------------------------------------------------------
// v2 KDF (hash_password_into — proper KDF API, raw salt)
// ---------------------------------------------------------------------------

fn derive_key_v2(passphrase: &str, salt: &[u8], params: &KdfParams) -> Result<[u8; 32]> {
    let argon2_params = argon2::Params::new(
        params.m_cost,
        params.t_cost as u32,
        params.p_cost as u32,
        Some(params.output_len as usize),
    )
    .map_err(|e| anyhow::anyhow!("argon2 v2 params: {e}"))?;
    let argon2 = Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2_params,
    );
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow::anyhow!("argon2 v2 KDF: {e}"))?;
    Ok(key)
}

// ---------------------------------------------------------------------------
// File I/O helpers
// ---------------------------------------------------------------------------

fn generate_raw_salt() -> Result<[u8; 16]> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("RNG error: {e}"))?;
    Ok(salt)
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("enc.tmp");
    std::fs::write(&tmp_path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Store (always writes v2)
// ---------------------------------------------------------------------------

/// Encrypt secret_hex with passphrase, write to enc_path in v2 format.
pub fn store_key_file(enc_path: &Path, passphrase: &str, secret_hex: &str) -> Result<()> {
    let params = KdfParams::v2_default();
    let salt = generate_raw_salt()?;
    let key_bytes = derive_key_v2(passphrase, &salt, &params)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| anyhow::anyhow!("aes-gcm init: {e}"))?;
    let nonce = Aes256Gcm::generate_nonce(&mut AesOsRng);
    let ciphertext = cipher
        .encrypt(&nonce, secret_hex.as_bytes())
        .map_err(|e| anyhow::anyhow!("aes-gcm encrypt: {e}"))?;

    let mut data = Vec::with_capacity(V2_HEADER_LEN + ciphertext.len());
    data.extend_from_slice(V2_MAGIC); // 4
    data.push(V2_VERSION); // 1
    data.extend_from_slice(&params.m_cost.to_le_bytes()); // 4
    data.push(params.t_cost); // 1
    data.push(params.p_cost); // 1
    data.push(params.output_len); // 1
    data.extend_from_slice(&salt); // 16
    data.extend_from_slice(nonce.as_slice()); // 12
    debug_assert_eq!(data.len(), V2_HEADER_LEN);
    data.extend_from_slice(&ciphertext);

    write_atomic(enc_path, &data)
}

/// Write a v1-format file (only for tests — backward compat verification).
#[cfg(test)]
fn store_key_file_v1(enc_path: &Path, passphrase: &str, secret_hex: &str) -> Result<()> {
    let salt = SaltString::generate(&mut AesOsRng);
    let key_bytes = derive_key_v1(passphrase, &salt)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| anyhow::anyhow!("aes-gcm init: {e}"))?;
    let nonce = Aes256Gcm::generate_nonce(&mut AesOsRng);
    let ciphertext = cipher
        .encrypt(&nonce, secret_hex.as_bytes())
        .map_err(|e| anyhow::anyhow!("aes-gcm encrypt: {e}"))?;

    let salt_str = salt.as_str();
    assert_eq!(salt_str.len(), V1_SALT_LEN);

    let mut data = Vec::with_capacity(V1_SALT_LEN + V1_NONCE_LEN + ciphertext.len());
    data.extend_from_slice(salt_str.as_bytes());
    data.extend_from_slice(nonce.as_slice());
    data.extend_from_slice(&ciphertext);

    write_atomic(enc_path, &data)
}

// ---------------------------------------------------------------------------
// Load (v1/v2 auto-detection)
// ---------------------------------------------------------------------------

/// Decrypt key.enc file with passphrase.
/// Returns (secret_hex, was_v1) — caller should migrate v1 → v2.
pub fn load_key_file(enc_path: &Path, passphrase: &str) -> Result<(Zeroizing<String>, bool)> {
    let data = std::fs::read(enc_path).context("reading key.enc")?;
    if data.starts_with(V2_MAGIC) {
        Ok((load_v2(&data, passphrase)?, false))
    } else {
        Ok((load_v1(&data, passphrase)?, true))
    }
}

fn load_v1(data: &[u8], passphrase: &str) -> Result<Zeroizing<String>> {
    if data.len() < V1_SALT_LEN + V1_NONCE_LEN {
        return Err(anyhow::anyhow!("key.enc v1 too short"));
    }
    let salt_str = std::str::from_utf8(&data[..V1_SALT_LEN])?;
    let salt =
        SaltString::from_b64(salt_str).map_err(|e| anyhow::anyhow!("invalid v1 salt: {e}"))?;
    let key_bytes = derive_key_v1(passphrase, &salt)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| anyhow::anyhow!("aes-gcm init: {e}"))?;
    let nonce = Nonce::from_slice(&data[V1_SALT_LEN..V1_SALT_LEN + V1_NONCE_LEN]);
    let plaintext = cipher
        .decrypt(nonce, &data[V1_SALT_LEN + V1_NONCE_LEN..])
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong passphrase?"))?;
    to_secret_string(plaintext)
}

fn load_v2(data: &[u8], passphrase: &str) -> Result<Zeroizing<String>> {
    if data.len() < V2_HEADER_LEN {
        return Err(anyhow::anyhow!("key.enc v2 too short"));
    }
    let version = data[4];
    if version != V2_VERSION {
        return Err(anyhow::anyhow!("unsupported key.enc version: {version}"));
    }
    let params = KdfParams {
        m_cost: u32::from_le_bytes(data[5..9].try_into().unwrap()),
        t_cost: data[9],
        p_cost: data[10],
        output_len: data[11],
    };
    let salt = &data[12..28];
    let nonce = Nonce::from_slice(&data[28..40]);
    let ciphertext = &data[40..];

    let key_bytes = derive_key_v2(passphrase, salt, &params)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| anyhow::anyhow!("aes-gcm init: {e}"))?;
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("decryption failed — wrong passphrase?"))?;
    to_secret_string(plaintext)
}

fn to_secret_string(plaintext: Vec<u8>) -> Result<Zeroizing<String>> {
    let secret = String::from_utf8(plaintext).map_err(|e| {
        let _ = e.into_bytes();
        anyhow::anyhow!("decrypted key is not valid UTF-8")
    })?;
    Ok(Zeroizing::new(secret))
}

// ---------------------------------------------------------------------------
// Passphrase
// ---------------------------------------------------------------------------

/// Passphrase account in keychain (separate from secret key account).
const PASSPHRASE_ACCOUNT: &str = "mycel-passphrase";

fn get_passphrase(prompt: &str) -> Result<Zeroizing<String>> {
    // 1. Env var (CI/headless override)
    if let Ok(p) = std::env::var("MYCEL_KEY_PASSPHRASE") {
        return Ok(Zeroizing::new(p));
    }

    // 2. Keychain-cached passphrase (non-interactive agent path)
    if let Ok(entry) = keyring::Entry::new(SERVICE, PASSPHRASE_ACCOUNT)
        && let Ok(p) = entry.get_password()
    {
        return Ok(Zeroizing::new(p));
    }

    // 3. Interactive TTY prompt (human path)
    let p = rpassword::prompt_password(prompt)?;

    // Cache passphrase in keychain for future non-interactive use
    if let Ok(entry) = keyring::Entry::new(SERVICE, PASSPHRASE_ACCOUNT) {
        let _ = entry.set_password(&p); // best-effort, don't fail if keychain unavailable
    }

    Ok(Zeroizing::new(p))
}

// ---------------------------------------------------------------------------
// Key lifecycle
// ---------------------------------------------------------------------------

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

    match try_store_keychain(&secret_hex) {
        Ok(true) => return Ok((keys, StorageBackend::Keychain)),
        Ok(false) => {
            eprintln!("Note: OS keychain not available. Using encrypted file instead.");
        }
        Err(e) => {
            eprintln!("Warning: keychain error: {e}");
            eprintln!("Falling back to encrypted file. Use `mycel init --file` to skip keychain.");
        }
    }

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
/// Auto-migrates v1 key files to v2 on successful unlock.
pub fn load_keys(enc_path: &Path, storage: IdentityStorage) -> Result<Keys> {
    match storage {
        IdentityStorage::Keychain => {
            if let Some(hex) = try_load_keychain()? {
                let secret_hex = Zeroizing::new(hex);
                let keys = Keys::parse(&secret_hex)
                    .map_err(|e| anyhow::anyhow!("invalid key from keychain: {e}"))?;
                return Ok(keys);
            }
            // Keychain configured but key not found — try file fallback
            if enc_path.exists() {
                return load_from_file(enc_path);
            }
            Err(MycelError::NotInitialized.into())
        }
        IdentityStorage::File => {
            if enc_path.exists() {
                return load_from_file(enc_path);
            }
            Err(MycelError::NotInitialized.into())
        }
    }
}

/// Load keys from encrypted file, auto-migrating v1 → v2.
fn load_from_file(enc_path: &Path) -> Result<Keys> {
    let passphrase = get_passphrase("Enter passphrase to unlock your key: ")?;
    let (hex, was_v1) = load_key_file(enc_path, &passphrase)?;
    let keys = Keys::parse(&hex).map_err(|e| anyhow::anyhow!("invalid key from file: {e}"))?;

    // Auto-migrate v1 → v2
    if was_v1 {
        match store_key_file(enc_path, &passphrase, &hex) {
            Ok(()) => tracing::info!("migrated key.enc v1 → v2"),
            Err(e) => tracing::warn!("failed to migrate key.enc to v2: {e}"),
        }
    }

    Ok(keys)
}

/// Check whether a key is already stored (without decrypting).
pub fn is_initialized(enc_path: &Path) -> bool {
    if enc_path.exists() {
        return true;
    }
    if let Ok(entry) = keyring::Entry::new(SERVICE, ACCOUNT) {
        match entry.get_password() {
            Ok(_) => return true,
            Err(keyring::Error::NoEntry) => {}
            Err(_) => {}
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
        assert_eq!(keys.secret_key().to_secret_bytes().len(), 32);
    }

    #[test]
    fn test_v2_roundtrip() {
        let dir = TempDir::new().unwrap();
        let enc_path = dir.path().join("key.enc");
        let passphrase = "test-passphrase-v2";

        let keys = Keys::generate();
        let secret_hex = Zeroizing::new(keys.secret_key().to_secret_hex());

        // Store (v2)
        store_key_file(&enc_path, passphrase, &secret_hex).expect("store v2");
        assert!(enc_path.exists());

        // Verify v2 magic
        let data = std::fs::read(&enc_path).unwrap();
        assert_eq!(&data[..4], V2_MAGIC, "file should start with MYKF magic");
        assert_eq!(data[4], V2_VERSION);

        // Load back
        let (loaded, was_v1) = load_key_file(&enc_path, passphrase).expect("load v2");
        assert!(!was_v1, "should detect as v2");
        assert_eq!(*loaded, *secret_hex, "round-trip must match");

        // Wrong passphrase
        let result = load_key_file(&enc_path, "wrong");
        assert!(result.is_err());
    }

    #[test]
    fn test_v1_roundtrip() {
        let dir = TempDir::new().unwrap();
        let enc_path = dir.path().join("key.enc");
        let passphrase = "test-passphrase-v1";

        let keys = Keys::generate();
        let secret_hex = Zeroizing::new(keys.secret_key().to_secret_hex());

        // Store as v1
        store_key_file_v1(&enc_path, passphrase, &secret_hex).expect("store v1");

        // Verify NO magic (v1 format)
        let data = std::fs::read(&enc_path).unwrap();
        assert_ne!(&data[..4], V2_MAGIC, "v1 file should not have MYKF magic");

        // Load (should detect as v1)
        let (loaded, was_v1) = load_key_file(&enc_path, passphrase).expect("load v1");
        assert!(was_v1, "should detect as v1");
        assert_eq!(*loaded, *secret_hex, "v1 round-trip must match");
    }

    #[test]
    fn test_v1_to_v2_migration() {
        let dir = TempDir::new().unwrap();
        let enc_path = dir.path().join("key.enc");
        let passphrase = "test-migration";

        let keys = Keys::generate();
        let secret_hex = Zeroizing::new(keys.secret_key().to_secret_hex());

        // Create v1 file
        store_key_file_v1(&enc_path, passphrase, &secret_hex).expect("store v1");
        let v1_data = std::fs::read(&enc_path).unwrap();
        assert_ne!(&v1_data[..4], V2_MAGIC);

        // Load (v1) and migrate
        let (hex, was_v1) = load_key_file(&enc_path, passphrase).expect("load v1");
        assert!(was_v1);

        // Re-encrypt as v2 (simulates what load_from_file does)
        store_key_file(&enc_path, passphrase, &hex).expect("migrate to v2");

        // Verify file is now v2
        let v2_data = std::fs::read(&enc_path).unwrap();
        assert_eq!(&v2_data[..4], V2_MAGIC, "file should now be v2");

        // Reload — should work as v2 now
        let (reloaded, was_v1_after) =
            load_key_file(&enc_path, passphrase).expect("load v2 after migration");
        assert!(!was_v1_after, "should detect as v2 after migration");
        assert_eq!(*reloaded, *secret_hex, "key must survive migration");
    }

    #[test]
    fn test_v1_wrong_passphrase_no_change() {
        let dir = TempDir::new().unwrap();
        let enc_path = dir.path().join("key.enc");

        let keys = Keys::generate();
        let secret_hex = Zeroizing::new(keys.secret_key().to_secret_hex());

        store_key_file_v1(&enc_path, "correct", &secret_hex).expect("store v1");
        let original = std::fs::read(&enc_path).unwrap();

        // Try wrong passphrase
        let result = load_key_file(&enc_path, "wrong");
        assert!(result.is_err());

        // File unchanged
        let after = std::fs::read(&enc_path).unwrap();
        assert_eq!(original, after, "file must not change on failed decrypt");
    }

    #[test]
    fn test_v2_params_in_file() {
        let dir = TempDir::new().unwrap();
        let enc_path = dir.path().join("key.enc");

        let keys = Keys::generate();
        let secret_hex = Zeroizing::new(keys.secret_key().to_secret_hex());

        store_key_file(&enc_path, "test", &secret_hex).unwrap();

        let data = std::fs::read(&enc_path).unwrap();
        assert_eq!(&data[..4], V2_MAGIC);
        assert_eq!(data[4], V2_VERSION);

        let m_cost = u32::from_le_bytes(data[5..9].try_into().unwrap());
        assert_eq!(m_cost, V2_M_COST, "m_cost should be stored in file");

        let t_cost = data[9];
        assert_eq!(t_cost, V2_T_COST);

        let p_cost = data[10];
        assert_eq!(p_cost, V2_P_COST);

        let output_len = data[11];
        assert_eq!(output_len, V2_OUTPUT_LEN);
    }

    #[test]
    fn test_argon2_v1_builds() {
        assert!(argon2_v1().is_ok());
    }

    #[test]
    fn test_derive_key_v2_works() {
        let salt = [0u8; 16];
        let params = KdfParams::v2_default();
        let result = derive_key_v2("test", &salt, &params);
        assert!(result.is_ok(), "v2 KDF should work");
        assert_eq!(result.unwrap().len(), 32);
    }

    #[test]
    #[ignore] // Run manually: cargo test benchmark -- --ignored
    fn test_benchmark_v2_latency() {
        let salt = generate_raw_salt().unwrap();
        let params = KdfParams::v2_default();
        let start = std::time::Instant::now();
        let _ = derive_key_v2("benchmark-passphrase", &salt, &params).unwrap();
        let elapsed = start.elapsed();
        eprintln!("v2 KDF latency: {:?}", elapsed);
        assert!(elapsed.as_secs() < 2, "v2 KDF should complete in < 2s");
    }
}
