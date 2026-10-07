//! 測試的 `CLAUDE_CONFIG_DIR` 一律是 `tt::track` 的暫存目錄，遠端是 ssh fake，不碰真的 `~/.claude`。
use super::*;
use crate::lifecycle::grok_transcript::{sync_locked, Synced};
use crate::testing as tt;
use serde_json::json;
use std::path::PathBuf;

const SID: &str = "99b1f189-867f-4f13-87d7-fb9464ef4041";

fn user(uuid: &str, text: &str) -> String {
    json!({"type": "user", "uuid": uuid, "isSidechain": false, "message": {"role": "user", "content": text}}).to_string()
}

fn user_at(uuid: &str, text: &str, at: &str) -> String {
    json!({"type": "user", "uuid": uuid, "timestamp": at, "isSidechain": false, "message": {"role": "user", "content": text}}).to_string()
}

fn assistant(text: &str, stop: Option<&str>, tool: bool) -> String {
    let mut content = vec![json!({"type": "text", "text": text})];
    if tool {
        content.push(json!({"type": "tool_use", "id": "t1", "name": "Read", "input": {}}));
    }
    json!({"type": "assistant", "isSidechain": false, "message": {"role": "assistant", "content": content, "stop_reason": stop}}).to_string()
}

fn tool_result() -> String {
    json!({"type": "user", "uuid": "tr", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}}).to_string()
}

fn log(lines: &[String]) -> String {
    lines.join("\n") + "\n"
}

/// 2026-10-07 m4p midplat：第一輪「我先讀那份 brief。」是帶 tool_use 的旁白，最後的 end_turn 才是回覆。
#[test]
fn the_reply_is_the_end_turn_text_not_the_narration_before_a_tool() {
    let text = log(&[
        user("u1", "請讀 brief 並開工"),
        assistant("我先讀那份 brief。", Some("tool_use"), true),
        tool_result(),
        assistant("brief 讀完，三件事都做好了。", Some("end_turn"), false),
        user("u2", "第二句"),
        assistant("寫到一半", None, false),
    ]);
    let ex = parse_exchanges(&text);
    assert_eq!(ex.len(), 2, "{ex:#?}");
    assert_eq!(ex[0].prompt, "請讀 brief 並開工");
    assert_eq!(ex[0].reply.as_deref(), Some("brief 讀完，三件事都做好了。"));
    assert!(ex[0].closed);
    assert_ne!(ex[0].prompt_index, ex[1].prompt_index, "uuid 不同＝鑰匙不同，同一句重複打也各是一問");
    assert!(!ex[1].closed && ex[1].reply.is_none(), "還在串流的半行不是回覆：{:?}", ex[1]);
}

#[test]
fn only_a_narration_so_far_leaves_the_exchange_open() {
    let ex = parse_exchanges(&log(&[user("u1", "開工"), assistant("我先讀那份 brief。", Some("tool_use"), true)]));
    assert_eq!(ex.len(), 1);
    assert!(!ex[0].closed, "旁白不是結尾");
    assert!(ex[0].reply.is_none());
}

#[test]
fn slash_echoes_meta_sidechain_and_interrupts_are_not_prompts() {
    let meta = json!({"type": "user", "isMeta": true, "message": {"content": "Caveat: x"}}).to_string();
    let side = json!({"type": "user", "isSidechain": true, "message": {"content": "子任務的 prompt"}}).to_string();
    let side_reply = json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "text", "text": "子任務的回覆"}], "stop_reason": "end_turn"}}).to_string();
    let ex = parse_exchanges(&log(&[
        meta,
        user("c1", "<command-name>/model</command-name>"),
        user("c2", "<local-command-stdout>Set model</local-command-stdout>"),
        user("u1", "真正的一問"),
        side,
        side_reply,
        user("i1", "[Request interrupted by user]"),
        user("u2", "接著做"),
        assistant("好", Some("end_turn"), false),
    ]));
    assert_eq!(ex.iter().map(|e| e.prompt.as_str()).collect::<Vec<_>>(), vec!["真正的一問", "接著做"], "{ex:#?}");
    assert!(ex[0].closed && ex[0].reply.is_none(), "被打斷：結束了，沒有回覆");
    assert_eq!(ex[1].reply.as_deref(), Some("好"));
}

