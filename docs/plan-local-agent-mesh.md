# Plan

Implement the accepted **local-first transport boundary** in bounded, additive phases so `mycel` keeps `local-direct` as the default same-user path while moving auth, dedup, and normalization into a shared ingress pipeline. This plan is tracked under the approved punk contract `ct_20260411160739118_v1` and is intentionally documentation-first: freeze the boundary, then land router + ingress, then move local/Nostr paths, then add optional adapters.

## Scope
- In:
  - transport-neutral core boundary (`Envelope` + router + unified ingress)
  - additive schema changes (`ingress_frames`, `messages.source_frame_id`, `agent_endpoints`)
  - local authenticity fix through recipient-side verification
  - Nostr receive migration onto the same ingest path
  - optional `local-gateway`, A2A bridge, and MCP outer-adapter follow-ups
- Out:
  - mandatory daemon/listener for local delivery
  - replacing canonical mycel message state with A2A task state
  - NATS/JetStream as a core dependency now
  - broad product-surface expansion unrelated to the boundary refactor

## Action items
[x] Freeze terminology in `docs/rfc-local-first-transport-boundary.md`, `docs/architecture.md`, `docs/roadmap.md`, `project.intent.md`, and `project.glossary.json`.
[x] Add a router/directory seam so `send` and `thread send` choose an endpoint instead of branching directly into SQLite-vs-Nostr behavior.
[x] Split transport abstractions into outbound delivery and inbound collection.
[x] Add `ingress_frames` and `messages.source_frame_id`, then implement a single `ingest()` path for parse/auth/trust/dedup/materialization.
[x] Change `local-direct` to persist the signed envelope JSON into recipient ingress and verify Schnorr signatures during ingest instead of trusting a pre-materialized row.
[x] Change Nostr sync to emit raw ingress frames and reuse the same ingest pipeline for `msg_id`, `thread_id`, `reply_to`, and trust-tier decisions.
[x] Add `agent_endpoints` and backfill local aliases from `[local.agents]` so routing stops living inside CLI handlers.
[ ] Add `local-gateway` only as an optional fallback for sandbox/container/user-boundary cases where direct DB writes are not viable.
[ ] Add `mycel-a2a` only as a separate bridge that maps Agent Cards and A2A payloads to/from ingress frames without making A2A the source of truth.
[ ] Add MCP only after the boundary is stable, and keep it limited to outer tools/resources over normalized mailbox operations.
[x] Validate the boundary-core phases with `cargo test`, targeted transport regressions, and explicit checks for signature verification, dedup, and cross-transport thread invariants.

## Open questions
- Resolved for v0.4.x: canonical transport naming is `local_direct` / `local_gateway`, while legacy `local` remains accepted during ingest for compatibility.
- Resolved for v0.4.x: `[local.agents]` remains the source of truth for local aliases, but runtime resolution now flows through `agent_endpoints`.
- Resolved for v0.4.x: the older v0.2 contracts RFC should carry an archival note pointing readers to `docs/rfc-local-first-transport-boundary.md` for transport/storage topology.
