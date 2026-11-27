//! 外部編譯（`remote-cargo` helper）的即時人數，給 `/api/build-slots` 與狀態列的「建置 N」晶片。
//!
//! 兩個來源：
//! - **名額**：守門在建置主機上 flock 住的 `<remote_root>/.slots/<n>`（正在跑）與 `.queue/<token>`（排隊）。
//!   [`slot_counts`] 在那台跑一段只數鎖、不拿鎖的腳本（本機執行器在本機跑，其他走一次 ssh）。
//! - **明細**：helper 在本機資料目錄的 `remote-cargo-runs/<call_id>.json` 登錄這次呼叫（誰、跑什麼、從何時、排到哪台），
//!   整段持有該檔的 flock；[`list`] 讀得到鎖沒被持有的檔就是殘檔，順手刪掉。

use crate::config::BuildRemoteCfg;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const RUNS_DIR: &str = "remote-cargo-runs";
const PURPOSE_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteRun {
    pub call_id: String,
    pub bot_id: Option<String>,
    pub purpose: String,
    pub since: String,
    /// 排到的那台（`target_label`）；`None`＝還在挑主機／排名額。
    pub target: Option<String>,
}

/// helper 這次呼叫的登錄；drop 時刪檔、放鎖。
pub struct RunGuard {
    file: File,
    path: PathBuf,
    run: RemoteRun,
}

fn flock(f: &File, op: libc::c_int) -> bool {
    unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) == 0 }
}

impl RunGuard {
    /// 登錄失敗只是看不到明細，不影響編譯：回 `None`。
    pub fn start(data_dir: &Path, call_id: &str, args: &[String]) -> Option<Self> {
        if call_id.is_empty() || !call_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let dir = data_dir.join(RUNS_DIR);
        std::fs::create_dir_all(&dir).ok()?;
        let tmp = dir.join(format!("{call_id}.tmp"));
        let path = dir.join(format!("{call_id}.json"));
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).mode(0o600).open(&tmp).ok()?;
        if !flock(&file, libc::LOCK_EX) {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        let run = RemoteRun {
            call_id: call_id.to_string(),
            bot_id: std::env::var("AM_BOT_ID").ok().filter(|s| !s.is_empty()),
            purpose: args.join(" ").chars().take(PURPOSE_CHARS).collect(),
            since: crate::db::now(),
            target: None,
        };
        let g = RunGuard { file, path, run };
        g.write();
        std::fs::rename(&tmp, &g.path).ok()?;
        Some(g)
    }

    /// 拿到租約、確定在哪台跑了。
    pub fn running_on(&mut self, target: &str) {
        self.run.target = Some(target.to_string());
        self.write();
    }

    fn write(&self) {
        if let Ok(bytes) = serde_json::to_vec(&self.run) {
            if self.file.write_all_at(&bytes, 0).is_ok() {
                let _ = self.file.set_len(bytes.len() as u64);
            }
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 現在登錄著的呼叫（照 `since` 排）。鎖沒被持有的是 helper 沒走到 drop 就死掉留下的，刪掉不列。
pub fn list(data_dir: &Path) -> Vec<RemoteRun> {
    let Ok(entries) = std::fs::read_dir(data_dir.join(RUNS_DIR)) else { return Vec::new() };
    let mut out: Vec<RemoteRun> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let path = e.path();
            let file = File::open(&path).ok()?;
            if flock(&file, libc::LOCK_SH) {
                let _ = std::fs::remove_file(&path);
                return None;
            }
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut &file, &mut buf).ok()?;
            serde_json::from_str(&buf).ok()
        })
        .collect();
    out.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.call_id.cmp(&b.call_id)));
    out
}

