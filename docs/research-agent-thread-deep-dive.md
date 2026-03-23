# Research: Agent Threads — Deep Dive

Date: 2026-03-23
Source: delve deep research, 4 parallel DIVE agents
Companion: research-agent-thread-landscape.md (landscape overview)

## Key Findings

### 1. NIP-17 Fan-Out: Practical for 3-5 Agents

| Agents | Events (50 msgs each) | Storage | Fetch/agent | Verdict |
|--------|----------------------|---------|-------------|---------|
| 2 | 200 | 0.6 MB | 320 KB | Trivial |
| 3 | 450 | 1.4 MB | 480 KB | Fine |
| 5 | 1,250 | 3.9 MB | 800 KB | Fine |
| 10 | 5,000 | 15.5 MB | 1.6 MB | Borderline |
| 20 | 20,000 | 62 MB | 3.2 MB | STK needed |
| 50 | 125,000 | 388 MB | 8 MB | Impractical |

**Gift Wrap event size**: ~3.2 KB per event (200-byte plaintext → 6x overhead from double NIP-44).
**Sender cost per message**: N × (ECDH + ChaCha20 + sign + publish). At N=5: ~5ms crypto + relay RTT.
**Relay query**: 250 events in single REQ → 200-800ms over public internet. Not a bottleneck.

**Verdict**: NIP-17 individual wrapping works for 2-10 agents. STK at 10-15+.

### 2. STK (Shared Thread Key) vs NIP-17 Pairwise

| Property | NIP-17 (pairwise) | Sender Keys / STK |
|----------|-------------------|-------------------|
| Events per message | O(N) | O(1) |
| Forward secrecy | Full (new ephemeral key each wrap) | Partial (ratchet) |
| Post-compromise | Full | Weak (key only rotated on membership change) |
| No shared secret | Yes (NIP-17 design goal) | No — STK is shared |
| Member removal | Trivial (stop sending wraps) | Requires full key rotation |
| Break-even N | ≤10 agents | ≥10 agents |

**Recommendation**: Start with NIP-17 pairwise (simpler, better security). Add STK only if
threads regularly exceed 10 agents. NIP-17 spec explicitly says groups >100 need different scheme.

### 3. NIP-17 Implementation Details (nostr-sdk 0.44)

**Available APIs**:
- `EventBuilder::gift_wrap(signer, receiver, rumor, extra_tags)` — full NIP-59 stack
- `client.gift_wrap_to(urls, receiver, rumor, extra_tags)` — already used by mycel
- `Tag::custom(TagKind::Subject, [...])` — subject tag for thread naming
- `Tag::custom(TagKind::e(), [...])` — e tag for reply chains
- `Tag::custom(TagKind::custom("mycel-thread-id"), [...])` — custom tags (invisible to relays)

**Critical gap**: `private_msg_rumor` adds only ONE p-tag. For group threads:
1. Build Kind 14 rumor manually with ALL p-tags + subject + e-tags + custom tags
2. Call `gift_wrap_to` N+1 times (each member + self-copy)

**Custom tags ride inside encrypted rumor** — invisible to relays and non-mycel clients.
Unknown tags are silently ignored by compliant Nostr clients. Safe to extend.

**Thread identity**: NIP-17 uses sorted pubkey set as room identity. Adding/removing member
= new room. Solve with `mycel-thread-id` custom tag (stable UUID across membership changes).

### 4. A2A Protocol → mycel Envelope v2 Mapping

| A2A concept | mycel Envelope v2 |
|-------------|------------------|
| `contextId` | `thread_id` (sha256 of topic or UUID v7) |
| `taskId` | `task_id` (UUID v7, optional) |
| `messageId` | `nostr_id` (Nostr event ID, unchanged) |
| `Message.role` | `role`: coordinator / reviewer / implementer |
| `TaskState` | `task_status` in DataPart: working / completed / failed |
| `Artifact` | DataPart with `application/x-mycel-verdict` mime |
| `Parts[]` | `parts: Vec<Part>` — text + structured data |

**Envelope v2 (concrete)**:
```json
{
  "v": 2,
  "from": "<pubkey>",
  "thread_id": "<sha256-or-uuid7>",
  "reply_to": "<parent-event-id>",
  "task_id": "<uuid7-or-null>",
  "role": "reviewer",
  "ts": "2026-03-23T10:00:00Z",
  "parts": [
    { "type": "text", "text": "Review complete. 3 issues found." },
    { "type": "data", "mime_type": "application/x-mycel-verdict",
      "data": { "verdict": "block", "task_status": "completed",
               "issues": [{"severity":"error","location":"src/lib.rs:42","msg":"unwrap in prod"}] }}
  ]
}
```

