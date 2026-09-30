//! The supervisor's re-send path: a turn with attachments that needs several attempts.
//!
//! Every attempt after the first that reaches the vendor session copies the request (input plus attachments, up to the 8 MiB
//! budget) and the recovery record digests it again. Time runs on a paused clock, so the backoff
//! costs nothing and the samples are the CPU cost of the retries. Run with:
//!
//! ```sh
//! cargo bench -p hub-host --bench retry -- <mib> <retries> <samples>
//! ```
//!
//! Prints one line: median and range in milliseconds and the process's peak resident set from
//! `/proc/self/status` (Linux only, `n/a` elsewhere), then every sample.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hub_host::testing::{FakeHubApi, FakeVendorSession, HubCallKind, ScriptedJitter, TurnAnswer};
use hub_host::{HubApi, RetryPolicy, Settled, Stop, Supervisor};
use mango_external_agents::{Attachment, AttachmentKind, SystemClock, TerminalStatus, TurnRequest};

const MIB: usize = 1024 * 1024;

fn request(mib: usize) -> TurnRequest {
    // Two attachments so the copy is not one allocation; the input is a realistic prompt.
    let each = mib * MIB / 2;
    let attachments = (0..2u8)
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
    let numbers: Vec<usize> = std::env::args()
        .filter_map(|argument| argument.parse().ok())
        .collect();
    let mib = numbers.first().copied().unwrap_or(8);
    let retries = numbers.get(1).copied().unwrap_or(3);
    let samples = numbers.get(2).copied().unwrap_or(15);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("expected a current-thread runtime with a paused clock");

    let mut millis = Vec::with_capacity(samples);
    for _ in 0..samples {
        let request = request(mib);
        let started = Instant::now();
        runtime.block_on(run_once(request, retries));
        millis.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let mut sorted = millis.clone();
    sorted.sort_by(f64::total_cmp);
    println!(
        "retry/{mib}MiB/{retries}retries median_ms={:.2} min_ms={:.2} max_ms={:.2} peak_rss={}",
        sorted[sorted.len() / 2],
        sorted[0],
        sorted[sorted.len() - 1],
        peak_resident_kib()
    );
    let listed: Vec<String> = millis.iter().map(|value| format!("{value:.2}")).collect();
    println!("samples_ms: {}", listed.join(" "));
}
