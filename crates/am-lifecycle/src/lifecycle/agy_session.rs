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
pub async fn record_status(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit), bot: &db::Bot, run: &db::Run, text: &str) {
    let model = run.runtime_model.as_deref().or(bot.model.as_deref());
    let Some(json) = crate::agy_support::status_json(model, crate::agy_support::last_input_tokens(text)) else { return };
    if run.status_json.as_deref() == Some(json.as_str()) {
        return;
    }
    if sqlx::query("UPDATE runs SET status_json = ? WHERE id = ?").bind(&json).bind(&run.id).execute(app.db()).await.is_ok() {
        crate::capabilities::BotStatusEmit::emit_bot_status(app, &bot.id).await;
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;
    use crate::lifecycle::grok_transcript::{sync_locked, Synced};
    use crate::testing as tt;
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

    fn step(i: u64, ty: &str, content: &str, tokens: Option<i64>) -> String {
        let mut v = serde_json::json!({"step_index": i, "source": if ty == "USER_INPUT" { "USER_EXPLICIT" } else { "MODEL" }, "type": ty, "status": "DONE", "content": content});
        if let Some(n) = tokens {
            v["input_tokens"] = n.into();
        }
        v.to_string()
    }
    fn user(i: u64, text: &str) -> String {
        step(i, "USER_INPUT", &format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"), None)
    }

    struct Child {
        env: tt::Env,
        run_id: String,
        turn_id: String,
        file: std::path::PathBuf,
    }

    const REMOTE_HOME: &str = "/Users/m4p";

    /// 一顆沒有 hook 的 agy 子 agent：adopted、`inject_hooks = 0`、已經綁了對話（pane 環境的探測不在這裡測）、有一筆在飛的回合。
    async fn agy_child(sid: &str, transcript: &str) -> Child {
        agy_child_on(tt::env().await, None, sid, transcript).await
    }

    /// `host`＝`None` 是本機（對話檔寫進 HOME）；遠端只建專案、bot、run，對話檔在那台（測試用假 ssh 回）。
    async fn agy_child_on(env: tt::Env, host: Option<&str>, sid: &str, transcript: &str) -> Child {
        let app = env.app.clone();
        let project_id = match host {
            None => env.project_id.clone(),
            Some(h) => {
                let id = db::ulid();
                sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)")
                    .bind(&id)
                    .bind(format!("{REMOTE_HOME}/project/wt"))
                    .bind("remote")
                    .bind(h)
                    .bind(db::now())
                    .execute(&app.db)
                    .await
                    .unwrap();
                id
            }
        };
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, model, args_json, autostart, inject_hooks, managed_by, hook_token, created_at)
             VALUES (?,?,'a1','agy','gemini-3.8-flash-medium','[]',0,0,'child','tok',?)",
        )
        .bind(&bot_id)
        .bind(&project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let file = match host {
            None => {
                let file = crate::agy_support::transcript_path(&crate::home::dir().unwrap(), sid);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, transcript).unwrap();
                file
            }
            Some(_) => crate::agy_support::transcript_path(Path::new(REMOTE_HOME), sid),
        };
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, adopted, agent_name, herdr_session, native_session_id, started_at)
             VALUES (?,?,'running','idle','ws-1',?,1,'parent-a1','test',?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(format!("pane-{bot_id}"))
        .bind(sid)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web','in_flight','ok','say PONG',?)")
            .bind(&turn_id)
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user','say PONG','web',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&turn_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        Child { env, run_id, turn_id, file }
    }

    async fn replies(c: &Child) -> Vec<(String, String)> {
        sqlx::query_as("SELECT content, source FROM messages WHERE turn_id = ? AND role = 'assistant'").bind(&c.turn_id).fetch_all(&c.env.app.db).await.unwrap()
    }

    /// 2026-10-04 a1：子 agent 沒有 hook，回覆以前永遠是整個 pane 畫面（terminal_fallback）。現在讀對話檔：回覆是 `PLANNER_RESPONSE` 的原文。
    #[tokio::test]
    async fn a_hookless_agy_child_gets_its_reply_from_the_transcript_not_the_pane() {
        let sid = format!("c-{}", db::ulid());
        let text = [user(0, "say PONG"), step(1, "PLANNER_RESPONSE", "PONG", Some(11824))].join("\n");
        let c = agy_child(&sid, &text).await;
        let app = c.env.app.clone();
        assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
        assert_eq!(replies(&c).await, [("PONG".to_string(), "transcript".to_string())]);
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id = ?").bind(&c.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.native_session_id.as_deref(), t.native_turn_id.as_deref()), ("completed", Some(sid.as_str()), Some("p0")));
        // 模型與 context 的 token 數（視窗大小不知道，不填百分比）。
        let r = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
        let st: serde_json::Value = serde_json::from_str(r.status_json.as_deref().expect("status_json")).unwrap();
        assert_eq!(st["model"]["id"], "gemini-3.8-flash-medium");
        assert_eq!(st["context_window"]["total_input_tokens"], 11824);
        assert_eq!(r.transcript_path.as_deref(), c.file.to_str());
        // 同一份再讀一次：不重複。
        assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 0, pending: None });
        assert_eq!(replies(&c).await.len(), 1);
    }

    #[tokio::test]
    async fn an_answer_still_being_worked_on_is_pending_and_nothing_is_recorded_yet() {
        let sid = format!("c-{}", db::ulid());
        let text = [user(0, "say PONG"), step(1, "PLANNER_RESPONSE", "let me check", None), step(2, "RUN_COMMAND", "ls", None)].join("\n");
        let c = agy_child(&sid, &text).await;
        let got = sync_locked(&c.env.app, &c.run_id).await.unwrap();
        assert_eq!(got, Synced::Read { imported: 0, pending: Some("say PONG".into()) });
        assert!(replies(&c).await.is_empty(), "旁白不是最終回覆");
        // 做完之後（最後一則是回覆）才收。
        let done = format!("{text}\n{}", step(3, "PLANNER_RESPONSE", "PONG", None));
        std::fs::write(&c.file, done).unwrap();
        assert_eq!(sync_locked(&c.env.app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
        assert_eq!(replies(&c).await[0].0, "PONG");
    }

    #[tokio::test]
    async fn a_daemon_started_agy_bot_with_hooks_and_a_run_with_no_conversation_stay_on_the_old_paths() {
        let c = agy_child("c-none", "").await;
        std::fs::remove_file(&c.file).unwrap();
        assert_eq!(sync_locked(&c.env.app, &c.run_id).await.unwrap(), Synced::Unavailable, "沒有紀錄檔：照舊看畫面");
        sqlx::query("UPDATE bots SET inject_hooks = 1 WHERE id = (SELECT bot_id FROM runs WHERE id = ?)").bind(&c.run_id).execute(&c.env.app.db).await.unwrap();
        std::fs::create_dir_all(c.file.parent().unwrap()).unwrap();
        std::fs::write(&c.file, [user(0, "x"), step(1, "PLANNER_RESPONSE", "y", None)].join("\n")).unwrap();
        assert_eq!(sync_locked(&c.env.app, &c.run_id).await.unwrap(), Synced::Unavailable, "有 hook 的 bot 由 hook 收，不重複記");
    }

    /// 遠端（m4p）的 agy 子 agent（issue #1788）：pane 裡的行程開著兩段對話，只有一段有紀錄檔 → 綁到有紀錄檔那段；
    /// `/proc` 換成那台的 `lsof`，紀錄檔的時間與尾巴都經 ssh 從那台讀，路徑是那台家目錄底下的。
    #[tokio::test]
    async fn a_remote_agy_child_binds_the_open_conversation_that_has_a_transcript_over_ssh() {
        let e = tt::env().await;
        let sock = tt::track(std::env::temp_dir().join(format!("am-agy-remote-{}", db::ulid())));
        std::fs::create_dir_all(&sock).unwrap();
        let host = format!("m4p-{}", db::ulid().to_ascii_lowercase());
        let conn = e
            .app
            .hosts
            .insert_remote_with_client_for_test(
                crate::config::HostCfg {
                    shared_session: false,
                    name: host.clone(),
                    ssh: "unused".into(),
                    ssh_port: 22,
                    ssh_opts: vec![],
                    herdr_session: "test".into(),
                    remote_path: String::new(),
                },
                crate::herdr::HerdrClient::new(sock.join("herdr.sock")),
            )
            .await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        *conn.remote_home.lock().await = Some(REMOTE_HOME.into());

        let sid = format!("c-{}", db::ulid());
        let other = format!("d-{}", db::ulid());
        let text = [user(0, "say PONG"), step(1, "PLANNER_RESPONSE", "PONG", Some(11824))].join("\n");
        let c = agy_child_on(e, Some(&host), &sid, &text).await;
        let pane = db::run(&c.env.app.db, &c.run_id).await.unwrap().unwrap().pane_id.expect("pane");
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        {
            let (seen, sid, other, pane, text) = (seen.clone(), sid.clone(), other.clone(), pane.clone(), text.clone());
            crate::hosts::set_ssh_fake(&host, move |script| {
                seen.lock().unwrap().push(script.to_string());
                Ok(if script.contains("ps -Awwo") {
                    // 行程表只要環境段：pane 裡的 agy 行程。
                    format!("---AM-ENV---\n  901 agy HERDR_PANE_ID={pane}\n")
                } else if script.contains("lsof") {
                    // 開著的兩段對話，外加一個不是對話的檔。
                    format!("p901\nn{REMOTE_HOME}/.zshrc\nn{REMOTE_HOME}/.gemini/antigravity-cli/conversations/{other}.db\nn{REMOTE_HOME}/.gemini/antigravity-cli/conversations/{sid}.db\n")
                } else if script.contains("date -r") {
                    // 只有 `sid` 有紀錄檔。
                    format!("{sid} 1700000000\n")
                } else if script.contains("tail -c") {
                    text.clone()
                } else {
                    String::new()
                })
            });
        }

        let app = c.env.app.clone();
        assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
        assert_eq!(replies(&c).await, [("PONG".to_string(), "transcript".to_string())]);
        let r = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
        assert_eq!(r.native_session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(r.transcript_path.as_deref(), c.file.to_str());
        assert!(c.file.starts_with(REMOTE_HOME), "{:?}", c.file);
        let scripts = seen.lock().unwrap().clone();
        assert!(scripts.iter().any(|s| s.contains("lsof") && s.contains("-p 901")), "{scripts:?}");
    }
}
