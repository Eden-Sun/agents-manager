//! 沒有 hook 的 agy run（典型是 bot 用 `herdr agent start --kind agy` 開的子 agent）的對話紀錄（SPEC §12a.9）。
//!
//! hook 與 statusLine 只有 AG Man 自己啟動的 agy bot 才有（pane env 帶自己的 `AM_BOT_ID`／`AM_HOOK_TOKEN`）；子 agent 的 pane env 是父 bot 的，
//! 它的 hook 會被當成別種 provider 擋掉——所以跟 grok 子 agent 一樣改讀對話檔（`grok_transcript` 把這裡讀到的問答記成回合）：
//! * **session 對 pane**：agy 沒有 `active_sessions.json`，但它的行程一直開著 `~/.gemini/antigravity-cli/conversations/<id>.db`。
//!   pane 內的行程（`memproc::pids_in_pane`，環境 `HERDR_PANE_ID` 指回 pane）的 `/proc/<pid>/fd` 就是對話 id；同一顆 pane 開過好幾段
//!   （`/clear`）取還開著、紀錄檔最新的那一段。別的在跑的 run 已經綁走的不算。找到的記在 `runs.native_session_id`／`transcript_path`。
//! * 遠端主機（issue #1788）：`/proc` 換成那台的 `lsof -Fn -p <pid>`，紀錄檔的時間與尾巴用 `date -r`、`tail -c`，全部經 ssh 在那台跑（家目錄用 `HostConn::home`）。
//! * 讀的是 `…/brain/<id>/.system_generated/logs/transcript_full.jsonl` 的尾巴（`agy_support::parse_turns`）。

use super::*;
use std::path::{Path, PathBuf};

const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

pub fn valid_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// 這些開著的對話裡，這個 run 該綁哪一段。`taken`＝別的在跑 run 已綁的；`mtime` 回紀錄檔的修改時間（沒有檔＝`None`＝不算）。
pub fn pick_conversation(
    open: &[String],
    known: Option<&str>,
    taken: &std::collections::HashSet<String>,
    mtime: impl Fn(&str) -> Option<std::time::SystemTime>,
) -> Option<String> {
    if let Some(k) = known.filter(|k| open.iter().any(|o| o == k)) {
        return Some(k.to_string());
    }
    open.iter().filter(|c| !taken.contains(*c)).filter_map(|c| mtime(c).map(|t| (t, c))).max().map(|(_, c)| c.clone())
}

/// 這台主機上的 agy 事實從哪裡讀（issue #1788）：本機直接讀 `/proc` 與檔案；遠端＝那台的 ssh（`lsof`、`date`、`tail`）。
enum Site {
    Local { home: PathBuf },
    Remote { conn: std::sync::Arc<crate::hosts::HostConn>, home: PathBuf },
}

impl Site {
    /// 拿不到家目錄、遠端沒連線＝`None`（照舊看畫面）。
    async fn of(app: &impl crate::hosts::HostsAccess, host: &str) -> Option<Site> {
        if host == LOCAL_HOST {
            return crate::home::dir().map(|home| Site::Local { home });
        }
        let conn = app.hosts().get(host).await?;
        if !conn.is_connected() {
            return None;
        }
        match conn.home().await {
            Ok(home) => Some(Site::Remote { conn, home: home.into() }),
            Err(e) => {
                tracing::debug!(host, error = ?e, "agy transcript: remote home unknown");
                None
            }
        }
    }

    fn path_of(&self, id: &str) -> PathBuf {
        let home = match self {
            Site::Local { home } | Site::Remote { home, .. } => home,
        };
        crate::agy_support::transcript_path(home, id)
    }

    /// pane 裡的行程開著的對話 id。
    async fn open_conversations(&self, pids: &[i32]) -> anyhow::Result<Vec<String>> {
        match self {
            Site::Local { .. } => {
                let pids = pids.to_vec();
                Ok(tokio::task::spawn_blocking(move || crate::agy_support::open_conversations(Path::new("/proc"), &pids)).await?)
            }
            Site::Remote { conn, .. } => {
                if pids.is_empty() {
                    return Ok(Vec::new());
                }
                let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
                let out = conn.ssh_exec_path(&format!("lsof -nP -Fn -p {list} 2>/dev/null; :")).await?;
                Ok(crate::agy_support::parse_lsof_conversations(&out))
            }
        }
    }

    /// 紀錄檔的修改時間（沒有紀錄檔的不在結果裡）。遠端一次 ssh 把全部候選問完。
    async fn mtimes(&self, ids: &[String]) -> std::collections::HashMap<String, std::time::SystemTime> {
        match self {
            Site::Local { .. } => ids
                .iter()
                .filter_map(|id| {
                    let t = std::fs::metadata(self.path_of(id)).and_then(|m| m.modified()).ok()?;
                    Some((id.clone(), t))
                })
                .collect(),
            Site::Remote { conn, .. } => {
                let script: String = ids
                    .iter()
                    .filter(|id| valid_session_id(id))
                    .map(|id| {
                        let f = crate::hosts::sh_quote(&self.path_of(id).to_string_lossy());
                        format!("if [ -f {f} ] && [ ! -L {f} ]; then printf '%s %s\\n' '{id}' \"$(date -r {f} +%s)\"; fi; ")
                    })
                    .collect();
                if script.is_empty() {
                    return Default::default();
                }
                match conn.ssh_exec_path(&format!("{script}:")).await {
                    Ok(out) => crate::agy_support::parse_mtimes(&out),
                    Err(e) => {
                        tracing::debug!(error = ?e, "agy transcript: remote transcript times unknown");
                        Default::default()
                    }
                }
            }
        }
    }

