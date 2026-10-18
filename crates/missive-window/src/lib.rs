//! Missive's reading window: one more client of the daemon and the local copy, like the command
//! line. It reads the copy, follows the daemon's events, and never writes: it is read only.

pub mod font;
pub mod images;
pub mod model;
pub mod view;
pub mod watch;
