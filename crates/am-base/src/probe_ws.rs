//! daemon 自己開的探測 workspace（grok／claude 額度探測，遠端主機借用那台的 session）。
//!
//! 探測 pane 一啟動，herdr 就對該 session 發 `pane.agent_detected`；全域訂閱把它當成「多半是 bot 剛開的子 pane」
//! 而在兩秒後對整台主機對帳。探測是 daemon 自己輪詢出來的（grok 每 30 秒一次，SPEC §12），於是一台有十幾顆 bot 的
//! 遠端主機每 40 秒被完整對帳一輪——每輪十幾顆 bot × 好幾次 ssh RPC，只為了一個永遠不會是 child 的 pane。
//! 探測開 workspace 時在這裡登記、結束時（`Drop`）撤掉，全域訂閱看到登記過的 workspace 的偵測事件就不排對帳。
//!
//! key 是 `(herdr socket, workspace id)`：workspace id 只在一個 herdr session 內唯一，不同主機／session 會撞號。
//!
//! 同一份登記也是「哪些 workspace 是我開的探測」的唯一依據（#1795，使用者 2026-10-17「名稱不應該被任何的 ref」）：
//! 清殘留與 pane 掃描排除探測都認 `(socket, workspace id)`，不看 workspace 的 label。還沒確認關掉的存在資料目錄的
//! `probe-workspaces.json`（[`init`]），daemon 重啟後才收得到上一輪留下的；資料目錄是每顆 daemon 各自的，
//! 所以共用 session 的主機上也只會清到自己開的。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

type Key = (PathBuf, String);

/// 探測結束（workspace 關掉）之後多留多久：agent 退出的 `pane.agent_detected`（`released: true`）會晚幾秒到。
const GRACE: Duration = Duration::from_secs(30);

/// `None` = 探測還在跑；`Some(t)` = 結束了，`t` 之前的事件還算它的。
fn registry() -> &'static Mutex<HashMap<Key, Option<Instant>>> {
    static R: OnceLock<Mutex<HashMap<Key, Option<Instant>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// 登記了、還沒確認 `workspace.close` 成功的探測 workspace（含開機時從檔案載回的）。
fn unclosed() -> &'static Mutex<HashSet<Key>> {
    static U: OnceLock<Mutex<HashSet<Key>>> = OnceLock::new();
    U.get_or_init(Default::default)
}

static PERSIST: OnceLock<PathBuf> = OnceLock::new();
const FILE: &str = "probe-workspaces.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    socket: String,
    workspace_id: String,
}

fn load_from(path: &Path) -> HashSet<Key> {
    let Ok(raw) = std::fs::read_to_string(path) else { return HashSet::new() };
    let entries: Vec<Entry> = serde_json::from_str(&raw).unwrap_or_default();
    entries.into_iter().map(|e| (PathBuf::from(e.socket), e.workspace_id)).collect()
}

fn save_to(path: &Path, set: &HashSet<Key>) {
    let mut entries: Vec<Entry> = set.iter().map(|(s, w)| Entry { socket: s.to_string_lossy().into_owned(), workspace_id: w.clone() }).collect();
    entries.sort_by(|a, b| (&a.socket, &a.workspace_id).cmp(&(&b.socket, &b.workspace_id)));
    let data = serde_json::to_vec(&entries).unwrap_or_default();
    if let Err(e) = crate::atomic_file::write(path, &data, crate::atomic_file::Mode::Exact(0o600)) {
        tracing::warn!(path = %path.display(), error = %e, "could not persist the probe workspace registry; leftovers from this run may be left behind after a restart");
    }
}

fn persist(set: &HashSet<Key>) {
    if let Some(dir) = PERSIST.get() {
        save_to(&dir.join(FILE), set);
    }
}

/// 開機時呼叫一次：之後的登記／撤銷寫進 `data_dir`，並把上一輪還沒確認關掉的載回來（[`leftovers`] 才收得到）。
pub fn init(data_dir: &Path) {
    if PERSIST.set(data_dir.to_path_buf()).is_ok() {
        let mut u = unclosed().lock().unwrap_or_else(|e| e.into_inner());
        u.extend(load_from(&data_dir.join(FILE)));
    }
}

/// `workspace.close` 成功（或確認它已經不在）之後撤掉：不再算殘留。
pub fn closed(socket: &Path, workspace_id: &str) {
    let mut u = unclosed().lock().unwrap_or_else(|e| e.into_inner());
    if u.remove(&(socket.to_path_buf(), workspace_id.to_string())) {
        persist(&u);
    }
}

/// 這個 session 上登記過、還沒確認關掉、而且此刻沒有探測在跑的 workspace id：殘留。
pub fn leftovers(socket: &Path) -> Vec<String> {
    let running: HashSet<Key> = registry().lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(_, v)| v.is_none()).map(|(k, _)| k.clone()).collect();
    let mut ids: Vec<String> = unclosed()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|k| k.0 == socket && !running.contains(*k))
        .map(|k| k.1.clone())
        .collect();
    ids.sort();
    ids
}