    /// 紀錄檔尾巴（最多 [`MAX_READ_BYTES`]）。讀不到＝`None`。
    async fn tail(&self, path: &Path) -> Option<String> {
        match self {
            Site::Local { .. } => crate::transcript_read::read_tail(path, MAX_READ_BYTES),
            Site::Remote { conn, .. } => {
                let f = crate::hosts::sh_quote(&path.to_string_lossy());
                let script = format!("if [ -f {f} ] && [ ! -L {f} ]; then tail -c {MAX_READ_BYTES} {f}; fi; :");
                match conn.ssh_exec_path(&script).await {
                    Ok(out) if !out.is_empty() => Some(out),
                    Ok(_) => None,
                    Err(e) => {
                        tracing::debug!(error = ?e, "agy transcript: remote transcript unreadable");
                        None
                    }
                }
            }
        }
    }
}

/// `Some((對話 id, 紀錄檔尾巴))`；找不到對話、讀不到檔＝`None`（照舊看畫面）。遠端的 `/proc`、對話檔都在那台，經 ssh 讀。
pub async fn load(app: &(impl crate::capabilities::Db + crate::hosts::HostsAccess), run: &db::Run, host: &str) -> anyhow::Result<Option<(String, String)>> {
    let Some(site) = Site::of(app, host).await else { return Ok(None) };
    let known = run.native_session_id.clone().filter(|s| valid_session_id(s));
    let mut open: Vec<String> = Vec::new();
    if let Some(pane) = run.pane_id.as_deref() {
        match crate::memproc::dump(app, host).await {
            Ok(out) => {
                let pids = crate::memproc::pids_in_pane(&out, pane, run.herdr_session.as_deref());
                match site.open_conversations(&pids).await {
                    Ok(ids) => open = ids,
                    Err(e) => tracing::debug!(run = %run.id, error = ?e, "agy transcript: open conversations unknown; keeping the known conversation"),
                }
            }
            Err(e) => tracing::debug!(run = %run.id, error = ?e, "agy transcript: process dump failed; keeping the known conversation"),
        }
    }
    let taken: std::collections::HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT native_session_id FROM runs WHERE state = 'running' AND id <> ? AND native_session_id IS NOT NULL",
    )
    .bind(&run.id)
    .fetch_all(app.db())
    .await?
    .into_iter()
    .collect();
    let candidates: Vec<String> = open.iter().chain(known.iter()).cloned().collect();
    let mtimes = site.mtimes(&candidates).await;
    let mtime = |id: &str| mtimes.get(id).copied();
    let picked = pick_conversation(&open, known.as_deref(), &taken, mtime).or(known.clone().filter(|k| mtime(k).is_some()));
    let Some(sid) = picked else { return Ok(None) };
    let path = site.path_of(&sid);
    let path_s = path.to_string_lossy().into_owned();
    if known.as_deref() != Some(sid.as_str()) || run.transcript_path.as_deref() != Some(path_s.as_str()) {
        sqlx::query("UPDATE runs SET native_session_id = ?, transcript_path = ? WHERE id = ?").bind(&sid).bind(&path_s).bind(&run.id).execute(app.db()).await?;
        tracing::info!(run = %run.id, session = %sid, host, "agy transcript: bound the pane to its conversation");
    }
    let Some(text) = site.tail(&path).await else { return Ok(None) };
    Ok(Some((sid, text)))
}

/// 讀到紀錄之後順手補網頁的 `status_json`（模型與 context token 數）。沒變就不寫。
pub async fn record_status(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit + crate::capabilities::Cfg), bot: &db::Bot, run: &db::Run, text: &str) {
    let model = run.runtime_model.as_deref().or(bot.model.as_deref());
    let window = app.cfg().get().await.agy.context_window_tokens;
    // 快取倒數用：最後一筆 assistant 回覆的時間（不論 status_json 有沒有變都記）。
    if let Some(at) = crate::agy_support::last_assistant_at(text) {
        crate::cache_clock::on_agy_transcript(&run.id, &at);
    }
    let Some(json) = crate::agy_support::status_json(model, crate::agy_support::last_input_tokens(text), window) else { return };
    if run.status_json.as_deref() == Some(json.as_str()) {
        return;
    }
    if sqlx::query("UPDATE runs SET status_json = ? WHERE id = ?").bind(&json).bind(&run.id).execute(app.db()).await.is_ok() {
        crate::capabilities::BotStatusEmit::emit_bot_status(app, &bot.id).await;
    }
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::{Duration, SystemTime};
    #[test]
    fn the_known_conversation_wins_while_it_is_open_otherwise_the_newest_free_one() {
        let t = |s: u64| Some(SystemTime::UNIX_EPOCH + Duration::from_secs(s));
        let open: Vec<String> = ["a", "b", "c"].map(String::from).into();
        let mtime = |id: &str| match id {
            "a" => t(10),
            "b" => t(30),
            "c" => None,
            _ => None,
        };
        let none = HashSet::new();
        assert_eq!(pick_conversation(&open, Some("a"), &none, mtime).as_deref(), Some("a"), "記過而且還開著：不換");
        assert_eq!(pick_conversation(&open, Some("gone"), &none, mtime).as_deref(), Some("b"), "記的那段已經關了（/clear）：取最新的");
        let taken: HashSet<String> = ["b".to_string()].into();
        assert_eq!(pick_conversation(&open, None, &taken, mtime).as_deref(), Some("a"), "別的 run 綁走的不給");
        assert_eq!(pick_conversation(&["c".to_string()], None, &none, mtime), None, "沒有紀錄檔的不算");
        assert_eq!(pick_conversation(&[], None, &none, mtime), None);
    }
}
