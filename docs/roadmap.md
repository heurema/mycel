# mycel Roadmap

## Pre-scaffold — Spikes (before cargo init)

- [x] Transport spike: nostr-sdk 0.44 + nip59 feature. send_private_msg_to + unwrap_gift_wrap round-trip on nos.lol
- [x] Name: mycel (crates.io free, mycel.run domain)
- [x] Freeze contracts: C1-C7 in architecture.md (send semantics, sync cursor, local encryption, key storage, --json, terminal safety, message size)
- [x] Address format: npub (standard bech32) for v0.1
- [x] Keyring spike: verified via Phase 0 implementation (keychain + file fallback work)
- [x] LICENSE file (MIT)
- [x] GitHub repo

## Phase 0 — Foundation (DONE — 2026-03-20)

- [x] Cargo project setup (single crate)
- [x] Key generation: secp256k1 keypair via nostr-sdk
- [x] Key encryption at rest (keychain preferred, passphrase+argon2 fallback)
- [x] SQLite schema (messages, contacts, sync_state, relays)
- [x] mycel envelope type (serialize/deserialize)
- [x] `mycel init` — keygen + encrypt + store + DB + config + relay test + print npub
- [x] `mycel id` — load key + print npub
- [x] 13 unit tests (keygen, key storage, roundtrip, DB, config, relay, no-overwrite, id)
- [x] Clippy clean, audit fixes (key file 0600, atomic write, init order, keychain fallback UX)

## Phase 1 — Send & Receive (DONE — 2026-03-20)

- [x] Relay connection (connect/disconnect, multi-relay)
- [x] NIP-44 encryption (via nostr-sdk)
- [x] NIP-59 Gift Wrap pipeline (Rumor → Seal → Gift Wrap)
- [x] `mycel send <contact> "message"` — full pipeline
- [x] `mycel inbox` — fetch, decrypt, display, sync cursor
- [x] Deduplication by nostr_id
- [x] Trust tier enforcement (KNOWN/UNKNOWN/BLOCKED)
- [x] `mycel contacts add/block/list`
- [x] Receive-side security: size validation, terminal sanitization of all fields, event-based cursor
- [x] 41 tests, clippy clean
- [ ] Integration test: send + receive via public relay

## Phase 2 — Polish & Ship (Week 3)

- [x] `mycel doctor` — relay health, key status, connectivity test
- [x] `mycel inbox --json` — machine-readable output for agents
- [x] `mycel inbox --all` — show quarantined messages
- [x] Config file (~/.config/mycel/config.toml)
- [x] Message size cap + clear error on oversized
- [x] README + install instructions
- [ ] Error handling: relay errors, network timeouts, key unlock failures
- [ ] GitHub repo
- [ ] Release binary (cargo-dist or manual)
- [ ] Publish to crates.io

## Future (v0.2+)

- [ ] `mycel watch` — long-running inbox monitor (proto-daemon)
- [ ] Claude Code hooks (PostToolUse → inbox check)
- [ ] MCP server (mycel_inbox/mycel_send tools)
- [ ] Daemon mode (background relay connection)
- [ ] Topics/channels (pub/sub via #t tags)
- [ ] Threads (in_reply_to chains)
- [ ] Structured payloads (JSON messages)
- [ ] Task delegation (NIP-90 DVM)
- [ ] Agent RPC (NIP-46 pattern)
- [ ] NATS JetStream (local/team transport)
- [ ] Private team relay
- [ ] NIP-05 aliases (agent@domain)
- [ ] Key recovery (seed phrase)
- [ ] Multi-device key sync
