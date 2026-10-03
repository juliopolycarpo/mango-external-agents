//! Owned messages on their way through the JSON-RPC client and the stdio link.
//!
//! `dispatch/*` feeds prebuilt frames to a [`Client`] and waits for the peer to end, so the timed
//! part is parsing and routing (including the hand-off to the handler). `stdio-send/*` writes
//! prebuilt messages through the link a stdio transport returns, into a sink that discards, and
//! times each send from the call to the end of its write: the link frees the message it was handed
//! after that, and what a free costs belongs to the allocator, not to the send. Frames and
//! messages are built in `setup`. Run with:
//!
//! ```sh
//! cargo bench -p mango-external-agents --bench copies
//! ```

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mango_external_agents::host::HostContext;
use mango_external_agents::jsonrpc::{
    Client, ClientOptions, PeerHandler, PeerTermination, RequestId, ServerRequestOutcome,
};
use mango_external_agents::process::{
    ByteSink, ByteSource, ExitStatus, LaunchSpec, ManagedProcess, ProcessControl, ProcessLauncher,
};
use mango_external_agents::transport::{ExecutablePath, StdioSpec};
use mango_external_agents::transports::stdio;
use mango_external_agents::{CancelReason, Link, LinkReceiver, Result};
use serde_json::{Value, json};
use support::{Bench, Unit};
use tokio::sync::Notify;

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

/// Text deltas dispatched per sample.
const DELTAS: usize = 1000;

/// Large frames dispatched per sample.
const LARGE_FRAMES: usize = 20;

/// Delivers a fixed queue of frames, then reports the peer gone.
///
/// Yields before every frame so the notification worker drains between frames and the queue caps
/// never decide the outcome of a case.
struct FrameSource(VecDeque<String>);

#[async_trait::async_trait]
impl LinkReceiver for FrameSource {
    async fn recv(&mut self) -> Result<Option<String>> {
        tokio::task::yield_now().await;
        Ok(self.0.pop_front())
    }
}

/// Discards outgoing messages.
struct DiscardSender;

#[async_trait::async_trait]
impl mango_external_agents::LinkSender for DiscardSender {
    async fn send(&mut self, _message: String) -> Result<()> {
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Counts notifications and wakes the bench when the peer has ended.
#[derive(Default)]
struct CountingHandler {
    notifications: AtomicUsize,
    ended: Notify,
}

#[async_trait::async_trait]
impl PeerHandler for CountingHandler {
    async fn on_notification(&self, method: String, params: Value) {
        std::hint::black_box((method, params));
        self.notifications.fetch_add(1, Ordering::AcqRel);
    }

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        ServerRequestOutcome::Answer(Value::Null)
    }

    async fn on_terminated(&self, _termination: PeerTermination) {
        self.ended.notify_one();
    }
}

/// A `text` of about `len` bytes.
fn text(len: usize) -> String {
    "the quick brown fox ".repeat(len / 20 + 1)[..len].to_owned()
}

/// A notification frame carrying a 1 KiB text delta.
fn delta_frame() -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "item/agentMessage/delta",
        "params": {"threadId": "thread-1", "turnId": "turn-1", "itemId": "item-1", "delta": text(KIB)},
    })
    .to_string()
}

/// A tree of about 500 KB shaped like a large diff: many changes, each with a path and a hunk.
fn diff_tree() -> Value {
    let changes: Vec<Value> = (0..2000)
        .map(|index| {
            json!({
                "path": format!("crates/example/src/module_{index}.rs"),
                "kind": {"type": "update", "move_path": null},
                "diff": text(200),
            })
        })
        .collect();
    json!({"threadId": "thread-1", "turnId": "turn-1", "itemId": "item-2", "changes": changes})
}

/// A notification frame carrying the diff tree.
fn diff_notification() -> String {
    json!({"jsonrpc": "2.0", "method": "item/fileChange/patchUpdated", "params": diff_tree()})
        .to_string()
}

/// A success response frame carrying the diff tree, for a request nobody is waiting on.
fn diff_response() -> String {
    json!({"jsonrpc": "2.0", "id": 424242, "result": diff_tree()}).to_string()
}

/// An error response frame with a 1 KiB message.
fn error_response() -> String {
    json!({"jsonrpc": "2.0", "id": 424242, "error": {"code": -32000, "message": text(KIB)}})
        .to_string()
}

/// Feeds `frames` to a client and returns once the peer has ended and every callback has run.
fn dispatch(rt: &tokio::runtime::Runtime, frames: VecDeque<String>, expected: usize) {
    rt.block_on(async {
        let handler = Arc::new(CountingHandler::default());
        let mut options = ClientOptions::new("bench peer");
        options.max_pending_notifications = 4096;
        options.max_pending_bytes = 64 * MIB;
        let link = Link::new(Box::new(DiscardSender), Box::new(FrameSource(frames)));
        let client = Client::connect(link, handler.clone(), options);
        handler.ended.notified().await;
        let seen = handler.notifications.load(Ordering::Acquire);
        assert_eq!(
            seen, expected,
            "expected {expected} notifications handled, received {seen}"
        );
        drop(client);
    });
}

/// A child that accepts input and never speaks.
struct SilentControl;

#[async_trait::async_trait]
impl ProcessControl for SilentControl {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        std::future::pending().await
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        Ok(())
    }
}

