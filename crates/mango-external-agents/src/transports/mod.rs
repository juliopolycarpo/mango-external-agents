//! How bytes reach a harness, one module per transport kind.
//!
//! Each one produces the same [`Link`](crate::Link), so everything above — the JSON-RPC client, a
//! harness's reducer — is written once and works over any of them. Which kinds a harness accepts
//! is declared in its descriptor and enforced before anything is spawned.

#[cfg(any(feature = "websocket", all(unix, feature = "unix-socket")))]
mod socket;
#[cfg(feature = "stdio")]
pub mod stdio;
#[cfg(all(unix, feature = "unix-socket"))]
pub mod unix_socket;
#[cfg(feature = "websocket")]
pub mod websocket;