#[test]
fn a_pasted_content_wrapper_is_unwrapped_to_what_was_sent() {
    let wrapped = "\n\n<pasted_content id=\"c4ab\">\n長長的一段\n</pasted_content id=\"c4ab\">\n";
    let ex = parse_exchanges(&log(&[user("u1", wrapped), assistant("收到", Some("end_turn"), false)]));
    assert_eq!(ex[0].prompt, "長長的一段");
}

#[test]
fn a_cut_off_first_line_does_not_overwrite_the_previous_reply() {
    let whole = log(&[user("u1", "第一問"), assistant("第一答", Some("end_turn"), false)]);
    let cut = format!("{whole}xxxx{{\"type\":\"assistant\"\n{}", assistant("切剩的後半", Some("end_turn"), false) + "\n");
    let ex = parse_exchanges(&cut);
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].reply.as_deref(), Some("第一答"));
}

#[test]
fn the_read_script_only_trusts_plain_files_under_the_listed_roots() {
    let script = read_script(&["/Users/m4p/.claude-cc0".to_string(), "/Users/m4p/.claude".to_string()], SID);
    assert!(script.contains("'/Users/m4p/.claude-cc0' '/Users/m4p/.claude'"), "{script}");
    assert!(script.contains(&format!("projects/*/{SID}.jsonl")), "{script}");
    assert!(script.contains("[ ! -L \"$f\" ]"), "symlink 不讀");
    let out = format!("{LOG_MARK}\n/Users/m4p/.claude/projects/-x/{SID}.jsonl\n{{\"a\":1}}\n");
    assert_eq!(split_output(&out), Some((format!("/Users/m4p/.claude/projects/-x/{SID}.jsonl"), "{\"a\":1}\n".to_string())));
    assert_eq!(split_output(""), None, "沒有這個檔");
}

struct Child {
    env: tt::Env,
    bot: db::Bot,
    run_id: String,
    conv: String,
    pane: String,
}

/// 沒有 hook 的 claude 子 agent（`herdr agent start` 開的、被 reconcile 認領的）。`host` 為 `None`＝本機。
async fn claude_child(config_dir: Option<&PathBuf>, host: Option<&str>) -> Child {
    claude_child_in(tt::env().await, config_dir, host).await
}

async fn claude_child_in(env: tt::Env, config_dir: Option<&PathBuf>, host: Option<&str>) -> Child {
    let app = env.app.clone();
    let project_id = match host {
        None => env.project_id.clone(),
        Some(h) => {
            let id = db::ulid();
            sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)")
                .bind(&id)
                .bind("/Users/m4p/project/wt")
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
    let env_json = config_dir.map(|d| json!({"CLAUDE_CONFIG_DIR": d.to_string_lossy()}).to_string()).unwrap_or_else(|| "{}".into());
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, managed_by, hook_token, env_json, created_at)
         VALUES (?,?,'midplat','claude','[]',0,0,'child','tok',?,?)",
    )
    .bind(&bot_id)
    .bind(&project_id)
    .bind(&env_json)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let pane = format!("pane-{bot_id}");
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','idle','ws-1',?,'tab-1',1,'hub-midplat','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(&pane)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    env.herdr.tabs.lock().unwrap().push(tt::MockTab { tab_id: "tab-1".into(), workspace_id: "ws-1".into(), label: "t".into(), panes: vec![pane.clone()] });
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
    Child { env, bot, run_id, conv, pane }
}

fn write_log(config_dir: &std::path::Path, sid: &str, text: &str) -> PathBuf {
    let dir = config_dir.join("projects").join("-Users-m4p-project-wt");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, text).unwrap();
    path
}

/// 這個 run 已經建過基準（空字串＝當時檔裡沒有任何一問）：測試要的是「之後的內容照記」，不是第一次接上的行為。
async fn baselined(app: &Arc<App>, run_id: &str) {
    sqlx::query("UPDATE runs SET transcript_baseline_at = '' WHERE id = ?").bind(run_id).execute(&app.db).await.unwrap();
}

async fn baseline(app: &Arc<App>, run_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT transcript_baseline_at FROM runs WHERE id = ?").bind(run_id).fetch_one(&app.db).await.unwrap()
}

