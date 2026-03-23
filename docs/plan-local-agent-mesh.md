# Plan: Local Agent Mesh — Unified Local + Remote Communication

Date: 2026-03-23
Status: brainstorm complete, not started
Source: 3-provider brainstorm (Explorer/Operator/Contrarian), 2 rounds

## Core Insight

"Local vs remote transparent" is solved at the **data model** level (unified schema,
`transport` field), not at the **transport** level (one protocol for everything).
Two delivery paths, one inbox.

## Architecture

```
Agent A (Claude Code)                Agent B (Codex CLI)
     |                                    |
     |-- local? --> direct SQLite write --+
     |                                    |
     |-- remote? --> Nostr relay -------> mycel inbox --+
     |                                                  |
     +-- mycel inbox <---------------------------------+
```

### Dual Transport

| Property | Local | Remote |
|----------|-------|--------|
| Wire | Direct SQLite write (WAL) | Nostr relay (WebSocket) |
| Encryption | Optional (same user, same fs) | NIP-59 Gift Wrap (3-layer) |
| Latency | ~microseconds | ~100-500ms |
| ID format | UUID v7 (monotonic) | Nostr EventId |
| Persistence | SQLite (recipient's mycel.db) | SQLite + relay |
| Daemon required | No (pull-model preserved) | No |

### Data Model Changes

Add to `messages` table:
```sql
transport TEXT NOT NULL DEFAULT 'nostr'  -- 'nostr' | 'local'
```

Envelope v2 — add `reply_to` for causal ordering:
```json
{
  "v": 2,
  "from": "<pubkey-hex>",
  "to": "<pubkey-hex>",
  "msg": "review complete, 3 issues",
  "ts": "2026-03-23T10:00:00Z",
  "reply_to": "<parent-event-id-or-null>"
}
```

### Local Delivery Mechanism

Direct write to recipient's SQLite DB via WAL (already configured with
`busy_timeout=5000`). No UDS, no daemon, no listener required.

Detection: check if recipient alias maps to a local path
(`~/.local/share/mycel/mycel.db` convention or explicit config).

```toml
# ~/.config/mycel/config.toml
[local]
enabled = true

[local.agents]
codex = { pubkey = "abc...", db = "~/.local/share/mycel-codex/mycel.db" }
gemini = { pubkey = "def...", db = "~/.local/share/mycel-gemini/mycel.db" }
```

ID generation: UUID v7 (monotonic, sortable) stored in `nostr_id` column.
Dedup via existing `INSERT OR IGNORE` on PRIMARY KEY.

### What NOT to Do

- No UDS (requires listener daemon, changes pull-model architecture)
- No separate protocol for local (two code paths = two maintenance burdens)
- No skipping NIP-59 for local by default (defer until benchmark proves overhead matters)
- No gossip overlay (structurally incompatible with sync-on-command)

## MCP Integration

MCP adapter as **entry point**, not core:

| | MCP | mycel |
|---|-----|-------|
| Direction | Agent -> Tool (unidirectional) | Agent <-> Agent (bidirectional) |
| Lifecycle | Request-scoped | Persistent across sessions |
| Encryption | None (stdio plaintext) | E2E (NIP-59) |
| Identity | None | secp256k1 keypair |
| Cross-machine | No | Yes |

`mycel-mcp` — thin wrapper crate. Tools: `mycel_send`, `mycel_inbox`, `mycel_identity`.
Paged inbox responses to avoid context budget poison.

## Async Patterns

### Viable

- **Blind quorum**: adversarial validation only (security audit, compliance).
  Agents intentionally isolated. M-of-N threshold. Narrow but real market.
- **Sleeping agent**: idempotent batch tasks only (metrics, reports, regression).
  Agent wakes on cron, processes inbox, responds, shuts down.
  Contract: task must declare itself idempotent and verifiable.
- **Causal ordering**: `reply_to` mandatory for non-root messages.
  Agent can reject if referenced parent not received.

### Not Viable

- **Generic async coordination**: worse than synchronous arbiter for interactive tasks.
- **Gossip mesh**: requires long-lived peers, incompatible with sync-on-command.
- **Full causality (vector clocks)**: LLM agents are non-deterministic, ordering
  at transport layer doesn't guarantee correct behavior at application layer.

## Compliance / Audit

Every mycel message is signed, timestamped, encrypted. Query:
"Show every instruction Claude gave Codex between 14:00 and 16:00" = SQL query.
Relevant: EU AI Act Article 13 transparency, NIST AI RMF.

## Implementation Sequence

### Phase A: Foundation (this sprint)
1. Add `transport` field to `messages` table (migration)
2. UUID v7 generation for local message IDs
3. `reply_to` field in Envelope v2
4. Benchmark: NIP-59 wrap latency (1000 wraps)

### Phase B: Local Delivery
5. `mycel send --local <alias>` — direct SQLite write PoC
6. Local agent config in `config.toml`
7. `mycel status` — show all local agent identities and last activity

### Phase C: MCP + Watch
8. `mycel-mcp` adapter (send, inbox, identity tools)
9. `mycel watch` — polling loop (2s interval), not daemon
10. Hook integration (PostToolUse -> inbox check)

### Phase D: Patterns
11. "Send to self" workflow (single-player value, cold start)
12. Blind quorum protocol (adversarial validation)
13. Sleeping agent contract (idempotent task declaration)

## Open Questions

- UUID v7 in `nostr_id` column — does dedup logic hold?
- Direct SQLite write — race conditions under concurrent writers?
- NIP-59 local overhead — 200us hypothesis, needs benchmark
- Threat model for local encryption — define explicitly
- How does agent identity survive CLI updates (codex upgrade, etc.)?

## Rejected Alternatives

| Alternative | Why rejected |
|-------------|-------------|
| UDS transport | Requires listener daemon, changes pull-model |
| Single protocol (Nostr only) | 100-500ms latency for local = unacceptable |
| Single protocol (local only) | Loses cross-machine, the core value prop |
| Skip encryption for local | Two code paths forever, marginal gain |
| Replace arbiter with mycel | Arbiter subprocess is correct for sync calls |
| Gossip overlay | Needs long-lived peers, wrong for CLI agents |
