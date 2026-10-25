//! Bounded, tail-first reads for Codex rollout reply recovery.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
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
    scanned_at: Instant,
}

#[derive(Default)]
struct NegativeCache(Vec<NegativeEntry>);

enum CachePlan {
    Hit,
    ScanFrom(u64),
}

impl NegativeCache {
    fn plan(&mut self, identity: &CacheIdentity, file_len: u64, now: Instant, limits: ScanLimits) -> CachePlan {
        self.expire(now, limits.cache_ttl);

        // A new run supersedes every negative result retained for this bot.
        if self.0.iter().any(|e| e.key.identity.bot_id == identity.bot_id && e.key.identity.run_id != identity.run_id) {
            self.0.retain(|e| e.key.identity.bot_id != identity.bot_id);
        }

        if self.0.iter().any(|e| e.key.identity == *identity && e.key.file_len == file_len) {
            return CachePlan::Hit;
        }

        // A shorter file means truncation/replacement; prior ranges no longer prove this file.
        self.0.retain(|e| !(e.key.identity == *identity && e.key.file_len > file_len));
        let previous_len = self.0.iter()
            .filter(|e| e.key.identity == *identity && e.key.file_len < file_len)
            .map(|e| e.key.file_len)
            .max();
        CachePlan::ScanFrom(previous_len.map_or(0, |n| n.saturating_sub(limits.cache_overlap_bytes)))
    }

    fn remember(&mut self, identity: CacheIdentity, file_len: u64, now: Instant, limits: ScanLimits) {
        self.expire(now, limits.cache_ttl);
        self.0.retain(|e| e.key.identity != identity);
        if limits.cache_cap == 0 {
            return;
        }
        if self.0.len() >= limits.cache_cap {
            if let Some(oldest) = self.0.iter().enumerate().min_by_key(|(_, e)| e.scanned_at).map(|(i, _)| i) {
                self.0.swap_remove(oldest);
            }
        }
        self.0.push(NegativeEntry { key: NegativeKey { identity, file_len }, scanned_at: now });
    }

    fn expire(&mut self, now: Instant, ttl: Duration) {
        self.0.retain(|e| now.saturating_duration_since(e.scanned_at) <= ttl);
    }
}

fn negative_cache() -> &'static Mutex<NegativeCache> {
    static CACHE: OnceLock<Mutex<NegativeCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

struct Chunk {
    start: u64,
    bytes: Vec<u8>,
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
    let started = Instant::now();
    let mut file = crate::transcript_read::open_regular(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let now = Instant::now();
    let plan = {
        let mut cache = negative_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache.plan(&identity, file_len, now, LIMITS)
    };
    let floor = match plan {
        CachePlan::Hit => {
            tracing::debug!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = file_len,
                bytes_scanned = 0_u64, elapsed_ms = started.elapsed().as_millis() as u64,
                "codex rollout negative scan cache hit");
            return None;
        }
        CachePlan::ScanFrom(floor) => floor,
    };

