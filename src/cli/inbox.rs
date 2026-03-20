use anyhow::Result;

pub async fn run(json: bool, all: bool) -> Result<()> {
    // TODO Phase 1:
    // 1. Load keypair
    // 2. Connect to relays
    // 3. Fetch kind 1059 events (since = last_sync - 120s, C2)
    // 4. Wait for EOSE
    // 5. Unwrap Gift Wrap → decrypt → parse envelope
    // 6. Dedup by nostr_id
    // 7. Trust tier check (KNOWN/UNKNOWN/BLOCKED)
    // 8. Store in inbox
    // 9. Update sync cursor
    // 10. Display (sanitized per C6, or JSONL per C5)
    let _ = (json, all);
    println!("mycel inbox — not yet implemented");
    Ok(())
}