async fn turn_count(app: &Arc<App>, conv: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?").bind(conv).fetch_one(&app.db).await.unwrap()
}

async fn messages(app: &Arc<App>, conv: &str) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT role, content, source FROM messages WHERE conversation_id = ? ORDER BY id").bind(conv).fetch_all(&app.db).await.unwrap()
}

fn two_exchanges() -> String {
    log(&[
        user("u1", "請讀 brief 並開工"),
        assistant("我先讀那份 brief。", Some("tool_use"), true),
        tool_result(),
        assistant("brief 讀完，三件事都做好了。", Some("end_turn"), false),
        user("u2", "再補一個測試"),
        assistant("測試補好了。", Some("end_turn"), false),
    ])
}

/// #878：沒有 hook 的 claude 子 agent，session 從 herdr 的 `agent_session` 來，回覆讀對話檔、不刮畫面。
#[tokio::test]
async fn a_hookless_claude_child_records_its_exchanges_from_its_transcript() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    let path = write_log(&cfg, "sess-1", &two_exchanges());
    baselined(&app, &c.run_id).await;
    c.env.herdr.set_agent("hub-midplat", &c.pane, true);
    c.env.herdr.set_screen(&c.pane, "⏺ 我先讀那份 brief。\n");

    assert!(crate::lifecycle::poller::capture_hookless_turn_locked(&app, &c.run_id, true).await.unwrap());
    let msgs = messages(&app, &c.conv).await;
    let got: Vec<(&str, &str, &str)> = msgs.iter().map(|(r, t, s)| (r.as_str(), t.as_str(), s.as_str())).collect();
    assert_eq!(
        got,
        vec![
            ("user", "請讀 brief 並開工", "transcript"),
            ("assistant", "brief 讀完，三件事都做好了。", "transcript"),
            ("user", "再補一個測試", "transcript"),
            ("assistant", "測試補好了。", "transcript"),
        ],
        "畫面上的開場白一個字都不存"
    );
    let run = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
    assert_eq!(run.native_session_id.as_deref(), Some("sess-1"));
    assert_eq!(run.transcript_path.as_deref(), Some(path.to_string_lossy().as_ref()));
    let status: Vec<String> = sqlx::query_scalar("SELECT status FROM turns WHERE conversation_id = ? ORDER BY created_at, rowid").bind(&c.conv).fetch_all(&app.db).await.unwrap();
    assert_eq!(status, vec!["completed", "completed"]);

    // 同一輪報好幾次 idle、daemon 重啟再認領：不重記。
    assert!(!crate::lifecycle::poller::capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
    assert_eq!(messages(&app, &c.conv).await.len(), 4);
}

/// herdr 的 `agent start` 逾時把名字拿掉：用名字查不到，改用 pane id 查，session 照樣找得到。
#[tokio::test]
async fn the_session_is_found_by_pane_id_when_herdr_dropped_the_agent_name() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    write_log(&cfg, "sess-9", &two_exchanges());
    baselined(&app, &c.run_id).await;
    c.env.herdr.set_unnamed_agent(&c.pane, Some("sess-9"));

    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 2, pending: None });
    let run = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
    assert_eq!(run.native_session_id.as_deref(), Some("sess-9"));
}

/// herdr 還沒綁 session、run 也沒記過：不猜，照舊看畫面（Unavailable）。
#[tokio::test]
async fn without_a_bound_session_the_pane_is_the_fallback() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    write_log(&cfg, "sess-1", &two_exchanges());
    c.env.herdr.set_agent("hub-midplat", &c.pane, false);
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Unavailable);
}

/// 有 session 但對話檔不在 root 底下（別的身分的目錄）：讀不到＝Unavailable，不從別處猜。
#[tokio::test]
async fn a_transcript_outside_the_config_roots_is_not_read() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let other = tt::track(std::env::temp_dir().join(format!("am-claude-other-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    write_log(&other, "sess-1", &two_exchanges());
    c.env.herdr.set_agent("hub-midplat", &c.pane, true);
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Unavailable);
}

/// 還在跑的一問：回合在飛時不收，`pending` 回報檔尾那一問。
#[tokio::test]
async fn the_unfinished_exchange_is_reported_as_pending() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    write_log(&cfg, "sess-1", &log(&[user("u1", "開工"), assistant("我先讀那份 brief。", Some("tool_use"), true)]));
    c.env.herdr.set_agent("hub-midplat", &c.pane, true);
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 0, pending: Some("開工".into()) });
    assert!(messages(&app, &c.conv).await.is_empty());
}

