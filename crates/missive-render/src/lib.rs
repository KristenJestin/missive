//! How Missive draws a message. Nothing here fetches anything: a message is read from its own
//! bytes, and reduced with the styles Blitz computes without a network.

mod flatten;
pub mod message;
pub mod reduce;
