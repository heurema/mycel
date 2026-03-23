# RFC: mycel v0.2 Phase 0 — Core Contracts

Date: 2026-03-23
Status: draft
Quorum: Gemini APPROVE@0.95, Codex BLOCK→fixed (signature scope + msg_id type)

## Overview

This RFC defines the contracts that must be frozen before any v0.2 code is written.
All contracts are specification-level — no implementation is prescribed.

## Contract 1: Message Identity

### Problem

v0.1 uses `nostr_id` (outer Gift Wrap event ID) as the sole message identifier.
In v0.2, one logical message produces N transport events (NIP-17 fan-out per member)
and may arrive via different transports (Nostr vs local SQLite). The single-ID model
breaks `reply_to`, dedup, self-copy, and cross-transport thread reconstruction.

### Contract

Every message has two identity layers:

| Field | Type | Source | Purpose |
|-------|------|--------|---------|
| `msg_id` | String (opaque) | Sender-generated, inside envelope | Logical identity. Stable across all copies. |
| `transport_msg_id` | String | Transport-assigned (event ID or local row ID) | Per-copy receipt. Not referenced by other messages. |

**`msg_id` generation rules:**
- New messages (v2+): UUIDv7 (monotonic timestamp + random, globally unique)
- Legacy backfill (v1→v2 migration): `legacy:<nostr_id>` prefix
- Type contract: `String`, not `Uuid`. Code must not assume UUIDv7 format.

**`reply_to` always references `msg_id`, never `transport_msg_id`.**

### Invariants

- `msg_id` is immutable once assigned by sender
- `msg_id` is identical in all copies of the same message (all fan-out events, self-copy)
- `INSERT OR IGNORE` dedup key is `msg_id`, not `transport_msg_id`
- Sorting by `msg_id` (UUIDv7) approximates chronological order for new messages

## Contract 2: Envelope v2

### Schema

```json
{
  "v": 2,
  "msg_id": "019d1a2b-3c4d-7e5f-8a9b-0c1d2e3f4a5b",
  "from": "<sender-pubkey-hex>",
  "ts": "2026-03-23T10:00:00Z",
  "thread_id": "<sha256-of-topic-or-null>",
  "reply_to": "<parent-msg_id-or-null>",
  "role": "reviewer",
  "parts": [
    {"type": "text", "text": "Review complete. 3 issues found."}
  ]
}
```

### Fields

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `v` | yes | u8 | `2` for this version |
| `msg_id` | yes | String | Sender-generated. See Contract 1. |
| `from` | yes | String | Sender pubkey hex |
| `ts` | yes | String | ISO 8601 UTC |
| `thread_id` | no | String | Groups messages into a thread. `sha256(topic_string)` or UUID. |
| `reply_to` | no | String | Parent `msg_id`. Required for non-root thread messages. |
| `role` | no | String | `user` / `agent` / `coordinator` / `reviewer` / `implementer` |
| `parts` | yes | Array | At least one Part. See below. |

**Removed from v1:** `to` field (recipient is in NIP-59 p-tag or local config, not envelope).
**Removed from v1:** `msg` field (replaced by `parts`).

### Part types

```
TextPart:  {"type": "text", "text": "..."}
DataPart:  {"type": "data", "mime_type": "...", "data": {...}}
```

DataPart is reserved for v0.4 structured payloads (verdicts, artifacts).
v0.2 uses TextPart only.

### Backward compatibility

Reader logic:
1. Parse JSON. Check `v` field.
2. If `v == 1` or `v` absent: read `msg` field as content, `from`/`to`/`ts` as before.
   Generate `msg_id = "legacy:<nostr_id>"` if not present.
3. If `v == 2`: read `parts`, `msg_id`, `thread_id`, `reply_to`, `role`.
4. Unknown `v` values: accept, store raw, display content best-effort.

Writer logic:
- Always write v2 envelope for new messages.
- Never downgrade to v1.

## Contract 3: Local Authenticity

### Problem

Nostr transport provides authenticity via NIP-59 Gift Wrap (sender signs the Seal).
Local transport (direct SQLite write) has no cryptographic layer — any local process
can write to another agent's DB. Without authenticity, local messages are untrusted.