**Three implementation levels**:
1. **Level 1** (MVP): `thread_id` + `reply_to` + `parts[]` — gives threads and causality
2. **Level 2**: DataPart with verdict/task_status — machine-readable results
3. **Level 3**: Full artifact schema with findings list

### 5. Local Threads: SQLite Blackboard Architecture

**SQLite WAL concurrent access** (benchmarked data):
- 1 writer: ~100K ops/sec
- 3-5 concurrent writer processes: ~5K ops/sec (POSIX advisory lock serialization)
- `busy_timeout=5000`: reliable for 3-5 writers. Unreliable at 20+.
- Critical: read→write upgrade causes SQLITE_BUSY ignoring busy_timeout. Use `BEGIN IMMEDIATE`.

**Recommended architecture**: Single coordinator writer + N reader/pollers.

**Thread SQLite schema**:
```sql
CREATE TABLE threads (
    thread_id   TEXT PRIMARY KEY,
    subject     TEXT,
    members     TEXT NOT NULL,  -- JSON array of pubkeys
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE TABLE thread_messages (
    msg_id      TEXT PRIMARY KEY,  -- ULID for monotonic ordering
    thread_id   TEXT NOT NULL REFERENCES threads(thread_id),
    agent_id    TEXT NOT NULL,
    role        TEXT NOT NULL,     -- coordinator|reviewer|implementer|observer
    content     TEXT NOT NULL,
    msg_type    TEXT NOT NULL DEFAULT 'text',  -- text|task|result|error
    task_status TEXT,              -- working|completed|failed (denormalized)
    parent_id   TEXT,              -- reply chain
    seq         INTEGER NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE INDEX idx_thread_messages_seq ON thread_messages(thread_id, seq);
```

**Poll pattern for readers**:
```sql
SELECT * FROM thread_messages WHERE thread_id = ? AND seq > ? ORDER BY seq ASC;
```

### 6. Gaps and Risks

| Gap | Impact | Mitigation |
|-----|--------|-----------|
| No relay-side thread filtering | Fetch ALL kind 1059, filter locally after decrypt | Acceptable at <1000 events. Optimize with `since` cursor. |
| NIP-17 room = pubkey set (unstable) | Add/remove member = new room | `mycel-thread-id` custom tag preserves identity |
| Subject last-writer-wins | Concurrent rename race | Use rumor `created_at` (not gift wrap outer ts) for ordering |
| No delivery confirmation | Can't verify all members received message | Out-of-scope for v1. Future: ack messages. |
| Double-encryption overhead for local | NIP-59 on localhost = wasted crypto | Skip for local transport (per dual-transport plan) |

## Architectural Decision

**For mycel threads v1**:
- Use NIP-17 as wire protocol (Kind 14 + Gift Wrap per member)
- Add `mycel-thread-id` custom tag for stable thread identity
- Envelope v2 with `thread_id` + `reply_to` + `parts[]`
- Start pairwise (no STK) — simpler, better security, fine for 3-5 agents
- Local threads: shared SQLite with WAL, single coordinator writer
- CLI: `mycel thread create/send/log/list`

**Defer to v2+**:
- STK (when >10 agents needed)
- Structured verdicts and artifacts (Level 2-3)
- MLS-based encryption (NIP-EE, if it stabilizes)

## Sources

- [A2A Protocol Specification](https://a2a-protocol.org/latest/specification/)
- [NIP-17: Private Direct Messages](https://github.com/nostr-protocol/nips/blob/master/17.md)
- [NIP-59: Gift Wrap](https://nips.nostr.com/59)
- [NIP-44: Encrypted Payloads](https://nips.nostr.com/44)
- [NIP-17 PR #686 (scalability discussion)](https://github.com/nostr-protocol/nips/pull/686)
- [Signal Sender Keys Protocol](https://en.wikipedia.org/wiki/Signal_Protocol)
- [SQLite WAL concurrent writes](https://oldmoe.blog/2024/07/08/the-write-stuff-concurrent-write-transactions-in-sqlite/)
- [SQLite database-is-locked errors](https://tenthousandmeters.com/blog/sqlite-concurrent-writes-and-database-is-locked-errors/)
- [AutoGen Group Chat](https://microsoft.github.io/autogen/stable//user-guide/core-user-guide/design-patterns/group-chat.html)
- [Mem0 memory layer](https://github.com/mem0ai/mem0)
- [MCP vs A2A](https://auth0.com/blog/mcp-vs-a2a/)
- [Top AI Agent Protocols 2026](https://getstream.io/blog/ai-agent-protocols/)
