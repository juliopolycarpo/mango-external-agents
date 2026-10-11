//! The line framing between a child's pipes and the connection's link.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use mango_external_agents::process::{ByteSink, ByteSource, LineLimits};
use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::{
    CancelReason, Error, ExitStatus, HostContext, Limits, ManagedProcess, ProcessControl, Result,
};

use super::frame;

/// What a [`RecordingStdin`] was asked to do, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StdinCall {
    Write(Vec<u8>),
    Close,
}

/// A child's stdin that records each write as the bytes of one call.
struct RecordingStdin {
    calls: Arc<Mutex<Vec<StdinCall>>>,
}

#[async_trait::async_trait]
impl ByteSink for RecordingStdin {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.calls
            .lock()
            .expect("expected the call record")
            .push(StdinCall::Write(bytes.to_vec()));
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.calls
            .lock()
            .expect("expected the call record")
            .push(StdinCall::Close);
        Ok(())
    }
}

/// A child's stdout that yields the scripted chunks and then ends.
struct ScriptedStdout {
    chunks: VecDeque<Vec<u8>>,
}

impl ScriptedStdout {
    fn of(chunks: &[&str]) -> Self {
        Self {
            chunks: chunks
                .iter()
                .map(|chunk| chunk.as_bytes().to_vec())
                .collect(),
        }
    }
}

#[async_trait::async_trait]
impl ByteSource for ScriptedStdout {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.chunks.pop_front())
    }
}

/// A child nobody needs to stop.
struct IdleChild;

#[async_trait::async_trait]
impl ProcessControl for IdleChild {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        Ok(ExitStatus {
            code: Some(0),
            signal: None,
        })
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        Ok(())
    }
}

fn host_with(limits: Limits) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(FakeLauncher::new()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(limits)
        .outbound_buffer_bytes(4_096)
        .build()
        .expect("expected a host")
}

/// A child with the scripted stdout and a recording stdin, and the record.
fn child(stdout: &[&str]) -> (ManagedProcess, Arc<Mutex<Vec<StdinCall>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let process = ManagedProcess {
        stdout: Box::new(ScriptedStdout::of(stdout)),
        stdin: Some(Box::new(RecordingStdin {
            calls: Arc::clone(&calls),
        })),
        control: Arc::new(IdleChild),
    };
    (process, calls)
}

#[tokio::test]
async fn a_message_is_written_with_its_newline_in_one_write_and_close_ends_stdin() {
    let (process, calls) = child(&[]);
    let mut launched = frame(process, &host_with(Limits::default())).expect("expected a link");

    launched
        .link
        .sender
        .send(String::from(
            r#"{"jsonrpc":"2.0","method":"session/cancel"}"#,
        ))
        .await
        .expect("expected the write to succeed");
    launched
        .link
        .sender
        .close()
        .await
        .expect("expected the close to succeed");

    let received = calls.lock().expect("expected the call record").clone();
    let expected = vec![
        StdinCall::Write(b"{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\"}\n".to_vec()),
        StdinCall::Close,
    ];
    assert_eq!(
        received, expected,
        "expected one write of the frame and its newline, then a close | received: {received:?}"
    );
}

#[tokio::test]
async fn stdout_is_read_one_message_per_line_until_it_ends() {
    let (process, _calls) = child(&["{\"a\":1}\n{\"b\"", ":2}\n"]);
    let mut launched = frame(process, &host_with(Limits::default())).expect("expected a link");

    let mut received = Vec::new();
    while let Some(line) = launched
        .link
        .receiver
        .recv()
        .await
        .expect("expected a readable stdout")
    {
        received.push(line);
    }
    let expected = [r#"{"a":1}"#, r#"{"b":2}"#];
    assert_eq!(
        received, expected,
        "expected one message per line: {expected:?} | received: {received:?}"
    );
}

#[tokio::test]
async fn a_line_over_the_hosts_cap_is_an_error_naming_the_limit() {
    let limits = Limits {
        line: LineLimits {
            max_line_bytes: 8,
            max_buffered_bytes: 64,
        },
        ..Limits::default()
    };
    let (process, _calls) = child(&["0123456789abcdef\n"]);
    let mut launched = frame(process, &host_with(limits)).expect("expected a link");

    let received = launched.link.receiver.recv().await;
    assert!(
        matches!(&received, Err(Error::LimitExceeded { limit: 8, .. })),
        "expected Err(LimitExceeded {{ limit: 8, .. }}) for a 16-byte line | received: {received:?}"
    );
}

#[tokio::test]
async fn the_hosts_outgoing_budget_travels_with_the_launched_agent() {
    let (process, _calls) = child(&[]);
    let launched = frame(process, &host_with(Limits::default())).expect("expected a link");
    assert_eq!(
        launched.outbound_buffer_bytes, 4_096,
        "expected the host's outbound_buffer_bytes: 4096 | received: {}",
        launched.outbound_buffer_bytes
    );
}

#[tokio::test]
async fn a_child_without_stdin_is_refused_as_a_link_that_cannot_be_written() {
    let process = ManagedProcess {
        stdout: Box::new(ScriptedStdout::of(&[])),
        stdin: None,
        control: Arc::new(IdleChild),
    };
    let refused = frame(process, &host_with(Limits::default()));
    assert!(
        matches!(&refused, Err(Error::Link { message, .. }) if message == "a child without a writable stdin"),
        "expected Err(Link {{ message: \"a child without a writable stdin\" }}) | received: {refused:?}"
    );
}
