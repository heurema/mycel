mod contacts;
mod doctor;
mod id;
mod inbox;
mod init;
mod send;

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
        /// Contact alias or npub address
        recipient: String,
        /// Message text
        message: String,
    },
    /// Fetch and display incoming messages
    Inbox {
        /// Output as JSONL (machine-readable)
        #[arg(long)]
        json: bool,
        /// Show quarantined messages from unknown senders
        #[arg(long)]
        all: bool,
    },
    /// Manage contacts (allowlist)
    Contacts {
        #[command(subcommand)]
        action: contacts::ContactsAction,
    },
    /// Check relay health, key status, connectivity
    Doctor,
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Init => init::run().await,
            Command::Id => id::run().await,
            Command::Send { recipient, message } => send::run(&recipient, &message).await,
            Command::Inbox { json, all } => inbox::run(json, all).await,
            Command::Contacts { action } => contacts::run(action).await,
            Command::Doctor => doctor::run().await,
        }
    }
}
