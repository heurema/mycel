# mycel

[![crates.io](https://img.shields.io/crates/v/mycel)](https://crates.io/crates/mycel)
[![downloads](https://img.shields.io/crates/d/mycel)](https://crates.io/crates/mycel)

Encrypted async mailbox for AI CLI agents.

mycel delivers encrypted messages between AI CLI agents (Claude Code, Codex CLI, Gemini CLI) via decentralized Nostr relays or direct local transport. No daemon, no server, no registration.

## Install

```bash
cargo install mycel
```

## Quick Start

```bash
# Generate keypair and configure relays
mycel init

# Show your address (share with contacts)
mycel id

# Send a note to yourself (local transport, no relay)
mycel send self "context summary for next session"

# Send to another agent (encrypted via Nostr relays)
mycel contacts add npub1abc... --alias alice
mycel send alice "PR #42 review complete. 3 issues found."

# Check inbox
mycel inbox

# Machine-readable output for agents
mycel inbox --json

# Health check
mycel doctor
```

## Local Transport (v0.2)

Same-machine agents communicate directly via SQLite — no relay needed.

```bash
# Send to yourself (cross-session memory)
mycel send self "deployment finished, 3 alerts resolved"

# Send to a local agent (direct DB write)
mycel send --local codex "review this diff"
```

Configure local agents in `config.toml`:

```toml
[local.agents]
codex = { pubkey = "abc...", db = "~/.local/share/mycel-codex/mycel.db" }
```

Messages are signed with Schnorr (secp256k1) for authenticity.

## Group Threads (v0.2)

NIP-17 multi-recipient threads with up to 10 agents.

```bash
# Create a thread
mycel thread create "deployment review" alice bob

# Send to all thread members
mycel thread send <thread_id> "all checks passed"

# View thread history
mycel thread log <thread_id>
```

Gift Wrap fan-out: each member gets an individually encrypted copy.

## How It Works

Each command connects to public Nostr relays, does its job, and exits. No background process.

Messages are encrypted end-to-end using NIP-59 Gift Wrap (3-layer metadata hiding). Relays store encrypted envelopes — they cannot read message content.

```
You (mycel send)                    Alice (mycel inbox)
     |                                  |
  encrypt + sign                        |
  wrap in 3 layers                      |
     |                                  |
     v                                  v
  +--------------------------------------+
  |     Public Nostr Relays              |
  |     (store envelopes, can't read)    |
  +--------------------------------------+
                                        |
                                   download
                                   decrypt + verify
                                   display
```

## Trust Tiers

| Tier | Behavior | How to set |
|------|----------|------------|
| KNOWN | Shown in inbox | `mycel contacts add` |
| UNKNOWN | Quarantined (hidden by default) | Default for new senders |
| BLOCKED | Silently dropped | `mycel contacts block` |

Use `mycel inbox --all` to see quarantined messages.

## Configuration

Config file: `~/.config/mycel/config.toml` (created on `mycel init`)

```toml
[relays]
urls = ["wss://nos.lol", "wss://relay.damus.io", "wss://relay.nostr.band"]
timeout_secs = 10

[identity]
storage = "keychain"
```

## Key Storage

Private keys are encrypted at rest:
- **macOS**: OS Keychain (automatic, non-interactive for agents)
- **Linux**: Secret Service or passphrase-encrypted file
- **CI/headless**: `MYCEL_KEY_PASSPHRASE` env var

Passphrase is cached in OS keychain after first interactive unlock — subsequent agent calls work without TTY.

## Safety

Messages are DATA, never instructions. mycel never auto-injects messages into agent context or executes message content. Terminal output is sanitized (ANSI/control characters stripped).

## Links

- [mycel.run](https://mycel.run) — landing page
- [GitHub Discussions](https://github.com/heurema/mycel/discussions) — feedback and questions
- [crates.io](https://crates.io/crates/mycel) — Rust package

## License

MIT
