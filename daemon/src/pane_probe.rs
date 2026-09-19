//! 打字前複查、關 shell 前確認要問的兩件外部事實：行程 dump（`ps`）與 listen port（`lsof`）。
//!
//! 正式路徑走真的 `ps`／`lsof`；測試可以換掉，餵決定性的結果。以前那兩條測試拿真的 pid 走真指令，
//! 結果隨機器（CI 的 macOS runner 上 `lsof` 慢到逾時）翻紅。`None`＝讀不到的語意不變：不是「沒有 port」。

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::state::App;

pub trait PaneProbe: Send + Sync {
    /// 一份 `memproc` 格式的行程 dump（樹＋環境）。
    fn dump<'a>(&'a self, app: &'a Arc<App>, host: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;
    /// 這些 pid 合起來 listen 的 port；`None`＝讀不到。
    fn listen_ports<'a>(&'a self, host: &'a str, pids: &'a [i32]) -> BoxFuture<'a, Option<Vec<u16>>>;
}

/// 真的 `ps`（本機）／ssh（遠端）與 `lsof`。
pub struct Real;

impl PaneProbe for Real {
    fn dump<'a>(&'a self, app: &'a Arc<App>, host: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(crate::memproc::dump(app, host))
    }
    fn listen_ports<'a>(&'a self, host: &'a str, pids: &'a [i32]) -> BoxFuture<'a, Option<Vec<u16>>> {
        Box::pin(crate::panes::listen_ports(host, pids))
    }
}

/// 決定性的假貨：行程樹是給定的字串，port 依 pid 查表；表裡沒有的 pid 沒有 port，`ports_unreadable` 則整個讀不到。
#[cfg(test)]
#[derive(Default)]
pub struct Fixed {
    pub dump: String,
    pub ports: HashMap<i32, Vec<u16>>,
    pub ports_unreadable: bool,
}

#[cfg(test)]
impl Fixed {
    /// `procs`：`(pid, ppid, argv)`；沒有環境段。
    pub fn tree(procs: &[(i32, i32, &str)]) -> Self {
        let mut dump = String::new();
        for (pid, ppid, argv) in procs {
            dump.push_str(&format!("{pid} {ppid} 1024 {argv}\n"));
        }
        dump.push_str("---AM-ENV---\n");
        Self { dump, ..Default::default() }
    }
    pub fn listening(mut self, pid: i32, port: u16) -> Self {
        self.ports.entry(pid).or_default().push(port);
        self
    }
}

#[cfg(test)]
impl PaneProbe for Fixed {
    fn dump<'a>(&'a self, _: &'a Arc<App>, _: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move { Ok(self.dump.clone()) })
    }
    fn listen_ports<'a>(&'a self, host: &'a str, pids: &'a [i32]) -> BoxFuture<'a, Option<Vec<u16>>> {
        Box::pin(async move {
            if host != crate::config::LOCAL_HOST {
                return Some(Vec::new());
            }
            if self.ports_unreadable {
                return None;
            }
            let mut ports: Vec<u16> = pids.iter().filter_map(|p| self.ports.get(p)).flatten().copied().collect();
            ports.sort_unstable();
            ports.dedup();
            Some(ports)
        })
    }
}
