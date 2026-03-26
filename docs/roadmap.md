# mycel Roadmap

## Shipped baseline

### v0.1.x — Foundation

- key generation + encrypted key storage (keychain + argon2 fallback)
- SQLite mailbox storage
- `mycel init`, `id`, `send`, `inbox`, `contacts`, `doctor`
- NIP-59 Gift Wrap send/receive
- trust tiers and basic safety constraints

### v0.2.x — Local transport + threads

- Envelope v2 (`msg_id`, `thread_id`, `reply_to`, `role`, `parts`)
- send-to-self and local alias routing
- direct local delivery via SQLite/WAL
- thread create/send/log flows
- schema migration for v2 metadata

### v0.3.x — Delivery hardening

- `mycel watch` foreground poller + `mycel status`
- local inbox mode
- outbox retry
- ACK support
- relay list routing + self-hosted relay
- NIP-77 Negentropy sync

---

## Current priority — Local-first transport boundary

Authoritative docs:

- RFC: `docs/rfc-local-first-transport-boundary.md`
- Architecture: `docs/architecture.md`
- Plan: `docs/plan-local-agent-mesh.md`

This is the next architectural cleanup, not a product pivot. Product direction stays:

- mailbox-first
- sync-on-command
- `local-direct` default for same-user local delivery
- `nostr` for remote async delivery
- A2A/MCP only as adapters around the core

### Phase 1 — Freeze the boundary

- [x] Accept the local-first transport-boundary RFC
- [x] Align architecture, roadmap, glossary, and planning docs
- [ ] Route `send` / `thread send` through a router abstraction instead of transport-specific CLI branches
- [ ] Split transport abstractions into outbound delivery vs inbound collection

### Phase 2 — Add unified ingress

- [ ] Add `ingress_frames`
- [ ] Add `messages.source_frame_id`
- [ ] Implement `ingest()` for parse/auth/trust/dedup/materialization
- [ ] Make inbox/watch drain pending ingress before rendering normalized messages

### Phase 3 — Fix local authenticity for real

- [ ] Change `local-direct` to persist signed envelope JSON as an ingress frame
- [ ] Verify Schnorr signatures on the recipient side during ingest
- [ ] Keep `msg_id` stable across sender outbox copy and recipient inbox copy
- [ ] Add regression tests for local signature verification and dedup

### Phase 4 — Move Nostr onto the same ingress path

- [ ] Change Nostr receive to emit raw ingress frames instead of final `messages` rows
- [ ] Keep NIP-59 auth data in frame metadata, not in normalized mailbox rows
- [ ] Reuse the same ingest path for thread metadata and trust-tier decisions
- [ ] Add regression tests for cross-transport `msg_id` / `reply_to` invariants

### Phase 5 — Add endpoint directory + router policy

- [ ] Add `agent_endpoints`
- [ ] Backfill local aliases from `[local.agents]`
- [ ] Separate trust (`contacts`) from endpoint resolution (`agent_endpoints` / router)
- [ ] Add tests for endpoint selection and route fallback rules

### Phase 6 — Optional local fallback

- [ ] Add `local-gateway` as an optional UDS/loopback fallback
- [ ] Keep `local-direct` as the default where direct DB writes are viable
- [ ] Avoid introducing a mandatory daemon

### Phase 7 — Optional ecosystem adapters

- [ ] Add `mycel-a2a` as a separate bridge/gateway
- [ ] Publish/consume Agent Cards there, not in the core mailbox model
- [ ] Add MCP only after ingress + router are stable
- [ ] Keep MCP limited to outer tools/resources over normalized mailbox operations

---

## Later, after the boundary is stable

### Structured payloads

- DataPart payload conventions for verdicts/artifacts
- task/result status fields that remain transport-neutral
- richer role semantics for multi-agent workflows

### Advanced coordination

- blind quorum / adversarial validation patterns
- sleeping-agent / batch handoff contracts
- larger thread coordination strategies

### Deprioritized / explicitly not core right now

- mandatory local listener/daemon
- replacing mycel’s core schema with A2A task state
- NATS JetStream as the default transport
- ACP as a foundational protocol dependency
