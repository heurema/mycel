# Research: Agent Thread Communication — Landscape & Approaches

Date: 2026-03-23
Source: delve deep research (run 20260323T080601Z-21363)
Status: verified (multi-source)

## Three Architectural Approaches

### 1. Blackboard / Shared State (in-process)

| Framework | Thread Model | How It Works |
|-----------|-------------|-------------|
| AutoGen GroupChat | Pub-sub + central manager | All agents subscribe to shared topic. GroupChatManager selects next speaker via LLM. TopicId(type, source) = session ID. Shared `_chat_history`. |
| LangGraph | State graph + checkpoints | Thread = stateful graph execution. State passed between nodes explicitly. Checkpoint persistence for resume. |
| CrewAI | Role-based crews | Shared crew context. Agents see output of predecessors through shared task context. |
| OpenAI Swarm | Handoff pattern | No threads. Agent passes context to next via function call. Linear pipeline. |

**Key insight:** All frameworks are in-process, synchronous, shared memory.
None solve cross-machine async messaging. This is blackboard inside one process.

### 2. Protocol-based Threading (HTTP/RPC)

| Protocol | Thread Model | Persistence | Encryption | CLI? |
|----------|-------------|-------------|------------|------|
| A2A | `contextId` groups tasks/messages. Multi-turn via taskId+contextId. States: WORKING→COMPLETED→FAILED | SSE streaming, polling | HTTPS transport only | HTTP API |
| ACP | REST-based, stateful context routing | Server-side state | Not specified | REST |
| ANP | JSON-LD semantic, DNS discovery | Not specified | E2E encryption | Web |
| MCP | No threading. Request-response only. | Ephemeral | None | stdio — CLI-native |

**Key insight:** A2A `contextId` is closest to what mycel needs — it's essentially a thread_id.
But A2A is HTTP JSON-RPC (needs a server). Not suitable for CLI sync-on-command model.
mycel can borrow A2A semantics (contextId, taskId, Message with role+parts) and implement
over Nostr/local transport.

### 3. Nostr-native Threading

| NIP | Model | Suitable? |
|-----|-------|-----------|
| NIP-17 | Group DM: Gift Wrap to each member. `subject` tag for topic. `e` tag for reply threading. Room = unique set of pubkeys. | **Yes — ready-made protocol for agent threads** |
| NIP-29 | Relay-based groups. Admin/membership. Paid relay. | No — too heavy, requires special relay |
| NIP-EE | MLS-based E2E groups (draft) | No — draft, complex, unimplemented |

## NIP-17 as Thread Foundation

NIP-17 already describes exactly what mycel threads need:

- Group = set of pubkeys (all `p` tags + sender)
- Subject tag = topic name
- `e` tag = reply threading (parent message)
- Gift Wrap = E2E encryption to each member
- No central coordinator
- Works on public relays
- No public group identifiers (privacy preserved)
- Any member can change subject by posting new `subject` tag
- New `p` tag added or removed = new room (clean history)

### Privacy guarantees (from NIP-17 spec)

1. No metadata leak — identities, timestamps, kinds hidden from public
2. No public group identifiers — no central converging identifier
3. No moderation — no admins, invitations, or bans
4. No shared secrets — each member gets individually wrapped copy
5. Full recoverability — messages recoverable with user's private key
6. Optional forward secrecy — disappearing messages available
7. Public relay compatible — privacy maintained through public infra

### Fan-out cost

NIP-17 wraps individually to each member: O(agents × messages).
- 3 agents, 20 messages = 60 events (acceptable)
- 5 agents, 100 messages = 500 events (borderline)
- 10 agents, 100 messages = 1000 events (needs STK optimization)

For 3-5 agent threads (primary use case), NIP-17 fan-out is acceptable.

## What to Borrow from Each Source

| Source | What to Take | How to Adapt |
|--------|-------------|-------------|
| A2A contextId | Semantic grouping ID | `thread_id` in mycel — same concept |
| A2A task states | WORKING→COMPLETED→FAILED lifecycle | Structured `status` field in messages |
| AutoGen TopicId | type + source = topic + session | `thread_id = sha256(topic_string)` |
| AutoGen GroupChatManager | Central speaker selection | Not needed for async — agents write when ready |
| NIP-17 group DM | Ready-made protocol | Use Kind 14 + subject + p-tags + e-tags |
| NIP-17 subject | Topic naming | First message carries `subject` tag |
| Blackboard pattern | Shared state, opportunistic agents | SQLite as local blackboard |
| LangGraph checkpoints | State persistence | Thread state = latest message + context |

## Rejected Approaches

| Approach | Why Rejected |
|----------|-------------|
| Custom thread protocol | NIP-17 already exists and is standard |
| Centralized coordinator (AutoGen-style) | Async agents don't need turn-taking |
| A2A HTTP server | Not CLI-native, requires daemon |
| NIP-29 relay groups | Requires paid/special relay, admin model |
| Shared filesystem only | Doesn't work cross-machine |

## Sources

- [A2A Protocol Specification](https://a2a-protocol.org/latest/specification/)
- [NIP-17: Private Direct Messages](https://github.com/nostr-protocol/nips/blob/master/17.md)
- [AutoGen Group Chat](https://microsoft.github.io/autogen/stable//user-guide/core-user-guide/design-patterns/group-chat.html)
- [Top AI Agent Protocols 2026](https://getstream.io/blog/ai-agent-protocols/)
- [MCP vs A2A](https://auth0.com/blog/mcp-vs-a2a/)
- [Multi-Agent Architectures](https://www.arunbaby.com/ai-agents/0029-multi-agent-architectures/)
- [CrewAI vs LangGraph vs AutoGen](https://www.datacamp.com/tutorial/crewai-vs-langgraph-vs-autogen)
