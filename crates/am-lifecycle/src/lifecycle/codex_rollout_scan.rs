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

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct NegativeKey {
    identity: CacheIdentity,
    file_len: u64,
}

struct NegativeEntry {
    key: NegativeKey,
    snapshot: FileSnapshot,
    scanned_at: Instant,
}

#[derive(Default)]
struct NegativeCache(Vec<NegativeEntry>);

#[derive(Clone, Copy)]
enum CachePlan {
    Hit,
    ScanFrom(u64),
}

impl NegativeCache {
    fn plan(&mut self, identity: &CacheIdentity, snapshot: FileSnapshot, now: Instant, limits: ScanLimits) -> CachePlan {
        self.expire(now, limits.cache_ttl);

        // A new run supersedes every negative result retained for this bot.
        if self.0.iter().any(|e| e.key.identity.bot_id == identity.bot_id && e.key.identity.run_id != identity.run_id) {
            self.0.retain(|e| e.key.identity.bot_id != identity.bot_id);
        }

        let same_len_snapshot = self.0.iter()
            .find(|e| e.key.identity == *identity && e.key.file_len == snapshot.len)
            .map(|entry| entry.snapshot);
        if let Some(cached_snapshot) = same_len_snapshot {
            if cached_snapshot.same_version(&snapshot) {
                return CachePlan::Hit;
            }
            // Same length with changed metadata means an overwrite or replacement. The old
            // complete scan is no longer a valid incremental-scan baseline.
            self.forget(identity);
            return CachePlan::ScanFrom(0);
        }

        // Without a reliable file instance, neither a negative hit nor an incremental range
        // can safely be reused. Unix records use dev/inode; other platforms fail closed here.
        let Some(instance) = snapshot.instance else {
            self.forget(identity);
            return CachePlan::ScanFrom(0);
        };

        let previous = self.0.iter()
            .filter(|e| e.key.identity == *identity)
            .max_by_key(|e| e.key.file_len)
            .map(|entry| (entry.key.file_len, entry.snapshot.instance));
        match previous {
            Some((file_len, previous_instance)) if snapshot.len > file_len && previous_instance == Some(instance) => {
                CachePlan::ScanFrom(file_len.saturating_sub(limits.cache_overlap_bytes))
            }
            Some(_) => {
                // Truncation or a different file instance invalidates the prior byte ranges.
                self.forget(identity);
                CachePlan::ScanFrom(0)
            }
            None => CachePlan::ScanFrom(0),
        }
    }

    fn remember(&mut self, identity: CacheIdentity, snapshot: FileSnapshot, now: Instant, limits: ScanLimits) {
        self.expire(now, limits.cache_ttl);
        self.0.retain(|e| e.key.identity != identity);
        if limits.cache_cap == 0 || snapshot.instance.is_none() {
            return;
        }
        if self.0.len() >= limits.cache_cap {
            if let Some(oldest) = self.0.iter().enumerate().min_by_key(|(_, e)| e.scanned_at).map(|(i, _)| i) {
                self.0.swap_remove(oldest);
            }
        }
        self.0.push(NegativeEntry {
            key: NegativeKey { identity, file_len: snapshot.len }, snapshot, scanned_at: now,
        });
    }

    fn forget(&mut self, identity: &CacheIdentity) {
        self.0.retain(|e| e.key.identity != *identity);
    }

    fn expire(&mut self, now: Instant, ttl: Duration) {
        self.0.retain(|e| now.saturating_duration_since(e.scanned_at) <= ttl);
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileInstance {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    #[cfg(test)]
    bytes_read: u64,
    #[cfg(test)]
    chunks_read: usize,
}

impl ReadOutcome {
    fn none(_bytes_read: u64) -> Self {
        Self {
            suffix: None,
            #[cfg(test)]
            bytes_read: _bytes_read,
            #[cfg(test)]
            chunks_read: 0,
        }
    }
}

fn scan_from_tail<R, F>(
    reader: &mut R,
    file_len: u64,
    floor: u64,
    limits: ScanLimits,
    mut is_candidate: F,
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
                suffix: None, candidate_offset: None, bytes_scanned, chunks_read: chunks.len(),
                complete: false, stop: StopReason::ScanLimit,
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
                            suffix: Some(suffix), candidate_offset: Some(candidate_offset), bytes_scanned,
                            chunks_read: chunks.len(), complete: false, stop: StopReason::Candidate,
                        });
                    }
                    line_reversed.clear();
                }
            } else {
                if line_reversed.len() >= limits.max_line_bytes {
                    return Ok(ScanResult {
                        suffix: None, candidate_offset: None, bytes_scanned, chunks_read: chunks.len(),
                        complete: false, stop: StopReason::LineTooLong,
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
                suffix: Some(suffix), candidate_offset: Some(0), bytes_scanned, chunks_read: chunks.len(),
                complete: false, stop: StopReason::Candidate,
            });
        }
    }

    let scan_limit_reached = cursor > floor && bytes_scanned >= limits.max_scan_bytes;
    Ok(ScanResult {
        suffix: None, candidate_offset: None, bytes_scanned, chunks_read: chunks.len(),
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
    is_candidate: F,
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
    is_candidate: F,
) -> Result<ScanResult, ScanFailure>
where
    F: FnMut(&str) -> bool,
{
    let floor = match plan {
        CachePlan::Hit => {
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
                complete: true,
                stop: StopReason::CompleteNoCandidate,
            });
        }
        CachePlan::ScanFrom(floor) => floor,
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
    read_suffix(path, identity, |line| super::codex_rollout::is_task_complete_candidate(line, &since))
}

