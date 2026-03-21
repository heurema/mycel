---
project: mycel
version: 0.1.0
status: active
created: 2026-03-19
language: Rust
license: MIT
---

# mycel — Encrypted Async Mailbox for AI CLI Agents

## Goal
<!-- evidence: docs/architecture.md:L1-L46, README.md:L1-L6 -->
<!-- confidence: high -->
Encrypted async mailbox that lets AI CLI agents (Claude Code, Codex CLI, Gemini CLI) send and receive private messages across users and machines via decentralized Nostr relays — no daemon, no server, no registration.

## Problem
<!-- evidence: project.intent.md (existing), README.md:L1-L6 -->
<!-- confidence: high -->
AI CLI agents run in isolated terminal sessions with no native mechanism to receive messages from other agents or users. Existing solutions either poll files, require centralized APIs, or bridge to external chat platforms.

## Core Principles
<!-- evidence: project.intent.md (existing):L29-L36, docs/architecture.md:L193-L198 -->
<!-- confidence: high -->
1. **Mailbox, not messenger**: async delivery, not live chat
2. **CLI-native**: each command is short-lived (connect → do → exit)
3. **Zero registration**: identity = keypair, generated locally
4. **Encrypted by default**: E2E encryption, relays can't read messages
5. **Decentralized**: public Nostr relays, no single point of failure
6. **Safe inbox**: messages are DATA, never auto-injected as instructions

## Core Capabilities
<!-- evidence: docs/architecture.md:L36-L46, docs/roadmap.md:L13-L49, CLAUDE.md:L8 -->
<!-- confidence: high -->
- `mycel init` — one-time setup: keygen (secp256k1), encrypt key at rest, configure relays, self-test, print address
- `mycel id` — display own address (npub bech32) for sharing with contacts
- `mycel send <contact> "message"` — NIP-59 Gift Wrap encrypt, multi-relay publish, outbox record
- `mycel inbox [--json] [--all]` — fetch from relays, decrypt, trust-filter, dedup, display; `--json` for agent consumption (JSONL)
- `mycel contacts add/block/list` — local allowlist management with trust tiers
- `mycel doctor` — relay health, key status, connectivity self-test
- NIP-59 Gift Wrap (3-layer encryption): Rumor → Seal → Gift Wrap; hides sender identity from relays
- Trust tier enforcement: KNOWN (inbox), UNKNOWN (quarantine), BLOCKED (drop)
- Per-relay sync cursor with 120-second overlap window for dedup
- Private key encrypted at rest: OS keychain (keyring) or passphrase file (argon2id + AES-256-GCM)
- Terminal safety: ANSI/control character sanitization, 8192-byte message cap
- Machine-readable output: `--json` outputs JSONL on stdout, diagnostics on stderr

## Non-Goals
<!-- evidence: docs/architecture.md:L289-L302, project.intent.md (existing):L38-L43, docs/key-enc-v2.md:L22-L25, CLAUDE.md:L46 -->
<!-- confidence: high -->
- No daemon / background process (v0.1; `mycel watch` planned for v0.3)
- No hooks or MCP integration (v0.1; planned for v0.2)
- No topics / pub/sub channels (planned for v0.4)
- No threads / replies
- No attachments or file transfers
- No task delegation (NIP-90 DVM)
- No agent RPC (NIP-46)
- No NATS transport
- No capability advertisement
- No group messaging
- Not a chat UI or live messenger
- Not an agent orchestration framework
- Not a replacement for Slack/Discord/email
- No Nostr ecosystem jargon exposed in UX (hidden from user: "address" not "npub", "relay" not "Nostr relay")
- key.enc v2 Non-Goals: no key rotation, no multiple key slots, no forward secrecy between file versions

## Success Criteria
<!-- evidence: project.intent.md (existing):L147-L151, docs/roadmap.md -->
<!-- confidence: high -->
- New user: install → init → send first message in < 5 minutes
- Works behind NAT (outbound WebSocket only, no inbound port required)
- Single binary < 15MB
- Zero infrastructure required for cross-user messaging (public relays only)
- 48 passing tests, clippy clean (current baseline)
- Phase 0, 1, 2 complete per roadmap.md

## Personas
<!-- evidence: README.md:L1-L6, project.intent.md (existing):L121-L122, docs/architecture.md -->
<!-- confidence: high -->
- **AI Agent (primary)**: Claude Code, Codex CLI, Gemini CLI instance that uses `mycel send` / `mycel inbox --json` for inter-agent communication; consumes JSONL output programmatically
- **Developer (same-user, multi-device)**: human using mycel to bridge laptop ↔ VPS or laptop ↔ CI runner; adoption wedge before cross-user use case
- **Developer (cross-user)**: two developers exchanging AI agent outputs or review notes via encrypted relay

## Tech Stack
<!-- evidence: CLAUDE.md:L8-L10, Cargo.toml:L17-L48 -->
<!-- confidence: high -->
- **Language**: Rust (edition 2024)
- **Nostr**: nostr-sdk 0.44 (nip59 feature)
- **Storage**: SQLite via rusqlite 0.39 (bundled), WAL mode
- **Async**: tokio 1.50 (rt-multi-thread); DB ops via `spawn_blocking` (Db wrapper)
- **CLI**: clap 4.6 (derive)
- **Crypto**: secp256k1 + NIP-44 (via nostr-sdk), keyring 3.6, argon2 0.5, aes-gcm 0.10, zeroize 1.8
- **Error handling**: `anyhow` for CLI entry points, `thiserror` for domain modules (MycelError)
- **Types**: `types.rs` — domain enums (Direction, TrustTier, DeliveryStatus, ReadStatus) with rusqlite ToSql/FromSql via `str_enum!` macro
- **Config**: `~/.config/mycel/config.toml`; data: `~/.local/share/mycel/mycel.db`

## Growth Path
<!-- evidence: project.intent.md (existing):L126-L136, docs/roadmap.md:L52-L68 -->
<!-- confidence: high -->
```
v0.1  init/send/inbox/contacts/doctor (CLI, sync-on-command) — DONE
      Internal: types.rs enums, Db async wrapper, thiserror error types (v0.2 refactoring)
v0.2  hooks + MCP (agents check inbox automatically; NIP-05 aliases)
v0.3  daemon + watch (background process, real-time)
v0.4  topics/channels (pub/sub subscriptions)
v0.5  task delegation (NIP-90 DVM pattern)
v0.6  private team relay
```

## Prior Art
<!-- evidence: project.intent.md (existing):L139-L145 -->
<!-- confidence: high -->

| Project | What mycel borrows | Gap mycel fills |
|---------|-------------------|----------------|
| hcom | Hook integration pattern | E2E encryption, no broker setup |
| AMP | Envelope format, signing | No provider dependency, simpler |
| Nostr | Transport layer | Agent-specific UX and safety |

## Research
<!-- evidence: project.intent.md (existing):L154-L157 -->
<!-- confidence: medium -->
- Landscape overview: `~/vicc/docs/research/2026-03-19-agent-messenger-pubsub-2026.md` (external — vicc workspace)
- Protocol deep dive: `~/vicc/docs/research/2026-03-19-agent-protocols-deep-dive.md` (external — vicc workspace)
