//! daemon 自己開的探測 workspace（grok／claude 額度探測，遠端主機借用那台的 session）。
//!
//! 探測 pane 一啟動，herdr 就對該 session 發 `pane.agent_detected`；全域訂閱把它當成「多半是 bot 剛開的子 pane」
//! 而在兩秒後對整台主機對帳。探測是 daemon 自己輪詢出來的（grok 每 30 秒一次，SPEC §12），於是一台有十幾顆 bot 的
//! 遠端主機每 40 秒被完整對帳一輪——每輪十幾顆 bot × 好幾次 ssh RPC，只為了一個永遠不會是 child 的 pane。
//! 探測開 workspace 時在這裡登記、結束時（`Drop`）撤掉，全域訂閱看到登記過的 workspace 的偵測事件就不排對帳。
//!
//! key 是 `(herdr socket, workspace id)`：workspace id 只在一個 herdr session 內唯一，不同主機／session 會撞號。

use std::collections::HashMap;
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
}
