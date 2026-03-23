# mycel Roadmap

## Completed

### v0.1.0 — Foundation + Send & Receive (2026-03-20)

- [x] Key generation (secp256k1), encryption at rest (keychain + argon2 fallback)
- [x] SQLite schema, envelope type, relay connection
- [x] `mycel init`, `mycel id`, `mycel send`, `mycel inbox`, `mycel contacts`
- [x] NIP-59 Gift Wrap pipeline, trust tiers (KNOWN/UNKNOWN/BLOCKED)
- [x] `mycel doctor`, `mycel inbox --json/--all`, config file, message size cap
- [x] Published to crates.io

### v0.1.1 — Security Hardening (2026-03-23)

- [x] Relay spam protection: max 1000 events per sync
- [x] Drop Blocked messages before DB insert (storage flooding prevention)
- [x] Send exit code 1 when no relay accepts message
- [x] chmod 0o600 on mycel.db
- [x] Sync overlap 120s → 300s (clock skew protection)
- [x] Published to crates.io

### HEAD (unreleased, post-0.1.1)

- [x] `mycel watch` — polling loop with backoff, singleton lock, heartbeat
- [x] `mycel status` — watch process status
- [x] `mycel inbox --local` — read from SQLite only, no relay fetch
- [x] 53 tests passing

---

## v0.2 — Threads & Local Agent Mesh

RFC: `docs/rfc-v0.2-phase0-contracts.md` (approved 2026-03-23)
Research: `docs/research-agent-thread-*.md`, `docs/plan-local-agent-mesh.md`

### Phase A — Envelope v2 + Schema Migration

| Task | Signum scope | Est |
|------|-------------|-----|
| A1: Add `uuid` crate (v7 feature) to Cargo.toml | low risk | 15min |
| A2: Envelope v2 struct (msg_id, thread_id, reply_to, role, parts[]) | medium | 2h |
| A3: v1 backward-compat reader (if parts absent, read msg field) | medium | 1h |
| A4: DB migration user_version 1→2 (new columns + backfill) | medium | 2h |
| A5: Canonical envelope hash + Schnorr signature (local authenticity) | high | 4h |
| A6: `mycel key export/import` (escape hatch before threads) | medium | 4h |

### Phase B — Send-to-Self + Local Transport

| Task | Signum scope | Est |
|------|-------------|-----|
| B1: `self` alias in send command (writes to own DB) | low | 1h |
| B2: Local agent config `[local.agents]` in config.toml | low | 1h |
| B3: `mycel send --local <alias>` — direct SQLite write with sig verify | medium | 4h |
| B4: Cursor/pagination for inbox (--since, --limit) | medium | 2h |
| B5: "Send to self" README section + cold start workflow | low | 1h |

### Phase C — Remote Threads (NIP-17)

| Task | Signum scope | Est |
|------|-------------|-----|
| C1: Multi-recipient gift wrap (build Kind 14 rumor with all p-tags) | high | 4h |
| C2: `mycel thread create <topic> --members a,b,c` | medium | 2h |
| C3: `mycel thread send <thread-id> "message"` | medium | 2h |
| C4: `mycel thread log <thread-id> [--json]` | medium | 2h |
| C5: NIP-17 custom tags (mycel-thread-id, mycel-msg-id) | medium | 1h |
| C6: Self-copy on thread send | low | 1h |
| C7: threads table + thread membership tracking | medium | 2h |

### Phase D — MCP Adapter

| Task | Signum scope | Est |
|------|-------------|-----|
| D1: `mycel-mcp` crate (separate, thin wrapper) | medium | 4h |
| D2: MCP tools: mycel_send, mycel_inbox, mycel_identity | medium | 2h |
| D3: Paged inbox responses (--since, --limit in MCP) | low | 1h |

---

## v0.3 — Watch & Hooks Hardening

- [ ] Per-relay cursor fix (global min-cursor → per-relay independent)
- [ ] Claude Code hooks (SessionStart + PostToolUse → inbox check)
- [ ] Watch heartbeat + exponential backoff improvements
- [ ] `mycel inbox --local-only` (alias for --local, clearer naming)
- [ ] NIP-05 aliases (agent@domain)

## v0.4 — Structured Payloads

- [ ] DataPart with `application/x-mycel-verdict` mime type
- [ ] task_id + task_status (working/completed/failed)
- [ ] Agent roles (coordinator/reviewer/implementer)
- [ ] Structured verdict fields (suggest/block/approve)
- [ ] Artifact schema with findings list

## v0.5 — Advanced Coordination

- [ ] STK (shared thread key) for 10+ agent threads
- [ ] Blind quorum protocol (adversarial validation)
- [ ] Sleeping agent pattern (cron + idempotent task contract)
- [ ] Task delegation (NIP-90 DVM pattern)

## Future

- [ ] Agent RPC (NIP-46 pattern)
- [ ] NATS JetStream (local/team transport)
- [ ] Private team relay
- [ ] Key recovery (seed phrase)
- [ ] Multi-device key sync
- [ ] SQLCipher (DB encryption at rest)