### Contract

Local messages carry a `sig` field — secp256k1 Schnorr signature over the **canonical
envelope hash**.

**Canonical envelope hash computation:**
1. Serialize envelope to JSON with keys sorted alphabetically (deterministic)
2. Remove `sig` field if present
3. SHA-256 hash of the canonical JSON bytes
4. Sign the hash with sender's secp256k1 secret key (Schnorr, same as Nostr event signing)

**Signed fields:** ALL envelope fields (`v`, `msg_id`, `from`, `ts`, `thread_id`,
`reply_to`, `role`, `parts`) are inside the canonical hash. Nothing is excluded.
This prevents tampering of metadata (`thread_id`, `reply_to`, `role`, `ts`) on
the local delivery path.

**Verification:**
1. Recipient reads `from` field → looks up pubkey in contacts
2. Recomputes canonical hash (same algorithm)
3. Verifies Schnorr signature against sender pubkey
4. If verification fails → message rejected (not stored), logged to stderr

**For Nostr transport:** `sig` field is redundant (NIP-59 provides authenticity).
Envelope MAY omit `sig` when sent via Nostr. Reader checks: if `transport == "nostr"`,
skip sig verification (already done by NIP-59 unwrap).

### Wire format (local envelope with sig)

```json
{
  "v": 2,
  "msg_id": "019d1a2b-...",
  "from": "pubkey-hex",
  "ts": "2026-03-23T10:00:00Z",
  "thread_id": "abc...",
  "reply_to": null,
  "role": "reviewer",
  "parts": [{"type": "text", "text": "LGTM"}],
  "sig": "schnorr-signature-hex-128-chars"
}
```

## Contract 4: DB Migration (user_version 1 → 2)

### New columns

```sql
-- Migration script (idempotent, runs on open if user_version < 2)
ALTER TABLE messages ADD COLUMN msg_id TEXT;
ALTER TABLE messages ADD COLUMN thread_id TEXT;
ALTER TABLE messages ADD COLUMN reply_to TEXT;
ALTER TABLE messages ADD COLUMN transport TEXT NOT NULL DEFAULT 'nostr';
ALTER TABLE messages ADD COLUMN transport_msg_id TEXT;

-- Backfill: legacy rows get msg_id derived from nostr_id
UPDATE messages SET msg_id = 'legacy:' || nostr_id WHERE msg_id IS NULL;
UPDATE messages SET transport_msg_id = nostr_id WHERE transport_msg_id IS NULL;

-- New primary key: msg_id (after backfill, all rows have msg_id)
-- Note: SQLite cannot change PK. Create new table, copy, swap.
-- Implementation decides: rename + recreate, or keep nostr_id as PK
-- and add UNIQUE index on msg_id. Recommend: keep nostr_id PK for v0.2,
-- add UNIQUE(msg_id) index. Revisit in v0.3.

CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id);
CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);

-- New table for threads
CREATE TABLE IF NOT EXISTS threads (
    thread_id   TEXT PRIMARY KEY,
    subject     TEXT,
    members     TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

PRAGMA user_version = 2;
```

### Migration invariants

- Migration is idempotent (safe to run multiple times)
- All existing v1 messages get `msg_id = "legacy:<nostr_id>"`
- All existing v1 messages get `transport = "nostr"`, `transport_msg_id = <nostr_id>`
- No data is deleted during migration
- If migration fails mid-way, next open retries (checks user_version)
- `inbox --json` output includes `msg_id` for v2 messages, omits for v1 (or includes legacy ID)

## Contract 5: Local Transport Topology

### Decision: per-recipient DB direct write

Each agent has its own `mycel.db`. Local delivery writes directly into the
recipient's DB file. No shared blackboard, no coordinator process.

### How it works

```
Sender: mycel send --local codex "review complete"
  1. Load sender keys
  2. Build Envelope v2 with msg_id (UUIDv7)
  3. Sign envelope (Contract 3 — canonical hash + Schnorr)
  4. Resolve recipient's DB path from config
  5. Open recipient's mycel.db (WAL mode, busy_timeout=10000)
  6. INSERT OR IGNORE into messages (msg_id as dedup key)
  7. Close connection
```

