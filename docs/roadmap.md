# mycel Roadmap

## Pre-scaffold — Spikes (before cargo init)

- [x] Transport spike: nostr-sdk 0.44 + nip59 feature. send_private_msg_to + unwrap_gift_wrap round-trip on nos.lol
- [x] Name: mycel (crates.io free, mycel.run domain)
- [x] Freeze contracts: C1-C7 in architecture.md (send semantics, sync cursor, local encryption, key storage, --json, terminal safety, message size)
- [x] Address format: npub (standard bech32) for v0.1
- [ ] Keyring spike: save/load secret on macOS, verify fallback path
- [ ] LICENSE file
- [ ] GitHub repo

## Phase 0 — Foundation (Week 1)

- [ ] Cargo project setup (single crate for v0.1)
- [ ] Key generation: secp256k1 keypair via nostr-sdk
- [ ] Key encryption at rest (keychain preferred, passphrase+argon2 fallback)
- [ ] SQLite schema (messages, contacts, sync_state, relays)
- [ ] mycel envelope type (serialize/deserialize)
- [ ] `mycel init` — full setup: keygen + relay test + print address
- [ ] `mycel id` — show own address
- [ ] Unit tests for core types and key management

## Phase 1 — Send & Receive (Week 2)

- [ ] Relay connection (connect/disconnect, multi-relay)
- [ ] NIP-44 encryption (via nostr-sdk)
- [ ] NIP-59 Gift Wrap pipeline (Rumor → Seal → Gift Wrap)
- [ ] `mycel send <contact> "message"` — full pipeline
- [ ] `mycel inbox` — fetch, decrypt, display, sync cursor
- [ ] Deduplication by nostr_id
- [ ] Trust tier enforcement (KNOWN/UNKNOWN/BLOCKED)
- [ ] `mycel contacts add/block/list`
- [ ] Integration test: send + receive via public relay

## Phase 2 — Polish & Ship (Week 3)

- [ ] `mycel doctor` — relay health, key status, connectivity test
- [ ] `mycel inbox --json` — machine-readable output for agents
- [ ] `mycel inbox --all` — show quarantined messages
- [ ] Config file (~/.config/mycel/config.toml)
- [ ] Message size cap + clear error on oversized
- [ ] Error handling: relay errors, network timeouts, key unlock failures
- [ ] README + install instructions
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