/// 遠端（m4p）：對話檔在那台機器上，走 ssh 讀；腳本只列該主機的 root，session 來自 herdr。
#[tokio::test]
async fn a_remote_claude_child_reads_its_transcript_over_ssh() {
    let e = tt::env().await;
    let sock = tt::track(std::env::temp_dir().join(format!("am-claude-remote-{}", db::ulid())));
    std::fs::create_dir_all(&sock).unwrap();
    let herdr = tt::MockHerdr::start(sock.join("herdr.sock"));
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
    *conn.remote_home.lock().await = Some("/Users/m4p".into());
    let transcript = format!("/Users/m4p/.claude/projects/-Users-m4p-project-wt/{SID}.jsonl");
    let body = two_exchanges();
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    {
        let (transcript, body, seen) = (transcript.clone(), body.clone(), seen.clone());
        crate::hosts::set_ssh_fake(&host, move |script| {
            seen.lock().unwrap().push(script.to_string());
            Ok(format!("{LOG_MARK}\n{transcript}\n{body}"))
        });
    }
    // 子 agent 的 bot／run 掛在遠端專案上；herdr 在那台機器上。
    let c = claude_child_in(e, None, Some(&host)).await;
    herdr.tabs.lock().unwrap().push(tt::MockTab { tab_id: "tab-1".into(), workspace_id: "ws-1".into(), label: "t".into(), panes: vec![c.pane.clone()] });
    herdr.set_agent("hub-midplat", &c.pane, true);
    // `set_agent` 綁的 session 是 sess-1；遠端 herdr 要回真正的 session id。
    herdr.agents.lock().unwrap().iter_mut().for_each(|a| {
        a["agent_session"]["value"] = json!(SID);
    });

    let app = c.env.app.clone();
    baselined(&app, &c.run_id).await;
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 2, pending: None });
    let scripts = seen.lock().unwrap().clone();
    assert!(scripts.iter().any(|s| s.contains(&format!("projects/*/{SID}.jsonl")) && s.contains("/Users/m4p/.claude")), "{scripts:?}");
    let run = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
    assert_eq!(run.native_session_id.as_deref(), Some(SID));
    assert_eq!(run.transcript_path.as_deref(), Some(transcript.as_str()));
    let msgs = messages(&app, &c.conv).await;
    assert_eq!(msgs.len(), 4, "{msgs:?}");
    assert!(msgs.iter().all(|(_, _, source)| source == "transcript"));
}

fn history() -> Vec<String> {
    vec![
        user_at("u1", "早就處理過的一問", "2026-10-07T10:00:00.000Z"),
        assistant("早就處理過的回覆。", Some("end_turn"), false),
        user_at("u2", "另一個早就處理過的一問", "2026-10-07T11:00:00.000Z"),
        assistant("另一個早就處理過的回覆。", Some("end_turn"), false),
    ]
}

/// #879：daemon 換版後第一次接上對話檔，裡面整段歷史（含已經回報過的）不能被當成新回合、不能再發 child_done。
#[tokio::test]
async fn the_first_read_only_takes_a_baseline_and_makes_no_turns() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    write_log(&cfg, "sess-1", &log(&history()));
    c.env.herdr.set_agent("hub-midplat", &c.pane, true);
    assert_eq!(baseline(&app, &c.run_id).await, None);

    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 0, pending: None });
    assert_eq!(turn_count(&app, &c.conv).await, 0, "歷史不生回合");
    assert!(messages(&app, &c.conv).await.is_empty());
    assert_eq!(baseline(&app, &c.run_id).await.as_deref(), Some("2026-10-07T11:00:00.000Z"));

    // 再讀（輪詢、再一次重啟）：基準之前的還是不記。
    for _ in 0..2 {
        assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 0, pending: None });
    }
    assert_eq!(turn_count(&app, &c.conv).await, 0);
}

