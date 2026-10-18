//! What Missive's programs share: IMAP, SMTP, parsing, local copy, search, keyring, socket
//! protocol.

pub mod account;
pub mod connect;
#[cfg(feature = "fake")]
pub mod fake;
pub mod keyring;
pub mod store;
pub mod sync;
pub mod tls;
