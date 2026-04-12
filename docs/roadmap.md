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

## Current release target — v0.4.x boundary core

Authoritative docs:

- RFC: `docs/rfc-local-first-transport-boundary.md`
- Architecture: `docs/architecture.md`
- Plan: `docs/plan-local-agent-mesh.md`

This release is the transport-boundary cleanup, not a product pivot. Product direction stays:

- mailbox-first
- sync-on-command
- `local-direct` default for same-user local delivery
- `nostr` for remote async delivery
- A2A/MCP only as adapters around the core

### Implemented for v0.4.x

- [x] Accept the local-first transport-boundary RFC
- [x] Align architecture, roadmap, glossary, and planning docs
- [x] Add a router/directory seam for direct send paths
- [x] Split transport abstractions into outbound delivery vs inbound collection
- [x] Add `ingress_frames`
- [x] Add `messages.source_frame_id`
- [x] Implement shared `ingest()` for parse/auth/trust/dedup/materialization
- [x] Make inbox/watch drain pending ingress before rendering normalized messages
- [x] Change `local-direct` to persist signed envelope JSON as an ingress frame
- [x] Verify Schnorr signatures on the recipient side during ingest
- [x] Keep `msg_id` stable across sender outbox copy and recipient inbox copy
- [x] Move Nostr receive onto the same raw-ingress path
- [x] Keep NIP-59 auth data in frame metadata instead of normalized mailbox rows
- [x] Add `agent_endpoints`
- [x] Backfill config-backed local aliases into the endpoint directory at runtime
- [x] Separate trust (`contacts`) from endpoint resolution (`agent_endpoints` / router)
- [x] Add tests for local signature verification, ingress behavior, and endpoint selection

### Remaining release polish before tagging v0.4.x

- [x] Normalize canonical transport naming to `local_direct` while keeping legacy `local` as a compatibility alias during ingest
- [x] Make the release baseline pass `cargo test`
- [x] Make the release baseline pass `cargo clippy --all-targets --all-features -- -D warnings`
- [x] Refresh README / public-facing release notes so the new boundary shows up in user-facing docs

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
