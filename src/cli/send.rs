use anyhow::Result;

pub async fn run(recipient: &str, message: &str) -> Result<()> {
    // TODO Phase 1:
    // 1. Load keypair
    // 2. Resolve recipient (alias → pubkey via contacts DB)
    // 3. Check message size (C7: max 8KB)
    // 4. Build mycel envelope
    // 5. Connect to relays
    // 6. NIP-59 Gift Wrap + publish
    // 7. Wait for ≥1 relay ack (C1)
    // 8. Store in outbox
    // 9. Print result
    let _ = (recipient, message);
    println!("mycel send — not yet implemented");
    Ok(())
}
