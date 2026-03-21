# key.enc v2 — Format Specification

## Problem

Current `key.enc` format (v1) hardcodes KDF parameters in code:
- Argon2id: m=19456 KiB, t=2, p=1 (OWASP minimum)
- File layout: `[22-byte salt][12-byte nonce][ciphertext]`
- No version marker or parameter storage — changing params breaks existing files

For key-encryption-at-rest in a CLI tool, RFC 9106 recommends stronger params:
m=65536 KiB (64 MiB), t=3, p=4 (matches Bitwarden defaults).

## Goals

1. Increase KDF params to RFC 9106 recommended levels
2. Self-describing format — params stored in file, not hardcoded
3. Backward-compatible read — v1 files auto-detected and decrypted
4. Automatic migration — v1 → v2 on successful unlock (re-encrypt in place)
5. Switch from `PasswordHasher::hash_password()` to `hash_password_into()` (proper KDF API)

## Non-Goals

- Key rotation (separate feature)
- Multiple key slots (LUKS-style)
- Forward secrecy between file versions

## File Format

### v1 (current, implicit)

No header. Raw binary:

```
[22 bytes: salt as base64 ASCII]
[12 bytes: AES-256-GCM nonce]
[N bytes:  ciphertext (encrypted secret key hex)]
```

Detection: file does NOT start with magic bytes `MYKF`.

### v2 (proposed)

```
Offset  Size   Field
0       4      Magic: "MYKF" (0x4D594B46)
4       1      Version: 0x02
5       4      m_cost (KiB), little-endian u32
9       1      t_cost (iterations), u8
10      1      p_cost (parallelism), u8
11      1      output_len, u8 (always 32)
12      16     salt (raw bytes, not base64)
28      12     AES-256-GCM nonce
40      N      ciphertext
```

Header: 40 bytes fixed. Total overhead vs v1: +6 bytes (magic+version+params) but salt shrinks from 22 (base64) to 16 (raw).

### v2 Default Parameters

| Param | Value | Rationale |
|-------|-------|-----------|
| m_cost | 65536 KiB (64 MiB) | RFC 9106 memory-constrained, Bitwarden default |
| t_cost | 3 | RFC 9106 secondary |
| p_cost | 4 | RFC 9106, leverages multi-core |
| output_len | 32 | AES-256 key size |

Expected latency: 200-500ms on Apple Silicon. Must benchmark before shipping.

## API Changes

### derive_key (internal)

```rust
// Before (v1): uses PasswordHasher trait
fn derive_key(passphrase: &str, salt: &SaltString) -> Result<[u8; 32]> {
    let argon2 = argon2_hasher()?;
    let hash = argon2.hash_password(passphrase.as_bytes(), salt)?;
    // ... extract 32 bytes from PHC string hash output
}

// After (v2): uses raw KDF API
fn derive_key_v2(passphrase: &str, salt: &[u8; 16], params: &KdfParams) -> Result<[u8; 32]> {
    let argon2_params = argon2::Params::new(params.m_cost, params.t_cost as u32, params.p_cost as u32, Some(32))?;
    let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, argon2_params);
    let mut key = [0u8; 32];
    argon2.hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow::anyhow!("argon2 KDF: {e}"))?;
    Ok(key)
}
```

### load_key_file

```rust
pub fn load_key_file(enc_path: &Path, passphrase: &str) -> Result<Zeroizing<String>> {
    let data = std::fs::read(enc_path)?;
    if data.starts_with(b"MYKF") {
        load_v2(&data, passphrase)
    } else {
        load_v1(&data, passphrase)
    }
}
```

### store_key_file

Always writes v2 format. No option to write v1.

### Migration on unlock

```rust
// In load_key_file, after successful v1 decrypt:
fn load_v1(data: &[u8], passphrase: &str) -> Result<Zeroizing<String>> {
    let secret = decrypt_v1(data, passphrase)?;
    // Auto-migrate: re-encrypt with v2 params and overwrite
    // Note: requires enc_path, so migration happens at call site
    Ok(secret)
}

// In load_keys() after successful v1 load:
if is_v1 {
    store_key_file(enc_path, passphrase, &secret_hex)?; // writes v2
    tracing::info!("migrated key.enc to v2 format");
}
```

## Migration Flow

```
User runs `mycel send "hello" alice`
  → load_keys(enc_path, storage)
    → load_key_file(enc_path, passphrase)
      → detect v1 (no MYKF magic)
      → decrypt with v1 params (m=19456, t=2, p=1)
      → SUCCESS → return (secret, needs_migration=true)
    → if needs_migration:
      → store_key_file(enc_path, passphrase, &secret) // writes v2
      → log "migrated key.enc v1 → v2"
    → return Keys
  → proceed normally
```

Migration is transparent. User sees no difference except:
- First post-upgrade command takes slightly longer (re-encrypt with stronger params)
- Subsequent commands use v2 params (slightly slower KDF, ~300ms vs ~100ms)

## Risks

1. **Power loss during migration** — atomic write (tmp + rename) mitigates
2. **Wrong passphrase on v1** — decryption fails, no migration attempted
3. **Latency regression** — v2 params are ~3x slower than v1; benchmark on CI targets
4. **p=4 on single-core machines** — still works, just no parallelism benefit

## Testing

1. `test_v1_roundtrip` — create v1 file, load, verify
2. `test_v2_roundtrip` — create v2 file, load, verify
3. `test_v1_to_v2_migration` — create v1, load (triggers migration), verify file is now v2, reload
4. `test_v1_wrong_passphrase_no_migration` — v1 with wrong pass, verify no file change
5. `test_v2_params_in_file` — write v2, read raw bytes, verify params match
6. `test_benchmark_v2_latency` — #[ignore], measure KDF time, assert < 2s

## Implementation Order

1. Add `KdfParams` struct + v2 constants
2. Implement `derive_key_v2` using `hash_password_into`
3. Implement `store_key_file_v2` (v2 writer)
4. Implement `load_v1` / `load_v2` detection + dispatch
5. Add migration logic to `load_keys`
6. Update `store_key_file` to always write v2
7. Benchmark on Apple M-series, adjust params if > 1s
8. Keep v1 reader permanently (old files in the wild)
