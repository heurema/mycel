# Resilience Patterns from Anthropic Channels Plugin

Source: https://github.com/anthropics/claude-plugins-official/external_plugins/{telegram,discord}/
Date: 2026-03-23
Total: 1603 lines (862 telegram + 741 discord), TypeScript/Bun, MCP servers

## Architecture

Both are self-contained MCP servers running as long-lived daemons:
- stdio transport (MCP SDK) for Claude Code communication
- grammy (Telegram) / discord.js (Discord) for platform API
- File-based state in `~/.claude/channels/<name>/`
- Access control via pairing codes (6-hex, 1h TTL, max 3 pending)

## Patterns to Apply in mycel

### 1. Crash Prevention (CRITICAL)

```typescript
// Last-resort — process stays alive on unhandled errors
process.on('unhandledRejection', err => {
  process.stderr.write(`component: unhandled rejection: ${err}\n`)
})
process.on('uncaughtException', err => {
  process.stderr.write(`component: uncaught exception: ${err}\n`)
})
```

**mycel**: Rust equivalent — custom panic hook + tokio UnhandledPanic::Ignore for tasks.

### 2. Graceful Shutdown (CRITICAL)

```typescript
let shuttingDown = false
function shutdown(): void {
  if (shuttingDown) return       // re-entrance guard
  shuttingDown = true
  setTimeout(() => process.exit(0), 2000)  // force-exit fallback
  void Promise.resolve(bot.stop()).finally(() => process.exit(0))
}
process.stdin.on('end', shutdown)   // MCP connection died
process.stdin.on('close', shutdown)
process.on('SIGTERM', shutdown)
process.on('SIGINT', shutdown)
```

Key insight: **stdin EOF = parent died**. Without this, bot becomes zombie holding token,
blocking next session with 409 Conflict.

**mycel**: Watch for parent process death (stdin close / SIGTERM). Flush pending
deliveries with 2s timeout before exit. AtomicBool for re-entrance guard.

### 3. Retry with Linear Backoff + Cap

```typescript
for (let attempt = 1; ; attempt++) {
  try {
    await bot.start(...)
    return
  } catch (err) {
    if (err instanceof GrammyError && err.error_code === 409) {
      const delay = Math.min(1000 * attempt, 15000)  // 1s → 15s cap
      await new Promise(r => setTimeout(r, delay))
      continue
    }
    // Distinguish expected errors (Aborted delay) from fatal
    if (err.message === 'Aborted delay') return
    // Fatal — log and exit
    return
  }
}
```

**mycel**: For Nostr relay reconnect. Distinguish transient (409, timeout) from
fatal (auth, config). Linear backoff 1s→15s cap. Add jitter for multiple agents.

### 4. Atomic State Writes

```typescript
function saveAccess(a: Access): void {
  if (STATIC) return
  mkdirSync(STATE_DIR, { recursive: true, mode: 0o700 })
  const tmp = ACCESS_FILE + '.tmp'
  writeFileSync(tmp, JSON.stringify(a, null, 2) + '\n', { mode: 0o600 })
  renameSync(tmp, ACCESS_FILE)  // atomic on POSIX
}
```

**mycel**: Already using this pattern for identity. Apply to all mailbox state:
write → `.tmp` → rename. Permissions: 0o600 for secrets, 0o700 for dirs.

### 5. Corruption Recovery

```typescript
function readAccessFile(): Access {
  try {
    const parsed = JSON.parse(readFileSync(ACCESS_FILE, 'utf8'))
    return { /* merge with defaults for missing fields */ }
  } catch (err) {
    if (err.code === 'ENOENT') return defaultAccess()  // first run
    try {
      renameSync(ACCESS_FILE, `${ACCESS_FILE}.corrupt-${Date.now()}`)
    } catch {}
    return defaultAccess()  // continue with safe defaults
  }
}
```

Key: corrupt files renamed to `.corrupt-<timestamp>`, never deleted.

**mycel**: Apply to mailbox index. On corruption: rename aside, rebuild from
message files. Never lose data silently.

### 6. Handler Error Isolation

```typescript
// Without this, any throw in a message handler stops polling PERMANENTLY
// (grammy's default calls bot.stop() and rethrows)
bot.catch(err => {
  process.stderr.write(`handler error (polling continues): ${err.error}\n`)
})
```

