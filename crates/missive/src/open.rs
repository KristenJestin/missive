//! `missive open`: a message's cleaned page ([`missive_core::original`]), written to a private
//! file and opened with the system's default handler ([`missive_core::opener`]): the desktop
//! portal, else `xdg-open`. Nothing names a browser.

use clap::Args;
use missive_core::opener;

use crate::read::{self, ShowError};

#[derive(Args)]
pub struct OpenArgs {
    /// The message's identifier, as `search` and `list` print it.
    #[arg(value_name = "MESSAGE ID")]
    id: String,
    /// The original message, cleaned, in the default browser. It is the only view until the
    /// window exists; scripts that want the browser pass it already.
    #[arg(long)]
    original: bool,
}

pub async fn run(args: OpenArgs) -> Result<(), ShowError> {
    // Without the window, the original is the only view: `--original` changes nothing yet.
    let OpenArgs { id, original: _ } = args;
    let stored = read::message(&id)?;
    let path = opener::open_original(&stored.raw).await?;
    println!("{}", path.display());
    Ok(())
}
