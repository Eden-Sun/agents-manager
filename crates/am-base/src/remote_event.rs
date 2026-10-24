//! remote-cargo 等待與終態逐事件記錄（issue #1960）。
//!
//! 在每次遠端編譯呼叫選定最終 lease 或確定 fallback 本機時，立即將其決策與等待時間 append 寫入資料目錄的
//! `remote-cargo-events.jsonl`。
//! 採用獨立鎖檔、非阻塞 flock (LOCK_NB)，遇鎖忙或 I/O 錯誤僅輸出警告並略過，不阻斷 build 流程
//! （同步檔案系統 I/O 不宣稱零延遲）。
//! 檔案具備單行限長（<=512 bytes）與容量邊界輪替（主檔滿 5 MiB 輪替至 .1，總容量 <= 10 MiB），
//! 所有檔案與鎖檔強制 POSIX 0600。

use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub const EVENT_FILE: &str = "remote-cargo-events.jsonl";
pub const LOCK_FILE: &str = "remote-cargo-events.lock";
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_LINE_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RemoteCargoEvent {
    pub ts: String,
    pub call_id: String,
    pub bot_id: Option<String>,
    pub host: String,
    pub phase: &'static str,
    pub outcome: &'static str,
    pub shared_wait_secs: Option<u64>,
    pub slot_wait_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecordOutcome {
    Recorded,
    LockBusy,
    LineTooLong,
    IoError,
}

struct LockGuard(File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// 嘗試寫入事件。任何非 Recorded 結果均印出 warn 並返回，不影響建置。
pub fn record(data_dir: &Path, ev: &RemoteCargoEvent) -> RecordOutcome {
    match record_inner(data_dir, ev) {
        Ok(outcome) => {
            if outcome == RecordOutcome::LockBusy {
                eprintln!("agents-manager: remote-cargo 事件鎖忙，略過記錄（承認漏事件，不影響建置）");
            }
            outcome
        }
        Err(e) => {
            eprintln!("agents-manager: 記錄 remote-cargo 事件失敗（{e}），略過（不影響建置）");
            RecordOutcome::IoError
        }
    }
}

fn record_inner(data_dir: &Path, ev: &RemoteCargoEvent) -> std::io::Result<RecordOutcome> {
    let mut line = serde_json::to_vec(ev).map_err(std::io::Error::other)?;
    line.push(b'\n');
    if line.len() > MAX_LINE_BYTES {
        eprintln!("agents-manager: remote-cargo 事件行過長（{} bytes），略過記錄", line.len());
        return Ok(RecordOutcome::LineTooLong);
    }

    let lock_path = data_dir.join(LOCK_FILE);
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .open(&lock_path)?;

    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))?;

    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) || err.raw_os_error() == Some(libc::EAGAIN) {
            return Ok(RecordOutcome::LockBusy);
        }
        return Err(err);
    }
    let _guard = LockGuard(lock_file);

    // 取得鎖後，執行大小檢查與必要輪替
    let file_path = data_dir.join(EVENT_FILE);
    let cur_len = match std::fs::metadata(&file_path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e),
    };

    if cur_len + (line.len() as u64) > MAX_BYTES {
        let rot_path = data_dir.join(format!("{EVENT_FILE}.1"));
        if file_path.exists() {
            std::fs::rename(&file_path, &rot_path)?;
            std::fs::set_permissions(&rot_path, std::fs::Permissions::from_mode(0o600))?;
        }
    }

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&file_path)?;
    std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o600))?;
    f.write_all(&line)?;
    f.flush()?;
    Ok(RecordOutcome::Recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScratchDir(std::path::PathBuf);
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch_dir(tag: &str) -> ScratchDir {
        let d = std::env::temp_dir().join(format!("am-remote-event-{tag}-{}", crate::db::ulid()));
        std::fs::create_dir_all(&d).unwrap();
        ScratchDir(d)
    }

    #[test]
    fn event_serialization_whitelist_and_null_formatting() {
        let ev = RemoteCargoEvent {
            ts: "2026-10-24T13:00:00.000Z".into(),
            call_id: "01JABCDEF0123456789ABCDEF0".into(),
            bot_id: None,
            host: "build-box".into(),
            phase: "final",
            outcome: "local_fallback",
            shared_wait_secs: Some(900),
            slot_wait_secs: None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"bot_id\":null"), "bot_id should serialize to null when None: {s}");
        assert!(s.contains("\"slot_wait_secs\":null"), "slot_wait_secs should serialize to null when None: {s}");
        assert!(s.contains("\"shared_wait_secs\":900"), "{s}");
        assert!(s.contains("\"host\":\"build-box\""), "{s}");
        assert!(!s.contains("user"), "{s}");
        assert!(!s.contains("token"), "{s}");
        assert!(s.len() < MAX_LINE_BYTES);
    }

    #[test]
    fn event_recording_and_rotation_with_permissions() {
        let dir = scratch_dir("rot");
        let data_dir = dir.0.as_path();
        let ev = RemoteCargoEvent {
            ts: "2026-10-24T13:00:00.000Z".into(),
            call_id: "test-call".into(),
            bot_id: Some("bot-1".into()),
            host: "h1".into(),
            phase: "final",
            outcome: "shared",
            shared_wait_secs: Some(15),
            slot_wait_secs: Some(0),
        };
        let out = record(data_dir, &ev);
        assert_eq!(out, RecordOutcome::Recorded);

        let file_path = data_dir.join(EVENT_FILE);
        let lock_path = data_dir.join(LOCK_FILE);
        assert!(file_path.exists());
        assert!(lock_path.exists());

        let meta = std::fs::metadata(&file_path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let lmeta = std::fs::metadata(&lock_path).unwrap();
        assert_eq!(lmeta.permissions().mode() & 0o777, 0o600);

        // 模擬檔案寫滿觸發輪替
        // 寫入 5 MiB 的資料
        let big = vec![b'a'; 5 * 1024 * 1024];
        std::fs::write(&file_path, big).unwrap();

        // 再次寫入一筆
        let out2 = record(data_dir, &ev);
        assert_eq!(out2, RecordOutcome::Recorded);

        let rot_path = data_dir.join(format!("{EVENT_FILE}.1"));
        assert!(rot_path.exists(), "rotated file must exist");
        let rmeta = std::fs::metadata(&rot_path).unwrap();
        assert_eq!(rmeta.permissions().mode() & 0o777, 0o600);

        // 新檔應該只有剛寫進去的那一行
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content.lines().count(), 1);
    }

    #[test]
    fn lock_busy_skips_and_returns_lock_busy() {
        let dir = scratch_dir("busy");
        let data_dir = dir.0.as_path();
        let lock_path = data_dir.join(LOCK_FILE);

        let lf = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(&lock_path)
            .unwrap();

        let rc = unsafe { libc::flock(lf.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0);

        let ev = RemoteCargoEvent {
            ts: "2026-10-24T13:00:00.000Z".into(),
            call_id: "test-call".into(),
            bot_id: None,
            host: "h1".into(),
            phase: "final",
            outcome: "shared",
            shared_wait_secs: None,
            slot_wait_secs: None,
        };

        // 在被鎖住的情境下寫入，必須立即返回 LockBusy
        let out = record(data_dir, &ev);
        assert_eq!(out, RecordOutcome::LockBusy);

        unsafe { libc::flock(lf.as_raw_fd(), libc::LOCK_UN) };
    }

    fn test_event(call_id: String) -> RemoteCargoEvent {
        RemoteCargoEvent {
            ts: "2026-10-24T13:00:00.000Z".into(), call_id,
            bot_id: None, host: "fixture-host".into(), phase: "final", outcome: "shared",
            shared_wait_secs: None, slot_wait_secs: Some(0),
        }
    }

    #[test]
    fn multi_process_writer_helper() {
        use std::io::BufRead;
        let Ok(dir) = std::env::var("REMOTE_EVENT_TEST_DIR") else { return };
        let worker = std::env::var("REMOTE_EVENT_TEST_WORKER").unwrap();
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "READY").unwrap();
        stdout.flush().unwrap();
        let mut go = String::new();
        assert_ne!(std::io::stdin().lock().read_line(&mut go).unwrap(), 0);
        let mut recorded = 0;
        let mut busy = 0;
        for i in 0..100 {
            match record(Path::new(&dir), &test_event(format!("worker-{worker}-{i}"))) {
                RecordOutcome::Recorded => recorded += 1,
                RecordOutcome::LockBusy => busy += 1,
                other => panic!("unexpected writer result: {other:?}"),
            }
        }
        writeln!(stdout, "COUNTS {recorded} {busy}").unwrap();
        stdout.flush().unwrap();
    }

    #[test]
    fn multi_process_concurrent_writers_and_rotation_boundary() {
        use std::io::{BufRead, Read};
        use std::process::{Child, Command, Stdio};
        struct OwnedChild(Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        let dir = scratch_dir("multi");
        let data_dir = dir.0.as_path();
        let active = data_dir.join(EVENT_FILE);
        let rotated = data_dir.join(format!("{EVENT_FILE}.1"));
        let mut preload = Vec::new();
        let preload_count = MAX_BYTES as usize / 512;
        for i in 0..preload_count {
            let mut line = serde_json::to_vec(&test_event(format!("preload-{i}"))).unwrap();
            let size = if i + 1 == preload_count { 384 } else { 512 };
            assert!(line.len() < size);
            line.resize(size - 1, b' ');
            line.push(b'\n');
            preload.extend_from_slice(&line);
        }
        assert_eq!(preload.len() as u64, MAX_BYTES - 128);
        std::fs::write(&active, preload).unwrap();
        std::fs::set_permissions(&active, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Every actual event exceeds the remaining 128 bytes, so the first recorded
        // event must rotate. All 400 attempts together cannot trigger a second rotation.
        assert!(serde_json::to_vec(&test_event("worker-0-0".into())).unwrap().len() > 128);
        let mut children = Vec::new();
        for worker in 0..4 {
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "remote_event::tests::multi_process_writer_helper", "--nocapture"])
                .env("REMOTE_EVENT_TEST_DIR", data_dir)
                .env("REMOTE_EVENT_TEST_WORKER", worker.to_string())
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
                .spawn().unwrap();
            let mut child = OwnedChild(child);
            let mut output = std::io::BufReader::new(child.0.stdout.take().unwrap());
            loop {
                let mut line = String::new();
                assert_ne!(output.read_line(&mut line).unwrap(), 0, "helper exited before READY");
                if line.trim() == "READY" { break; }
            }
            children.push((child, output));
        }
        for (child, _) in &mut children {
            child.0.stdin.as_mut().unwrap().write_all(b"GO\n").unwrap();
            child.0.stdin.take();
        }
        let mut recorded = 0usize;
        let mut busy = 0usize;
        for (mut child, mut output) in children {
            let mut rest = String::new();
            output.read_to_string(&mut rest).unwrap();
            assert!(child.0.wait().unwrap().success(), "helper failed: {rest}");
            let counts: Vec<_> = rest.lines().filter_map(|l| l.strip_prefix("COUNTS ")).collect();
            assert_eq!(counts.len(), 1, "missing or duplicate helper result: {rest}");
            let numbers: Vec<usize> = counts[0].split_whitespace().map(|s| s.parse().unwrap()).collect();
            assert_eq!(numbers.len(), 2);
            assert_eq!(numbers[0] + numbers[1], 100);
            recorded += numbers[0]; busy += numbers[1];
        }
        assert_eq!(recorded + busy, 400);
        assert!(recorded > 0);
        assert!(rotated.exists());
        let mut ids = std::collections::HashSet::new();
        let mut total_bytes = 0;
        for path in [&active, &rotated] {
            let meta = std::fs::metadata(path).unwrap();
            assert!(meta.len() <= MAX_BYTES);
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            total_bytes += meta.len();
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(bytes.last(), Some(&b'\n'));
            for line in bytes.split_inclusive(|b| *b == b'\n') {
                assert!(line.len() <= MAX_LINE_BYTES);
                let value: serde_json::Value = serde_json::from_slice(line).unwrap();
                let id = value["call_id"].as_str().unwrap().to_owned();
                assert!(ids.insert(id), "duplicate call_id");
            }
        }
        assert!(total_bytes <= 2 * MAX_BYTES);
        assert_eq!(ids.len(), preload_count + recorded);
        assert_eq!(std::fs::metadata(data_dir.join(LOCK_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
