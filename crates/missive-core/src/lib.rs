//! What Missive's programs share: IMAP, SMTP, parsing, local copy, search, keyring, socket
//! protocol.

pub mod account;
pub mod actions;
pub mod body;
pub mod compose;
pub mod connect;
pub mod copy;
pub mod drafts;
pub mod events;
#[cfg(feature = "fake")]
pub mod fake;
pub mod keyring;
pub mod mailto;
pub mod opener;
pub mod original;
pub mod outbox;
pub mod protocol;
pub mod reply;
pub mod store;
pub mod submit;
pub mod sync;
pub mod tls;
