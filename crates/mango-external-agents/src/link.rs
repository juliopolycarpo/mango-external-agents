//! A message link, whatever is carrying it.
//!
//! One message in, one message out. A child's stdout framed by lines and a WebSocket's text
//! frames are the same thing at this level, which is why the JSON-RPC client above knows nothing
//! about processes or sockets — and why a test can drive it from a script with no IO at all.
//!
//! The two halves are owned separately so a pump can read while a caller writes.

use crate::error::Result;

/// Sends messages to the peer.
#[async_trait::async_trait]
pub trait LinkSender: Send {
    /// Sends one message.
    ///
    /// # Errors
    ///
    /// [`Error::Link`](crate::Error::Link) when the peer is gone.
    async fn send(&mut self, message: String) -> Result<()>;

    /// Closes this side of the link. Idempotent.
    ///
    /// # Errors
    ///
    /// [`Error::Link`](crate::Error::Link) when the close itself failed.
    async fn close(&mut self) -> Result<()>;
}

/// Receives messages from the peer.
#[async_trait::async_trait]
pub trait LinkReceiver: Send {
    /// The next message, or `None` once the peer has gone.
    ///
    /// # Errors
    ///
    /// [`Error::Link`](crate::Error::Link) when the carrier failed, and
    /// [`Error::LimitExceeded`](crate::Error::LimitExceeded) when the peer sent more than the
    /// library will assemble.
    async fn recv(&mut self) -> Result<Option<String>>;
}

/// Both halves of one connection.
pub struct Link {
    /// Messages going out.
    pub sender: Box<dyn LinkSender>,
    /// Messages coming in.
    pub receiver: Box<dyn LinkReceiver>,
}

impl Link {
    /// One link from its two halves.
    pub fn new(sender: Box<dyn LinkSender>, receiver: Box<dyn LinkReceiver>) -> Self {
        Self { sender, receiver }
    }

    /// Takes the halves apart, for a caller that moves them to different tasks.
    pub fn split(self) -> (Box<dyn LinkSender>, Box<dyn LinkReceiver>) {
        (self.sender, self.receiver)
    }
}

impl std::fmt::Debug for Link {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Link").finish_non_exhaustive()
    }
}
