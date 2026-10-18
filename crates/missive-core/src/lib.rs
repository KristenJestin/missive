//! What Missive's programs share: IMAP, SMTP, parsing, local copy, search, keyring, socket
//! protocol.

pub mod account;
pub mod actions;
pub mod body;
pub mod connect;
pub mod copy;
pub mod events;
#[cfg(feature = "fake")]
pub mod fake;
pub mod keyring;
pub mod original;
pub mod protocol;
pub mod store;
pub mod sync;
pub mod tls;
