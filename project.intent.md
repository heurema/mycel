---
project: mycel
version: 0.0.0
status: pre-code
created: 2026-03-19
language: Rust
license: MIT
---

# mycel — Encrypted Async Mailbox for AI CLI Agents

## One-liner

Encrypted async mailbox that lets AI CLI agents send and receive messages across users and machines.

## Problem

AI CLI agents (Claude Code, Codex CLI, Gemini CLI) run in isolated terminal sessions
with no native mechanism to receive messages from other agents or users. Existing solutions
either poll files, require centralized APIs, or bridge to external chat platforms.

## Solution

mycel is a CLI tool that delivers encrypted messages between agents via decentralized
Nostr relays. No daemon, no server, no registration — each command connects, does its
job, and exits. Like `git push` / `git pull`, but for agent messages.

## Core Principles

1. **Mailbox, not messenger**: async delivery, not live chat
2. **CLI-native**: each command is short-lived (connect → do → exit)
3. **Zero registration**: identity = keypair, generated locally
4. **Encrypted by default**: E2E encryption, relays can't read messages
5. **Decentralized**: public Nostr relays, no single point of failure
6. **Safe inbox**: messages are DATA, never auto-injected as instructions

## Non-Goals

- Not a chat UI or live messenger
- Not an agent orchestration framework
- Not a replacement for Slack/Discord/email
- Not a file sync tool
- Not a daemon-first architecture (v0.1)

## How It Works

```
You (mycel send)                    Alice (mycel inbox)
     │                                  │
  encrypt                               │
  sign                                  │
  wrap in 3 layers                      │
     │                                  │
     ▼                                  ▼
  ┌──────────────────────────────────────┐
  │     Public Nostr Relays              │
  │     (store envelopes, can't read)    │
  │     relay-1.com, relay-2.com, ...    │
  └──────────────────────────────────────┘
                                        │
                                   download
                                   decrypt
                                   verify signature
                                   display
```

Each command: connect → do work → disconnect. No background process.

## v0.1 Scope

```
mycel init                     # generate encrypted keypair, configure relays, self-test
mycel id                       # show own address (for sharing with contacts)
mycel send <contact> "message" # connect → Gift Wrap → publish → exit
mycel inbox [--json]           # connect → fetch → decrypt → display → exit
mycel contacts add <addr>      # local op (allowlist)
mycel contacts block <addr>    # local op
mycel contacts list            # local op
mycel doctor                   # relay health, key status, self-test
```

8 commands. No daemon. No hooks. No MCP. No topics.

## Identity

- One secp256k1 keypair per user (generated on `mycel init`)
- Agent name = label, not separate identity
- Private key encrypted at rest (passphrase or OS keychain)
- Public key = address (npub bech32 format)
- Shareable directly; NIP-05 aliases (user@domain) planned for v0.2+

## Transport: Nostr

Nostr is used as **plumbing**, not as ecosystem integration. Nostr jargon is hidden from the user.

- NIP-44 v2 encryption (ECDH → HKDF-SHA256 → ChaCha20 + HMAC)
- NIP-59 Gift Wrap (3-layer metadata hiding)
- Public relays for zero-config cross-user delivery
- Multi-relay for resilience (message on 3+ relays)
- Relays store encrypted messages until fetched

## Security

- Messages encrypted E2E (relays can't read)
- Every message signed (recipient verifies sender)
- Private key encrypted at rest
- Trust tiers: KNOWN (deliver), UNKNOWN (quarantine), BLOCKED (drop)
- Messages are DATA — safe inbox policy, never auto-execute
- Hard cap on message size

## Tech Stack

- **Language**: Rust
- **Nostr**: nostr-sdk
- **Storage**: SQLite (via rusqlite)
- **Crypto**: secp256k1 + NIP-44 (via nostr-sdk)
- **CLI**: clap
- **Key encryption**: OS keychain (keyring crate) or passphrase (argon2)

## Adoption Wedge

Best early use case: **same user, multi-device** (laptop ↔ VPS, laptop ↔ CI runner).
No second person needed. Then: cross-user between developers.

## Growth Path

```
v0.1  init/send/inbox/contacts/doctor (CLI, sync-on-command)
v0.2  hooks + MCP (agents check inbox automatically)
v0.3  daemon + watch (background process, real-time)
v0.4  topics/channels (pub/sub subscriptions)
v0.5  task delegation (NIP-90 DVM pattern)
v0.6  private team relay
```

Each step adds on top without breaking previous.

## Prior Art

| Project | What mycel borrows | Gap mycel fills |
|---------|-------------------|----------------|
| hcom | Hook integration pattern | E2E encryption, no broker setup |
| AMP | Envelope format, signing | No provider dependency, simpler |
| Nostr | Transport layer | Agent-specific UX and safety |

## Success Metrics

- New user: install → init → send first message in < 5 minutes
- Works behind NAT (outbound WebSocket only)
- Single binary < 15MB
- Zero infrastructure for cross-user messaging

## Research

- [Landscape overview](../../vicc/docs/research/2026-03-19-agent-messenger-pubsub-2026.md)
- [Protocol deep dive](../../vicc/docs/research/2026-03-19-agent-protocols-deep-dive.md)
