// Key management: generation, encryption at rest, loading
//
// Phase 0 implementation:
// - Generate secp256k1 keypair via nostr-sdk
// - Store encrypted via keyring (macOS/Linux) or passphrase file (argon2id + AES-256-GCM)
// - MYCEL_KEY_PASSPHRASE env var for headless/CI
