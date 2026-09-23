//! Budget the official SDK's frame boundary before its unbounded actor channels can grow.

use std::collections::VecDeque;
use std::sync::Arc;

use agent_client_protocol::{Channel, ConnectTo, TransportFrame, role::Role};
use futures::{FutureExt, SinkExt, StreamExt, future::BoxFuture};
use mango_external_agents::Limits;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use super::{IncomingLines, OutgoingLines};

/// A host-framed ACP transport with bounded queued frame counts and serialized bytes.
///
/// Constructed by [`super::frame`]; pass `launched.transport` to the official client's
/// `connect_with` method. Exceeding either budget fails the connection instead of waiting behind
/// a stalled protocol actor or physical writer.
pub struct BoundedTransport {
    outgoing: OutgoingLines,
    incoming: IncomingLines,
    limits: Limits,
}

impl BoundedTransport {
    pub(super) fn new(outgoing: OutgoingLines, incoming: IncomingLines, limits: Limits) -> Self {
        Self {
            outgoing,
            incoming,
            limits,
        }
    }

    fn parts(
        self,
    ) -> (
        Channel,
        BoxFuture<'static, agent_client_protocol::Result<()>>,
    ) {
        let (transport, client) = Channel::duplex();
        let Self {
            incoming,
            outgoing,
            limits,
        } = self;
        let future = async move {
            let (pending, writing) = mpsc::channel(frame_limit(&limits));
            let budget = Arc::new(Semaphore::new(
                limits.turn_buffer_bytes.min(u32::MAX as usize),
            ));
            futures::try_join!(
                read_frames(incoming, transport.tx, limits),
                queue_output(transport.rx, pending, budget, limits),
                write_frames(outgoing, writing),
            )?;
            Ok(())
        }
        .boxed();
        (client, future)
    }
}

impl<R: Role> ConnectTo<R> for BoundedTransport {
    async fn connect_to(
        mut self,
        client: impl ConnectTo<R::Counterpart>,
    ) -> agent_client_protocol::Result<()> {
        let stop_input = mango_external_agents::CancelToken::new();
        let input_stopped = stop_input.clone();
        self.incoming = Box::pin(self.incoming.take_until(async move {
            input_stopped.cancelled().await;
        }));
        let (channel, transport) = self.parts();
        let close_output = channel.tx.clone();
        let client = async move {
            let result = client.connect_to(channel).await;
            // Match the SDK carrier: a finished peer stops reading physical input while already
            // accepted outgoing frames drain before the writer closes.
            stop_input.cancel();
            close_output.close_channel();
            result
        };
        futures::try_join!(client, transport)?;
        Ok(())
    }

    fn into_channel_and_future(
        self,
    ) -> (
        Channel,
        BoxFuture<'static, agent_client_protocol::Result<()>>,
    ) {
        self.parts()
    }
}

/// Account for a single producer's queued frames using the SDK channel's current queue length.
/// Each queued frame is charged its bytes and its JSON-RPC message count (one, or every member of a
/// batch). Charges are removed only after the consumer has taken the corresponding frame; a
/// concurrent dequeue can make this conservative but can never let extra messages or bytes past
/// the cap.
async fn read_frames(
    mut source: IncomingLines,
    target: futures::channel::mpsc::UnboundedSender<TransportFrame>,
    limits: Limits,
) -> agent_client_protocol::Result<()> {
    let mut charges: VecDeque<(usize, usize)> = VecDeque::new();
    let mut bytes = 0_usize;
    let mut messages = 0_usize;
    while let Some(line) = source.next().await {
        let line = line.map_err(|_| failure("ACP framed input failed"))?;
        let queued = target.len();
        while charges.len() > queued {
            let (size, count) = charges.pop_front().expect("queued frame charge");
            bytes -= size;
            messages -= count;
        }
        let frame = TransportFrame::parse_json(&line);
        let count = match &frame {
            TransportFrame::Batch(batch) => batch.len().max(1),
            _ => 1,
        };
        bytes = bytes.saturating_add(line.len());
        messages = messages.saturating_add(count);
        if messages > frame_limit(&limits) || bytes > limits.turn_buffer_bytes {
            return Err(failure(format!(
                "ACP incoming queue exceeded its message or byte budget (Limits::turn_channel_capacity, Limits::turn_buffer_bytes): received {messages} messages and {bytes} bytes; expected at most {} messages and {} bytes",
                frame_limit(&limits),
                limits.turn_buffer_bytes,
            )));
        }
        charges.push_back((line.len(), count));
        target
            .unbounded_send(frame)
            .map_err(|_| failure("ACP incoming frame receiver closed"))?;
        // The SDK's unbounded send has no cooperative scheduling point. Give its protocol actor
        // and our output budget a chance to run even when a source always returns ready chunks.
        tokio::task::yield_now().await;
    }
    Ok(())
}

struct PendingFrame {
    line: String,
    _bytes: OwnedSemaphorePermit,
}

async fn queue_output(
    mut frames: futures::channel::mpsc::UnboundedReceiver<TransportFrame>,
    pending: mpsc::Sender<PendingFrame>,
    budget: Arc<Semaphore>,
    limits: Limits,
) -> agent_client_protocol::Result<()> {
    while let Some(frame) = frames.next().await {
        let line = frame.to_json()?;
        let byte_limit = limits.turn_buffer_bytes.min(u32::MAX as usize);
        if line.len() > byte_limit {
            return Err(failure(format!(
                "ACP output frame exceeded the byte budget: received {} bytes; expected at most {byte_limit}",
                line.len(),
            )));
        }
        let size = u32::try_from(line.len()).expect("frame length fits the byte budget");
        let bytes = Arc::clone(&budget)
            .try_acquire_many_owned(size)
            .map_err(|_| failure(format!(
                "ACP outgoing frame queue exceeded the byte budget: received {} more bytes; expected at most {} available bytes",
                line.len(), budget.available_permits(),
            )))?;
        pending
            .try_send(PendingFrame {
                line,
                _bytes: bytes,
            })
            .map_err(|_| failure(format!(
                "ACP outgoing frame queue exceeded the frame budget (Limits::turn_channel_capacity): received another frame; expected at most {} queued frames",
                pending.max_capacity(),
            )))?;
    }
    Ok(())
}

async fn write_frames(
    mut output: OutgoingLines,
    mut pending: mpsc::Receiver<PendingFrame>,
) -> agent_client_protocol::Result<()> {
    while let Some(frame) = pending.recv().await {
        output
            .send(frame.line)
            .await
            .map_err(|_| failure("ACP framed output failed"))?;
        // Retain the byte permit through the physical write, including a stalled ByteSink.
        drop(frame._bytes);
    }
    output
        .close()
        .await
        .map_err(|_| failure("ACP framed output close failed"))
}

/// The most frames queued at the SDK boundary in either direction.
///
/// Frames are mostly `session/update` notifications, each of which becomes at most one turn event,
/// so the queue shares the turn's event capacity. Request concurrency (`max_pending_requests`) is a
/// different quantity and is enforced by the SDK-facing request paths. For example, the default
/// limits allow 1,024 queued frames.
fn frame_limit(limits: &Limits) -> usize {
    limits.turn_channel_capacity.max(1)
}

fn failure(message: impl Into<String>) -> agent_client_protocol::Error {
    let mut error = agent_client_protocol::Error::internal_error();
    error.message = message.into();
    error
}

#[cfg(test)]
mod tests;