/// 數名額的腳本：只試鎖（`flock -n … true` 拿到立刻放），不建檔、不刪檔。輸出 `SLOTS\t<跑>\t<排>`。
const COUNT_SH: &str = r#"root=@ROOT@
a=0; w=0
for f in "$root"/.slots/*; do
  [ -f "$f" ] || continue
  flock -n "$f" true 2>/dev/null || a=$((a + 1))
done
for f in "$root"/.queue/*; do
  [ -f "$f" ] || continue
  case "$f" in *.tmp.*) continue ;; esac
  flock -n "$f" true 2>/dev/null || w=$((w + 1))
done
printf 'SLOTS\t%s\t%s\n' "$a" "$w"
"#;

fn count_script(remote: &BuildRemoteCfg) -> String {
    let root = match remote.remote_root.trim().trim_end_matches('/') {
        "" => crate::config::default_remote_build_root(),
        r => r.to_string(),
    };
    COUNT_SH.replace("@ROOT@", &crate::remote_cargo::sh_quote(&root))
}

pub fn parse_counts(stdout: &str) -> Option<(u64, u64)> {
    let line = stdout.lines().find_map(|l| l.strip_prefix("SLOTS\t"))?;
    let (a, w) = line.split_once('\t')?;
    Some((a.trim().parse().ok()?, w.trim().parse().ok()?))
}

/// 這台建置主機正在跑與排隊的守門數。阻塞、最多 `limit`；連不上或讀不懂回 `Err`。
pub fn slot_counts(remote: &BuildRemoteCfg, data_dir: &Path, limit: Duration) -> anyhow::Result<(u64, u64)> {
    let cmd = crate::remote_cargo::host_shell(remote, data_dir, &count_script(remote))?;
    let out = crate::remote_cargo::output_with_timeout(cmd, limit)?;
    if !out.status.success() {
        anyhow::bail!("數名額失敗：{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    parse_counts(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| anyhow::anyhow!("數名額的輸出看不懂"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = crate::testing::track(std::env::temp_dir().join(format!("am-runs-{}", crate::db::ulid())));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_live_run_is_listed_and_gone_after_drop() {
        let d = tmp();
        let mut g = RunGuard::start(d.as_path(), "01ABC", &["test".into(), "-p".into(), "x".into()]).unwrap();
        let runs = list(d.as_path());
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].purpose, "test -p x");
        assert_eq!(runs[0].target, None);
        g.running_on("ubuntu@h:22");
        assert_eq!(list(d.as_path())[0].target.as_deref(), Some("ubuntu@h:22"));
        drop(g);
        assert!(list(d.as_path()).is_empty());
    }

    #[test]
    fn an_unlocked_leftover_is_removed() {
        let d = tmp();
        let dir = d.as_path().join(RUNS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("01DEAD.json");
        std::fs::write(&stale, r#"{"call_id":"01DEAD","bot_id":null,"purpose":"build","since":"x","target":null}"#).unwrap();
        assert!(list(d.as_path()).is_empty());
        assert!(!stale.exists());
    }

    #[test]
    fn a_bad_call_id_does_not_register() {
        let d = tmp();
        assert!(RunGuard::start(d.as_path(), "../x", &[]).is_none());
    }

    #[test]
    fn counts_parse_from_the_script_line() {
        assert_eq!(parse_counts("noise\nSLOTS\t2\t1\n"), Some((2, 1)));
        assert_eq!(parse_counts("SLOTS\tx\t1\n"), None);
        assert_eq!(parse_counts(""), None);
    }

    /// 真的跑一次腳本：被鎖住的名額與號碼牌才算，沒被鎖的不算、`.tmp.` 不算。
    #[cfg(target_os = "linux")]
    #[test]
    fn the_count_script_counts_only_held_locks() {
        let d = tmp();
        let root = d.as_path().join("pool");
        std::fs::create_dir_all(root.join(".slots")).unwrap();
        std::fs::create_dir_all(root.join(".queue")).unwrap();
        let open = |p: PathBuf| OpenOptions::new().create(true).append(true).open(p).unwrap();
        let s1 = open(root.join(".slots/1"));
        let _s2 = open(root.join(".slots/2"));
        let q = open(root.join(".queue/tok"));
        let qt = open(root.join(".queue/tok.tmp.1"));
        assert!(flock(&s1, libc::LOCK_EX) && flock(&q, libc::LOCK_EX) && flock(&qt, libc::LOCK_EX));
        let script = COUNT_SH.replace("@ROOT@", &crate::remote_cargo::sh_quote(&root.display().to_string()));
        let out = std::process::Command::new("sh").arg("-c").arg(script).output().unwrap();
        assert_eq!(parse_counts(&String::from_utf8_lossy(&out.stdout)), Some((1, 1)));
    }
}
