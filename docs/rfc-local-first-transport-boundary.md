# RFC: Local-first Transport Boundary for mycel

Date: 2026-04-11  
Status: accepted  
Scope: architecture + storage boundary, additive migration only

## Decision

`mycel` keeps **`local-direct` via SQLite/WAL as the default same-user local transport**.
That remains the best fit for the project’s core shape: **sync-on-command, no daemon by default, mailbox-not-messenger**.

What changes is the internal seam:

- **core truth** becomes the transport-neutral `mycel` model: canonical `Envelope`, `Part`, `msg_id`, `thread_id`, `reply_to`, normalized `messages`
- **router** chooses an endpoint, not a CLI branch
- **all transports land raw frames in unified ingress**, then a single `ingest()` path performs parse/auth/trust/dedup/materialization
- **A2A** is added later as an external gateway/adapter, not as core truth
- **MCP** is allowed only as an outer adapter/tool surface, never as a transport or source of truth
- **ACP** is not a strategic dependency for this architecture

## Why this RFC exists

The codebase already has most of the right logical fields (`msg_id`, `thread_id`, `reply_to`, `transport`, `transport_msg_id`, local send, self alias, threads), but the transport/storage boundary is still too transport-specific.

Current gaps this RFC closes:

1. **CLI-local branching owns routing and storage decisions**  
   `send` still branches directly into local-vs-Nostr behavior instead of routing through a transport-neutral boundary.

2. **The current `Transport` trait is Nostr-shaped**  
   It assumes `recipient: PublicKey`, `env_json: &str`, and `since: u64`, which does not generalize cleanly to local gateways or A2A endpoints.

3. **Local authenticity is incomplete at the boundary**  
   Local send signs `Envelope v2`, but the signed envelope is not persisted as a transport artifact. The recipient receives a materialized `messages` row, not a verifiable envelope frame.

4. **Inbound normalization is split by transport**  
   Nostr receive logic does parse/auth/trust/dedup before storing, while local delivery bypasses that receive path.

5. **Routing inputs are fragmented**  
   Trust lives in `contacts`, local routing lives in `[local.agents]`, and endpoint resolution is not yet a first-class directory/router concern.

## Core architectural rule

The canonical pipeline is:

```text
send -> router -> transport -> ingress_frames -> ingest() -> messages -> inbox/watch/adapters
```

Meaning:

- sender builds one canonical `Envelope`
- router resolves a recipient into an endpoint
- transport delivers a **raw frame**, not a final `messages` row
- raw frame is stored in `ingress_frames`
- a single `ingest()` path validates and materializes into `messages`
- inbox/watch/MCP/A2A bridge read normalized `messages`, not transport-specific artifacts

## Transport roles

| Layer | Role | Default / status |
|------|------|-------------------|
| `local-direct` | Same-user direct delivery via recipient SQLite/WAL | **Default local path** |
| `local-gateway` | Optional UDS/loopback fallback when direct DB write is not possible | Planned |
| `nostr` | Remote async delivery over relays | Existing remote path |
| `a2a` | Ecosystem interop via Agent Card + protocol binding | Planned as gateway |
| `mcp` | Tool/resource surface for agents | Optional outer adapter |

## Core truth vs adapters

### Core truth

The internal source of truth stays `mycel`-native:

- `Envelope`
- `Part`
- `msg_id`
- `thread_id`
- `reply_to`
- `messages`
- `threads`
- trust tiers and delivery state

### Adapters

These are adapters around the core, not replacements for it:

- **Nostr**: remote async delivery transport
- **A2A**: gateway that maps A2A message/task/artifact concepts to/from mycel envelopes
- **MCP**: tool/resource interface over normalized mailbox operations

## Proposed boundary types

Illustrative shape only; names may change during implementation.

```rust
pub enum TransportKind {
    LocalDirect,
    LocalGateway,
    Nostr,
    A2A,
}

pub struct EndpointRef {
    pub endpoint_id: String,
    pub agent_ref: String,
    pub kind: TransportKind,
    pub address: String,
    pub metadata_json: Option<String>,
}

pub struct IngressFrame {
    pub frame_id: String,
    pub transport: TransportKind,
    pub endpoint_id: Option<String>,
    pub transport_msg_id: Option<String>,
    pub sender_hint: Option<String>,
    pub recipient_hint: Option<String>,
    pub envelope_json: String,
    pub auth_meta_json: Option<String>,
    pub received_at: String,
}

#[async_trait]
pub trait OutboundTransport {
    fn kind(&self) -> TransportKind;
    async fn send(&self, endpoint: &EndpointRef, envelope: &Envelope) -> Result<SendReport>;
}

#[async_trait]
pub trait Collector {
    fn kind(&self) -> TransportKind;
    async fn collect(&self) -> Result<Vec<IngressFrame>>;
}
```

Design intent:

- `local-direct` may implement outbound only
- `nostr` may implement outbound + collector
- `a2a` may start outbound-only, with inbound handled by a dedicated gateway server
- `since: u64` stops leaking into every transport abstraction

## Proposed storage additions

