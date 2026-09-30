//! The supervisor's re-send path: a turn with attachments that needs several attempts.
//!
//! Every attempt after the first that reaches the vendor session copies the request (input plus attachments, up to the 8 MiB
//! budget) and the recovery record digests it again. Time runs on a paused clock, so the backoff
//! costs nothing and the samples are the CPU cost of the retries. Run with:
//!
//! ```sh
//! cargo bench -p hub-host --bench retry
//! BENCH_SAMPLES=25 cargo bench -p hub-host --bench retry -- 8MiB/3retries
//! ```
//!
//! The cases are named `retry/<attachments>/<retries>retries`, where `<attachments>` is the total
//! size of the request's two attachments or `none`. Timing, the sample count, the case filter and
//! the printed environment header come from the shared runner in `support/mod.rs`, the same as
//! every other crate's benches. After the cases this bench prints the process's peak resident set
//! from `/proc/self/status` (Linux only, `n/a` elsewhere), which the runner does not measure.

mod support;

use std::sync::Arc;
use std::time::Duration;

use hub_host::testing::{FakeHubApi, FakeVendorSession, HubCallKind, ScriptedJitter, TurnAnswer};
use hub_host::{HubApi, RetryPolicy, Settled, Stop, Supervisor};
use mango_external_agents::{Attachment, AttachmentKind, SystemClock, TerminalStatus, TurnRequest};
use support::{Bench, Unit};

const MIB: usize = 1024 * 1024;

/// The cases: name, total attachment MiB (`0` is a request with none) and the re-sends each needs.
const CASES: [(&str, usize, usize); 5] = [
    ("retry/8MiB/3retries", 8, 3),
    ("retry/8MiB/1retries", 8, 1),
    ("retry/2MiB/3retries", 2, 3),
    ("retry/1MiB/3retries", 1, 3),
    ("retry/none/3retries", 0, 3),
];

fn request(mib: usize) -> TurnRequest {
    // Two attachments so the copy is not one allocation; the input is a realistic prompt.
    let each = mib * MIB / 2;
    let attachments = (0..2u8)
        .filter(|_| mib > 0)
        .map(|index| Attachment {
            id: format!("file-{index}"),
            name: format!("file-{index}.bin"),
            mime_type: String::from("application/octet-stream"),
            kind: AttachmentKind::Data,
            bytes: vec![index + 1; each],
        })
        .collect();
    TurnRequest::new("bench-turn", "summarise the attached files").with_attachments(attachments)
}

async fn run_once(request: TurnRequest, retries: usize) {
    // Each re-send reserves at the Hub and then reaches the vendor session, which answers that the
    // request never left the host: the supervisor takes a newer attempt and builds a new request.
    let hub = Arc::new(FakeHubApi::new());
    let session = FakeVendorSession::new().answering(vec![TurnAnswer::NotSubmitted; retries]);
    let policy = RetryPolicy::new(
        Duration::from_millis(10),
        Duration::from_millis(80),
        Duration::from_secs(30),
        Arc::new(ScriptedJitter::maximum()),
    );
    let mut supervisor = Supervisor::new(
        Box::new(session.clone()),
        Arc::clone(&hub) as Arc<dyn HubApi>,
        policy,
        Arc::new(Stop::new()),
        Arc::new(SystemClock),
    );
    let settled = supervisor
        .run(request)
        .await
        .expect("expected the turn to settle");
    assert!(
        matches!(
            settled,
            Settled::Committed {
                terminal: TerminalStatus::Completed,
                ..
            }
        ),
        "expected a committed completed turn | received {settled:?}"
    );
    let reserves = hub.count(HubCallKind::Reserve);
    assert_eq!(
        reserves,
        retries + 1,
        "expected {} reservations | received {reserves}",
        retries + 1
    );
    assert_eq!(
        session.start_count(),
        1,
        "expected only the last attempt to start vendor work | received {} starts",
        session.start_count()
    );
}

fn peak_resident_kib() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .map(|rest| rest.trim().to_owned())
            })
        })
        .unwrap_or_else(|| String::from("n/a"))
}

fn main() {
    // A paused clock makes every backoff free, so a sample is the retries' CPU cost alone.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("expected a current-thread runtime with a paused clock");
    let bench = Bench::new("retry");
    for (name, mib, retries) in CASES {
        bench.run(
            name,
            Unit::new(retries as u64, "retry"),
            || request(mib),
            |request| runtime.block_on(run_once(request, retries)),
        );
    }
    println!(
        "# peak resident set (process high-water mark): {}",
        peak_resident_kib()
    );
}