/// 這個 workspace 是 daemon 自己開的探測（還在跑、剛結束、或還沒確認關掉）：pane 掃描據此排除，不看 label。
pub fn is_ours(socket: &Path, workspace_id: &str) -> bool {
    is_probe(socket, workspace_id) || unclosed().lock().unwrap_or_else(|e| e.into_inner()).contains(&(socket.to_path_buf(), workspace_id.to_string()))
}

/// 登記期間內，這個 workspace 是探測用的。丟掉就開始倒數 [`GRACE`]。
pub struct ProbeWorkspace(Key);

impl ProbeWorkspace {
    pub fn register(socket: &Path, workspace_id: &str) -> Self {
        let key = (socket.to_path_buf(), workspace_id.to_string());
        let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        // 順手把過了寬限的帶走：探測每 30 秒一個，workspace id 不重複，不清就只增不減。
        r.retain(|_, ends| ends.is_none_or(|t| t > now));
        r.insert(key.clone(), None);
        drop(r);
        let mut u = unclosed().lock().unwrap_or_else(|e| e.into_inner());
        u.insert(key.clone());
        persist(&u);
        Self(key)
    }
}

impl Drop for ProbeWorkspace {
    fn drop(&mut self) {
        registry().lock().unwrap_or_else(|e| e.into_inner()).insert(self.0.clone(), Some(Instant::now() + GRACE));
    }
}

pub fn is_probe(socket: &Path, workspace_id: &str) -> bool {
    is_probe_at(socket, workspace_id, Instant::now())
}

fn is_probe_at(socket: &Path, workspace_id: &str, now: Instant) -> bool {
    match registry().lock().unwrap_or_else(|e| e.into_inner()).get(&(socket.to_path_buf(), workspace_id.to_string())) {
        None => false,
        Some(None) => true,
        Some(Some(until)) => now < *until,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registered_workspace_is_a_probe_only_for_its_own_socket_and_a_little_past_its_end() {
        let (a, b) = (Path::new("/tmp/probe-a.sock"), Path::new("/tmp/probe-b.sock"));
        assert!(!is_probe(a, "w1"));
        let guard = ProbeWorkspace::register(a, "w1");
        assert!(is_probe(a, "w1"));
        assert!(!is_probe(b, "w1"), "別的 session 的同號 workspace 不是探測");
        assert!(!is_probe(a, "w2"));
        drop(guard);
        assert!(is_probe(a, "w1"), "剛結束：agent 退出的事件還會晚幾秒到");
        assert!(!is_probe_at(a, "w1", Instant::now() + GRACE + Duration::from_secs(1)), "過了寬限就不算");
        // 下一次登記順手把過期的清掉（這裡用寬限內外都夠的做法：直接看 map 大小不會無限長）。
        let _next = ProbeWorkspace::register(a, "w3");
    }

    /// #1795：殘留只認登記過的 `(socket, workspace id)`，跟 label 無關；還在跑的不算殘留，確認關掉之後就撤掉。
    #[test]
    fn leftovers_are_the_registered_unclosed_ids_of_that_socket_and_not_the_running_ones() {
        let (a, b) = (Path::new("/tmp/probe-lo-a.sock"), Path::new("/tmp/probe-lo-b.sock"));
        assert!(leftovers(a).is_empty(), "沒登記過的 workspace（不管叫什麼名字）不是殘留");
        let first = ProbeWorkspace::register(a, "w10");
        let _other = ProbeWorkspace::register(b, "w10");
        assert!(leftovers(a).is_empty(), "還在跑的不算殘留");
        assert!(is_ours(a, "w10"));
        drop(first);
        assert_eq!(leftovers(a), vec!["w10".to_string()], "跑完但沒確認關掉＝殘留");
        assert_eq!(leftovers(b), Vec::<String>::new(), "別的 session 同號的不受影響");
        closed(a, "w10");
        assert!(leftovers(a).is_empty(), "確認關掉就撤掉");
        assert!(!is_ours(a, "w11"), "同一個 session 上沒登記的不是");
    }

    /// 重啟後還收得到：登記寫檔、載回來；壞檔當空的。
    #[test]
    fn the_unclosed_set_survives_a_restart_through_the_file() {
        let dir = std::env::temp_dir().join(format!("am-probe-ws-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        let set: HashSet<Key> = [(PathBuf::from("/tmp/s.sock"), "w3".to_string()), (PathBuf::from("/tmp/t.sock"), "w3".to_string())].into_iter().collect();
        save_to(&path, &set);
        assert_eq!(load_from(&path), set);
        std::fs::write(&path, "not json").unwrap();
        assert!(load_from(&path).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
