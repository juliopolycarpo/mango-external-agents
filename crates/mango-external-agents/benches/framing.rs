//! `LineStream` framing: how long a child's stdout takes to become lines.
//!
//! Every case builds its chunks in `setup`, so the timed part is `LineStream` and the async
//! `ByteSource` hand-off, not the test data. Run with:
//!
//! ```sh
//! cargo bench -p mango-external-agents --bench framing
//! ```

mod support;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use mango_external_agents::{ByteSource, LineLimits, LineStream, Result};
use support::{Bench, Unit};

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

/// How many times the fixture corpus is concatenated, so one sample takes milliseconds.
const FIXTURE_REPLAYS: usize = 20;

/// A source that hands out exactly the chunks it was built with, in order.
struct ChunkSource {
    chunks: VecDeque<Vec<u8>>,
}

impl ChunkSource {
    /// `bytes` cut into `chunk`-sized reads, the way a pipe delivers them.
    fn new(bytes: &[u8], chunk: usize) -> Self {
        Self {
            chunks: bytes.chunks(chunk).map(<[u8]>::to_vec).collect(),
        }
    }
}

#[async_trait::async_trait]
impl ByteSource for ChunkSource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.chunks.pop_front())
    }
}

/// One line of `len` bytes of `fill`, terminated by a newline.
fn one_line(len: usize, fill: u8) -> Vec<u8> {
    let mut bytes = vec![fill; len];
    bytes.push(b'\n');
    bytes
}

/// `count` records of `record_len` bytes each, newline included.
fn short_records(count: usize, record_len: usize) -> Vec<u8> {
    let mut record = vec![b'y'; record_len];
    record[record_len - 1] = b'\n';
    record.repeat(count)
}

/// A line of `raw_len` bytes that is invalid UTF-8 throughout, so every byte needs repair.
fn invalid_utf8_line(raw_len: usize) -> Vec<u8> {
    let mut bytes = b"{\"v\":\"".to_vec();
    bytes.extend(std::iter::repeat_n(0xff_u8, raw_len - 9));
    bytes.extend_from_slice(b"\"}\n");
    bytes
}

/// The lines of every captured transcript under `fixtures/`, as the vendor wrote them.
///
/// A transcript line carries a `>>` or `<<` direction marker that the vendor never sent; it is
/// stripped. Directory-level manifests and non-JSONL files are skipped. Returns nothing when the
/// fixtures are not next to the crate, as in a packaged copy.
fn fixture_lines() -> Vec<Vec<u8>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let mut files = Vec::new();
    collect_jsonl(&root, &mut files);
    files.sort();
    let mut lines = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            let body = line
                .strip_prefix("<<")
                .or_else(|| line.strip_prefix(">>"))
                .unwrap_or(line);
            if body.starts_with('{') {
                lines.push(body.as_bytes().to_vec());
            }
        }
    }
    lines
}

fn collect_jsonl(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, files);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
}

/// Reads `bytes` to the end at `chunk`-sized reads and returns (lines, decoded bytes).
fn frame(rt: &tokio::runtime::Runtime, source: ChunkSource) -> (usize, usize) {
    rt.block_on(async {
        let mut stream = LineStream::new(Box::new(source), LineLimits::default());
        let (mut lines, mut bytes) = (0, 0);
        while let Some(line) = stream
            .next_line()
            .await
            .expect("expected the bench input to stay within the default line limits")
        {
            lines += 1;
            bytes += line.len();
        }
        (lines, bytes)
    })
}

fn main() {
    let bench = Bench::new("framing (LineStream over a chunked ByteSource)");
    let rt = support::runtime();

    // One long line, the quadratic case: 1 MiB less the newline is the longest line the default
    // limits accept.
    let longest = MIB - 1;
    for (label, line_len, chunk) in [
        ("line-128KiB/chunk-16KiB", 128 * KIB, 16 * KIB),
        ("line-1MiB/chunk-4KiB", longest, 4 * KIB),
        ("line-1MiB/chunk-16KiB", longest, 16 * KIB),
        ("line-1MiB/chunk-1MiB", longest, MIB),
    ] {
        let bytes = one_line(line_len, b'x');
        bench.run(
            &format!("framing/{label}"),
            Unit::new(bytes.len() as u64, "byte"),
            || ChunkSource::new(&bytes, chunk),
            |source| {
                let (lines, decoded) = frame(&rt, source);
                assert_eq!(
                    (lines, decoded),
                    (1, line_len),
                    "expected one line of {line_len} bytes, received {lines} lines of {decoded} bytes"
                );
            },
        );
    }

    // Many records per read, then a record that spans reads: total framing cost, not only the
    // drain. 16 KiB is what the launcher reads from a child's stdout at a time. 15 bytes is the
    // floor, 100 bytes a short notification, 1 KiB a text delta, 64 KiB a tool result.
    for (label, record_len, records) in [
        ("records-15B", 15, 1092 * 256),
        ("records-100B", 100, 16 * KIB),
        ("records-1KiB", KIB, 2 * KIB),
        ("records-64KiB", 64 * KIB, 32),
    ] {
        let bytes = short_records(records, record_len);
        bench.run(
            &format!("framing/{label}/chunk-16KiB"),
            Unit::new(records as u64, "record"),
            || ChunkSource::new(&bytes, 16 * KIB),
            |source| {
                let (lines, _) = frame(&rt, source);
                assert_eq!(
                    lines, records,
                    "expected {records} records, received {lines}"
                );
            },
        );
    }

    // Invalid UTF-8 takes the lossy repair path. 256 KiB raw repairs to 768 KiB, which stays
    // inside the default buffer budget whether or not the repaired size is checked.
    let bytes = invalid_utf8_line(256 * KIB);
    bench.run(
        "framing/invalid-utf8-256KiB/chunk-16KiB",
        Unit::new(bytes.len() as u64, "byte"),
        || ChunkSource::new(&bytes, 16 * KIB),
        |source| {
            let (lines, _) = frame(&rt, source);
            assert_eq!(lines, 1, "expected one repaired line, received {lines}");
        },
    );

    // Traffic shaped like a real session: every captured transcript line, in order.
    let corpus = fixture_lines();
    if corpus.is_empty() {
        println!("# skipped framing/fixtures/*: no fixtures/ directory beside the crate");
        return;
    }
    let longest_fixture = corpus.iter().map(Vec::len).max().unwrap_or(0);
    let mut once = Vec::new();
    for line in &corpus {
        once.extend_from_slice(line);
        once.push(b'\n');
    }
    println!(
        "# fixture corpus: {} lines, {} bytes, longest line {} bytes; replayed {FIXTURE_REPLAYS}x",
        corpus.len(),
        once.len(),
        longest_fixture
    );
    // A single pass is under a millisecond, which timer and scheduler noise would swamp.
    let bytes = once.repeat(FIXTURE_REPLAYS);
    let expected_lines = corpus.len() * FIXTURE_REPLAYS;
    for chunk in [4 * KIB, 16 * KIB] {
        bench.run(
            &format!("framing/fixtures/chunk-{}KiB", chunk / KIB),
            Unit::new(expected_lines as u64, "line"),
            || ChunkSource::new(&bytes, chunk),
            |source| {
                let (lines, _) = frame(&rt, source);
                assert_eq!(
                    lines, expected_lines,
                    "expected {expected_lines} fixture lines, received {lines}"
                );
            },
        );
    }
}
