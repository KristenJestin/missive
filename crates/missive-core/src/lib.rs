//! What Missive's programs share: IMAP, SMTP, parsing, local copy, search, keyring, socket
//! protocol.

#[cfg(feature = "fake")]
pub mod fake;
pub mod tls;
