---
project: mycel
version: 0.3.0
status: active
created: 2026-03-19
language: Rust
license: MIT
---

# mycel — Encrypted Async Mailbox for AI CLI Agents

## Goal
<!-- evidence: README.md, docs/architecture.md, docs/rfc-local-first-transport-boundary.md -->
<!-- confidence: high -->
Encrypted async mailbox that lets AI CLI agents (Claude Code, Codex CLI, Gemini CLI) exchange private messages across both **same-user local environments** and **remote machines**. Local delivery should stay fast and daemon-free; remote delivery should stay end-to-end encrypted over Nostr relays.

## Problem
<!-- evidence: project docs + current CLI surface -->
<!-- confidence: high -->
AI CLI agents run in isolated terminal sessions with no native mailbox for cross-session or cross-machine coordination. Existing options usually force one of three bad tradeoffs: plaintext local files, centralized APIs/chat platforms, or always-on servers that do not match CLI-native workflows.

## Core Principles
<!-- evidence: docs/architecture.md, docs/rfc-local-first-transport-boundary.md -->
<!-- confidence: high -->
1. **Mailbox, not messenger**: async delivery and durable handoff, not live chat.
2. **Sync-on-command by default**: each command does bounded work and exits.
3. **Local-first**: same-user local delivery should be the simplest and fastest path.
4. **Transport-neutral core**: canonical envelope + router + unified ingress, not transport-specific mailbox state.
5. **Encrypted by default**: remote transport is end-to-end encrypted; local authenticity must be cryptographically verifiable.
6. **Safe inbox**: messages are DATA, never auto-injected as instructions.

## Core Capabilities
<!-- evidence: README.md, CLAUDE.md, docs/roadmap.md -->
<!-- confidence: high -->
- `mycel init` — one-time setup: keygen, encrypted key storage, relay config, self-test, address output
- `mycel id` — display own address for sharing
- `mycel send <contact> "message"` — remote async delivery over Nostr relays
- `mycel send self "message"` / local alias routing — same-user local delivery via SQLite/WAL
- `mycel inbox [--json] [--all] [--local]` — fetch or read normalized mailbox state; `--json` for agent consumption
- `mycel contacts add/block/list` — trust source for senders
- `mycel thread create/send/log` — threaded coordination with stable `msg_id` / `thread_id` / `reply_to`
- `mycel watch` + `mycel status` — foreground poller and runtime status
- outbox retry, ACK support, inbox relay routing, and NIP-77 sync
- machine-readable mailbox output for agent workflows

## Non-Goals
<!-- evidence: docs/architecture.md, docs/rfc-local-first-transport-boundary.md, docs/key-enc-v2.md -->
<!-- confidence: high -->
- No mandatory daemon/listener as the default runtime model
- No A2A task model as core truth inside `mycel`
- No MCP-as-transport or MCP-as-database boundary
- No mandatory `local-gateway` for same-user local delivery
- No NATS JetStream core transport right now
- No capability advertisement inside the core mailbox model
- No replacement for Slack/Discord/email
- Not a general agent orchestration framework
- No Nostr jargon in UX when plain-language terms are enough
- key.enc v2 non-goals: no key rotation, no multiple key slots, no forward secrecy between file versions

## Success Criteria
<!-- evidence: docs/roadmap.md, current product intent -->
<!-- confidence: high -->
- install → init → first successful local or remote message in under 5 minutes
- same-user local agents can exchange messages with no network and no daemon
- remote messaging still works with outbound-only network access
- `msg_id`, `thread_id`, and `reply_to` stay transport-neutral across copies
- inbox/watch/adapters read normalized mailbox state, not transport-specific artifacts
- `cargo test` and `cargo clippy` remain clean at the release baseline

## Personas
<!-- evidence: README.md, docs/architecture.md -->
<!-- confidence: high -->
- **AI Agent (primary)**: Claude Code, Codex CLI, Gemini CLI instance that uses `mycel send` / `mycel inbox --json` for inter-agent communication
- **Developer (same-user, multi-workspace)**: human using mycel to bridge local agents, shells, and sessions on one machine
- **Developer (cross-machine / cross-user)**: human coordinating AI-agent output across laptop, CI, VPS, or another developer via encrypted relay delivery

## Tech Stack
<!-- evidence: CLAUDE.md, Cargo.toml -->
<!-- confidence: high -->
- **Language**: Rust (edition 2024)
- **Nostr**: `nostr-sdk` 0.44
- **Storage**: SQLite via `rusqlite` 0.39 (bundled), WAL mode
- **Async**: `tokio` 1.50; DB ops via `spawn_blocking`
- **CLI**: `clap` 4.6
- **Crypto**: secp256k1 + NIP-44, `keyring`, `argon2`, `aes-gcm`, `zeroize`
- **IDs**: UUID v7
- **Config**: `~/.config/mycel/config.toml`; data: `~/.local/share/mycel/mycel.db`

## Growth Path
<!-- evidence: docs/roadmap.md, docs/rfc-local-first-transport-boundary.md -->
<!-- confidence: high -->
```text
v0.1.x  foundation: init/send/inbox/contacts/doctor
v0.2.x  Envelope v2 + local transport + threads
v0.3.x  watch/status + outbox retry + ACK + NIP-77 + relay routing
next    transport-neutral core boundary: router + unified ingress + endpoint directory
later   optional local-gateway, A2A bridge, MCP outer adapter, richer structured payloads
```

## Prior Art
<!-- evidence: existing project research -->
<!-- confidence: high -->

| Project | What mycel borrows | Gap mycel fills |
|---------|-------------------|----------------|
| hcom | Hook integration pattern | E2E encryption, no broker setup |
| AMP | Envelope format, signing | No provider dependency, simpler mailbox model |
| Nostr | Remote transport layer | Agent-specific UX and safety |
| A2A | Interop boundary ideas (agent cards, tasks/messages/artifacts) | `mycel` stays mailbox-first and daemon-free locally |

## Research
<!-- evidence: internal docs + external notes -->
<!-- confidence: medium -->
- transport boundary + local-first direction: `docs/rfc-local-first-transport-boundary.md`
- implementation plan: `docs/plan-local-agent-mesh.md`
- earlier landscape notes: `docs/research-agent-thread-*.md`
- external protocol notes: `~/vicc/docs/research/2026-03-19-agent-protocols-deep-dive.md`
