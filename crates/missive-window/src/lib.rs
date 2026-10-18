//! Missive's reading window: one more client of the daemon and the local copy, like the command
//! line. It reads the copy, follows the daemon's events, and asks the daemon for what the user
//! does to mail: it never writes the copy nor talks to a server itself.

pub mod ask;
pub mod font;
pub mod images;
pub mod model;
pub mod view;
pub mod watch;
