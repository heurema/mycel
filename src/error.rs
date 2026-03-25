use thiserror::Error;

#[derive(Error, Debug)]
pub enum MycelError {
    #[error("not initialized — run `mycel init` first")]
    NotInitialized,

    #[error("already initialized — use `mycel id` to view your address")]
    AlreadyInitialized,

    #[error("message too large ({size} bytes, max {max})")]
    MessageTooLarge { size: usize, max: usize },

    #[error("message cannot be empty")]
    EmptyMessage,

    #[error("no relays reachable")]
    NoRelays,

    #[error("alias already in use: '{alias}' (by {pubkey})")]
    AliasCollision { alias: String, pubkey: String },

    #[error("thread not found: {thread_id}")]
    ThreadNotFound { thread_id: String },

    #[error("thread member limit (10) exceeded")]
    ThreadMemberLimitExceeded,

    #[error("invalid thread id: {thread_id}")]
    #[allow(dead_code)] // Used when thread log validates thread_id format
    InvalidThreadId { thread_id: String },
}

/// Max message payload size (C7)
pub const MAX_MESSAGE_SIZE: usize = 8192;

/// Maximum number of outbox delivery retries before marking a message as failed_permanent.
pub const MAX_RETRIES: u32 = 10;

/// Sync cursor overlap window in seconds (C2)
/// NIP-59 Gift Wrap randomises the outer event timestamp by up to ±2 days
/// for metadata privacy. The overlap must cover this window so that
/// `since`-filtered relay queries never miss randomised events.
pub const SYNC_OVERLAP_SECS: u64 = 2 * 24 * 3600; // 48 hours

/// Max events to process per sync cycle (relay spam protection)
pub const MAX_EVENTS_PER_SYNC: usize = 1000;