Additive migration only. Do **not** rewrite `messages`, `outbox`, or `threads` in one step.

### `ingress_frames`

```sql
CREATE TABLE IF NOT EXISTS ingress_frames (
    frame_id           TEXT PRIMARY KEY,
    transport          TEXT NOT NULL,
    endpoint_id        TEXT,
    agent_ref          TEXT,
    transport_msg_id   TEXT,
    sender_hint        TEXT,
    recipient_hint     TEXT,
    envelope_json      TEXT NOT NULL,
    auth_meta_json     TEXT,
    received_at        TEXT NOT NULL,
    processed_at       TEXT,
    status             TEXT NOT NULL DEFAULT 'pending',
    error              TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_ingress_transport_msg
    ON ingress_frames(transport, transport_msg_id);
```

### `messages.source_frame_id`

```sql
ALTER TABLE messages ADD COLUMN source_frame_id TEXT;

CREATE INDEX IF NOT EXISTS idx_messages_source_frame_id
    ON messages(source_frame_id);
```

### `agent_endpoints`

```sql
CREATE TABLE IF NOT EXISTS agent_endpoints (
    endpoint_id        TEXT PRIMARY KEY,
    agent_ref          TEXT NOT NULL,
    transport          TEXT NOT NULL,
    address            TEXT NOT NULL,
    priority           INTEGER NOT NULL DEFAULT 100,
    enabled            INTEGER NOT NULL DEFAULT 1,
    metadata_json      TEXT,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_endpoints_unique
    ON agent_endpoints(agent_ref, transport, address);
```

`contacts` remains the initial trust source. `messages` remains the canonical mailbox history.

## Routing policy

Router policy is endpoint selection, not CLI branching:

| Recipient shape | Preferred route |
|-----------------|-----------------|
| `self` | `local-direct` |
| alias backed by local endpoint and writable DB | `local-direct` |
| alias backed by local endpoint but direct write not viable | `local-gateway` |
| remote contact / npub / remote relay identity | `nostr` |
| `a2a:https://...` or discovered Agent Card endpoint | `a2a` |

## Transport-specific expectations

### `local-direct`

After this RFC, local delivery must:

1. build canonical `Envelope`
2. sign it
3. persist the **signed envelope JSON** into recipient `ingress_frames`
4. let recipient-side `ingest()`:
   - parse envelope
   - verify Schnorr signature
   - apply trust tier
   - dedup on `msg_id`
   - materialize `messages`

This is the critical change that makes local authenticity real rather than advisory.

### `nostr`

Nostr receive should stop writing final `messages` rows directly.
It should produce `IngressFrame { transport = nostr, transport_msg_id = event_id, envelope_json = rumor_content, auth_meta_json = ... }` and then reuse the same `ingest()` path.

### `a2a`

A2A belongs in a separate bridge layer, ideally an optional `mycel-a2a` binary/crate that:

- publishes `.well-known/agent-card.json`
- accepts A2A calls on supported bindings
- maps inbound A2A payloads into `IngressFrame`
- maps outbound mycel message/thread state into A2A message/task/artifact concepts
- caches remote Agent Cards and capability metadata

A2A is **not** the canonical database model and **must not** become the source of truth for local mailbox state.

### `mcp`

MCP is permitted only as an outer adapter:

- `mycel_send` as a tool
- inbox/thread/status views as tools or resources
- reads normalized `messages`
- never replaces router/transport/ingest

## What this RFC explicitly does not do

- does **not** make A2A the core message model
- does **not** require a daemon or listener for default local delivery
- does **not** make `local-gateway` the default
- does **not** pull NATS JetStream into the core now
- does **not** rewrite `contacts`, `messages`, and `outbox` in one big-bang migration
- does **not** require `thread_id == a2a task_id`

## Migration strategy

Implementation is phased and additive:

1. freeze terminology and boundary docs
2. route send/thread through router abstractions
3. add `ingress_frames` + `ingest()`
4. make `local-direct` land signed envelopes in ingress
5. make Nostr receive land raw frames in ingress
6. add `agent_endpoints`
7. add optional `local-gateway`
8. add optional `mycel-a2a`
9. add MCP only after the boundary is stable

See `docs/plan-local-agent-mesh.md` for the execution plan.

## Acceptance criteria

The boundary is successful when all of the following are true:

- two local agents of the same user can exchange messages without network access or a daemon
- recipient-side local delivery is cryptographically verifiable
- `msg_id` stays stable across local/Nostr/A2A copies of the same logical message
- `reply_to` and `thread_id` are independent of transport-specific IDs
- `send.rs` and `thread.rs` stop knowing whether the destination is recipient SQLite, relay, or A2A endpoint
- inbox/watch/adapters read normalized `messages`; transport-specific parsing happens only before ingest

## Relationship to older docs

- `docs/rfc-v0.2-phase0-contracts.md` still defines message-level contracts like `msg_id`, `reply_to`, and envelope semantics.
- This RFC becomes the authoritative document for **transport boundary, routing, ingress, and adapter layering**.
- `docs/plan-local-agent-mesh.md` is now the implementation plan for this RFC rather than a free-form brainstorm.
