//! Bounded, tail-first reads for Codex rollout reply recovery.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
#[cfg(not(unix))]
use std::time::UNIX_EPOCH as STANDARD_UNIX_EPOCH;
use sha2::{Digest, Sha256};

pub(super) const CODEX_ROLLOUT_SCAN_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const MAX_CODEX_ROLLOUT_SCAN_BYTES: u64 = 128 * 1024 * 1024;
pub(super) const MAX_CODEX_ROLLOUT_LINE_BYTES: usize = 8 * 1024 * 1024;
const CODEX_ROLLOUT_CACHE_OVERLAP_BYTES: u64 = MAX_CODEX_ROLLOUT_LINE_BYTES as u64 + 1;
const PREFIX_ANCHOR_BYTES: u64 = 4096;
const NEGATIVE_CACHE_CAP: usize = 256;
const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy)]
struct ScanLimits {
    chunk_bytes: u64,
    max_scan_bytes: u64,
    max_line_bytes: usize,
    cache_overlap_bytes: u64,
    cache_cap: usize,
    cache_ttl: Duration,
}

const LIMITS: ScanLimits = ScanLimits {
    chunk_bytes: CODEX_ROLLOUT_SCAN_CHUNK_BYTES,
    max_scan_bytes: MAX_CODEX_ROLLOUT_SCAN_BYTES,
    max_line_bytes: MAX_CODEX_ROLLOUT_LINE_BYTES,
    cache_overlap_bytes: CODEX_ROLLOUT_CACHE_OVERLAP_BYTES,
    cache_cap: NEGATIVE_CACHE_CAP,
    cache_ttl: NEGATIVE_CACHE_TTL,
};

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
enum BoundaryKey {
    InFlight { prompt_hash: [u8; 32] },
    Hookless,
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct CacheIdentity {
    bot_id: String,
    run_id: String,
    since: String,
    boundary: BoundaryKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteAnchor {
    end: u64,
    start: u64,
    len: u64,
    sha256: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PrefixProof {
    no_match_end: u64,
    anchor: ByteAnchor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObservedEof {
    end: u64,
    anchor: ByteAnchor,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CandidateOffsets {
    first_seen_match: Option<u64>,
    latest_match: Option<u64>,
    latest_start: Option<u64>,
    latest_started: Option<u64>,
    last_complete: Option<u64>,
    suffix_start: Option<u64>,
    native: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryState {
    NegativeNoCandidate,
    NegativePartialEof,
    PendingWaitingCompletion,
    ReadyCandidate,
}

#[derive(Clone)]
struct NegativeEntry {
    identity: CacheIdentity,
    snapshot: FileSnapshot,
    prefix: PrefixProof,
    observed: ObservedEof,
    state: EntryState,
    candidate: CandidateOffsets,
    scanned_at: Instant,
}

#[derive(Default)]
struct NegativeCache(Vec<NegativeEntry>);

#[derive(Clone)]
enum CachePlan {
    Hit,
    PendingHit,
    Ready(NegativeEntry),
    ScanFrom {
        floor: u64,
        previous: Option<NegativeEntry>,
    },
}

impl NegativeCache {
    fn plan(&mut self, identity: &CacheIdentity, snapshot: FileSnapshot, now: Instant, limits: ScanLimits) -> CachePlan {
        self.expire(now, limits.cache_ttl);

        // A new run supersedes every negative result retained for this bot.
        if self
            .0
            .iter()
            .any(|e| e.identity.bot_id == identity.bot_id && e.identity.run_id != identity.run_id)
        {
            self.0.retain(|e| e.identity.bot_id != identity.bot_id);
        }

        // Without a reliable file instance, neither a negative hit nor an incremental range
        // can safely be reused. Unix records use dev/inode; other platforms fail closed here.
        let Some(instance) = snapshot.instance else {
            self.forget(identity);
            return CachePlan::ScanFrom {
                floor: 0,
                previous: None,
            };
        };

        let previous = self
            .0
            .iter()
            .find(|entry| entry.identity == *identity)
            .cloned();
        match previous {
            Some(entry)
                if entry.snapshot.instance == Some(instance)
                    && snapshot.len == entry.snapshot.len =>
            {
                if !entry.snapshot.same_version(&snapshot) {
                    self.forget(identity);
                    CachePlan::ScanFrom {
                        floor: 0,
                        previous: None,
                    }
                } else {
                    match entry.state {
                        EntryState::NegativeNoCandidate | EntryState::NegativePartialEof => {
                            CachePlan::Hit
                        }
                        EntryState::PendingWaitingCompletion => CachePlan::PendingHit,
                        EntryState::ReadyCandidate => CachePlan::Ready(entry),
                    }
                }
            }
            Some(entry)
                if entry.snapshot.instance == Some(instance)
                    && snapshot.len > entry.snapshot.len =>
            {
                CachePlan::ScanFrom {
                    floor: entry
                        .observed
                        .end
                        .saturating_sub(limits.cache_overlap_bytes),
                    previous: Some(entry),
                }
            }
            Some(_) => {
                // Truncation or a different file instance invalidates every proof/cursor.
                self.forget(identity);
                CachePlan::ScanFrom {
                    floor: 0,
                    previous: None,
                }
            }
            None => CachePlan::ScanFrom {
                floor: 0,
                previous: None,
            },
        }
    }

    fn store(&mut self, entry: NegativeEntry, now: Instant, limits: ScanLimits) {
        self.expire(now, limits.cache_ttl);
        self.forget(&entry.identity);
        if limits.cache_cap == 0 || entry.snapshot.instance.is_none() {
            return;
        }
        if self.0.len() >= limits.cache_cap {
            if let Some(oldest) = self.0.iter().enumerate().min_by_key(|(_, e)| e.scanned_at).map(|(i, _)| i) {
                self.0.swap_remove(oldest);
            }
        }
        self.0.push(entry);
    }

    fn store_observation(
        &mut self,
        identity: CacheIdentity,
        snapshot: FileSnapshot,
        prefix: PrefixProof,
        observed: ObservedEof,
        state: EntryState,
        candidate: CandidateOffsets,
        now: Instant,
        limits: ScanLimits,
    ) {
        self.store(
            NegativeEntry {
                identity,
                snapshot,
                prefix,
                observed,
                state,
                candidate,
                scanned_at: now,
            },
            now,
            limits,
        );
    }

    fn forget(&mut self, identity: &CacheIdentity) {
        self.0.retain(|entry| entry.identity != *identity);
    }

    fn expire(&mut self, now: Instant, ttl: Duration) {
        self.0
            .retain(|entry| now.saturating_duration_since(entry.scanned_at) <= ttl);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileSnapshot {
    len: u64,
    instance: Option<FileInstance>,
    modified: Option<FileTime>,
    changed: Option<FileTime>,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
struct FileInstance {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
struct FileInstance;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileTime {
    seconds: i64,
    nanoseconds: i64,
}

impl FileSnapshot {
    #[cfg(unix)]
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            instance: Some(FileInstance { device: metadata.dev(), inode: metadata.ino() }),
            modified: Some(FileTime { seconds: metadata.mtime(), nanoseconds: metadata.mtime_nsec() }),
            changed: Some(FileTime { seconds: metadata.ctime(), nanoseconds: metadata.ctime_nsec() }),
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        // No reliable cross-platform file-instance id is available in std on these targets.
        // Disable negative reuse rather than treating path and length as identity.
        let modified = metadata.modified().ok().and_then(|time| {
            time.duration_since(STANDARD_UNIX_EPOCH).ok().map(|duration| FileTime {
                seconds: duration.as_secs() as i64,
                nanoseconds: duration.subsec_nanos() as i64,
            })
        });
        Self { len: metadata.len(), instance: None, modified, changed: None }
    }

    fn same_instance(&self, other: &Self) -> bool {
        self.instance.is_some() && self.instance == other.instance
    }

    fn same_version(&self, other: &Self) -> bool {
        self.same_instance(other) && self == other
    }
}

fn current_snapshot(path: &Path, file: &File) -> io::Result<FileSnapshot> {
    let opened = FileSnapshot::from_metadata(&file.metadata()?);
    let named = FileSnapshot::from_metadata(&fs::metadata(path)?);
    if opened != named {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "rollout path changed during scan"));
    }
    Ok(opened)
}

fn negative_cache() -> &'static Mutex<NegativeCache> {
    static CACHE: OnceLock<Mutex<NegativeCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

struct Chunk {
    start: u64,
    bytes: Vec<u8>,
}

struct CountingReader<R> {
    inner: R,
    bytes_read: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadSetError {
    Budget,
    LineTooLong,
    Io(io::ErrorKind),
}

struct ReadSet {
    chunks: Vec<Chunk>,
    bytes_read: u64,
    limits: ScanLimits,
}

impl ReadSet {
    fn empty(limits: ScanLimits) -> Self {
        Self {
            chunks: Vec::new(),
            bytes_read: 0,
            limits,
        }
    }

    fn from_scan(scan: &ScanResult, limits: ScanLimits) -> Self {
        Self {
            bytes_read: scan
                .chunks
                .iter()
                .map(|chunk| chunk.bytes.len() as u64)
                .sum(),
            chunks: scan
                .chunks
                .iter()
                .map(|chunk| Chunk {
                    start: chunk.start,
                    bytes: chunk.bytes.clone(),
                })
                .collect(),
            limits,
        }
    }

    fn read_range<R: Read + Seek>(
        &mut self,
        reader: &mut CountingReader<R>,
        start: u64,
        end: u64,
    ) -> Result<(), ReadSetError> {
        let mut cursor = start;
        while cursor < end {
            if let Some(chunk) = self.chunks.iter().find(|chunk| {
                chunk.start <= cursor && cursor < chunk.start + chunk.bytes.len() as u64
            }) {
                cursor = end.min(chunk.start + chunk.bytes.len() as u64);
                continue;
            }
            let next_chunk = self
                .chunks
                .iter()
                .map(|chunk| chunk.start)
                .filter(|start| *start > cursor)
                .min()
                .unwrap_or(end);
            let read_end = end
                .min(next_chunk)
                .min(cursor.saturating_add(self.limits.chunk_bytes));
            let read_len = read_end.saturating_sub(cursor);
            if self.bytes_read.saturating_add(read_len) > self.limits.max_scan_bytes {
                return Err(ReadSetError::Budget);
            }
            let size = usize::try_from(read_len)
                .map_err(|_| ReadSetError::Io(io::ErrorKind::InvalidInput))?;
            let mut bytes = vec![0; size];
            reader
                .seek(SeekFrom::Start(cursor))
                .map_err(|error| ReadSetError::Io(error.kind()))?;
            let before = reader.bytes_read;
            let result = reader.read_exact(&mut bytes);
            self.bytes_read = self
                .bytes_read
                .saturating_add(reader.bytes_read.saturating_sub(before));
            result.map_err(|error| ReadSetError::Io(error.kind()))?;
            self.chunks.push(Chunk {
                start: cursor,
                bytes,
            });
            cursor = read_end;
        }
        Ok(())
    }

    fn bytes(&mut self, start: u64, end: u64) -> io::Result<Vec<u8>> {
        let expected = usize::try_from(end.saturating_sub(start))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rollout range too large"))?;
        self.chunks.sort_by_key(|chunk| chunk.start);
        let mut out = Vec::with_capacity(expected);
        let mut cursor = start;
        for chunk in &self.chunks {
            let chunk_end = chunk.start + chunk.bytes.len() as u64;
            if chunk_end <= cursor || chunk.start >= end {
                continue;
            }
            if chunk.start > cursor {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "rollout read set has a gap",
                ));
            }
            let from = cursor.saturating_sub(chunk.start) as usize;
            let through = end.min(chunk_end).saturating_sub(chunk.start) as usize;
            out.extend_from_slice(&chunk.bytes[from..through]);
            cursor = chunk_end.min(end);
            if cursor == end {
                break;
            }
        }
        if cursor != end || out.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "rollout read set has a gap",
            ));
        }
        Ok(out)
    }

    fn suffix(&mut self, start: u64, end: u64) -> io::Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes(start, end)?).into_owned())
    }

