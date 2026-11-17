//! The integration tests of `mango-agent-codex`, linked as one binary.
//!
//! One module per former test file: a target of its own costs a link of the whole dependency tree.

mod activity_pairing;
mod cancel_deadlines;
mod debug_carriers;
mod debug_records;
mod notification_decode;
mod pending_turn_claim;
mod refused_event;
mod replay;
mod support;
mod terminal_closes;
#[cfg(all(unix, feature = "unix-socket"))]
mod unix_socket;
mod version_floor;
