use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum ContactsAction {
    /// Add a contact to allowlist
    Add {
        /// npub address
        address: String,
        /// Friendly alias
        #[arg(long)]
        alias: Option<String>,
    },
    /// Block a contact
    Block {
        /// npub address or alias
        address: String,
    },
    /// List all contacts
    List,
}

pub async fn run(action: ContactsAction) -> Result<()> {
    match action {
        ContactsAction::Add { address, alias } => {
            let _ = (address, alias);
            println!("mycel contacts add — not yet implemented");
        }
        ContactsAction::Block { address } => {
            let _ = address;
            println!("mycel contacts block — not yet implemented");
        }
        ContactsAction::List => {
            println!("mycel contacts list — not yet implemented");
        }
    }
    Ok(())
}