    fn anchor(&mut self, end: u64) -> io::Result<ByteAnchor> {
        let start = end.saturating_sub(PREFIX_ANCHOR_BYTES);
        let bytes = self.bytes(start, end)?;
        Ok(ByteAnchor {
            end,
            start,
            len: bytes.len() as u64,
            sha256: Sha256::digest(&bytes).into(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanPurpose {
    InFlightPrompt,
    HooklessComplete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanDisposition {
    NoCandidate,
    PendingCandidate,
    CandidateReady,
    Incomplete,
}

fn event_kind(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    (value.get("type")?.as_str()? == "event_msg")
        .then(|| value.pointer("/payload/type")?.as_str().map(str::to_owned))
        .flatten()
}

fn is_task_started(line: &str) -> bool {
    event_kind(line).as_deref() == Some("task_started")
}

fn is_native_boundary(line: &str) -> bool {
    let Some(kind) = event_kind(line) else {
        return false;
    };
    if matches!(
        kind.as_str(),
        "task_started" | "task_aborted" | "turn_aborted" | "turn_cancelled"
    ) {
        return true;
    }
    if kind != "task_complete" {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .is_some_and(|value| value.pointer("/payload/turn_id").is_some())
}

fn visit_lines<F>(
    bytes: &[u8],
    base: u64,
    skip_first_partial: bool,
    max_line_bytes: usize,
    mut visit: F,
) -> Result<(), ReadSetError>
where
    F: FnMut(u64, &str, u64, bool),
{
    let mut offset = 0_usize;
    if skip_first_partial && base > 0 {
        let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
            return if bytes.len() > max_line_bytes {
                Err(ReadSetError::LineTooLong)
            } else {
                Ok(())
            };
        };
        if newline > max_line_bytes {
            return Err(ReadSetError::LineTooLong);
        }
        offset = newline + 1;
    }
    while offset < bytes.len() {
        let line_start = offset;
        let line_end = bytes[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|n| offset + n);
        let Some(end) = line_end else {
            if bytes.len() - line_start > max_line_bytes {
                return Err(ReadSetError::LineTooLong);
            }
            let line = String::from_utf8_lossy(&bytes[line_start..]);
            visit(
                base + line_start as u64,
                line.as_ref(),
                base + bytes.len() as u64,
                false,
            );
            break;
        };
        if end - line_start > max_line_bytes {
            return Err(ReadSetError::LineTooLong);
        }
        let line = String::from_utf8_lossy(&bytes[line_start..end]);
        visit(
            base + line_start as u64,
            line.as_ref(),
            base + end as u64 + 1,
            true,
        );
        offset = end + 1;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct RecordSummary {
    first_match: Option<u64>,
    latest_match: Option<u64>,
    first_start_before_match: Option<u64>,
    latest_start_before_match: Option<u64>,
    latest_complete_after_match: Option<u64>,
    latest_complete: Option<u64>,
    has_complete_after_match: bool,
    has_native_after_match: bool,
    latest_started: Option<u64>,
    sealed_prefix_end: u64,
}

fn summarize_range<R, F>(
    reader: &mut CountingReader<R>,
    read_set: &mut ReadSet,
    start: u64,
    end: u64,
    limits: ScanLimits,
    purpose: ScanPurpose,
    seed_latest_started: Option<u64>,
    mut is_candidate: F,
) -> Result<RecordSummary, ReadSetError>
where
    R: Read + Seek,
    F: FnMut(&str) -> bool,
{
    read_set.read_range(reader, start, end)?;
    let bytes = read_set
        .bytes(start, end)
        .map_err(|error| ReadSetError::Io(error.kind()))?;
    let mut summary = RecordSummary {
        latest_started: seed_latest_started,
        sealed_prefix_end: start,
        ..RecordSummary::default()
    };
    let mut most_recent_start = seed_latest_started;
    visit_lines(
        &bytes,
        start,
        start > 0,
        limits.max_line_bytes,
        |offset, line, line_end, terminated| {
            if terminated {
                summary.sealed_prefix_end = line_end;
            }
            let kind = event_kind(line);
            let is_complete = kind.as_deref() == Some("task_complete");
            let is_started = kind.as_deref() == Some("task_started");
            let native_marker = is_native_boundary(line);
            if is_complete {
                summary.latest_complete = Some(offset);
            }
            if is_started {
                if most_recent_start.map_or(true, |previous| offset > previous) {
                    most_recent_start = Some(offset);
                    summary.latest_started = Some(offset);
                }
            }

            if is_candidate(line) {
                if summary.first_match.is_none() {
                    summary.first_match = Some(offset);
                    summary.first_start_before_match = most_recent_start;
                }
                summary.latest_match = Some(offset);
                summary.latest_start_before_match = most_recent_start;
                summary.latest_complete_after_match =
                    if purpose == ScanPurpose::HooklessComplete && is_complete {
                        Some(offset)
                    } else {
                        None
                    };
                summary.has_complete_after_match =
                    purpose == ScanPurpose::HooklessComplete && is_complete;
                summary.has_native_after_match = native_marker;
            } else if summary.latest_match.is_some() {
                if is_complete {
                    summary.latest_complete_after_match = Some(offset);
                    summary.has_complete_after_match = true;
                }
                summary.has_native_after_match |= native_marker;
            }
        },
    )?;
    Ok(summary)
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> Self { Self { inner, bytes_read: 0 } }

    fn get_ref(&self) -> &R { &self.inner }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.bytes_read += read as u64;
        Ok(read)
    }
}

impl<R: Seek> Seek for CountingReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> { self.inner.seek(position) }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopReason {
    Candidate,
    CompleteNoCandidate,
    ScanLimit,
    LineTooLong,
}

struct ScanResult {
    suffix: Option<String>,
    candidate_offset: Option<u64>,
    bytes_scanned: u64,
    chunks_read: usize,
    chunks: Vec<Chunk>,
    complete: bool,
    stop: StopReason,
}

#[derive(Debug)]
struct ScanFailure {
    file_bytes: Option<u64>,
    bytes_scanned: u64,
    kind: io::ErrorKind,
}

impl ScanFailure {
    fn new(error: io::Error, file_bytes: Option<u64>, bytes_scanned: u64) -> Self {
        Self { file_bytes, bytes_scanned, kind: error.kind() }
    }
}

struct ReadOutcome {
    suffix: Option<String>,
    disposition: ScanDisposition,
    #[cfg(test)]
    bytes_read: u64,
    #[cfg(test)]
    chunks_read: usize,
}

impl ReadOutcome {
    fn none(_bytes_read: u64) -> Self {
        Self {
            suffix: None,
            disposition: ScanDisposition::Incomplete,
            #[cfg(test)]
            bytes_read: _bytes_read,
            #[cfg(test)]
            chunks_read: 0,
        }
    }

    fn pending(_bytes_read: u64) -> Self {
        Self {
            disposition: ScanDisposition::PendingCandidate,
            ..Self::none(_bytes_read)
        }
    }

    fn no_candidate(_bytes_read: u64) -> Self {
        Self {
            disposition: ScanDisposition::NoCandidate,
            ..Self::none(_bytes_read)
        }
    }
}

fn scan_from_tail<R, F>(
    reader: &mut R,
    file_len: u64,
    floor: u64,
    limits: ScanLimits,
    is_candidate: &mut F,
) -> io::Result<ScanResult>
where
    R: Read + Seek,
    F: FnMut(&str) -> bool,
{
    let mut cursor = file_len;
    let mut bytes_scanned = 0_u64;
    let mut chunks = Vec::new();
    let mut line_reversed = Vec::new();

    // For an incremental scan, floor is old_len minus a line-sized overlap. Any record
    // completed by the append is therefore wholly inside the scanned range; the prefix
    // before floor was already completely scanned at the previous file length.
    while cursor > floor {
        let remaining = limits.max_scan_bytes.saturating_sub(bytes_scanned);
        if remaining == 0 {
            return Ok(ScanResult {
                suffix: None,
                candidate_offset: None,
                bytes_scanned,
                chunks_read: chunks.len(),
                chunks,
                complete: false,
                stop: StopReason::ScanLimit,
            });
        }
        let chunk_len = limits.chunk_bytes.min(cursor - floor).min(remaining);
        let start = cursor - chunk_len;
        let chunk_size = usize::try_from(chunk_len).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rollout chunk too large"))?;
        let mut bytes = vec![0; chunk_size];
        reader.seek(SeekFrom::Start(start))?;
        reader.read_exact(&mut bytes)?;
        bytes_scanned += chunk_len;
        chunks.push(Chunk { start, bytes });

        let current = chunks.last().expect("just pushed a rollout chunk");
        for index in (0..current.bytes.len()).rev() {
            let byte = current.bytes[index];
            let offset = start + index as u64;
            if byte == b'\n' {
                if !line_reversed.is_empty() {
                    line_reversed.reverse();
                    let line = String::from_utf8_lossy(&line_reversed);
                    if is_candidate(line.as_ref()) {
                        let candidate_offset = offset + 1;
                        let suffix = suffix_from_chunks(&chunks, candidate_offset, file_len)?;
                        return Ok(ScanResult {
                            suffix: Some(suffix),
                            candidate_offset: Some(candidate_offset),
                            bytes_scanned,
                            chunks_read: chunks.len(),
                            chunks,
                            complete: false,
                            stop: StopReason::Candidate,
                        });
                    }
                    line_reversed.clear();
                }
            } else {
                if line_reversed.len() >= limits.max_line_bytes {
                    return Ok(ScanResult {
                        suffix: None,
                        candidate_offset: None,
                        bytes_scanned,
                        chunks_read: chunks.len(),
                        chunks,
                        complete: false,
                        stop: StopReason::LineTooLong,
                    });
                }
                line_reversed.push(byte);
            }
        }

        cursor = start;
    }

    // At byte zero, the first record may not have a preceding newline.
    if floor == 0 && !line_reversed.is_empty() {
        line_reversed.reverse();
        let line = String::from_utf8_lossy(&line_reversed);
        if is_candidate(line.as_ref()) {
            let suffix = suffix_from_chunks(&chunks, 0, file_len)?;
            return Ok(ScanResult {
                suffix: Some(suffix),
                candidate_offset: Some(0),
                bytes_scanned,
                chunks_read: chunks.len(),
                chunks,
                complete: false,
                stop: StopReason::Candidate,
            });
        }
    }

    let scan_limit_reached = cursor > floor && bytes_scanned >= limits.max_scan_bytes;
    Ok(ScanResult {
        suffix: None,
        candidate_offset: None,
        bytes_scanned,
        chunks_read: chunks.len(),
        chunks,
        complete: !scan_limit_reached,
        stop: if scan_limit_reached { StopReason::ScanLimit } else { StopReason::CompleteNoCandidate },
    })
}

fn scan_counted<R, F>(
    reader: &mut CountingReader<R>,
    file_len: u64,
    floor: u64,
    limits: ScanLimits,
    file_bytes: Option<u64>,
    is_candidate: &mut F,
) -> Result<ScanResult, ScanFailure>
where
    R: Read + Seek,
    F: FnMut(&str) -> bool,
{
    scan_from_tail(reader, file_len, floor, limits, is_candidate)
        .map_err(|error| ScanFailure::new(error, file_bytes, reader.bytes_read))
}

fn scan_open_file<F>(
    path: &Path,
    reader: &mut CountingReader<File>,
    before: FileSnapshot,
    plan: CachePlan,
    limits: ScanLimits,
    is_candidate: &mut F,
) -> Result<ScanResult, ScanFailure>
where
    F: FnMut(&str) -> bool,
{
    let floor = match plan {
        CachePlan::Hit | CachePlan::PendingHit => {
            let after = match current_snapshot(path, reader.get_ref()) {
                Ok(snapshot) => snapshot,
                Err(error) => return Err(ScanFailure::new(error, Some(before.len), reader.bytes_read)),
            };
            if !before.same_version(&after) {
                return Err(ScanFailure::new(
                    io::Error::new(io::ErrorKind::InvalidData, "rollout changed during cache check"),
                    Some(before.len),
                    reader.bytes_read,
                ));
            }
            return Ok(ScanResult {
                suffix: None,
                candidate_offset: None,
                bytes_scanned: 0,
                chunks_read: 0,
                chunks: Vec::new(),
                complete: true,
                stop: StopReason::CompleteNoCandidate,
            });
        }
        CachePlan::Ready(entry) => entry.candidate.latest_match.unwrap_or(0),
        CachePlan::ScanFrom { floor, .. } => floor,
    };

    let result = scan_counted(reader, before.len, floor, limits, Some(before.len), is_candidate)?;
    let after = match current_snapshot(path, reader.get_ref()) {
        Ok(snapshot) => snapshot,
        Err(error) => return Err(ScanFailure::new(error, Some(before.len), reader.bytes_read)),
    };
    if before != after {
        return Err(ScanFailure::new(
            io::Error::new(io::ErrorKind::InvalidData, "rollout changed during scan"),
            Some(before.len),
            reader.bytes_read,
        ));
    }
    Ok(result)
}

fn suffix_from_chunks(chunks: &[Chunk], start: u64, file_len: u64) -> io::Result<String> {
    let expected = usize::try_from(file_len.saturating_sub(start))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rollout suffix too large"))?;
    let mut bytes = Vec::with_capacity(expected);
    for chunk in chunks.iter().rev() {
        let end = chunk.start + chunk.bytes.len() as u64;
        if end <= start {
            continue;
        }
        let from = start.saturating_sub(chunk.start) as usize;
        bytes.extend_from_slice(&chunk.bytes[from..]);
    }
    if bytes.len() != expected {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "rollout suffix has a gap"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn prompt_hash(sent: &[String]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for text in sent {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }
    hasher.finalize().into()
}

pub(super) fn read_in_flight_suffix(path: &Path, bot_id: &str, run_id: &str, sent: &[String], since_iso: &str) -> Option<String> {
    let since = chrono::DateTime::parse_from_rfc3339(since_iso).ok()?;
    let identity = CacheIdentity {
        bot_id: bot_id.to_string(), run_id: run_id.to_string(), since: since_iso.to_string(),
        boundary: BoundaryKey::InFlight { prompt_hash: prompt_hash(sent) },
    };
    read_suffix(path, identity, |line| super::codex_rollout::is_turn_prompt_candidate(line, sent, &since))
}

pub(super) fn read_hookless_suffix(path: &Path, bot_id: &str, run_id: &str, since_iso: &str) -> Option<String> {
    let since = chrono::DateTime::parse_from_rfc3339(since_iso).ok()?;
    let identity = CacheIdentity {
        bot_id: bot_id.to_string(), run_id: run_id.to_string(), since: since_iso.to_string(), boundary: BoundaryKey::Hookless,
    };
    read_suffix(path, identity, |line| {
        super::codex_rollout::is_task_complete_candidate(line, &since)
    })
}

fn read_suffix<F>(path: &Path, identity: CacheIdentity, is_candidate: F) -> Option<String>
where
    F: FnMut(&str) -> bool,
{
    read_suffix_with(path, identity, LIMITS, is_candidate).suffix
}

fn boundary_purpose(identity: &CacheIdentity) -> ScanPurpose {
    match identity.boundary {
        BoundaryKey::InFlight { .. } => ScanPurpose::InFlightPrompt,
        BoundaryKey::Hookless => ScanPurpose::HooklessComplete,
    }
}

fn remember_observation(
    identity: &CacheIdentity,
    snapshot: FileSnapshot,
    prefix: PrefixProof,
    observed: ObservedEof,
    state: EntryState,
    candidate: CandidateOffsets,
    now: Instant,
    limits: ScanLimits,
) {
    negative_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .store_observation(
            identity.clone(),
            snapshot,
            prefix,
            observed,
            state,
            candidate,
            now,
            limits,
        );
}

fn failed_closed(identity: &CacheIdentity, reader: &CountingReader<File>) -> ReadOutcome {
    negative_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .forget(identity);
    ReadOutcome::none(reader.bytes_read)
}

fn stable_cache_return(
    path: &Path,
    reader: &mut CountingReader<File>,
    before: FileSnapshot,
    identity: &CacheIdentity,
    disposition: ScanDisposition,
) -> ReadOutcome {
    let after = match current_snapshot(path, reader.get_ref()) {
        Ok(snapshot) => snapshot,
        Err(_) => return failed_closed(identity, reader),
    };
    if !before.same_version(&after) {
        return failed_closed(identity, reader);
    }
    match disposition {
        ScanDisposition::NoCandidate => ReadOutcome::no_candidate(reader.bytes_read),
        ScanDisposition::PendingCandidate => ReadOutcome::pending(reader.bytes_read),
        _ => ReadOutcome::none(reader.bytes_read),
    }
}

fn materialize_ready(
    path: &Path,
    reader: &mut CountingReader<File>,
    before: FileSnapshot,
    entry: NegativeEntry,
    limits: ScanLimits,
) -> ReadOutcome {
    let Some(start) = entry.candidate.suffix_start else {
        return failed_closed(&entry.identity, reader);
    };
    let mut read_set = ReadSet::empty(limits);
    if read_set.read_range(reader, start, before.len).is_err() {
        return failed_closed(&entry.identity, reader);
    }
    let after = match current_snapshot(path, reader.get_ref()) {
        Ok(snapshot) => snapshot,
        Err(_) => return failed_closed(&entry.identity, reader),
    };
    if !before.same_version(&after) {
        return failed_closed(&entry.identity, reader);
    }
    let suffix = match read_set.suffix(start, before.len) {
        Ok(suffix) => suffix,
        Err(_) => return failed_closed(&entry.identity, reader),
    };
    ReadOutcome {
        suffix: Some(suffix),
        disposition: ScanDisposition::CandidateReady,
        #[cfg(test)]
        bytes_read: reader.bytes_read,
        #[cfg(test)]
        chunks_read: read_set.chunks.len(),
    }
}

fn commit_read_set_entry(
    path: &Path,
    reader: &mut CountingReader<File>,
    before: FileSnapshot,
    identity: &CacheIdentity,
    read_set: &mut ReadSet,
    x: u64,
    state: EntryState,
    candidate: CandidateOffsets,
    now: Instant,
    limits: ScanLimits,
) -> Result<(), ()> {
    read_set
        .read_range(reader, x.saturating_sub(PREFIX_ANCHOR_BYTES), x)
        .map_err(|_| ())?;
    read_set
        .read_range(
            reader,
            before.len.saturating_sub(PREFIX_ANCHOR_BYTES),
            before.len,
        )
        .map_err(|_| ())?;
    let after = current_snapshot(path, reader.get_ref()).map_err(|_| ())?;
    if before != after {
        return Err(());
    }
    let prefix_anchor = read_set.anchor(x).map_err(|_| ())?;
    let eof_anchor = read_set.anchor(before.len).map_err(|_| ())?;
    remember_observation(
        identity,
        before,
        PrefixProof {
            no_match_end: x,
            anchor: prefix_anchor,
        },
        ObservedEof {
            end: before.len,
            anchor: eof_anchor,
        },
        state,
        candidate,
        now,
        limits,
    );
    Ok(())
}

fn finish_full_observation(
    path: &Path,
    reader: &mut CountingReader<File>,
    before: FileSnapshot,
    identity: &CacheIdentity,
    purpose: ScanPurpose,
    summary: RecordSummary,
    read_set: &mut ReadSet,
    now: Instant,
    limits: ScanLimits,
) -> ReadOutcome {
    let Some(latest_match) = summary.latest_match else {
        let candidate = CandidateOffsets {
            latest_started: summary.latest_started,
            ..CandidateOffsets::default()
        };
        let state = if summary.sealed_prefix_end == before.len {
            EntryState::NegativeNoCandidate
        } else {
            EntryState::NegativePartialEof
        };
        if commit_read_set_entry(
            path,
            reader,
            before,
            identity,
            read_set,
            summary.sealed_prefix_end,
            state,
            candidate,
            now,
            limits,
        )
        .is_err()
        {
            return failed_closed(identity, reader);
        }
        return ReadOutcome::no_candidate(reader.bytes_read);
    };

    if summary.has_native_after_match
        && match purpose {
            ScanPurpose::InFlightPrompt => {
                summary.first_start_before_match.is_none()
                    && summary.latest_start_before_match.is_none()
            }
            ScanPurpose::HooklessComplete => summary.latest_start_before_match.is_none(),
        }
    {
        return failed_closed(identity, reader);
    }

    let has_completion =
        purpose == ScanPurpose::HooklessComplete || summary.has_complete_after_match;
    if purpose == ScanPurpose::InFlightPrompt && !has_completion {
        let first = summary.first_match.unwrap_or(latest_match);
        let first_start = summary.first_start_before_match;
        let candidate = CandidateOffsets {
            first_seen_match: Some(first),
            latest_match: Some(latest_match),
            latest_start: summary.latest_start_before_match,
            latest_started: summary.latest_started,
            last_complete: None,
            suffix_start: Some(first.min(first_start.unwrap_or(first))),
            native: first_start.is_some()
                || summary.latest_start_before_match.is_some()
                || summary.has_native_after_match,
        };
        let x = first;
        if commit_read_set_entry(
            path,
            reader,
            before,
            identity,
            read_set,
            x,
            EntryState::PendingWaitingCompletion,
            candidate,
            now,
            limits,
        )
        .is_err()
        {
            return failed_closed(identity, reader);
        }
        return ReadOutcome::pending(reader.bytes_read);
    }

    let first = summary.first_match.unwrap_or(latest_match);
    let associated_start = if purpose == ScanPurpose::HooklessComplete {
        summary.latest_start_before_match
    } else {
        summary.first_start_before_match
    };
    let native = summary.has_native_after_match
        || associated_start.is_some()
        || summary.latest_start_before_match.is_some();
    let suffix_start = if purpose == ScanPurpose::HooklessComplete {
        associated_start.unwrap_or(latest_match)
    } else if native {
        first.min(associated_start.unwrap_or(first))
    } else {
        latest_match
    };
    let suffix = match read_set.suffix(suffix_start, before.len) {
        Ok(suffix) => suffix,
        Err(_) => return failed_closed(identity, reader),
    };
    let candidate = CandidateOffsets {
        first_seen_match: Some(first),
        latest_match: Some(latest_match),
        latest_start: summary.latest_start_before_match,
        latest_started: summary.latest_started,
        last_complete: summary
            .latest_complete_after_match
            .or_else(|| (purpose == ScanPurpose::HooklessComplete).then_some(latest_match)),
        suffix_start: Some(suffix_start),
        native,
    };
    let x = if purpose == ScanPurpose::InFlightPrompt {
        first
    } else {
        summary.sealed_prefix_end.min(first)
    };
    if commit_read_set_entry(
        path,
        reader,
        before,
        identity,
        read_set,
        x,
        EntryState::ReadyCandidate,
        candidate,
        now,
        limits,
    )
    .is_err()
    {
        return failed_closed(identity, reader);
    }
    ReadOutcome {
        suffix: Some(suffix),
        disposition: ScanDisposition::CandidateReady,
        #[cfg(test)]
        bytes_read: reader.bytes_read,
        #[cfg(test)]
        chunks_read: read_set.chunks.len(),
    }
}

fn nearest_started_before<R: Read + Seek>(
    reader: &mut CountingReader<R>,
    read_set: &mut ReadSet,
    mut cursor: u64,
    limits: ScanLimits,
) -> Result<Option<u64>, ReadSetError> {
    let mut reversed = Vec::new();
    while cursor > 0 {
        let start = cursor.saturating_sub(limits.chunk_bytes);
        read_set.read_range(reader, start, cursor)?;
        let bytes = read_set
            .bytes(start, cursor)
            .map_err(|error| ReadSetError::Io(error.kind()))?;
        for index in (0..bytes.len()).rev() {
            let byte = bytes[index];
            let offset = start + index as u64;
            if byte == b'\n' {
                if !reversed.is_empty() {
                    if reversed.len() > limits.max_line_bytes {
                        return Err(ReadSetError::LineTooLong);
                    }
                    reversed.reverse();
                    let line = String::from_utf8_lossy(&reversed);
                    if is_task_started(line.as_ref()) {
                        return Ok(Some(offset + 1));
                    }
                    reversed.clear();
                }
            } else {
                if reversed.len() >= limits.max_line_bytes {
                    return Err(ReadSetError::LineTooLong);
                }
                reversed.push(byte);
            }
        }
        cursor = start;
    }
    if !reversed.is_empty() {
        reversed.reverse();
        let line = String::from_utf8_lossy(&reversed);
        if is_task_started(line.as_ref()) {
            return Ok(Some(0));
        }
    }
    Ok(None)
}

fn full_observation<F>(
    path: &Path,
    reader: &mut CountingReader<File>,
    before: FileSnapshot,
    identity: &CacheIdentity,
    purpose: ScanPurpose,
    read_set: &mut ReadSet,
    now: Instant,
    limits: ScanLimits,
    is_candidate: &mut F,
) -> ReadOutcome
where
    F: FnMut(&str) -> bool,
{
    let summary = match summarize_range(
        reader,
        read_set,
        0,
        before.len,
        limits,
        purpose,
        None,
        is_candidate,
    ) {
        Ok(summary) => summary,
        Err(_) => return failed_closed(identity, reader),
    };
    finish_full_observation(
        path, reader, before, identity, purpose, summary, read_set, now, limits,
    )
}

fn append_observation<F>(
    path: &Path,
    reader: &mut CountingReader<File>,
    before: FileSnapshot,
    identity: &CacheIdentity,
    previous: NegativeEntry,
    purpose: ScanPurpose,
    now: Instant,
    limits: ScanLimits,
    is_candidate: &mut F,
) -> ReadOutcome
where
    F: FnMut(&str) -> bool,
{
    let mut read_set = ReadSet::empty(limits);
    let delta_start = previous
        .observed
        .end
        .saturating_sub(limits.cache_overlap_bytes);
    if read_set
        .read_range(reader, delta_start, before.len)
        .is_err()
    {
        return failed_closed(identity, reader);
    }
    if read_set
        .read_range(
            reader,
            previous.prefix.anchor.start,
            previous.prefix.anchor.end,
        )
        .is_err()
        || read_set
            .read_range(
                reader,
                previous.observed.anchor.start,
                previous.observed.anchor.end,
            )
            .is_err()
    {
        return failed_closed(identity, reader);
    }
    let anchors_match = read_set.anchor(previous.prefix.no_match_end).ok()
        == Some(previous.prefix.anchor)
        && read_set.anchor(previous.observed.end).ok() == Some(previous.observed.anchor);
    let stable = current_snapshot(path, reader.get_ref())
        .ok()
        .is_some_and(|after| before == after);
    if !stable {
        return failed_closed(identity, reader);
    }
    if !anchors_match {
        negative_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .forget(identity);
        return full_observation(
            path,
            reader,
            before,
            identity,
            purpose,
            &mut read_set,
            now,
            limits,
            is_candidate,
        );
    }

    let summary = match summarize_range(
        reader,
        &mut read_set,
        delta_start,
        before.len,
        limits,
        purpose,
        previous.candidate.latest_started,
        is_candidate,
    ) {
        Ok(summary) => summary,
        Err(_) => return failed_closed(identity, reader),
    };

    let old = previous.candidate;
    let first_seen_match = match (old.first_seen_match, summary.first_match) {
        (Some(old), Some(new)) => Some(old.min(new)),
        (Some(old), None) => Some(old),
        (None, new) => new,
    };
    let latest_match = match (old.latest_match, summary.latest_match) {
        (Some(old), Some(new)) => Some(old.max(new)),
        (Some(old), None) => Some(old),
        (None, new) => new,
    };
    let latest_started = match (old.latest_started, summary.latest_started) {
        (Some(old), Some(new)) => Some(old.max(new)),
        (Some(old), None) => Some(old),
        (None, new) => new,
    };
    let latest_start = if summary
        .latest_match
        .is_some_and(|offset| Some(offset) == latest_match)
    {
        summary.latest_start_before_match.or(old.latest_start)
    } else {
        old.latest_start
    };
    let latest_complete = match (old.last_complete, summary.latest_complete) {
        (Some(old), Some(new)) => Some(old.max(new)),
        (Some(old), None) => Some(old),
        (None, new) => new,
    };
    let completion_after_match = latest_match
        .zip(latest_complete)
        .filter(|(matched, complete)| complete > matched)
        .map(|(_, complete)| complete);

    let state = match purpose {
        ScanPurpose::HooklessComplete => {
            if latest_match.is_some() {
                EntryState::ReadyCandidate
            } else if summary.sealed_prefix_end == before.len {
                EntryState::NegativeNoCandidate
            } else {
                EntryState::NegativePartialEof
            }
        }
        ScanPurpose::InFlightPrompt => match latest_match {
            None if summary.sealed_prefix_end == before.len => EntryState::NegativeNoCandidate,
            None => EntryState::NegativePartialEof,
            Some(_) if completion_after_match.is_some() => EntryState::ReadyCandidate,
            Some(_) => EntryState::PendingWaitingCompletion,
        },
    };

    let mut candidate = CandidateOffsets {
        first_seen_match,
        latest_match,
        latest_start,
        latest_started,
        last_complete: if purpose == ScanPurpose::HooklessComplete {
            latest_match
        } else {
            completion_after_match.or_else(|| {
                (state == EntryState::ReadyCandidate)
                    .then_some(old.last_complete)
                    .flatten()
            })
        },
        suffix_start: old.suffix_start,
        native: old.native || summary.has_native_after_match || latest_start.is_some(),
    };

    let newer_match = match (old.latest_match, latest_match) {
        (Some(old), Some(new)) => new > old,
        (None, Some(_)) => true,
        _ => false,
    };
    if !matches!(
        state,
        EntryState::NegativeNoCandidate | EntryState::NegativePartialEof
    ) && (candidate.suffix_start.is_none()
        || (purpose == ScanPurpose::HooklessComplete && newer_match))
    {
        let first = if purpose == ScanPurpose::HooklessComplete {
            candidate.latest_match.unwrap_or(0)
        } else {
            candidate
                .first_seen_match
                .or(candidate.latest_match)
                .unwrap_or(0)
        };
        let start = if purpose == ScanPurpose::HooklessComplete {
            match candidate
                .latest_match
                .map(|matched| nearest_started_before(reader, &mut read_set, matched, limits))
                .transpose()
            {
                Ok(start) => start.flatten(),
                Err(_) => return failed_closed(identity, reader),
            }
        } else {
            summary
                .first_start_before_match
                .or(summary.latest_start_before_match)
                .or(candidate.latest_start)
        };
        if summary.has_native_after_match && start.is_none() {
            return failed_closed(identity, reader);
        }
        candidate.native |= start.is_some();
        candidate.suffix_start = Some(first.min(start.unwrap_or(first)));
    }

    let old_x = previous.prefix.no_match_end;
    let x = match state {
        EntryState::NegativeNoCandidate | EntryState::NegativePartialEof => {
            old_x.max(summary.sealed_prefix_end).min(before.len)
        }
        EntryState::PendingWaitingCompletion | EntryState::ReadyCandidate => {
            old_x.min(candidate.first_seen_match.unwrap_or(old_x))
        }
    };

    let suffix = if state == EntryState::ReadyCandidate {
        let Some(start) = candidate.suffix_start else {
            return failed_closed(identity, reader);
        };
        if read_set.read_range(reader, start, before.len).is_err() {
            return failed_closed(identity, reader);
        }
        match read_set.suffix(start, before.len) {
            Ok(suffix) => Some(suffix),
            Err(_) => return failed_closed(identity, reader),
        }
    } else {
        None
    };

    if commit_read_set_entry(
        path,
        reader,
        before,
        identity,
        &mut read_set,
        x,
        state,
        candidate,
        now,
        limits,
    )
    .is_err()
    {
        return failed_closed(identity, reader);
    }
    match state {
        EntryState::NegativeNoCandidate | EntryState::NegativePartialEof => {
            ReadOutcome::no_candidate(reader.bytes_read)
        }
        EntryState::PendingWaitingCompletion => ReadOutcome::pending(reader.bytes_read),
        EntryState::ReadyCandidate => ReadOutcome {
            suffix,
            disposition: ScanDisposition::CandidateReady,
            #[cfg(test)]
            bytes_read: reader.bytes_read,
            #[cfg(test)]
            chunks_read: read_set.chunks.len(),
        },
    }
}

fn read_suffix_with<F>(
    path: &Path,
    identity: CacheIdentity,
    limits: ScanLimits,
    mut is_candidate: F,
) -> ReadOutcome
where
    F: FnMut(&str) -> bool,
{
    let started = Instant::now();
    let Ok(file) = crate::transcript_read::open_regular(path) else { return ReadOutcome::none(0) };
    let mut reader = CountingReader::new(file);
    let before = match current_snapshot(path, reader.get_ref()) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let mut cache = negative_cache().lock().unwrap_or_else(|e| e.into_inner());
            cache.forget(&identity);
            tracing::warn!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = "unknown",
                bytes_scanned = reader.bytes_read, max_scan_bytes = limits.max_scan_bytes,
                max_line_bytes = limits.max_line_bytes, elapsed_ms = started.elapsed().as_millis() as u64,
                error_kind = ?error.kind(), "codex rollout metadata check failed closed");
            return ReadOutcome::none(reader.bytes_read);
        }
    };
    let now = Instant::now();
    let plan = {
        let mut cache = negative_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache.plan(&identity, before, now, limits)
    };
    let purpose = boundary_purpose(&identity);
    match plan.clone() {
        CachePlan::Hit => {
            return stable_cache_return(
                path,
                &mut reader,
                before,
                &identity,
                ScanDisposition::NoCandidate,
            )
        }
        CachePlan::PendingHit => {
            return stable_cache_return(
                path,
                &mut reader,
                before,
                &identity,
                ScanDisposition::PendingCandidate,
            )
        }
        CachePlan::Ready(entry) => {
            return materialize_ready(path, &mut reader, before, entry, limits)
        }
        CachePlan::ScanFrom {
            previous: Some(entry),
            ..
        } => {
            return append_observation(
                path,
                &mut reader,
                before,
                &identity,
                entry,
                purpose,
                now,
                limits,
                &mut is_candidate,
            );
        }
        CachePlan::ScanFrom { previous: None, .. } => {}
    }

    let result = match scan_open_file(
        path,
        &mut reader,
        before,
        plan.clone(),
        limits,
        &mut is_candidate,
    ) {
        Ok(result) => result,
        Err(failure) => {
            negative_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .forget(&identity);
            tracing::warn!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = ?failure.file_bytes,
                bytes_scanned = failure.bytes_scanned, max_scan_bytes = limits.max_scan_bytes,
                max_line_bytes = limits.max_line_bytes, elapsed_ms = started.elapsed().as_millis() as u64,
                error_kind = ?failure.kind, "codex rollout scan failed closed");
            return ReadOutcome::none(failure.bytes_scanned);
        }
    };
    match result.stop {
        StopReason::CompleteNoCandidate if result.complete => {
            let mut read_set = ReadSet::from_scan(&result, limits);
            return full_observation(
                path,
                &mut reader,
                before,
                &identity,
                purpose,
                &mut read_set,
                now,
                limits,
                &mut is_candidate,
            );
        }
        StopReason::ScanLimit | StopReason::LineTooLong => {
            return failed_closed(&identity, &reader)
        }
        StopReason::CompleteNoCandidate => return failed_closed(&identity, &reader),
        StopReason::Candidate => {}
    }

    let candidate_offset = result.candidate_offset.unwrap_or(0);
    let mut read_set = ReadSet::from_scan(&result, limits);
    match purpose {
        ScanPurpose::InFlightPrompt => full_observation(
            path,
            &mut reader,
            before,
            &identity,
            purpose,
            &mut read_set,
            now,
            limits,
            &mut is_candidate,
        ),
        ScanPurpose::HooklessComplete => {
            let start = match nearest_started_before(
                &mut reader,
                &mut read_set,
                candidate_offset,
                limits,
            ) {
                Ok(start) => start,
                Err(_) => return failed_closed(&identity, &reader),
            };
            let suffix_start = start.unwrap_or(candidate_offset);
            let suffix = match read_set.suffix(suffix_start, before.len) {
                Ok(suffix) => suffix,
                Err(_) => return failed_closed(&identity, &reader),
            };
            let native_marker = suffix.lines().any(is_native_boundary);
            if native_marker && start.is_none() {
                return failed_closed(&identity, &reader);
            }
            let candidate = CandidateOffsets {
                first_seen_match: Some(candidate_offset),
                latest_match: Some(candidate_offset),
                latest_start: start,
                latest_started: start,
                last_complete: Some(candidate_offset),
                suffix_start: Some(suffix_start),
                native: native_marker,
            };
            let mut read_set = read_set;
            if commit_read_set_entry(
                path,
                &mut reader,
                before,
                &identity,
                &mut read_set,
                0,
                EntryState::ReadyCandidate,
                candidate,
                now,
                limits,
            )
            .is_err()
            {
                return failed_closed(&identity, &reader);
            }
            ReadOutcome {
                suffix: Some(suffix),
                disposition: ScanDisposition::CandidateReady,
                #[cfg(test)]
                bytes_read: reader.bytes_read,
                #[cfg(test)]
                chunks_read: read_set.chunks.len(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, FileTimes, OpenOptions};
    use std::io::{Cursor, Write};
    use std::time::UNIX_EPOCH;

    struct TempRollout(std::path::PathBuf);

    impl TempRollout {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("am-codex-rollout-{}", ulid::Ulid::new())); // scratch-guard: allow fixture is removed by Drop
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> std::path::PathBuf { self.0.join("rollout.jsonl") }
    }

    impl Drop for TempRollout {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn limits(chunk: u64, max_scan: u64, max_line: usize, overlap: u64) -> ScanLimits {
        ScanLimits { chunk_bytes: chunk, max_scan_bytes: max_scan, max_line_bytes: max_line,
            cache_overlap_bytes: overlap, cache_cap: NEGATIVE_CACHE_CAP, cache_ttl: NEGATIVE_CACHE_TTL }
    }

    fn identity() -> CacheIdentity {
        CacheIdentity {
            bot_id: "bot".into(), run_id: "run".into(), since: "2026-10-25T01:00:00Z".into(),
            boundary: BoundaryKey::InFlight { prompt_hash: [1; 32] },
        }
    }

    fn unique_test_identity() -> CacheIdentity {
        let unique = ulid::Ulid::new().to_string();
        let sent = vec!["cache invalidation prompt".to_string()];
        CacheIdentity {
            bot_id: format!("scanner-test-{unique}"),
            run_id: unique,
            since: "2026-10-25T00:59:00Z".into(),
            boundary: BoundaryKey::InFlight { prompt_hash: prompt_hash(&sent) },
        }
    }

    fn identity_for(sent: &[String], since: &str) -> CacheIdentity {
        let unique = ulid::Ulid::new().to_string();
        CacheIdentity {
            bot_id: format!("scanner-test-{unique}"),
            run_id: unique,
            since: since.into(),
            boundary: BoundaryKey::InFlight {
                prompt_hash: prompt_hash(sent),
            },
        }
    }

    fn cached_entry_state(identity: &CacheIdentity) -> Option<EntryState> {
        negative_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .0
            .iter()
            .find(|entry| entry.identity == *identity)
            .map(|entry| entry.state)
    }

    fn interval_union_len(mut intervals: Vec<(u64, u64)>) -> u64 {
        intervals.sort_by_key(|(start, _)| *start);
        let mut total = 0;
        let mut current: Option<(u64, u64)> = None;
        for (start, end) in intervals {
            current = match current {
                Some((current_start, current_end)) if start <= current_end => {
                    Some((current_start, current_end.max(end)))
                }
                Some((current_start, current_end)) => {
                    total += current_end - current_start;
                    Some((start, end))
                }
                None => Some((start, end)),
            };
        }
        if let Some((start, end)) = current {
            total += end - start;
        }
        total
    }

    fn user_msg(at: &str, text: &str) -> String {
        serde_json::json!({
            "type": "response_item", "timestamp": at,
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}
        }).to_string()
    }

    fn task_complete(at: &str, reply: &str) -> String {
        serde_json::json!({
            "type": "event_msg", "timestamp": at,
            "payload": {"type": "task_complete", "last_agent_message": reply}
        }).to_string()
    }

    #[test]
    fn macos_local_codex_rollout_extracted_predicates_match_original_inline_conditions() {
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-25T01:00:00.500Z").unwrap();
        let sent = vec!["known prompt".to_string()];
        let timestamp = since.timestamp();
        let rows = vec![
            user_msg("2026-10-25T01:00:00.499Z", "known prompt"),
            user_msg("2026-10-25T01:00:00.500Z", "known prompt"),
            user_msg("2026-10-25T01:00:00.501Z", "known prompt"),
            user_msg("2026-10-25T01:00:01Z", "different prompt"),
            task_complete("2026-10-25T01:00:00.499Z", "a"),
            task_complete("2026-10-25T01:00:00.500Z", "a"),
            task_complete("2026-10-25T01:00:00.501Z", "a"),
            serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete", "completed_at":timestamp}}).to_string(),
            serde_json::json!({"type":"event_msg", "timestamp":"bad", "payload":{"type":"task_complete", "completed_at":timestamp+1}}).to_string(),
            serde_json::json!({"type":"event_msg", "timestamp":"bad", "payload":{"type":"task_complete"}}).to_string(),
            "not json".to_string(),
        ];
        for line in &rows {
            // Original inline predicates from origin/main, retained as independent comparison.
            let old_prompt = (|| {
                let is_match = crate::lifecycle::delivery::codex_user_text(line)
                    .is_some_and(|t| sent.iter().any(|s| s == &t));
                if !is_match { return false; }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return false };
                let Some(ts_str) = v.get("timestamp").and_then(serde_json::Value::as_str) else { return false };
                let Ok(at) = chrono::DateTime::parse_from_rfc3339(ts_str) else { return false };
                at >= since
            })();
            let old_complete = (|| {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return false };
                if v.get("type").and_then(serde_json::Value::as_str) != Some("event_msg")
                    || v.pointer("/payload/type").and_then(serde_json::Value::as_str) != Some("task_complete") { return false; }
                if let Some(ts) = v.get("timestamp").and_then(serde_json::Value::as_str) {
                    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(ts) { return at > since; }
                }
                if let Some(at) = v.pointer("/payload/completed_at").and_then(serde_json::Value::as_i64) { return at > timestamp; }
                false
            })();
            assert_eq!(super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since), old_prompt, "prompt: {line}");
            assert_eq!(super::super::codex_rollout::is_task_complete_candidate(line, &since), old_complete, "complete: {line}");
        }
        assert!(super::super::codex_rollout::is_turn_prompt_candidate(&rows[1], &sent, &since));
        assert!(!super::super::codex_rollout::is_task_complete_candidate(&rows[5], &since));
        assert!(super::super::codex_rollout::is_task_complete_candidate(&rows[8], &since));
    }

    #[test]
    fn macos_local_codex_rollout_short_suffix_and_old_tail_produce_identical_replies() {
        let temp = TempRollout::new();
        let path = temp.file();
        let since = "2026-10-25T00:59:00Z";
        let sent = vec!["synthetic prompt".to_string()];
        let logs = [
            format!("{}\n{}\n", user_msg("2026-10-25T01:00:00Z", &sent[0]), task_complete("2026-10-25T01:00:05Z", "answer")),
            format!("{}\n{}\n{}\n", native_event("2026-10-25T01:00:00Z", "task_started", "one", None), user_msg("2026-10-25T01:00:01Z", &sent[0]), native_event("2026-10-25T01:00:05Z", "task_complete", "one", Some("native answer"))),
            format!("{}\n", user_msg("2026-10-25T01:00:00Z", &sent[0])),
            format!("{}\n{}\n", user_msg("2026-10-25T01:00:00Z", "other"), task_complete("2026-10-25T01:00:05Z", "answer")),
            format!("{}\n{}\n{}\n{}\n", user_msg("2026-10-25T00:58:00Z", "earlier prompt"), task_complete("2026-10-25T00:58:05Z", "earlier answer"), user_msg("2026-10-25T01:00:00Z", &sent[0]), task_complete("2026-10-25T01:00:05Z", "answer after earlier lines")),
        ];
        for (index, log) in logs.iter().enumerate() {
            for size in [log.len(), CODEX_ROLLOUT_SCAN_CHUNK_BYTES as usize - 1, CODEX_ROLLOUT_SCAN_CHUNK_BYTES as usize, CODEX_ROLLOUT_SCAN_CHUNK_BYTES as usize + 1] {
                let padding = size - log.len();
                let contents = if padding == 0 { log.clone() } else {
                    assert!(padding > 256);
                    format!("{}\n{log}", noise_line(padding - 1))
                };
                assert_eq!(contents.len(), size);
                fs::write(&path, &contents).unwrap();
                let old = crate::transcript_read::read_tail(&path, CODEX_ROLLOUT_SCAN_CHUNK_BYTES).unwrap();
                let tag = format!("parity-{index}-{size}-{}", ulid::Ulid::new());
                let new = read_in_flight_suffix(&path, &tag, &tag, &sent, since);
                let old_reply = super::super::codex_rollout::codex_reply_for_turn(&old, &sent, since);
                let new_reply = new.as_deref().and_then(|s| super::super::codex_rollout::codex_reply_for_turn(s, &sent, since));
                if index == 3 && size <= CODEX_ROLLOUT_SCAN_CHUNK_BYTES as usize {
                    assert_eq!(old, contents, "old reader returns the entire short file");
                    assert!(new.is_none(), "the new reader returns None without a matching prompt");
                    assert!(old_reply.is_none(), "the old full file has no matching reply");
                }
                if index == 4 {
                    assert!(new.as_ref().is_some_and(|suffix| suffix.len() < old.len()), "earlier lines must actually be trimmed");
                    assert!(old.contains("earlier prompt"));
                    assert!(!new.as_ref().unwrap().contains("earlier prompt"), "the earlier prompt must be excluded");
                    assert_eq!(old_reply.as_deref(), Some("answer after earlier lines"));
                }
                assert_eq!(new_reply, old_reply, "in-flight case {index}, size {size}");
                let new = read_hookless_suffix(&path, &tag, &tag, since);
                let old_reply = super::super::codex_rollout::codex_latest_reply_since(&old, since);
                let new_reply = new.as_deref().and_then(|s| super::super::codex_rollout::codex_latest_reply_since(s, since));
                assert_eq!(new_reply, old_reply, "hookless case {index}, size {size}");
            }
        }
    }

    fn native_event(at: &str, kind: &str, id: &str, reply: Option<&str>) -> String {
        let mut payload = serde_json::json!({"type": kind, "turn_id": id});
        if let Some(reply) = reply {
            payload["last_agent_message"] = serde_json::json!(reply);
        }
        serde_json::json!({"type": "event_msg", "timestamp": at, "payload": payload}).to_string()
    }

    // The returned JSONL record is exactly output_len bytes (the fixture uses ASCII output).
    fn noise_line(output_len: usize) -> String {
        let base = serde_json::json!({
            "type": "response_item", "payload": {"type": "function_call_output", "output": ""}
        }).to_string();
        serde_json::json!({
            "type": "response_item", "payload": {
                "type": "function_call_output", "output": "x".repeat(output_len.saturating_sub(base.len()))
            }
        }).to_string()
    }

    fn cached_prompt_log(file_len: usize) -> Vec<u8> {
        let prompt = user_msg("2026-10-25T01:00:00Z", "cache invalidation prompt");
        let complete = task_complete("2026-10-25T01:00:05Z", "recovered synthetic reply");
        let mut bytes = format!("{prompt}\n{complete}\n").into_bytes();
        assert!(bytes.len() <= file_len);
        bytes.resize(file_len, b'x');
        bytes
    }

    fn set_fixture_old_mtime(path: &Path) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        let old_time = UNIX_EPOCH + Duration::from_secs(1);
        file.set_times(FileTimes::new().set_modified(old_time)).unwrap();
    }

    fn read_cached_prompt(path: &Path, identity: &CacheIdentity, limits: ScanLimits) -> ReadOutcome {
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-25T00:59:00Z").unwrap();
        let sent = vec!["cache invalidation prompt".to_string()];
        read_suffix_with(path, identity.clone(), limits, |line| {
            super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
        })
    }

    fn recovered_cached_reply(suffix: &str) -> Option<String> {
        let sent = vec!["cache invalidation prompt".to_string()];
        super::super::codex_rollout::codex_reply_for_turn(
            suffix,
            &sent,
            "2026-10-25T00:59:00Z",
        )
    }

    fn scan_file<F>(
        path: &Path,
        identity: &CacheIdentity,
        cache: &mut NegativeCache,
        limits: ScanLimits,
        now: Instant,
        mut candidate: F,
    ) -> (ScanResult, u64)
    where
        F: FnMut(&str) -> bool,
    {
        let file = File::open(path).unwrap();
        let mut reader = CountingReader::new(file);
        let before = current_snapshot(path, reader.get_ref()).unwrap();
        let plan = cache.plan(identity, before, now, limits);
        let cache_hit = matches!(plan, CachePlan::Hit | CachePlan::PendingHit);
        let result = scan_open_file(
            path,
            &mut reader,
            before,
            plan.clone(),
            limits,
            &mut candidate,
        )
        .unwrap();
        if !cache_hit && result.stop == StopReason::CompleteNoCandidate {
            let mut read_set = ReadSet::from_scan(&result, limits);
            let bytes = read_set.bytes(0, before.len).unwrap();
            let x = bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |offset| offset as u64 + 1);
            let prefix = read_set.anchor(x).unwrap();
            let eof = read_set.anchor(before.len).unwrap();
            cache.store_observation(
                identity.clone(),
                before,
                PrefixProof {
                    no_match_end: x,
                    anchor: prefix,
                },
                ObservedEof {
                    end: before.len,
                    anchor: eof,
                },
                if x == before.len {
                    EntryState::NegativeNoCandidate
                } else {
                    EntryState::NegativePartialEof
                },
                CandidateOffsets::default(),
                now,
                limits,
            );
        }
        let bytes_read = reader.bytes_read;
        (result, bytes_read)
    }

    #[test]
    fn macos_local_codex_rollout_prompt_more_than_four_mib_away_crossing_boundary_recovers_reply() {
        let temp = TempRollout::new();
        let path = temp.file();
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-25T00:59:00Z").unwrap();
        let prompt_line = format!("{}\n", user_msg("2026-10-25T01:00:00Z", "synthetic prompt"));
        let complete_line = format!("{}\n", task_complete("2026-10-25T01:00:05Z", "synthetic reply"));
        let suffix_len = CODEX_ROLLOUT_SCAN_CHUNK_BYTES as usize - prompt_line.len() / 2;
        let noise_len = suffix_len - complete_line.len() - 1;
        let noise = noise_line(noise_len);
        assert_eq!(noise.len() + 1 + complete_line.len(), suffix_len);
        let mut file = File::create(&path).unwrap();
        file.write_all(prompt_line.as_bytes()).unwrap();
        file.write_all(noise.as_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(complete_line.as_bytes()).unwrap();
        drop(file);

        let sent = vec!["synthetic prompt".to_string()];
        let mut cache = NegativeCache::default();
        let (result, bytes_read) = scan_file(&path, &identity(), &mut cache, LIMITS, Instant::now(), |line| {
            super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
        });
        let suffix = result.suffix.expect("the prompt line crosses the first chunk boundary");
        assert_eq!(
            super::super::codex_rollout::codex_reply_for_turn(&suffix, &sent, "2026-10-25T00:59:00Z"),
            Some("synthetic reply".into())
        );
        assert!(result.candidate_offset.is_some());
        assert!(bytes_read > CODEX_ROLLOUT_SCAN_CHUNK_BYTES);
        assert_eq!(result.chunks_read, 2);
    }

    #[test]
    fn macos_local_codex_rollout_candidate_inside_last_four_mib_proves_prefix_once() {
        let temp = TempRollout::new();
        let path = temp.file();
        let log = format!("{}\n{}\n",
            user_msg("2026-10-25T01:00:00Z", "synthetic prompt"),
            task_complete("2026-10-25T01:00:05Z", "synthetic reply"));
        let prefix = noise_line(CODEX_ROLLOUT_SCAN_CHUNK_BYTES as usize + 128);
        let mut file = File::create(&path).unwrap();
        file.write_all(prefix.as_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(log.as_bytes()).unwrap();
        drop(file);
        let file_len = fs::metadata(&path).unwrap().len();
        assert!(file_len > CODEX_ROLLOUT_SCAN_CHUNK_BYTES);

        let since = chrono::DateTime::parse_from_rfc3339("2026-10-25T00:59:00Z").unwrap();
        let sent = vec!["synthetic prompt".to_string()];
        let outcome = read_suffix_with(&path, unique_test_identity(), LIMITS, |line| {
            super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
        });
        let suffix = outcome.suffix.expect("the candidate is in the tail chunk");
        assert_eq!(
            super::super::codex_rollout::codex_reply_for_turn(&suffix, &sent, "2026-10-25T00:59:00Z"),
            Some("synthetic reply".into())
        );
        assert_eq!(
            outcome.chunks_read, 2,
            "the first candidate poll proves the earlier prefix once"
        );
        assert_eq!(
            outcome.bytes_read, file_len,
            "initial native-capable prompt proof covers byte zero through EOF"
        );
    }

    #[test]
    fn macos_local_codex_rollout_same_prompt_before_since_is_not_a_candidate() {
        let temp = TempRollout::new();
        let path = temp.file();
        let log = format!("{}\n{}\n",
            user_msg("2026-10-25T00:50:00Z", "synthetic prompt"),
            task_complete("2026-10-25T01:00:05Z", "synthetic reply"));
        fs::write(&path, log).unwrap();
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-25T01:00:00Z").unwrap();
        let sent = vec!["synthetic prompt".to_string()];
        let mut cache = NegativeCache::default();
        let (result, _) = scan_file(&path, &identity(), &mut cache, limits(32, 1024, 256, 257), Instant::now(), |line| {
            super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
        });
        assert_eq!(result.stop, StopReason::CompleteNoCandidate);
        assert!(result.suffix.is_none());
    }

    #[test]
    fn macos_local_codex_rollout_user_input_after_task_complete_rejects_both_paths() {
        let temp = TempRollout::new();
        let path = temp.file();
        let log = format!("{}\n{}\n{}\n",
            user_msg("2026-10-25T01:00:00Z", "synthetic prompt"),
            task_complete("2026-10-25T01:00:05Z", "synthetic reply"),
            user_msg("2026-10-25T01:00:06Z", "next input"));
        fs::write(&path, log).unwrap();
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-25T00:59:00Z").unwrap();
        let sent = vec!["synthetic prompt".to_string()];
        let mut cache = NegativeCache::default();
        let test_limits = limits(1024, 4096, 2048, 2049);
        let (result, _) = scan_file(&path, &identity(), &mut cache, test_limits, Instant::now(), |line| {
            super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
        });
        assert_eq!(
            super::super::codex_rollout::codex_reply_for_turn(result.suffix.as_deref().unwrap(), &sent, "2026-10-25T00:59:00Z"),
            None
        );

        let mut cache = NegativeCache::default();
        let (result, _) = scan_file(&path, &identity(), &mut cache, test_limits, Instant::now(), |line| {
            super::super::codex_rollout::is_task_complete_candidate(line, &since)
        });
        assert_eq!(
            super::super::codex_rollout::codex_latest_reply_since(result.suffix.as_deref().unwrap(), "2026-10-25T00:59:00Z"),
            None
        );
    }

    #[test]
    fn macos_local_codex_rollout_scan_limit_fails_closed_without_negative_cache() {
        let temp = TempRollout::new();
        let path = temp.file();
        fs::write(&path, b"x\n".repeat(128)).unwrap();
        let mut cache = NegativeCache::default();
        let (result, bytes_read) = scan_file(&path, &identity(), &mut cache, limits(16, 64, 32, 33), Instant::now(), |_| false);
        assert_eq!(result.stop, StopReason::ScanLimit);
        assert!(!result.complete);
        assert_eq!(bytes_read, 64);
        assert!(cache.0.is_empty(), "an incomplete scan is not a cached no-candidate result");
    }

    #[test]
    fn macos_local_codex_rollout_oversized_line_fails_before_reading_the_whole_line() {
        let temp = TempRollout::new();
        let path = temp.file();
        let line_len = 128;
        let mut file = File::create(&path).unwrap();
        for _ in 0..line_len / 16 {
            file.write_all(b"xxxxxxxxxxxxxxxx").unwrap();
        }
        drop(file);
        let mut cache = NegativeCache::default();
        let (result, bytes_read) = scan_file(&path, &identity(), &mut cache, limits(16, 1024, 32, 33), Instant::now(), |_| false);
        assert_eq!(result.stop, StopReason::LineTooLong);
        assert!(bytes_read < line_len as u64);
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_complete_negative_scan_is_not_repeated_at_same_file_length() {
        let temp = TempRollout::new();
        let path = temp.file();
        fs::write(&path, b"noise\nnoise\nnoise\n").unwrap();
        let identity = unique_test_identity();
        let configured = limits(8, 1024, 32, 33);
        let first = read_suffix_with(&path, identity.clone(), configured, |_| false);
        assert!(first.suffix.is_none());
        assert_eq!(first.bytes_read, fs::metadata(&path).unwrap().len());
        let second = read_suffix_with(&path, identity, configured, |_| false);
        assert!(second.suffix.is_none());
        assert_eq!(second.chunks_read, 0);
        assert_eq!(second.bytes_read, 0);
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_same_length_in_place_overwrite_invalidates_cache() {
        let temp = TempRollout::new();
        let path = temp.file();
        let file_len = 512;
        fs::write(&path, vec![b'n'; file_len]).unwrap();
        set_fixture_old_mtime(&path);

        let identity = unique_test_identity();
        let configured = limits(64, 4096, 1024, 1025);
        let first = read_cached_prompt(&path, &identity, configured);
        assert!(first.suffix.is_none());
        let unchanged = read_cached_prompt(&path, &identity, configured);
        assert_eq!(unchanged.bytes_read, 0, "unchanged metadata cache hit must not read payload bytes");

        let before = FileSnapshot::from_metadata(&fs::metadata(&path).unwrap());
        let replacement = cached_prompt_log(file_len);
        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&replacement).unwrap();
        file.sync_all().unwrap();
        let after = FileSnapshot::from_metadata(&fs::metadata(&path).unwrap());
        assert_eq!(before.len, after.len);
        assert_eq!(before.instance, after.instance);
        assert_ne!(before.modified, after.modified, "fixture mtime is fixed old before the overwrite");

        let rescanned = read_cached_prompt(&path, &identity, configured);
        assert!(rescanned.bytes_read > 0, "same-length metadata change must invalidate the negative hit");
        assert_eq!(
            recovered_cached_reply(rescanned.suffix.as_deref().unwrap()),
            Some("recovered synthetic reply".into()),
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_same_length_atomic_replacement_invalidates_cache() {
        let temp = TempRollout::new();
        let path = temp.file();
        let file_len = 512;
        fs::write(&path, vec![b'n'; file_len]).unwrap();
        set_fixture_old_mtime(&path);

        let identity = unique_test_identity();
        let configured = limits(64, 4096, 1024, 1025);
        let first = read_cached_prompt(&path, &identity, configured);
        assert!(first.suffix.is_none());
        let unchanged = read_cached_prompt(&path, &identity, configured);
        assert_eq!(unchanged.bytes_read, 0, "unchanged metadata cache hit must not read payload bytes");

        let old_instance = FileSnapshot::from_metadata(&fs::metadata(&path).unwrap()).instance;
        let replacement_path = temp.0.join("replacement.jsonl");
        fs::write(&replacement_path, cached_prompt_log(file_len)).unwrap();
        let new_instance = FileSnapshot::from_metadata(&fs::metadata(&replacement_path).unwrap()).instance;
        assert_ne!(old_instance, new_instance, "replacement fixture must have a distinct inode");
        fs::rename(&replacement_path, &path).unwrap();
        assert_eq!(
            FileSnapshot::from_metadata(&fs::metadata(&path).unwrap()).instance,
            new_instance,
            "rename must install the replacement inode at the original path",
        );

        let rescanned = read_cached_prompt(&path, &identity, configured);
        assert!(rescanned.bytes_read > 0, "replacement inode must invalidate the negative hit");
        assert_eq!(
            recovered_cached_reply(rescanned.suffix.as_deref().unwrap()),
            Some("recovered synthetic reply".into()),
        );
    }

    struct FailAfterReader {
        inner: Cursor<Vec<u8>>,
        fail_after: u64,
        bytes_read: u64,
    }

    impl Read for FailAfterReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.bytes_read >= self.fail_after {
                return Err(io::Error::other("injected read failure"));
            }
            let allowed = buffer.len().min((self.fail_after - self.bytes_read) as usize);
            let count = self.inner.read(&mut buffer[..allowed])?;
            self.bytes_read += count as u64;
            Ok(count)
        }
    }

    impl Seek for FailAfterReader {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> { self.inner.seek(position) }
    }

    #[test]
    fn macos_local_codex_rollout_partial_io_failure_fails_closed_with_actual_bytes() {
        let fail_after = 7;
        let source = vec![b'n'; 64];
        let mut reader = CountingReader::new(FailAfterReader {
            inner: Cursor::new(source),
            fail_after,
            bytes_read: 0,
        });
        let mut no_candidate = |_line: &str| false;
        let result = scan_counted(
            &mut reader,
            64,
            0,
            limits(16, 64, 32, 33),
            None,
            &mut no_candidate,
        );
        let (suffix, reported_bytes) = match result {
            Ok(scan) => (scan.suffix, scan.bytes_scanned),
            Err(failure) => (None, failure.bytes_scanned),
        };
        assert_eq!(reader.bytes_read, fail_after);
        assert_eq!(reported_bytes, fail_after);
        assert!(suffix.is_none(), "partial I/O failure must return no rollout suffix");
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_increased_file_length_reads_append_and_overlap() {
        let temp = TempRollout::new();
        let path = temp.file();
        fs::write(&path, b"noise\n".repeat(64)).unwrap();
        let identity = unique_test_identity();
        let configured = limits(64, 4096, 32, 33);
        let first = read_suffix_with(&path, identity.clone(), configured, |_| false);
        assert!(first.suffix.is_none());
        let old_len = fs::metadata(&path).unwrap().len();
        let appended = b"new-noise\n";
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(appended).unwrap();
        drop(file);
        let second = read_suffix_with(&path, identity, configured, |_| false);
        assert!(second.suffix.is_none());
        assert_eq!(
            second.bytes_read,
            fs::metadata(&path).unwrap().len(),
            "the required prefix anchor extends before this small fixture's delta overlap"
        );
        assert_eq!(second.bytes_read, old_len + appended.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_partial_user_row_keeps_x_at_sealed_line_and_enters_pending() {
        let temp = TempRollout::new();
        let path = temp.file();
        let identity = unique_test_identity();
        let complete_row = user_msg("2026-10-25T01:00:00Z", "cache invalidation prompt");
        let partial = &complete_row[..complete_row.len() - 1];
        let mut initial = b"noise\n".to_vec();
        initial.extend_from_slice(partial.as_bytes());
        fs::write(&path, &initial).unwrap();
        let configured = limits(4096, 1 << 20, 4096, CODEX_ROLLOUT_CACHE_OVERLAP_BYTES);

        let first = read_cached_prompt(&path, &identity, configured);
        assert_eq!(first.disposition, ScanDisposition::NoCandidate);
        assert_eq!(first.bytes_read, initial.len() as u64);
        assert_eq!(
            cached_entry_state(&identity),
            Some(EntryState::NegativePartialEof)
        );
        let before = {
            let cache = negative_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            cache
                .0
                .iter()
                .find(|entry| entry.identity == identity)
                .unwrap()
                .clone()
        };
        assert_eq!(
            before.prefix.no_match_end,
            b"noise\n".len() as u64,
            "X must stop after the last newline, before the unsealed user row"
        );

        let appended = b"}\n";
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(appended).unwrap();
        drop(file);
        let new_len = fs::metadata(&path).unwrap().len();
        let second = read_cached_prompt(&path, &identity, configured);
        assert_eq!(second.disposition, ScanDisposition::PendingCandidate);
        assert!(
            second.suffix.is_none(),
            "an incomplete prompt must not be cached as ready"
        );
        assert_eq!(
            second.bytes_read, new_len,
            "the small file is wholly inside delta plus overlap"
        );
        assert_eq!(
            cached_entry_state(&identity),
            Some(EntryState::PendingWaitingCompletion)
        );
        let pending = {
            let cache = negative_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            cache
                .0
                .iter()
                .find(|entry| entry.identity == identity)
                .unwrap()
                .clone()
        };
        assert_eq!(pending.prefix.no_match_end, b"noise\n".len() as u64);
        assert_eq!(
            pending.candidate.first_seen_match,
            Some(b"noise\n".len() as u64)
        );

        let unchanged = read_cached_prompt(&path, &identity, configured);
        assert_eq!(
            unchanged.bytes_read, 0,
            "unchanged pending polls do not read payload or suffix bytes"
        );
        assert_eq!(
            cached_entry_state(&identity),
            Some(EntryState::PendingWaitingCompletion)
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_sealed_proof_offsets_count_raw_bytes() {
        let temp = TempRollout::new();
        let path = temp.file();
        let identity = unique_test_identity();
        let bytes = b"\xffnoise\n";
        fs::write(&path, bytes).unwrap();

        let outcome = read_suffix_with(&path, identity.clone(), LIMITS, |_| false);
        assert_eq!(outcome.disposition, ScanDisposition::NoCandidate);
        assert_eq!(cached_entry_state(&identity), Some(EntryState::NegativeNoCandidate));
        let cache = negative_cache().lock().unwrap_or_else(|error| error.into_inner());
        let entry = cache.0.iter().find(|entry| entry.identity == identity).unwrap();
        assert_eq!(entry.prefix.no_match_end, bytes.len() as u64,
            "lossy UTF-8 decoding must not shift the byte proof cursor");
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_pending_second_and_third_polls_read_only_delta_overlap_and_prefix_anchor(
    ) {
        let temp = TempRollout::new();
        let path = temp.file();
        let identity = unique_test_identity();
        let noise = noise_line(256);
        let prompt = user_msg("2026-10-25T01:00:00Z", "cache invalidation prompt");
        let mut file = File::create(&path).unwrap();
        for _ in 0..4096 {
            file.write_all(noise.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.write_all(prompt.as_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
        for _ in 0..36_864 {
            file.write_all(noise.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
        drop(file);
        let configured = limits(
            4 * 1024 * 1024,
            32 * 1024 * 1024,
            4096,
            CODEX_ROLLOUT_CACHE_OVERLAP_BYTES,
        );

        let first = read_cached_prompt(&path, &identity, configured);
        assert_eq!(first.disposition, ScanDisposition::PendingCandidate);
        assert_eq!(
            first.bytes_read,
            fs::metadata(&path).unwrap().len(),
            "the first proof scans the prefix once"
        );
        assert_eq!(
            cached_entry_state(&identity),
            Some(EntryState::PendingWaitingCompletion)
        );
        let first_entry = {
            let cache = negative_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            cache
                .0
                .iter()
                .find(|entry| entry.identity == identity)
                .unwrap()
                .clone()
        };
        assert!(first_entry.prefix.no_match_end > PREFIX_ANCHOR_BYTES);
        assert!(
            first_entry.prefix.anchor.end
                < first_entry.observed.end - configured.cache_overlap_bytes,
            "the X anchor is deliberately outside the appended tail overlap"
        );

        for append in [&b"second-poll\n"[..], &b"third-poll\n"[..]] {
            let old_eof = fs::metadata(&path).unwrap().len();
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(append).unwrap();
            drop(file);
            let outcome = read_cached_prompt(&path, &identity, configured);
            assert_eq!(outcome.disposition, ScanDisposition::PendingCandidate);
            assert!(outcome.suffix.is_none());
            assert_eq!(
                outcome.bytes_read,
                configured.cache_overlap_bytes + append.len() as u64 + PREFIX_ANCHOR_BYTES,
                "Y anchor is in overlap; only the distant X anchor adds 4 KiB"
            );
            assert_eq!(
                cached_entry_state(&identity),
                Some(EntryState::PendingWaitingCompletion)
            );
            let updated = {
                let cache = negative_cache()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                cache
                    .0
                    .iter()
                    .find(|entry| entry.identity == identity)
                    .unwrap()
                    .clone()
            };
            assert_eq!(updated.observed.end, old_eof + append.len() as u64);
        }
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_pending_completion_append_reads_exact_interval_union() {
        let temp = TempRollout::new();
        let path = temp.file();
        let identity = unique_test_identity();
        let noise = noise_line(256);
        let start = native_event("2026-10-25T00:59:59Z", "task_started", "turn-pending", None);
        let prompt = user_msg("2026-10-25T01:00:00Z", "cache invalidation prompt");
        let mut file = File::create(&path).unwrap();
        for _ in 0..4096 {
            file.write_all(noise.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.write_all(start.as_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(prompt.as_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
        for _ in 0..36_864 {
            file.write_all(noise.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
        drop(file);
        let configured = limits(
            4 * 1024 * 1024,
            32 * 1024 * 1024,
            4096,
            CODEX_ROLLOUT_CACHE_OVERLAP_BYTES,
        );

        let first = read_cached_prompt(&path, &identity, configured);
        assert_eq!(first.disposition, ScanDisposition::PendingCandidate);
        let previous = {
            let cache = negative_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            cache
                .0
                .iter()
                .find(|entry| entry.identity == identity)
                .unwrap()
                .clone()
        };
        assert_eq!(previous.state, EntryState::PendingWaitingCompletion);
        let suffix_start = previous.candidate.suffix_start.unwrap();
        let delta_start = previous
            .observed
            .end
            .saturating_sub(configured.cache_overlap_bytes);
        assert!(suffix_start < delta_start, "the old parser window precedes the append delta");

        let complete = native_event(
            "2026-10-25T01:00:05Z",
            "task_complete",
            "turn-pending",
            Some("append completion reply"),
        );
        let appended = format!("{complete}\n");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(appended.as_bytes()).unwrap();
        drop(file);
        let new_len = fs::metadata(&path).unwrap().len();

        let completed = read_cached_prompt(&path, &identity, configured);
        assert_eq!(completed.disposition, ScanDisposition::CandidateReady);
        assert_eq!(cached_entry_state(&identity), Some(EntryState::ReadyCandidate));
        assert_eq!(
            super::super::codex_rollout::codex_reply_for_turn(
                completed.suffix.as_deref().unwrap(),
                &["cache invalidation prompt".into()],
                "2026-10-25T00:59:00Z"
            ),
            Some("append completion reply".into())
        );

        let expected_union = interval_union_len(vec![
            (delta_start, new_len),
            (previous.prefix.anchor.start, previous.prefix.anchor.end),
            (previous.observed.anchor.start, previous.observed.anchor.end),
            (suffix_start, new_len),
        ]);
        assert_eq!(completed.bytes_read, expected_union,
            "Pending→Ready reads exactly delta, old anchors, and reconstructed [S, EOF) union");
        assert!(completed.bytes_read <= configured.max_scan_bytes,
            "append, anchor verification, and suffix reconstruction share the call budget");
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_anchor_mismatch_rescan_uses_same_budget_and_drops_entry() {
        let temp = TempRollout::new();
        let path = temp.file();
        let identity = unique_test_identity();
        let prompt = user_msg("2026-10-25T01:00:00Z", "cache invalidation prompt");
        let mut bytes = b"noise\n".repeat(2000);
        bytes.extend_from_slice(prompt.as_bytes());
        bytes.extend_from_slice(b"\n");
        bytes.extend_from_slice(&b"tail\n".repeat(1000));
        fs::write(&path, &bytes).unwrap();
        let configured = limits(4096, bytes.len() as u64, 1024, 8193);

        let first = read_cached_prompt(&path, &identity, configured);
        assert_eq!(first.disposition, ScanDisposition::PendingCandidate);
        assert_eq!(first.bytes_read, bytes.len() as u64);
        assert_eq!(cached_entry_state(&identity), Some(EntryState::PendingWaitingCompletion));

        let old_len = bytes.len() as u64;
        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(old_len - 1)).unwrap();
        file.write_all(b"x").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"\nnoise\n").unwrap();
        drop(file);

        let failed = read_cached_prompt(&path, &identity, configured);
        assert!(failed.suffix.is_none());
        assert_eq!(failed.disposition, ScanDisposition::Incomplete);
        assert!(failed.bytes_read <= configured.max_scan_bytes,
            "delta, anchors, and invalidation rescan share the initial 128 MiB style call budget");
        assert_eq!(cached_entry_state(&identity), None,
            "a proof that cannot be rebuilt inside the remaining budget leaves no cache entry");
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_ready_same_eof_rebuilds_suffix_for_parser_retry() {
        let temp = TempRollout::new();
        let path = temp.file();
        let identity = unique_test_identity();
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-25T00:59:00Z").unwrap();
        let sent = vec!["cache invalidation prompt".to_string()];
        let start = native_event("2026-10-25T01:00:00.000Z", "task_started", "turn-a", None);
        let prompt = user_msg("2026-10-25T01:00:00.100Z", &sent[0]);
        let complete = native_event(
            "2026-10-25T01:00:00.200Z",
            "task_complete",
            "turn-a",
            Some("retry reply"),
        );
        let prefix = "prefix-noise\n";
        let log = format!("{prefix}{start}\n{prompt}\n{complete}\n");
        fs::write(&path, &log).unwrap();
        let configured = limits(4096, 1 << 20, 4096, CODEX_ROLLOUT_CACHE_OVERLAP_BYTES);
        let read = || {
            read_suffix_with(&path, identity.clone(), configured, |line| {
                super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
            })
        };

        let first = read();
        let first_suffix = first
            .suffix
            .expect("complete native candidate is recoverable");
        assert_eq!(
            cached_entry_state(&identity),
            Some(EntryState::ReadyCandidate)
        );
        assert_eq!(
            super::super::codex_rollout::codex_reply_for_turn(
                &first_suffix,
                &sent,
                "2026-10-25T00:59:00Z"
            ),
            Some("retry reply".into()),
        );

        let second = read();
        let second_suffix = second
            .suffix
            .expect("same EOF must retry parser recovery after a missed DB write");
        assert_eq!(second_suffix, first_suffix);
        assert_eq!(
            second.bytes_read,
            log.len() as u64 - prefix.len() as u64,
            "Ready re-reads only the uncached suffix beginning at task_started"
        );
        assert_eq!(
            super::super::codex_rollout::codex_reply_for_turn(
                &second_suffix,
                &sent,
                "2026-10-25T00:59:00Z"
            ),
            Some("retry reply".into()),
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_native_duplicate_prompt_since_a_rejects_since_b_returns_b() {
        let temp = TempRollout::new();
        let path = temp.file();
        let sent = vec!["same native prompt".to_string()];
        let rows = [
            native_event("2026-10-25T01:00:00.000Z", "task_started", "turn-a", None),
            user_msg("2026-10-25T01:00:00.100Z", &sent[0]),
            native_event(
                "2026-10-25T01:00:00.200Z",
                "task_complete",
                "turn-a",
                Some("reply A"),
            ),
            native_event("2026-10-25T01:00:00.300Z", "task_started", "turn-b", None),
            user_msg("2026-10-25T01:00:00.400Z", &sent[0]),
            native_event(
                "2026-10-25T01:00:00.500Z",
                "task_complete",
                "turn-b",
                Some("reply B"),
            ),
        ];
        let log = rows.join("\n") + "\n";
        fs::write(&path, &log).unwrap();

        for (since_iso, expected) in [
            ("2026-10-25T01:00:00.050Z", None),
            ("2026-10-25T01:00:00.300Z", Some("reply B")),
        ] {
            let identity = identity_for(&sent, since_iso);
            let since = chrono::DateTime::parse_from_rfc3339(since_iso).unwrap();
            let outcome = read_suffix_with(&path, identity, LIMITS, |line| {
                super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
            });
            let suffix = outcome
                .suffix
                .expect("scanner must preserve earlier matching rows and native begins");
            assert_eq!(
                super::super::codex_rollout::codex_reply_for_turn(&suffix, &sent, since_iso),
                expected.map(str::to_owned)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_latest_native_start_preserves_earlier_prompt_for_parser_rejection() {
        let temp = TempRollout::new();
        let path = temp.file();
        let sent = vec!["same native prompt".to_string()];
        let since_iso = "2026-10-25T01:00:00.050Z";
        let task_complete_without_id = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-10-25T01:00:00.500Z",
            "payload": {"type": "task_complete", "last_agent_message": "must be rejected"}
        }).to_string();
        let rows = [
            user_msg("2026-10-25T01:00:00.100Z", &sent[0]),
            native_event("2026-10-25T01:00:00.300Z", "task_started", "turn-b", None),
            user_msg("2026-10-25T01:00:00.400Z", &sent[0]),
            task_complete_without_id,
        ];
        let log = rows.join("\n") + "\n";
        fs::write(&path, &log).unwrap();
        let identity = identity_for(&sent, since_iso);
        let since = chrono::DateTime::parse_from_rfc3339(since_iso).unwrap();
        let outcome = read_suffix_with(&path, identity.clone(), LIMITS, |line| {
            super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
        });

        let suffix = outcome.suffix.expect("scanner must return the complete window to the existing parser");
        assert!(suffix.starts_with(&rows[0]), "suffix must retain the earlier matching prompt");
        assert!(suffix.contains(&rows[1]), "suffix must retain the latest target prompt's native start");
        assert_eq!(cached_entry_state(&identity), Some(EntryState::ReadyCandidate));
        assert_eq!(
            super::super::codex_rollout::codex_reply_for_turn(&suffix, &sent, since_iso),
            None,
            "the existing parser sees the earlier matching prompt before turn B and rejects"
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_native_abort_backwards_clock_and_millisecond_window_reach_parser()
    {
        let temp = TempRollout::new();
        let path = temp.file();
        let sent = vec!["integration prompt".to_string()];
        let cases = [
            (
                vec![
                    native_event("2026-10-25T01:00:00.100Z", "task_started", "turn-a", None),
                    user_msg("2026-10-25T01:00:00.200Z", &sent[0]),
                    native_event("2026-10-25T01:00:00.300Z", "task_aborted", "turn-a", None),
                    native_event(
                        "2026-10-25T01:00:00.400Z",
                        "task_complete",
                        "turn-a",
                        Some("aborted reply"),
                    ),
                ],
                "2026-10-25T01:00:00.000Z",
                None,
            ),
            (
                vec![
                    native_event("2026-10-25T01:00:00.300Z", "task_started", "turn-a", None),
                    user_msg("2026-10-25T01:00:00.200Z", &sent[0]),
                    native_event(
                        "2026-10-25T01:00:00.400Z",
                        "task_complete",
                        "turn-a",
                        Some("backwards reply"),
                    ),
                ],
                "2026-10-25T01:00:00.000Z",
                None,
            ),
            (
                vec![
                    native_event("2026-10-25T01:00:00.010Z", "task_started", "turn-a", None),
                    user_msg("2026-10-25T01:00:00.020Z", &sent[0]),
                    native_event(
                        "2026-10-25T01:00:00.030Z",
                        "task_complete",
                        "turn-a",
                        Some("reply A"),
                    ),
                    native_event("2026-10-25T01:00:00.040Z", "task_started", "turn-b", None),
                    user_msg("2026-10-25T01:00:00.050Z", &sent[0]),
                    native_event(
                        "2026-10-25T01:00:00.060Z",
                        "task_complete",
                        "turn-b",
                        Some("millisecond reply B"),
                    ),
                ],
                "2026-10-25T01:00:00.035Z",
                Some("millisecond reply B"),
            ),
        ];

        for (rows, since, expected) in cases {
            let log = rows.join("\n") + "\n";
            fs::write(&path, &log).unwrap();
            let identity = identity_for(&sent, since);
            let since_at = chrono::DateTime::parse_from_rfc3339(since).unwrap();
            let outcome = read_suffix_with(&path, identity, LIMITS, |line| {
                super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since_at)
            });
            let suffix = outcome
                .suffix
                .expect("native parser rejection still requires a complete scanner window");
            assert_eq!(
                super::super::codex_rollout::codex_reply_for_turn(&suffix, &sent, since),
                expected.map(str::to_owned),
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_hookless_suffix_starts_at_nearest_native_task() {
        let temp = TempRollout::new();
        let path = temp.file();
        let since_iso = "2026-10-25T00:59:00Z";
        let identity = CacheIdentity {
            bot_id: format!("scanner-hookless-{}", ulid::Ulid::new()),
            run_id: ulid::Ulid::new().to_string(),
            since: since_iso.into(),
            boundary: BoundaryKey::Hookless,
        };
        let old_start = native_event("2026-10-25T00:58:00Z", "task_started", "old", None);
        let current_start = native_event("2026-10-25T01:00:00Z", "task_started", "current", None);
        let complete = native_event(
            "2026-10-25T01:00:05Z",
            "task_complete",
            "current",
            Some("hookless reply"),
        );
        let log = format!("{old_start}\n{current_start}\n{complete}\n");
        fs::write(&path, &log).unwrap();
        let since = chrono::DateTime::parse_from_rfc3339(since_iso).unwrap();
        let outcome = read_suffix_with(&path, identity.clone(), LIMITS, |line| {
            super::super::codex_rollout::is_task_complete_candidate(line, &since)
        });
        let suffix = outcome
            .suffix
            .expect("hookless candidate must retain the nearest started row");
        assert!(suffix.starts_with(&current_start));
        assert!(!suffix.contains(&old_start));
        assert_eq!(
            super::super::codex_rollout::codex_latest_reply_since(&suffix, since_iso),
            Some("hookless reply".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_id_contradiction_keeps_suffix_for_existing_parser_to_reject() {
        let temp = TempRollout::new();
        let path = temp.file();
        let sent = vec!["contradiction prompt".to_string()];
        let since_iso = "2026-10-25T00:59:00Z";
        let rows = [
            native_event("2026-10-25T01:00:00Z", "task_started", "turn-a", None),
            user_msg("2026-10-25T01:00:01Z", &sent[0]),
            native_event(
                "2026-10-25T01:00:02Z",
                "task_complete",
                "turn-b",
                Some("wrong reply"),
            ),
        ];
        fs::write(&path, rows.join("\n") + "\n").unwrap();
        let identity = identity_for(&sent, since_iso);
        let since = chrono::DateTime::parse_from_rfc3339(since_iso).unwrap();
        let outcome = read_suffix_with(&path, identity.clone(), LIMITS, |line| {
            super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
        });
        let suffix = outcome
            .suffix
            .expect("ID contradiction is parser evidence, not an absent scanner window");
        assert_eq!(
            cached_entry_state(&identity),
            Some(EntryState::ReadyCandidate)
        );
        assert_eq!(
            super::super::codex_rollout::codex_reply_for_turn(&suffix, &sent, since_iso),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_native_window_without_started_row_fails_closed_without_cache() {
        let temp = TempRollout::new();
        let path = temp.file();
        let sent = vec!["missing start prompt".to_string()];
        let since_iso = "2026-10-25T00:59:00Z";
        let rows = [
            user_msg("2026-10-25T01:00:01Z", &sent[0]),
            native_event(
                "2026-10-25T01:00:02Z",
                "task_complete",
                "turn-a",
                Some("incomplete window"),
            ),
        ];
        fs::write(&path, rows.join("\n") + "\n").unwrap();
        let identity = identity_for(&sent, since_iso);
        let since = chrono::DateTime::parse_from_rfc3339(since_iso).unwrap();
        let outcome = read_suffix_with(&path, identity.clone(), LIMITS, |line| {
            super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
        });
        assert!(outcome.suffix.is_none());
        assert_eq!(outcome.disposition, ScanDisposition::Incomplete);
        assert_eq!(
            cached_entry_state(&identity),
            None,
            "an incomplete native window is not a negative cache entry"
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_local_codex_rollout_each_entry_state_handles_each_mutation_with_exact_reads() {
        #[derive(Clone, Copy, Debug)]
        enum Mutation {
            Append,
            SameLengthVersion,
            Truncate,
            Replace,
            AnchorMismatch,
        }
        let states = [
            EntryState::NegativeNoCandidate,
            EntryState::NegativePartialEof,
            EntryState::PendingWaitingCompletion,
            EntryState::ReadyCandidate,
        ];
        let mutations = [
            Mutation::Append,
            Mutation::SameLengthVersion,
            Mutation::Truncate,
            Mutation::Replace,
            Mutation::AnchorMismatch,
        ];
        let configured = limits(4096, 1 << 20, 1024, 8193);
        let prefix = b"noise\n".repeat(5000);
        let sent = vec!["cache invalidation prompt".to_string()];

        for state in states {
            for mutation in mutations {
                let temp = TempRollout::new();
                let path = temp.file();
                let identity = unique_test_identity();
                let mut initial = prefix.clone();
                match state {
                    EntryState::NegativeNoCandidate => initial.extend_from_slice(b"noise\n"),
                    EntryState::NegativePartialEof => initial.extend_from_slice(b"unfinished"),
                    EntryState::PendingWaitingCompletion => {
                        initial.extend_from_slice(
                            user_msg("2026-10-25T01:00:00Z", &sent[0]).as_bytes(),
                        );
                        initial.push(b'\n');
                        initial.extend_from_slice(b"tail\n");
                    }
                    EntryState::ReadyCandidate => {
                        initial.extend_from_slice(
                            user_msg("2026-10-25T01:00:00Z", &sent[0]).as_bytes(),
                        );
                        initial.push(b'\n');
                        initial.extend_from_slice(
                            task_complete("2026-10-25T01:00:05Z", "matrix reply").as_bytes(),
                        );
                        initial.push(b'\n');
                        initial.extend_from_slice(b"tail\n");
                    }
                }
                fs::write(&path, &initial).unwrap();
                let setup = read_cached_prompt(&path, &identity, configured);
                assert_eq!(
                    cached_entry_state(&identity),
                    Some(state),
                    "fixture must enter each real cache state"
                );
                assert_eq!(setup.suffix.is_some(), state == EntryState::ReadyCandidate);
                let old_len = initial.len() as u64;

                let expected_state =
                    match mutation {
                        Mutation::Append => {
                            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
                            file.write_all(b"noise\n").unwrap();
                            match state {
                                EntryState::NegativeNoCandidate
                                | EntryState::NegativePartialEof => EntryState::NegativeNoCandidate,
                                EntryState::PendingWaitingCompletion => {
                                    EntryState::PendingWaitingCompletion
                                }
                                EntryState::ReadyCandidate => EntryState::ReadyCandidate,
                            }
                        }
                        Mutation::SameLengthVersion => {
                            let mut file = OpenOptions::new()
                                .read(true)
                                .write(true)
                                .open(&path)
                                .unwrap();
                            file.seek(SeekFrom::Start(0)).unwrap();
                            file.write_all(b"X").unwrap();
                            file.sync_all().unwrap();
                            file.set_times(
                                FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)),
                            )
                            .unwrap();
                            state
                        }
                        Mutation::Truncate => {
                            OpenOptions::new()
                                .write(true)
                                .open(&path)
                                .unwrap()
                                .set_len(prefix.len() as u64)
                                .unwrap();
                            EntryState::NegativeNoCandidate
                        }
                        Mutation::Replace => {
                            let replacement = temp.0.join("replacement.jsonl");
                            fs::write(&replacement, b"replacement\n").unwrap();
                            fs::rename(&replacement, &path).unwrap();
                            EntryState::NegativeNoCandidate
                        }
                        Mutation::AnchorMismatch => {
                            let mut file = OpenOptions::new()
                                .read(true)
                                .write(true)
                                .open(&path)
                                .unwrap();
                            file.seek(SeekFrom::Start(old_len - 1)).unwrap();
                            file.write_all(b"x").unwrap();
                            file.sync_all().unwrap();
                            drop(file);
                            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
                            file.write_all(b"\nnoise\n").unwrap();
                            match state {
                                EntryState::NegativeNoCandidate
                                | EntryState::NegativePartialEof => EntryState::NegativeNoCandidate,
                                EntryState::PendingWaitingCompletion => {
                                    EntryState::PendingWaitingCompletion
                                }
                                EntryState::ReadyCandidate => EntryState::ReadyCandidate,
                            }
                        }
                    };

                let after_len = fs::metadata(&path).unwrap().len();
                let outcome = read_cached_prompt(&path, &identity, configured);
                assert_eq!(
                    cached_entry_state(&identity),
                    Some(expected_state),
                    "state/mutation cell {state:?}/{mutation:?}"
                );
                assert_eq!(
                    outcome.suffix.is_some(),
                    expected_state == EntryState::ReadyCandidate,
                    "candidate states remain distinct from negative and pending cells"
                );
                let expected_bytes = match mutation {
                    Mutation::Append => after_len - old_len + configured.cache_overlap_bytes,
                    Mutation::SameLengthVersion
                    | Mutation::Truncate
                    | Mutation::Replace
                    | Mutation::AnchorMismatch => after_len,
                };
                assert_eq!(
                    outcome.bytes_read,
                    expected_bytes,
                    "state/mutation cell {state:?}/{mutation:?} must count delta, anchors, and any full rescan"
                );
                negative_cache()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .forget(&identity);
            }
        }
    }
}
