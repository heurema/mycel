# mycel Architecture (v0.1)

## Design: Sync-on-Command

No daemon. Each CLI command is a short-lived process:

```
mycel send alice "msg"
  │
  ├── read encrypted key from disk → unlock (passphrase/keychain)
  ├── connect to configured relays (WebSocket)
  ├── build mycel envelope → NIP-44 encrypt → NIP-59 Gift Wrap
  ├── publish to relays
  ├── store in local SQLite outbox
  ├── disconnect
  └── exit

mycel inbox
  │
  ├── read encrypted key from disk → unlock
  ├── connect to configured relays
  ├── REQ filter: kind 1059 (Gift Wrap) where p-tag = own pubkey, since = last_sync
  ├── receive events → EOSE (end of stored events)
  ├── for each event: unwrap Gift Wrap → decrypt Seal → decrypt Rumor
  ├── verify sender signature
  ├── check trust tier (KNOWN → deliver, UNKNOWN → quarantine, BLOCKED → drop)
  ├── store in local SQLite inbox
  ├── update last_sync cursor
  ├── disconnect
  ├── display messages
  └── exit
```

## Components

### 1. mycel CLI (single binary)

```
mycel init                      # one-time setup
mycel send <contact> "message"  # send private message
mycel inbox [--json]            # fetch and display messages
mycel contacts add/block/list   # manage allowlist
mycel doctor                    # health check
```

No subcommand groups beyond this. Flat, simple.

### 2. Local Storage (SQLite)

Single file: `~/.local/share/mycel/mycel.db`

```sql
CREATE TABLE messages (
    nostr_id         TEXT PRIMARY KEY,     -- Nostr event id (dedup + PK)
    direction        TEXT NOT NULL,        -- 'in' | 'out'
    sender           TEXT NOT NULL,        -- sender pubkey hex
    recipient        TEXT NOT NULL,        -- recipient pubkey hex
    content          TEXT NOT NULL,        -- decrypted message text
    delivery_status  TEXT NOT NULL,        -- in: 'received' | out: 'delivered' | 'failed'
    read_status      TEXT NOT NULL,        -- 'unread' | 'read'
    created_at       TEXT NOT NULL,        -- sender timestamp
    received_at      TEXT NOT NULL         -- local receipt time
);

CREATE TABLE contacts (
    pubkey      TEXT PRIMARY KEY,
    alias       TEXT,
    trust_tier  TEXT NOT NULL DEFAULT 'unknown',  -- known | unknown | blocked
    added_at    TEXT NOT NULL
);

CREATE TABLE sync_state (
    relay_url   TEXT PRIMARY KEY,
    last_sync   INTEGER NOT NULL DEFAULT 0  -- Unix timestamp for `since` filter
);

CREATE TABLE relays (
    url         TEXT PRIMARY KEY,
    enabled     BOOLEAN DEFAULT TRUE
);
```

### 3. Key Storage

Private key encrypted at rest:

**Option A: OS keychain** (preferred)
- macOS Keychain, Linux Secret Service, Windows Credential Manager
- via `keyring` crate
- No passphrase prompt on each command

**Option B: Passphrase-encrypted file**
- `~/.config/mycel/key.enc`
- Argon2id KDF → AES-256-GCM
- Prompt on each command (or cache via env var / agent)

### 4. Encryption Pipeline (send)

```
message text
  ↓ build mycel envelope JSON {"v":1, "from":..., "to":..., "msg":..., "ts":...}
  ↓ NIP-44 encrypt to recipient pubkey
  ↓ wrap as Rumor (unsigned event, kind 14)
  ↓ NIP-44 encrypt to recipient
  ↓ Seal (kind 13, signed by sender's real key)
  ↓ NIP-44 encrypt to recipient
  ↓ Gift Wrap (kind 1059, signed with EPHEMERAL random key, p-tag = recipient)
  ↓ publish to recipient's relays
```

Relay sees: ephemeral sender key + recipient pubkey. Nothing else.

### 5. Decryption Pipeline (inbox)

```
kind 1059 event from relay
  ↓ decrypt outer layer (NIP-44, our key + ephemeral sender)
  ↓ extract Seal (kind 13)
  ↓ decrypt seal (NIP-44, our key + real sender)
  ↓ extract Rumor (kind 14)
  ↓ parse mycel envelope from content
  ↓ verify: sender pubkey matches contact?
  ↓ trust tier check
  ↓ dedup by nostr_id
  ↓ store in SQLite
```

### 6. Wire Format (mycel envelope)

Carried inside Nostr event content (encrypted):

```json
{
  "v": 1,
  "from": "<sender-pubkey-hex>",
  "to": "<recipient-pubkey-hex>",
  "msg": "Review of PR #42 complete. 3 issues found.",
  "ts": "2026-03-19T15:00:00Z"
}
```

ID = Nostr event id (outer Gift Wrap). No separate envelope ID needed.
Minimal. No threads, no types, no context — v0.1 is just text messages.

### 7. Relay Management

- Default relay preset shipped with binary (3-5 tested public relays)
- `mycel init` tests connectivity to each relay
- `mycel doctor` re-tests and reports status
- Config: `~/.config/mycel/config.toml`

```toml
[relays]
urls = [
  "wss://relay.damus.io",
  "wss://nos.lol",
  "wss://relay.nostr.band"
]

[identity]
# pubkey derived from stored key
storage = "keychain"  # or "file"
```

### 8. Contact Exchange