    let result = match scan_from_tail(&mut file, file_len, floor, LIMITS, is_candidate) {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = file_len,
                bytes_scanned = 0_u64, max_scan_bytes = LIMITS.max_scan_bytes,
                max_line_bytes = LIMITS.max_line_bytes, elapsed_ms = started.elapsed().as_millis() as u64,
                error_kind = ?error.kind(), "codex rollout scan failed closed");
            return None;
        }
    };

    if file.metadata().ok()?.len() != file_len {
        tracing::warn!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = file_len,
            bytes_scanned = result.bytes_scanned, max_scan_bytes = LIMITS.max_scan_bytes,
            max_line_bytes = LIMITS.max_line_bytes, elapsed_ms = started.elapsed().as_millis() as u64,
            "codex rollout changed during scan; failing closed");
        return None;
    }

    match result.stop {
        StopReason::ScanLimit | StopReason::LineTooLong => {
            tracing::warn!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = file_len,
                bytes_scanned = result.bytes_scanned, candidate_offset = result.candidate_offset,
                max_scan_bytes = LIMITS.max_scan_bytes, max_line_bytes = LIMITS.max_line_bytes,
                elapsed_ms = started.elapsed().as_millis() as u64, reason = ?result.stop,
                "codex rollout scan failed closed");
            // A capped scan is incomplete, so it must never become a negative-cache entry.
            return None;
        }
        StopReason::CompleteNoCandidate => {
            negative_cache().lock().unwrap_or_else(|e| e.into_inner())
                .remember(identity.clone(), file_len, now, LIMITS);
        }
        StopReason::Candidate => {}
    }
    tracing::debug!(bot = %identity.bot_id, run = %identity.run_id, file_bytes = file_len,
        bytes_scanned = result.bytes_scanned, candidate_offset = result.candidate_offset,
        chunks_read = result.chunks_read, elapsed_ms = started.elapsed().as_millis() as u64,
        negative_cached = result.complete && result.suffix.is_none(), "codex rollout scan complete");
    result.suffix
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;

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

    struct CountedReader {
        file: File,
        bytes_read: u64,
    }

    impl Read for CountedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.file.read(buf)?;
            self.bytes_read += n as u64;
            Ok(n)
        }
    }

    impl Seek for CountedReader {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> { self.file.seek(pos) }
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

    fn scan_file<F>(
        path: &Path, identity: &CacheIdentity, cache: &mut NegativeCache, limits: ScanLimits, now: Instant,
        candidate: F,
    ) -> (ScanResult, u64)
    where
        F: FnMut(&str) -> bool,
    {
        let file = File::open(path).unwrap();
        let file_len = file.metadata().unwrap().len();
        let mut reader = CountedReader { file, bytes_read: 0 };
        let result = scan_cached(&mut reader, file_len, identity, cache, now, limits, candidate);
        let bytes_read = reader.bytes_read;
        (result, bytes_read)
    }

    fn scan_cached<R, F>(
        reader: &mut R, file_len: u64, identity: &CacheIdentity, cache: &mut NegativeCache, now: Instant,
        limits: ScanLimits, candidate: F,
    ) -> ScanResult
    where
        R: Read + Seek,
        F: FnMut(&str) -> bool,
    {
        let floor = match cache.plan(identity, file_len, now, limits) {
            CachePlan::Hit => return ScanResult {
                suffix: None, candidate_offset: None, bytes_scanned: 0, chunks_read: 0,
                complete: true, stop: StopReason::CompleteNoCandidate,
            },
            CachePlan::ScanFrom(floor) => floor,
        };
        let result = scan_from_tail(reader, file_len, floor, limits, candidate).unwrap();
        if result.stop == StopReason::CompleteNoCandidate {
            cache.remember(identity.clone(), file_len, now, limits);
        }
        result
    }

    #[test]
    fn prompt_more_than_four_mib_away_crossing_chunk_boundary_recovers_reply() {
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
    fn candidate_inside_last_four_mib_reads_one_chunk() {
        let temp = TempRollout::new();
        let path = temp.file();
        let log = format!("{}\n{}\n",
            user_msg("2026-10-25T01:00:00Z", "synthetic prompt"),
            task_complete("2026-10-25T01:00:05Z", "synthetic reply"));
        fs::write(&path, &log).unwrap();
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-25T00:59:00Z").unwrap();
        let sent = vec!["synthetic prompt".to_string()];
        let mut cache = NegativeCache::default();
        let (result, bytes_read) = scan_file(&path, &identity(), &mut cache, LIMITS, Instant::now(), |line| {
            super::super::codex_rollout::is_turn_prompt_candidate(line, &sent, &since)
        });
        assert!(result.suffix.is_some());
        assert_eq!(result.chunks_read, 1);
        assert_eq!(bytes_read, log.len() as u64);
    }

    #[test]
    fn same_prompt_before_since_is_not_a_candidate() {
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
    fn user_input_after_task_complete_rejects_both_reply_paths() {
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
    fn scan_limit_fails_closed_without_negative_cache() {
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
    fn oversized_single_line_fails_before_reading_the_whole_line() {
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

    #[test]
    fn complete_negative_scan_is_not_repeated_at_same_file_length() {
        let temp = TempRollout::new();
        let path = temp.file();
        fs::write(&path, b"noise\nnoise\nnoise\n").unwrap();
        let mut cache = NegativeCache::default();
        let now = Instant::now();
        let configured = limits(8, 1024, 32, 33);
        let (first, first_bytes) = scan_file(&path, &identity(), &mut cache, configured, now, |_| false);
        assert_eq!(first.stop, StopReason::CompleteNoCandidate);
        assert_eq!(first_bytes, fs::metadata(&path).unwrap().len());
        let (second, second_bytes) = scan_file(&path, &identity(), &mut cache, configured, now, |_| false);
        assert_eq!(second.stop, StopReason::CompleteNoCandidate);
        assert_eq!(second.chunks_read, 0);
        assert_eq!(second_bytes, 0);
        assert_eq!(second.bytes_scanned, 0);
    }

    #[test]
    fn increased_file_length_reads_only_appended_bytes_and_overlap() {
        let temp = TempRollout::new();
        let path = temp.file();
        fs::write(&path, b"noise\n".repeat(64)).unwrap();
        let mut cache = NegativeCache::default();
        let now = Instant::now();
        let configured = limits(64, 4096, 32, 33);
        let (first, _) = scan_file(&path, &identity(), &mut cache, configured, now, |_| false);
        assert_eq!(first.stop, StopReason::CompleteNoCandidate);
        let old_len = fs::metadata(&path).unwrap().len();
        let appended = b"new-noise\n";
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(appended).unwrap();
        drop(file);
        let (second, bytes_read) = scan_file(&path, &identity(), &mut cache, configured, now, |_| false);
        assert_eq!(second.stop, StopReason::CompleteNoCandidate);
        assert_eq!(bytes_read, appended.len() as u64 + configured.cache_overlap_bytes);
        assert_eq!(bytes_read, fs::metadata(&path).unwrap().len() - old_len + configured.cache_overlap_bytes);
    }
}