**mycel**: Per-message error isolation. One malformed/oversized/corrupt message
must not crash the inbox. Log + skip + continue.

### 7. Lazy Resource Loading

```typescript
// Photos: defer download until AFTER the gate approves
await handleInbound(ctx, caption, async () => {
  // Only download if the message passes access control
  const file = await ctx.api.getFile(best.file_id)
  ...
})
```

```typescript
// Discord: attachments listed (name/type/size) but NOT downloaded
// Model calls download_attachment only when it wants them
```

**mycel**: Don't decrypt message bodies until the recipient requests them.
List metadata first, fetch content on demand.

### 8. Outbound Security

```typescript
// Prevent leaking internal state via reply tool
function assertSendable(f: string): void {
  let real = realpathSync(f)        // resolve symlinks
  let stateReal = realpathSync(STATE_DIR)
  const inbox = join(stateReal, 'inbox')
  if (real.startsWith(stateReal + sep) && !real.startsWith(inbox + sep)) {
    throw new Error(`refusing to send channel state: ${f}`)
  }
}
```

```typescript
// Sanitize user-controlled filenames (prevent delimiter injection)
function safeName(s: string | undefined): string | undefined {
  return s?.replace(/[<>\[\]\r\n;]/g, '_')
}
```

**mycel**: Sanitize all metadata fields in messages. Prevent key material
from being included in message content. Validate paths with realpath.

### 9. Configurable State Directory

```typescript
const STATE_DIR = process.env.TELEGRAM_STATE_DIR
  ?? join(homedir(), '.claude', 'channels', 'telegram')
```

Enables: multiple bots on same machine, custom storage locations, testing.

**mycel**: `MYCEL_STATE_DIR` env var. Default `~/.mycel/`. Test fixtures
use temp dirs.

### 10. Static Mode (Production Lockdown)

```typescript
const STATIC = process.env.TELEGRAM_ACCESS_MODE === 'static'
// Config snapshotted at boot, never written
// Pairing downgraded to allowlist (can't mutate state)
const BOOT_ACCESS: Access | null = STATIC ? (() => {
  const a = readAccessFile()
  if (a.dmPolicy === 'pairing') {
    a.dmPolicy = 'allowlist'  // graceful downgrade
  }
  a.pending = {}
  return a
})() : null
```

**mycel**: `MYCEL_MODE=readonly` for production inboxes that only receive,
never modify identity or keys.

### 11. Anti-Injection in Notifications

```typescript
// image_path goes in META only — an in-content annotation like
// "[image attached — read: PATH]" is forgeable by any sender
mcp.notification({
  method: 'notifications/claude/channel',
  params: {
    content: text,           // user-controlled
    meta: { image_path },    // structured, not in content
  },
})
```

```typescript
// Multi-line content in history forges adjacent rows
const text = m.content.replace(/[\r\n]+/g, ' ⏎ ')
```

**mycel**: Keep structured data in envelope metadata, not message body.
Sanitize newlines in any field that will be displayed inline.

### 12. Partial Delivery Tracking

```typescript
try {
  for (let i = 0; i < chunks.length; i++) {
    const sent = await ch.send({ content: chunks[i], ... })
    sentIds.push(sent.id)
  }
} catch (err) {
  throw new Error(`reply failed after ${sentIds.length} of ${chunks.length} chunk(s) sent: ${msg}`)
}
```

**mycel**: For multi-relay publishing. Track which relays accepted the message.
Report "published to 2 of 3 relays" on partial failure.

## Architecture Decisions Worth Adopting

| Decision | Rationale |
|----------|-----------|
| Re-read config on every message | Changes take effect without restart |
| Approval via filesystem (`approved/<id>`) | Cross-process IPC without sockets |
| Fire-and-forget for non-critical ops | `void bot.api.sendChatAction(...).catch(() => {})` |
| Typing indicator = processing signal | Immediate feedback before actual work |
| Edit ≠ notification | "send a new reply when done so the device pings" |
| Cap pending entries (max 3) | Prevent DoS via pairing flood |
| Numeric IDs over usernames | Immutable identifiers only |

## Not Applicable to mycel

- Long-polling (Telegram) — mycel uses Nostr WebSocket subscriptions
- Grammy/discord.js specifics — different transport
- MarkdownV2 escaping — Nostr uses plain text / NIP-formatted content