struct EndedSource;

#[async_trait::async_trait]
impl ByteSource for EndedSource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

/// What a [`CountingSink`] saw: the bytes, the writes, and when the latest write ended.
struct Written {
    bytes: AtomicUsize,
    writes: AtomicUsize,
    latest: Mutex<Instant>,
}

impl Written {
    fn new() -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            latest: Mutex::new(Instant::now()),
        }
    }

    /// When the latest write ended.
    fn latest(&self) -> Instant {
        *self
            .latest
            .lock()
            .expect("expected the write clock to be unpoisoned")
    }
}

/// Counts the bytes written and the number of writes, and notes when each write ended.
struct CountingSink(Arc<Written>);

#[async_trait::async_trait]
impl ByteSink for CountingSink {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.bytes.fetch_add(bytes.len(), Ordering::AcqRel);
        self.0.writes.fetch_add(1, Ordering::AcqRel);
        std::hint::black_box(bytes);
        // Read the clock first, so storing the reading is not part of what it measures.
        let ended = Instant::now();
        *self
            .0
            .latest
            .lock()
            .expect("expected the write clock to be unpoisoned") = ended;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

struct SilentLauncher(Arc<Written>);

#[async_trait::async_trait]
impl ProcessLauncher for SilentLauncher {
    async fn spawn(&self, _spec: LaunchSpec) -> Result<ManagedProcess> {
        Ok(ManagedProcess {
            stdout: Box::new(EndedSource),
            stdin: Some(Box::new(CountingSink(Arc::clone(&self.0)))),
            control: Arc::new(SilentControl),
        })
    }
}

/// Sends every message through a stdio link, checks each went out as one write, and returns the
/// time the sends took.
///
/// A send is timed from its call to the end of its write. `LinkSender::send` takes the message by
/// value and frees it once the write returns; that part is left out, because a 1 MiB buffer goes
/// back to the kernel on glibc and the case then reports the allocator instead of the send.
fn stdio_send(rt: &tokio::runtime::Runtime, messages: Vec<String>) -> Duration {
    rt.block_on(async {
        let written = Arc::new(Written::new());
        let host = HostContext::builder()
            .launcher(Arc::new(SilentLauncher(Arc::clone(&written))))
            .cwd(std::env::temp_dir())
            .client_info("bench", "0")
            .build()
            .expect("expected the bench host to build");
        let mut transport = stdio::open(
            &host,
            &StdioSpec::new(["peer"]),
            &ExecutablePath::default(),
            &[],
        )
        .await
        .expect("expected the silent child to open");
        let count = messages.len();
        let expected: usize = messages.iter().map(|message| message.len() + 1).sum();
        let mut sending = Duration::ZERO;
        for message in messages {
            let started = Instant::now();
            transport
                .link
                .sender
                .send(message)
                .await
                .expect("expected the sink to accept the message");
            sending += written.latest().duration_since(started);
        }
        let (bytes, calls) = (
            written.bytes.load(Ordering::Acquire),
            written.writes.load(Ordering::Acquire),
        );
        assert_eq!(
            (bytes, calls),
            (expected, count),
            "expected {expected} bytes in {count} writes, received {bytes} bytes in {calls} writes"
        );
        sending
    })
}

fn main() {
    let bench = Bench::new("copies (json-rpc dispatch, stdio send)");
    let rt = support::runtime();

    let frames = |build: fn() -> String, count: usize| {
        move || {
            std::iter::repeat_with(build)
                .take(count)
                .collect::<VecDeque<_>>()
        }
    };
    bench.run(
        "dispatch/notification-1KiB",
        Unit::new(DELTAS as u64, "frame"),
        frames(delta_frame, DELTAS),
        |queue| dispatch(&rt, queue, DELTAS),
    );
    bench.run(
        "dispatch/notification-500KB-diff",
        Unit::new(LARGE_FRAMES as u64, "frame"),
        frames(diff_notification, LARGE_FRAMES),
        |queue| dispatch(&rt, queue, LARGE_FRAMES),
    );
    bench.run(
        "dispatch/response-500KB-diff",
        Unit::new(LARGE_FRAMES as u64, "frame"),
        frames(diff_response, LARGE_FRAMES),
        |queue| dispatch(&rt, queue, 0),
    );
    bench.run(
        "dispatch/error-response-1KiB",
        Unit::new(DELTAS as u64, "frame"),
        frames(error_response, DELTAS),
        |queue| dispatch(&rt, queue, 0),
    );

    // Built the way the client builds a request (`Value::to_string`), so the spare capacity a
    // real message carries is present.
    let messages = |len: usize, count: usize| {
        move || {
            (0..count)
                .map(|_| {
                    json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt", "params": {"text": text(len)}})
                        .to_string()
                })
                .collect::<Vec<String>>()
        }
    };
    bench.run_measured(
        "stdio-send/1KiB",
        Unit::new(DELTAS as u64, "message"),
        messages(KIB, DELTAS),
        |batch| (stdio_send(&rt, batch), ()),
    );
    bench.run_measured(
        "stdio-send/1MiB",
        Unit::new(LARGE_FRAMES as u64, "message"),
        messages(MIB, LARGE_FRAMES),
        |batch| (stdio_send(&rt, batch), ()),
    );
}