### Config

```toml
# ~/.config/mycel/config.toml
[local.agents]
codex = { pubkey = "abc...", db = "~/.local/share/mycel-codex/mycel.db" }
gemini = { pubkey = "def...", db = "~/.local/share/mycel-gemini/mycel.db" }
```

### Why not shared blackboard

- Single-writer per DB avoids POSIX lock contention
- Matches existing architecture (each mycel instance owns its DB)
- No new processes (no coordinator daemon)
- WAL mode handles concurrent reader (inbox) + writer (send) safely

### Constraints

- Sender must have filesystem write access to recipient's DB
- Both must be same OS user (permissions 0o600 on DB)
- `BEGIN IMMEDIATE` for all write transactions (avoid read→write upgrade SQLITE_BUSY)

## Contract 6: NIP-17 Remote Threading

### Wire protocol

Thread messages use NIP-17 group DM format:
- Kind 14 (chat message rumor, unsigned)
- `p` tags for all thread members
- `subject` tag for thread topic (first message only, or on rename)
- `e` tag for reply chain (parent event ID within NIP-17)
- Custom `mycel-thread-id` tag for stable thread identity
- Gift Wrap (Kind 1059) to each member individually + self-copy

### Custom tags (inside encrypted rumor, invisible to relays)

```json
["mycel-thread-id", "<thread_id from envelope>"],
["mycel-msg-id", "<msg_id from envelope>"]
```

These tags allow mycel to:
- Track threads across NIP-17 room changes (member add/remove)
- Map NIP-17 event IDs back to logical msg_ids for `reply_to` resolution

### Fan-out budget

v0.2 supports threads with up to 10 agents (pairwise NIP-17 wrapping).
At 10 agents × 50 messages = 5000 events, 15.5 MB relay storage.
STK (shared thread key) deferred to v0.3+ if needed.

## Contract 7: Send-to-Self

### Purpose

First acceptance test for local transport and Envelope v2.

### Behavior

```bash
mycel send self "context summary for next session"
```

- `self` is a reserved alias = sender's own pubkey
- Transport: local (writes to own DB, no relay)
- Envelope: v2, `thread_id = null`, `reply_to = null`
- Appears in `mycel inbox` as a regular message (direction: "in", sender: self)

### Why first

- Validates Envelope v2 serialization/deserialization
- Validates `msg_id` generation (UUIDv7)
- Validates local DB write path
- Validates signature sign+verify roundtrip
- No second agent needed — zero-dependency test
- Immediate single-player value (cross-session memory bridge)

## Implementation Sequence

After this RFC is approved:

```
Phase A: Envelope v2 + Schema Migration
  - envelope.rs: Envelope v2 struct + v1 compat reader
  - types.rs: Part, AgentRole enums
  - store/mod.rs: migration v1→v2
  - msg_id generation (uuid crate, v7 feature)
  - sig: canonical hash + Schnorr sign/verify

Phase B: Local Transport + Send-to-Self
  - cli/send.rs: --local flag + self alias
  - Local DB write (open recipient DB, insert, close)
  - Config: [local.agents] section
  - Tests: send-to-self roundtrip

Phase C: Remote Threads (NIP-17)
  - nostr/mod.rs: multi-recipient gift wrap
  - cli/thread.rs: create/send/log subcommands
  - NIP-17 custom tags (mycel-thread-id, mycel-msg-id)
  - threads table management

Phase D: MCP Adapter
  - mycel-mcp crate (separate, thin wrapper)
  - Tools: mycel_send, mycel_inbox, mycel_identity
  - Paged inbox responses (--since, --limit)
```

## Open Questions (deferred, not blocking)

1. Thread member removal — re-key or just stop sending?
2. Cursor/pagination for `inbox --json` (--since, --limit, --cursor)
3. `task_id` + `task_status` — pull into v0.2 late or keep in v0.4?
4. `mycel key export/import` — ship before or with v0.2?
5. NIP-05 aliases (agent@domain) — v0.3 scope
