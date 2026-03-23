mod contacts;
mod doctor;
mod id;
mod inbox;
mod init;
mod send;
mod status;
mod thread;
mod watch;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mycel", version, about = "Encrypted async mailbox for AI CLI agents")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate keypair, configure relays, run self-test
    Init,
    /// Show your address (for sharing with contacts)
    Id,
    /// Send an encrypted message to a contact
    Send {
        /// Contact alias or npub address (use 'self' for send-to-self via local transport)
        recipient: String,
        /// Message text
        message: String,
        /// Deliver directly to recipient's local SQLite DB (no relay required)
        #[arg(long)]
        local: bool,
    },
    /// Fetch and display incoming messages
    Inbox {
        /// Output as JSONL (machine-readable)
        #[arg(long)]
        json: bool,
        /// Show quarantined messages from unknown senders
        #[arg(long)]
        all: bool,
        /// Read from local DB only (no relay fetch)
        #[arg(long)]
        local: bool,
    },
    /// Manage contacts (allowlist)
    Contacts {
        #[command(subcommand)]
        action: contacts::ContactsAction,
    },
    /// Check relay health, key status, connectivity
    Doctor,
    /// Watch inbox for new messages (foreground, runs in tmux pane)
    Watch {
        /// Poll interval in seconds
        #[arg(long, default_value = "30")]
        interval: Option<u64>,
    },
    /// Check if watch is running
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage and participate in NIP-17 group message threads
    Thread {
        #[command(subcommand)]
        action: thread::ThreadCommand,
    },
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Init => init::run().await,
            Command::Id => id::run().await,
            Command::Send { recipient, message, local } => send::run(&recipient, &message, local).await,
            Command::Inbox { json, all, local } => inbox::run(json, all, local).await,
            Command::Contacts { action } => contacts::run(action).await,
            Command::Doctor => doctor::run().await,
            Command::Watch { interval } => watch::run(interval).await,
            Command::Status { json } => status::run(json),
            Command::Thread { action } => thread::run(action).await,
        }
    }
}
