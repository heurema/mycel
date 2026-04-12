# mycel Architecture

Date: 2026-04-11  
Status: current runtime + boundary-core implementation

`mycel` is an encrypted async **mailbox** for AI CLI agents. The product shape stays the same:

- mailbox, not messenger
- sync-on-command, not always-on runtime
- local-first for same-user delivery
- Nostr for remote async delivery
- transport-neutral core state

The accepted boundary is defined in `docs/rfc-local-first-transport-boundary.md`. This page is the short architectural map.

## Runtime model

Default runtime remains short-lived CLI execution:

```text
command starts -> load config + keys -> do one bounded unit of work -> exit
```

Implications:

- no daemon is required for standard send/inbox flows
- `mycel watch` is a foreground poller, not the architectural center
- same-user local delivery must work without standing up a listener

## Core truth

The source of truth is **not** a Nostr event, a SQLite row shape, or an A2A task.
The source of truth is the canonical mycel message model:

- `Envelope`
- `Part`
- `msg_id`
- `thread_id`
- `reply_to`
- normalized `messages`
- normalized `threads`
- trust tiers + delivery/read state

## Accepted transport boundary

Canonical pipeline:

```text
send -> router -> transport -> ingress_frames -> ingest() -> messages -> inbox/watch/adapters
```

### Why this matters

This boundary fixes the current asymmetry where:

- local send writes directly into `messages`
- Nostr receive performs parse/auth/trust/dedup before `messages`
- routing rules live in CLI branches instead of a router/directory layer

With the new boundary, every transport lands **raw frames** and one ingest path decides what becomes mailbox state.

## Delivery modes

| Mode | Role | Notes |
|------|------|-------|
| `local-direct` | default same-user local delivery | direct SQLite/WAL write into recipient ingress, no daemon |
| `local-gateway` | optional local fallback | for sandbox/container/user-boundary cases where direct DB write is not viable |
| `nostr` | remote async delivery | existing encrypted relay path |
| `a2a` | optional ecosystem bridge | gateway/adapter, not source of truth |
| `mcp` | optional tool/resource surface | outer adapter only |

## Storage layers

### Canonical mailbox storage

These remain the normalized read model:

- `messages`
- `threads`
- `contacts`
- `sync_state`
- `outbox`

### Ingress layer

These additive pieces are now part of the runtime:

- `ingress_frames` — raw inbound artifacts before auth/trust/dedup/materialization
- `messages.source_frame_id` — traceability from normalized message to source frame
- `agent_endpoints` — transport-capable endpoint directory used by router

## Current implementation status

What already exists in code:

- Envelope v2 fields: `msg_id`, `thread_id`, `reply_to`, `role`, `parts`
- router seam in `core/router` + store-backed directory in `core/directory`
- `ingress_frames`, `messages.source_frame_id`, and shared `core/ingest`
- local send landing signed envelope JSON in recipient ingress and verifying signatures during ingest
- Nostr sync landing raw frames in ingress before normalization
- persistent `agent_endpoints` directory for local + cached/discovered Nostr endpoints
- thread commands and normalized thread metadata in `messages`
- trust tiers, outbox, ACK, sync logic

What is still intentionally pending:

- `src/transport/mod.rs` still exposes transitional, Nostr-shaped traits and is not yet the runtime seam
- `local-gateway` does not exist yet
- A2A and MCP adapters do not exist yet
- local aliases are still config-authoritative even though runtime resolution now passes through `agent_endpoints`

## Target module split

Current crate layout stays valid, but the target responsibility split is:

```text
cli/*           -> gather user intent, call core
core/router     -> resolve agent ref to endpoint
core/directory  -> unify contacts/local endpoints/future remote endpoints
core/ingest     -> parse/auth/trust/dedup/materialize
transport/*     -> send or collect raw frames only
store/*         -> persist ingress + normalized state
```

The important change is responsibility, not folder names.

## Adapter policy

### A2A

Use A2A only as an interop boundary:

- Agent Card publication/discovery
- HTTP/JSON-RPC + streaming bindings where needed
- mapping between A2A payloads and mycel ingress frames

Do **not** make A2A task/message state the internal truth of `mycel`.

### MCP

Use MCP only as an outer tool/resource adapter over normalized mailbox operations.
It is not a transport and it is not the storage model.

### ACP

Not a strategic dependency for the core architecture.

## References

- `docs/rfc-v0.2-phase0-contracts.md` — message identity and envelope contracts
- `docs/rfc-local-first-transport-boundary.md` — accepted transport boundary
- `docs/plan-local-agent-mesh.md` — phased implementation plan
