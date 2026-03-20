use thiserror::Error;

#[derive(Error, Debug)]
pub enum MycelError {
    #[error("not initialized — run `mycel init` first")]
    NotInitialized,

    #[error("key not found or cannot be unlocked")]
    KeyError(#[source] anyhow::Error),

    #[error("message too large ({size} bytes, max {max})")]
    MessageTooLarge { size: usize, max: usize },

    #[error("no relays reachable")]
    NoRelays,

    #[error("contact not found: {0}")]
    ContactNotFound(String),

    #[error("sender blocked: {0}")]
    SenderBlocked(String),

    #[error("database error")]
    Database(#[from] rusqlite::Error),
}

/// Max message payload size (C7)
pub const MAX_MESSAGE_SIZE: usize = 8192;

/// Sync cursor overlap window in seconds (C2)
pub const SYNC_OVERLAP_SECS: u64 = 120;
