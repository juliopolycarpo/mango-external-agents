//! A link driven from a script, with no process and no socket behind it.

use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;

use crate::error::{Error, Result};
use crate::link::{Link, LinkReceiver, LinkSender};

/// A [`Link`] a test writes both sides of.
///
/// Messages pushed with [`ScriptedLink::push_line`] arrive at the peer under test in order, and
/// everything it sends is recorded for [`ScriptedLink::sent`]. Clones share one script, so a test
/// keeps a handle while the link itself moves into the client.
///
/// # Example
///
/// ```
/// use mango_external_agents::testing::ScriptedLink;
///
/// let link = ScriptedLink::new();
/// link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"pong"}"#);
/// assert!(link.sent().is_empty());
/// ```
#[derive(Clone, Debug, Default)]
pub struct ScriptedLink {
    state: Arc<ScriptState>,
}

#[derive(Debug, Default)]
struct ScriptState {
    incoming: Mutex<Vec<String>>,
    sent: Mutex<Vec<String>>,
    ended: Mutex<bool>,
    send_failure: Mutex<Option<String>>,
    changed: Notify,
}

impl ScriptedLink {
    /// An empty script: nothing arrives until a test says so.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one message for the peer under test to read.
    pub fn push_line(&self, message: impl Into<String>) {
        self.state
            .incoming
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message.into());
        self.state.changed.notify_waiters();
    }

    /// Ends the stream, as a peer that exited would.
    pub fn end(&self) {
        *self
            .state
            .ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.state.changed.notify_waiters();
    }

    /// Makes every send fail, as a child whose stdin is gone would.
    pub fn fail_sends(&self, message: impl Into<String>) {
        *self
            .state
            .send_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(message.into());
    }

    /// Everything the peer under test has sent, in order.
    pub fn sent(&self) -> Vec<String> {
        self.state
            .sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until at least `count` messages have been sent.
    ///
    /// For the ordinary shape of a protocol test: queue an answer only once the question is
    /// actually on the wire, rather than sleeping and hoping.
    pub async fn wait_for_sent(&self, count: usize) {
        loop {
            let changed = self.state.changed.notified();
            if self.sent().len() >= count {
                return;
            }
            changed.await;
        }
    }

    /// The link itself, for whatever is being tested.
    pub fn into_link(self) -> Link {
        Link::new(
            Box::new(ScriptedSender {
                state: Arc::clone(&self.state),
            }),
            Box::new(ScriptedReceiver { state: self.state }),
        )
    }
}

struct ScriptedSender {
    state: Arc<ScriptState>,
}

#[async_trait::async_trait]
impl LinkSender for ScriptedSender {
    async fn send(&mut self, message: String) -> Result<()> {
        // A closed link refuses, as a child whose stdin was dropped would. Recording the write
        // instead would make this fake the one place a post-close send succeeds, so a peer that
        // writes after closing would pass here and hang against the real thing.
        if *self
            .state
            .ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(Error::Closed {
                subject: "scripted link",
            });
        }
        if let Some(failure) = self
            .state
            .send_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Err(Error::Link {
                peer: String::from("scripted link"),
                message: failure,
            });
        }
        self.state
            .sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message);
        self.state.changed.notify_waiters();
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        *self
            .state
            .ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.state.changed.notify_waiters();
        Ok(())
    }
}

struct ScriptedReceiver {
    state: Arc<ScriptState>,
}

#[async_trait::async_trait]
impl LinkReceiver for ScriptedReceiver {
    async fn recv(&mut self) -> Result<Option<String>> {
        loop {
            let changed = self.state.changed.notified();
            {
                let mut incoming = self
                    .state
                    .incoming
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if !incoming.is_empty() {
                    return Ok(Some(incoming.remove(0)));
                }
                if *self
                    .state
                    .ended
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                {
                    return Ok(None);
                }
            }
            changed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ScriptedLink;

    #[tokio::test]
    async fn delivers_queued_messages_in_order_then_ends() {
        let link = ScriptedLink::new();
        link.push_line("first");
        link.push_line("second");
        link.end();
        let (_, mut receiver) = link.into_link().split();

        assert_eq!(
            receiver.recv().await.expect("expected a message"),
            Some(String::from("first"))
        );
        assert_eq!(
            receiver.recv().await.expect("expected a message"),
            Some(String::from("second"))
        );
        assert_eq!(receiver.recv().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn a_receiver_waits_for_a_message_that_has_not_been_queued_yet() {
        let link = ScriptedLink::new();
        let waiting = {
            let link = link.clone();
            tokio::spawn(async move {
                let (_, mut receiver) = link.into_link().split();
                receiver.recv().await
            })
        };

        link.push_line("late");
        assert_eq!(
            waiting
                .await
                .expect("expected the task to finish")
                .expect("expected a message"),
            Some(String::from("late"))
        );
    }

    /// A fake that kept accepting writes after `close` would be the one link on which a
    /// post-close send succeeds, so a peer that writes into a link it already ended would pass
    /// every test here and hang against a real child.
    #[tokio::test]
    async fn a_send_after_the_link_was_closed_is_refused_rather_than_recorded() {
        let link = ScriptedLink::new();
        let (mut sender, _) = link.clone().into_link().split();

        sender
            .send(String::from("one"))
            .await
            .expect("expected the send to land");
        sender.close().await.expect("expected a clean close");

        let error = sender
            .send(String::from("two"))
            .await
            .expect_err("expected a refusal, received a send");
        assert!(
            matches!(
                error,
                crate::error::Error::Closed {
                    subject: "scripted link"
                }
            ),
            "received {error:?}"
        );
        assert_eq!(link.sent(), vec![String::from("one")]);
    }

    #[tokio::test]
    async fn records_what_was_sent_and_can_refuse_to_send_at_all() {
        let link = ScriptedLink::new();
        let (mut sender, _) = link.clone().into_link().split();

        sender
            .send(String::from("one"))
            .await
            .expect("expected the send to land");
        assert_eq!(link.sent(), vec![String::from("one")]);

        link.fail_sends("EPIPE");
        let error = sender
            .send(String::from("two"))
            .await
            .expect_err("expected a refusal, received a send");
        assert!(error.to_string().contains("EPIPE"), "received {error}");
        assert_eq!(link.sent().len(), 1);
    }
}