/// 基準之後才開始的問照記：歷史不記，新的一問結束時記成一個回合。
#[tokio::test]
async fn exchanges_after_the_baseline_are_recorded() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    let mut lines = history();
    write_log(&cfg, "sess-1", &log(&lines));
    c.env.herdr.set_agent("hub-midplat", &c.pane, true);
    sync_locked(&app, &c.run_id).await.unwrap();

    lines.push(user_at("u3", "基準之後的新問題", "2026-10-07T12:00:00.000Z"));
    write_log(&cfg, "sess-1", &log(&lines));
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 0, pending: Some("基準之後的新問題".into()) }, "還沒答完：不記");

    lines.push(assistant("新問題的回覆。", Some("end_turn"), false));
    write_log(&cfg, "sess-1", &log(&lines));
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
    let got: Vec<(String, String)> = messages(&app, &c.conv).await.into_iter().map(|(r, t, _)| (r, t)).collect();
    assert_eq!(got, vec![("user".into(), "基準之後的新問題".into()), ("assistant".into(), "新問題的回覆。".into())]);
    assert_eq!(turn_count(&app, &c.conv).await, 1);
}

/// 第一次讀到時正在跑的一問（基準時還沒結束）：不在基準裡，結束時照記。
#[tokio::test]
async fn the_exchange_running_at_the_baseline_is_still_recorded_when_it_ends() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    let mut lines = history();
    lines.push(user_at("u3", "第一次讀到時正在跑", "2026-10-07T12:00:00.000Z"));
    write_log(&cfg, "sess-1", &log(&lines));
    c.env.herdr.set_agent("hub-midplat", &c.pane, true);
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 0, pending: Some("第一次讀到時正在跑".into()) });
    assert_eq!(baseline(&app, &c.run_id).await.as_deref(), Some("2026-10-07T11:00:00.000Z"));

    lines.push(assistant("跑完了。", Some("end_turn"), false));
    write_log(&cfg, "sess-1", &log(&lines));
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
    assert_eq!(turn_count(&app, &c.conv).await, 1);
}

/// 第一次讀到時，派工開的在飛回合對得上檔裡的那一問：照舊收掉它（回覆補進去），只是歷史不另開回合。
#[tokio::test]
async fn the_baseline_still_settles_the_turn_that_was_dispatched() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    let mut lines = history();
    lines.push(user_at("u3", "請讀 brief 並開工", "2026-10-07T12:00:00.000Z"));
    lines.push(assistant("開工，做完了。", Some("end_turn"), false));
    write_log(&cfg, "sess-1", &log(&lines));
    c.env.herdr.set_agent("hub-midplat", &c.pane, true);
    let parent = tt::claude_bot(&app, &c.env.project_id, "parent").await;
    let run = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
    let tid = crate::lifecycle::relay_watch::open_turn(&app, &run, &parent.id, "請讀 brief 並開工").await.unwrap();

    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
    assert_eq!(turn_count(&app, &c.conv).await, 1, "只有派工開的那一筆");
    let (status, reply): (String, String) = sqlx::query_as(
        "SELECT t.status, (SELECT content FROM messages WHERE turn_id = t.id AND role = 'assistant') FROM turns t WHERE t.id = ?",
    )
    .bind(&tid)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!((status.as_str(), reply.as_str()), ("completed", "開工，做完了。"));
}

/// 檔裡還沒有任何一問就建基準（空字串）：之後的問全部算新的。
#[tokio::test]
async fn an_empty_transcript_baseline_lets_every_later_exchange_through() {
    let cfg = tt::track(std::env::temp_dir().join(format!("am-claude-cfg-{}", db::ulid())));
    let c = claude_child(Some(&cfg), None).await;
    let app = c.env.app.clone();
    write_log(&cfg, "sess-1", "");
    c.env.herdr.set_agent("hub-midplat", &c.pane, true);
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 0, pending: None });
    assert_eq!(baseline(&app, &c.run_id).await.as_deref(), Some(""));

    write_log(&cfg, "sess-1", &log(&[user_at("u1", "第一問", "2026-10-07T09:00:00.000Z"), assistant("第一答。", Some("end_turn"), false)]));
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
}
