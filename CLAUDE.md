# mycel — Encrypted Async Mailbox for AI CLI Agents

@project.intent.md
@project.glossary.json

## Stack

Core: Rust | nostr-sdk | rusqlite | clap | tokio | serde
Crypto: secp256k1 + NIP-44 (via nostr-sdk) | keyring | argon2 | aes-gcm | zeroize
Utils: toml | directories | rpassword | tracing | anyhow | thiserror

## Build & Test

```bash
cargo build            # debug build
cargo build --release  # release build
cargo test             # all tests
cargo clippy           # lint
```

## Architecture

v0.1 = sync-on-command (no daemon). Each CLI command connects, does work, exits.
See docs/architecture.md for full design.

Structure (single crate for v0.1):
```
src/
  main.rs        — CLI entry point (clap)
  cli/           — command handlers (init, send, inbox, contacts, doctor)
  crypto/        — key management, NIP-44 encrypt/decrypt, NIP-59 Gift Wrap
  nostr/         — relay connection, event publishing, subscription
  store/         — SQLite operations (messages, contacts, sync_state)
  envelope.rs    — mycel wire format (serialize/deserialize)
  config.rs      — config file parsing
  error.rs       — error types
```

## Conventions

- Error handling: `anyhow` for CLI, `thiserror` for library modules
- Async: tokio runtime (required by nostr-sdk). DB ops via `spawn_blocking`
- Config: `~/.config/mycel/config.toml`
- Data: `~/.local/share/mycel/mycel.db` (SQLite)
- Keys: OS keychain (keyring crate) or `~/.config/mycel/key.enc` (argon2+AES)
- Nostr jargon hidden from UX — use "address" not "npub", "relay" not "Nostr relay"