User-facing: npub address (standard Nostr bech32):

```bash
$ mycel id
Your address: npub1abc123...

# Copy and share with contacts.
# NIP-05 aliases (user@domain) planned for v0.2+
```

```bash
$ mycel contacts add npub1def456... --alias alice
Added alice (npub1def456...)
```

### 9. Trust Tiers

| Tier | Behavior | How to set |
|------|----------|------------|
| KNOWN | Messages shown in inbox | `mycel contacts add` |
| UNKNOWN | Stored but hidden (quarantine) | Default for new senders |
| BLOCKED | Silently dropped, logged | `mycel contacts block` |

`mycel inbox` shows KNOWN only. `mycel inbox --all` shows quarantine too.

### 10. Safety Policy

Messages from other agents/users are **DATA**, never instructions:
- mycel never auto-injects messages into agent context
- mycel never executes message content
- `--json` output for agent consumption is clearly marked as external data
- Rate limiting: max messages per unknown sender before auto-block

## Contracts (frozen before scaffold)

### C1: Send Success Semantics

`mycel send` publishes to ALL configured relays in parallel. Timeout: 10 seconds per relay.

| Outcome | Exit code | Outbox delivery_status | User message |
|---------|-----------|------------------------|-------------|
| ≥1 relay accepted (OK) | 0 | `delivered` | "Sent (N/M relays)" |
| 0 relays accepted | 1 | `failed` | "Failed: no relay accepted" |
| Partial (some OK, some error) | 0 | `delivered` | "Sent (N/M relays, K failed)" |
| Network error (all unreachable) | 1 | `failed` | "Failed: no relays reachable" |
| Timeout (no ack within 10s) | 1 | `failed` | "Failed: relay timeout" |

Rule: **1 relay ack = success.** Return immediately after first ack, don't wait for others.
Always write to outbox (both success and failure, for audit trail).
Outbox uses `delivery_status` (delivered/failed), not inbox's `read_status` (pending/read).

### C2: Sync Cursor and Dedup

`mycel inbox` fetches with `since = last_sync - overlap_window`.

- `overlap_window` = 120 seconds (covers relay propagation delay)
- Dedup by `nostr_id` (UNIQUE constraint in SQLite, INSERT OR IGNORE)
- `last_sync` updated to `now()` AFTER successful EOSE from each relay
- Per-relay cursor (different relays may lag differently)

This means: some events fetched twice → deduped locally. Best-effort: covers most clock skew and relay lag scenarios within 120s window.

### C3: Local Storage Encryption

v0.1 decision: **encrypted in transit, plaintext at rest in SQLite.**

Rationale:
- Key is already encrypted at rest (keychain or passphrase file)
- Encrypting individual DB rows adds complexity with no clear threat model improvement
  (if attacker has disk access, they can also extract decrypted key from memory)
- SQLite file permissions: 0600 (owner only)
- Documented honestly in README: "messages are encrypted in transit; local database is not encrypted"

Future: SQLite encryption extension (SQLCipher) or per-row encryption in v0.2+.

### C4: Key Storage Default

| Environment | Default | Fallback |
|-------------|---------|----------|
| macOS | Keychain (via `keyring`) | Passphrase file |
| Linux desktop | Secret Service (via `keyring`) | Passphrase file |
| Linux headless / CI | Passphrase file | Env var `MYCEL_KEY_PASSPHRASE` |

Detection: `mycel init` tries keychain first.
- Keychain **unavailable** (no backend) → auto-fallback to passphrase file + inform user.
- Keychain **error** (denied/locked) → show error, suggest `mycel init --file` to force file backend.
- Keychain **available** → use it silently.

Passphrase file: `~/.config/mycel/key.enc` (argon2id KDF → AES-256-GCM).

CI/headless mode: `MYCEL_KEY_PASSPHRASE` env var skips interactive prompt.

### C5: --json Output Contract

`mycel inbox --json` outputs one JSON object per line (JSONL):

```json
{"v":1,"nostr_id":"abc...","from":"npub1...","content":"message text","ts":"2026-03-20T10:00:00Z","status":"pending"}
```

- stdout = machine data only (JSONL)
- stderr = diagnostics, warnings, errors
- `--json` flag suppresses all human-friendly formatting
- `"v":1` embedded in each line (schema versioning)
- `--json` content is **raw** (not sanitized) — agents handle their own safety
- `--json` + `--raw` is redundant (json always raw)

### C6: Terminal Safety

Incoming message content is sanitized before display (human mode only, not --json):
- Strip ANSI escape sequences
- Strip control characters (except \n)
- Truncate at **8192 bytes** with "[truncated]" marker (matches C7 cap)
- `--raw` flag to disable sanitization (explicit opt-in)

### C7: Message Size Cap

- Max message **input** payload: 8192 bytes (UTF-8 text, before encryption)
- `mycel send` rejects oversized with clear error and byte count
- Post-encryption overhead (~2-3x for Gift Wrap) stays within typical relay limits (16-64KB)
- Relay-specific limits handled gracefully: if relay rejects, report which relay and why

## What v0.1 Does NOT Have

- No daemon / background process
- No hooks / MCP integration
- No topics / pub/sub channels
- No threads / replies
- No attachments / files
- No task delegation (NIP-90)
- No agent RPC (NIP-46)
- No NATS transport
- No capability advertisement
- No group messaging

All of these are v0.2+ additions that build on top of v0.1 foundation.
