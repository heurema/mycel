# mycel — Encrypted Async Mailbox for AI CLI Agents

@project.intent.md
@project.glossary.json

## Stack

Core: Rust (edition 2024) | nostr-sdk 0.44 | rusqlite 0.39 | clap 4.6 | tokio 1.50 | serde
Crypto: secp256k1 + NIP-44 (via nostr-sdk) | keyring | argon2 | aes-gcm | zeroize
IDs: uuid (v7 feature)
Utils: toml | directories | rpassword | tracing | anyhow | thiserror

## Build & Test

```bash
cargo build            # debug build
cargo build --release  # release build
cargo test             # all tests (67 tests)
cargo clippy           # lint (0 warnings expected)
```

## Architecture

v0.2 = sync-on-command + local transport + threads. Each CLI command connects, does work, exits.
See docs/architecture.md for full design, docs/rfc-v0.2-phase0-contracts.md for v0.2 contracts.

Structure (single crate):
```
src/
  main.rs        — CLI entry point (clap)
  cli/           — command handlers
    init.rs      — keypair generation, relay setup
    send.rs      — send via Nostr relay or local transport (--local, self)
    inbox.rs     — fetch + decrypt + display messages
    contacts.rs  — contact management (add, block, list)
    thread.rs    — NIP-17 group threads (create, send, log)
    doctor.rs    — relay health, key status, connectivity
    watch.rs     — foreground inbox poller
    status.rs    — check if watch is running
    id.rs        — display own address
  crypto/        — key management (keychain + encrypted file), passphrase caching
  nostr/         — relay connection, Gift Wrap publish, multi-recipient fan-out
  store/         — SQLite (messages, contacts, threads, sync_state, migration v1→v2)
  envelope.rs    — Envelope v2 wire format, canonical hash, Schnorr sign/verify
  types.rs       — domain enums, Part, AgentRole, MessageMeta, ThreadMember
  sync.rs        — shared sync core (fetch → decrypt → store)
  config.rs      — config file parsing, [local.agents] section, tilde expansion
  error.rs       — error types (MycelError)
```

## Key Design Decisions

- Envelope v2: msg_id (UUIDv7 String), thread_id, reply_to, role, parts[] (TextPart/DataPart)
- Backward compat: v1 envelopes deserialized via EnvelopeWire adapter, msg→parts auto-conversion
- Local transport: direct SQLite write to recipient DB, Schnorr signed, msg_id dedup
- "self" alias: auto-routes to local transport (no --local flag needed)
- Passphrase: env var → keychain cache → TTY prompt (non-interactive agent path)
- DB migration: idempotent v1→v2, runs on open() when user_version < 2
- UNIQUE index on (msg_id, direction) — allows self-send in+out copies
- Thread fan-out: max 10 members, NIP-17 Gift Wrap per member + self-copy

## Conventions

- Error handling: `anyhow` for CLI, `thiserror` for library modules
- Async: tokio runtime (required by nostr-sdk). DB ops via `spawn_blocking`
- Config: `~/.config/mycel/config.toml` (macOS: `~/Library/Application Support/run.mycel.mycel/`)
- Data: `~/.local/share/mycel/mycel.db` (SQLite, WAL mode)
- Keys: OS keychain (keyring crate) or `~/.config/mycel/key.enc` (argon2id+AES-256-GCM)
- Nostr jargon hidden from UX — use "address" not "npub", "relay" not "Nostr relay"
- Part serde: `#[serde(tag = "type")]` with `#[serde(rename = "text")]` / `#[serde(rename = "data")]`
- No MCP adapter — CLI + JSONL is sufficient for agent integration