fn read_suffix<F>(path: &Path, identity: CacheIdentity, is_candidate: F) -> Option<String>
where
    F: FnMut(&str) -> bool,
{
    read_suffix_with(path, identity, LIMITS, is_candidate).suffix
}

fn read_suffix_with<F>(path: &Path, identity: CacheIdentity, limits: ScanLimits, is_candidate: F) -> ReadOutcome
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
    let cache_hit = matches!(plan, CachePlan::Hit);
    let result = match scan_open_file(path, &mut reader, before, plan, limits, is_candidate) {
        Ok(result) => result,
        Err(failure) => {
            negative_cache().lock().unwrap_or_else(|e| e.into_inner()).forget(&identity);
            tracing::warn!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = ?failure.file_bytes,
                bytes_scanned = failure.bytes_scanned, max_scan_bytes = limits.max_scan_bytes,
                max_line_bytes = limits.max_line_bytes, elapsed_ms = started.elapsed().as_millis() as u64,
                error_kind = ?failure.kind, "codex rollout scan failed closed");
            return ReadOutcome::none(failure.bytes_scanned);
        }
    };

    if cache_hit {
        tracing::debug!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = before.len,
            bytes_scanned = reader.bytes_read, elapsed_ms = started.elapsed().as_millis() as u64,
            "codex rollout negative scan cache hit");
        return ReadOutcome::none(reader.bytes_read);
    }

    if result.stop == StopReason::CompleteNoCandidate {
        negative_cache().lock().unwrap_or_else(|e| e.into_inner())
            .remember(identity.clone(), before, now, limits);
    }

    match result.stop {
        StopReason::ScanLimit | StopReason::LineTooLong => {
            tracing::warn!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = before.len,
                bytes_scanned = result.bytes_scanned, candidate_offset = result.candidate_offset,
                max_scan_bytes = limits.max_scan_bytes, max_line_bytes = limits.max_line_bytes,
                elapsed_ms = started.elapsed().as_millis() as u64, reason = ?result.stop,
                "codex rollout scan failed closed");
            // A capped scan is incomplete, so it must never become a negative-cache entry.
            return ReadOutcome::none(result.bytes_scanned);
        }
        StopReason::CompleteNoCandidate => {}
        StopReason::Candidate => {}
    }
    tracing::debug!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = before.len,
        bytes_scanned = result.bytes_scanned, candidate_offset = result.candidate_offset,
        chunks_read = result.chunks_read, elapsed_ms = started.elapsed().as_millis() as u64,
        negative_cached = result.complete && result.suffix.is_none(), "codex rollout scan complete");
    ReadOutcome {
        suffix: result.suffix,
        #[cfg(test)]
        bytes_read: reader.bytes_read,
        #[cfg(test)]
        chunks_read: result.chunks_read,
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
        path: &Path, identity: &CacheIdentity, cache: &mut NegativeCache, limits: ScanLimits, now: Instant,
        candidate: F,
    ) -> (ScanResult, u64)
    where
        F: FnMut(&str) -> bool,
    {
        let file = File::open(path).unwrap();
        let mut reader = CountingReader::new(file);
        let before = current_snapshot(path, reader.get_ref()).unwrap();
        let plan = cache.plan(identity, before, now, limits);
        let cache_hit = matches!(plan, CachePlan::Hit);
        let result = scan_open_file(path, &mut reader, before, plan, limits, candidate).unwrap();
        if !cache_hit && result.stop == StopReason::CompleteNoCandidate {
            cache.remember(identity.clone(), before, now, limits);
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
    fn macos_local_codex_rollout_candidate_inside_last_four_mib_reads_one_chunk() {
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
        assert_eq!(outcome.chunks_read, 1);
        assert_eq!(outcome.bytes_read, CODEX_ROLLOUT_SCAN_CHUNK_BYTES);
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
        let result = scan_counted(&mut reader, 64, 0, limits(16, 64, 32, 33), None, |_| false);
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
        assert_eq!(second.bytes_read, appended.len() as u64 + configured.cache_overlap_bytes);
        assert_eq!(second.bytes_read, fs::metadata(&path).unwrap().len() - old_len + configured.cache_overlap_bytes);
    }
}
