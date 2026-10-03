//! fixtures 取自 grok 1.0.46 真實的 `chat_history.jsonl`（agents-manager 的 grok 子 agent g5／g8），去敏：
//! system prompt、`<user_info>`、system reminder、工具輸出與長 prompt 截短，reasoning 的加密內容換成 `REDACTED`。
//! 測試的 `GROK_HOME` 一律是 `tt::track` 的暫存目錄，不碰真的 `~/.grok`。
use super::*;
use crate::testing as tt;
use std::path::{Path, PathBuf};

const FRESH: &str = include_str!("fixtures/grok-1.0.46-chat_history.jsonl");
const COMPACTED: &str = include_str!("fixtures/grok-1.0.46-chat_history-compacted.jsonl");
const WELCOME: &str = include_str!("fixtures/grok-1.0.46-welcome.txt");
const SID: &str = "01a10136-1726-7ec2-9cd8-d321b39fda2e";

#[test]
fn a_fresh_history_is_two_exchanges_without_the_context_entries() {
    let ex = parse_chat_history(FRESH);
    assert_eq!(ex.len(), 2, "`<user_info>` 與 system reminder 不是對話：{ex:#?}");
    assert_eq!(ex[0].prompt_index, Some(0));
    assert!(ex[0].prompt.starts_with("你是 agents-manager-mkng2n-g5（repo Eden-Sun/agents-manager"), "{}", ex[0].prompt);
    assert!(!ex[0].prompt.contains("<user_query>"), "外層標籤拿掉");
    assert!(ex[0].closed);
    // 帶 tool_calls 的那幾則是中途的旁白，回覆是最後一則沒有 tool_calls 的。
    assert!(ex[0].reply.as_deref().unwrap().starts_with("已接手 `fix/xreview-grok` 的未提交改動並完成收尾。"), "{:?}", ex[0].reply);
    assert_eq!(ex[1].prompt_index, Some(1));
    assert!(ex[1].prompt.starts_with("繼續剛才的接手工作"));
    assert!(ex[1].reply.as_deref().unwrap().starts_with("`fix/xreview-grok` 已收尾"));
    assert_eq!((ex[0].key(), ex[1].key()), ("p0".to_string(), "p1".to_string()));
}

/// 壓縮後 grok 重寫整個檔：前面的問答不見了，進行中那一問以沒有 `prompt_index` 的形式重新出現在摘要前面。
#[test]
fn a_compacted_history_keeps_the_replayed_prompt_and_the_pending_one() {
    let ex = parse_chat_history(COMPACTED);
    assert_eq!(ex.len(), 2, "{ex:#?}");
    assert_eq!(ex[0].prompt_index, None);
    assert!(ex[0].prompt.starts_with("做得好。最後兩支同樣的收尾 rebase"));
    assert!(ex[0].key().starts_with('q'), "沒有 index 用內容雜湊");
    assert_eq!(ex[0].key(), ex[0].clone().key(), "同一句同一把鑰匙");
    assert!(ex[0].reply.as_deref().unwrap().starts_with("**(1) `fix/xreview-grok-2`** 已推到"));
    assert_eq!(ex[1].prompt_index, Some(5));
    assert!(!ex[1].closed && ex[1].reply.is_none(), "還在跑的一問");
}

#[test]
fn the_session_is_picked_by_the_pane_process_then_by_a_unique_cwd() {
    let s = |sid: &str, pid: i64, cwd: &str| ActiveSession { session_id: sid.into(), pid, cwd: cwd.into() };
    // 2026-10-03 真機：同一個 cwd 同時開著四顆 grok。
    let active = parse_active(
        r#"[{"session_id":"01a10136-175e-7480-a7a0-64b5b49c5e54","pid":1875628,"cwd":"/home/u/agents-manager","opened_at":"2026-10-03T10:01:25.135106874Z"},
            {"session_id":"01a10187-dcbd-7702-8927-d12eab9fd583","pid":513147,"cwd":"/home/u/agents-manager","opened_at":"2026-10-03T11:30:44.042502623Z"},
            {"session_id":"01a10187-dcb8-7f92-a98a-9391fc3210e1","pid":513154,"cwd":"/home/u/agents-manager","opened_at":"2026-10-03T11:30:44.050464499Z"},
            {"session_id":"bad;rm -rf","pid":1,"cwd":"/x"}]"#,
    );
    assert_eq!(active.len(), 3, "不合法的 session id 不收");
    let none = HashSet::new();
    assert_eq!(pick_session(&active, &[513154, 513160], None, &none).as_deref(), Some("01a10187-dcb8-7f92-a98a-9391fc3210e1"));
    assert_eq!(pick_session(&active, &[42], Some("/home/u/agents-manager"), &none), None, "pane 的行程讀得到卻不在名單上：還沒登記，不退回猜");
    assert_eq!(pick_session(&active, &[], Some("/home/u/agents-manager"), &none), None, "讀不到行程、同 cwd 不只一個：不猜");
    let only = vec![s("a", 1, "/p/"), s("b", 2, "/q")];
    assert_eq!(pick_session(&only, &[], Some("/p"), &none).as_deref(), Some("a"));
    let taken: HashSet<String> = ["a".to_string()].into();
    assert_eq!(pick_session(&only, &[], Some("/p"), &taken), None, "別的 run 綁走的不能再給");
}

#[test]
fn the_startup_menu_is_not_a_reply() {
    assert!(is_startup_screen(WELCOME));
    assert!(is_startup_screen("main ~/x\n\nNew worktree   ctrl+w\nResume session   ctrl+r\nChangelog\nQuit   ctrl+q\n"));
    assert!(!is_startup_screen("回覆裡提到 Resume session ctrl+r 這個快捷鍵"));
}

#[test]
fn slash_commands_are_not_conversation() {
    assert!(is_slash_command("/effort high"));
    assert!(is_slash_command(" /model grok-4.7 "));
    assert!(!is_slash_command("/home/ubuntu/x 看一下這個檔"));
    assert!(!is_slash_command("/effort high\n然後繼續"));
    assert!(!is_slash_command("請跑 /verify"));
}

#[test]
fn a_cut_off_last_line_does_not_become_the_reply() {
    let whole = one_exchange(Some(0), "問一句", "完整回覆");
    let cut = format!("{whole}{{\"type\":\"assistant\",\"content\":\"寫到一半");
    let ex = parse_chat_history(&cut);
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].reply.as_deref(), Some("完整回覆"));
    assert!(ex[0].closed);
}

#[test]
fn a_truncated_echo_still_matches_its_prompt() {
    let p = "g8（repo Eden-Sun/agents-manager，父 bot agents-manager-mkng2n）。接手任務：前一顆 GPT-6-Luna 子 agent 因額度用完停在半路";
    assert!(prompt_matches("g8（repo Eden-Sun/agents-\nmanager，父 bot agents-manager- …", p));
    assert!(!prompt_matches("/effort high", p));
    assert!(!prompt_matches("…", p));
}

struct Child {
    env: tt::Env,
    bot: db::Bot,
    run_id: String,
    conv: String,
    home: PathBuf,
}

/// 沒有 hook 的 grok 子 agent（`herdr agent start --kind grok` 開的），`GROK_HOME` 指到暫存目錄。
async fn grok_child(history: Option<&str>) -> Child {
    let env = tt::env().await;
    let app = env.app.clone();
    let home = tt::track(std::env::temp_dir().join(format!("am-grok-home-{}", db::ulid())));
    let cwd = env.repo.to_string_lossy().into_owned();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, managed_by, hook_token, env_json, cwd, created_at)
         VALUES (?,?,'g8','grok','[]',0,0,'child','tok',?,?,?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(json!({"GROK_HOME": home.to_string_lossy()}).to_string())
    .bind(&cwd)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','idle','ws-1',?,'tab-1',1,'parent-g8','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(format!("pane-{bot_id}"))
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    env.herdr.tabs.lock().unwrap().push(tt::MockTab {
        tab_id: "tab-1".into(),
        workspace_id: "ws-1".into(),
        label: "t".into(),
        panes: vec![format!("pane-{bot_id}")],
    });
    env.herdr.set_screen(&format!("pane-{bot_id}"), WELCOME);
    if let Some(h) = history {
        write_session(&home, &cwd, SID, 999_999_999, h);
    }
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
    Child { env, bot, run_id, conv, home }
}

/// grok 的目錄長相：`sessions/<cwd 的 URL 編碼>/<id>/chat_history.jsonl`，`active_sessions.json` 一行一顆開著的 grok。
fn write_session(home: &Path, cwd: &str, sid: &str, pid: i64, history: &str) {
    let dir = home.join("sessions").join(cwd.replace('/', "%2F")).join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("chat_history.jsonl"), history).unwrap();
    let path = home.join("active_sessions.json");
    let mut active: Vec<Value> = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    active.push(json!({"session_id": sid, "pid": pid, "cwd": cwd, "opened_at": "2026-10-03T10:01:25.135106874Z"}));
    std::fs::write(&path, serde_json::to_string(&active).unwrap()).unwrap();
}

async fn messages(app: &Arc<App>, conv: &str) -> Vec<(String, String, String, Option<String>)> {
    sqlx::query_as("SELECT role, content, source, relay_from FROM messages WHERE conversation_id = ? ORDER BY id")
        .bind(conv)
        .fetch_all(&app.db)
        .await
        .unwrap()
}

/// 2026-10-03 g8：對話裡第一則是歡迎畫面的選單、交辦沒記、回覆是窄 pane 刮下來斷行加 `…` 的殘片、後面幾輪沒有。
/// 對話檔讀得到時，每一問一個回合、原文照存，畫面上的東西一個字都不存。
#[tokio::test]
async fn a_hookless_grok_child_records_its_exchanges_from_chat_history() {
    let c = grok_child(Some(FRESH)).await;
    let app = c.env.app.clone();
    let want = parse_chat_history(FRESH);

    assert!(super::super::poller::capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
    let msgs = messages(&app, &c.conv).await;
    let got: Vec<(&str, &str, &str)> = msgs.iter().map(|(r, t, s, _)| (r.as_str(), t.as_str(), s.as_str())).collect();
    assert_eq!(
        got,
        vec![
            ("user", want[0].prompt.as_str(), "transcript"),
            ("assistant", want[0].reply.as_deref().unwrap(), "transcript"),
            ("user", want[1].prompt.as_str(), "transcript"),
            ("assistant", want[1].reply.as_deref().unwrap(), "transcript"),
        ],
    );
    let run = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
    assert_eq!(run.native_session_id.as_deref(), Some(SID), "記住這個 pane 的 session");
    let keys: Vec<(String, String)> = sqlx::query_as("SELECT status, native_turn_id FROM turns WHERE conversation_id = ? ORDER BY created_at, rowid")
        .bind(&c.conv)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(keys, vec![("completed".into(), "p0".into()), ("completed".into(), "p1".into())]);

    // herdr 同一輪報好幾次 idle、daemon 重啟再認領：不重記。
    assert!(!super::super::poller::capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
    assert!(!super::super::poller::capture_hookless_turn_locked(&app, &c.run_id, true).await.unwrap());
    assert_eq!(messages(&app, &c.conv).await.len(), 4);
}

/// 對話檔還沒有（grok 停在啟動選單、session 還沒開）時退回畫面，但選單不是回覆。
#[tokio::test]
async fn without_a_session_the_startup_menu_is_not_stored() {
    let c = grok_child(None).await;
    let app = c.env.app.clone();
    sqlx::query("UPDATE runs SET last_read_tail_hash = 'x' WHERE id = ?").bind(&c.run_id).execute(&app.db).await.unwrap();
    assert!(!super::super::poller::capture_hookless_turn_locked(&app, &c.run_id, true).await.unwrap());
    assert!(!super::super::poller::capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
    assert!(messages(&app, &c.conv).await.is_empty(), "歡迎畫面的選單不能變成 assistant 訊息");
}

/// 同一個 cwd 開著好幾顆 grok：靠行程環境的 `HERDR_PANE_ID` 認出這顆 pane 自己的那一個，不是最新的、也不是第一個。
#[tokio::test]
async fn several_grok_sessions_in_one_cwd_each_pane_reads_its_own() {
    let c = grok_child(None).await;
    let app = c.env.app.clone();
    let cwd = c.env.repo.to_string_lossy().into_owned();
    let pane = format!("pane-{}", c.bot.id);
    let mut proc = std::process::Command::new("sleep")
        .arg("60")
        .env("HERDR_PANE_ID", &pane)
        .env("HERDR_SESSION", "test")
        .spawn()
        .unwrap();
    write_session(&c.home, &cwd, "01a10187-dcbd-7702-8927-d12eab9fd583", 1, COMPACTED);
    write_session(&c.home, &cwd, SID, i64::from(proc.id() as i32), FRESH);
    write_session(&c.home, &cwd, "01a10187-dcb8-7f92-a98a-9391fc3210e1", 2, COMPACTED);

    let synced = sync_locked(&app, &c.run_id).await;
    let _ = proc.kill();
    let _ = proc.wait();
    assert_eq!(synced.unwrap(), Synced::Read { imported: 2, pending: None });
    let run = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
    assert_eq!(run.native_session_id.as_deref(), Some(SID));
}

/// 父 bot `herdr agent prompt` 先送 `/effort high` 再送交辦：slash 指令不是對話、不開回合，交辦才是使用者訊息，
/// 回覆掛在同一個回合上，標寄件者。
#[tokio::test]
async fn a_relayed_prompt_is_the_user_message_and_its_reply_lands_on_the_same_turn() {
    let c = grok_child(Some(FRESH)).await;
    let app = c.env.app.clone();
    let want = parse_chat_history(FRESH);
    let parent = tt::claude_bot(&app, &c.env.project_id, "parent").await;
    let run = db::run(&app.db, &c.run_id).await.unwrap().unwrap();

    assert!(super::super::relay_watch::open_turn(&app, &run, &parent.id, "/effort high").await.is_none(), "slash 指令不開回合");
    assert!(messages(&app, &c.conv).await.is_empty(), "slash 指令的回音不是對話");
    let tid = super::super::relay_watch::open_turn(&app, &run, &parent.id, &want[0].prompt).await.expect("交辦開回合");
    // 收件方忙的時候送來的下一句：先記下來，等它自己的回合。
    assert!(super::super::relay_watch::open_turn(&app, &run, &parent.id, &want[1].prompt).await.is_none());

    assert!(super::super::poller::try_fallback(&app, &c.run_id, Some(&tid)).await.unwrap(), "對話檔收掉這一回合");
    let msgs = messages(&app, &c.conv).await;
    let got: Vec<(&str, &str, &str, Option<&str>)> =
        msgs.iter().map(|(r, t, s, f)| (r.as_str(), t.as_str(), s.as_str(), f.as_deref())).collect();
    assert_eq!(
        got,
        vec![
            ("user", want[0].prompt.as_str(), "hook", Some(parent.id.as_str())),
            ("user", want[1].prompt.as_str(), "hook", Some(parent.id.as_str())),
            ("assistant", want[0].reply.as_deref().unwrap(), "transcript", None),
            ("assistant", want[1].reply.as_deref().unwrap(), "transcript", None),
        ],
    );
    let turns: Vec<(String, String, String)> =
        sqlx::query_as("SELECT m.content, t.id, t.status FROM messages m JOIN turns t ON t.id = m.turn_id WHERE m.conversation_id = ? AND m.role = 'user' ORDER BY m.id")
            .bind(&c.conv)
            .fetch_all(&app.db)
            .await
            .unwrap();
    assert_eq!(turns[0].1, tid, "交辦的回覆掛在派工開的那一回合");
    assert_eq!(turns[0].2, "completed");
    assert_ne!(turns[1].1, tid, "第二句有自己的回合");
}

/// 對話檔讀得到之前，畫面備援已經收了一輪（截斷的回音、折行的回覆）：讀到之後換成原文，不另記一份。
#[tokio::test]
async fn a_scraped_exchange_is_replaced_by_the_transcript() {
    let c = grok_child(Some(FRESH)).await;
    let app = c.env.app.clone();
    let want = parse_chat_history(FRESH);
    let tid = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, completed_at) VALUES (?,?,?,'external','completed_fallback','ok',?,?)",
    )
    .bind(&tid)
    .bind(&c.conv)
    .bind(&c.run_id)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let clipped: String = want[0].prompt.chars().take(30).collect::<String>() + " …";
    insert_message(&app, &c.conv, Some(&tid), "user", &clipped, "terminal_fallback", false, None).await.unwrap();
    insert_message(&app, &c.conv, Some(&tid), "assistant", "我先讀 AG Man 指示，並在既有\nworktree 看…", "terminal_fallback", true, Some("snap")).await.unwrap();

    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 2, pending: None });
    let msgs = messages(&app, &c.conv).await;
    assert_eq!(msgs.len(), 4, "{msgs:#?}");
    assert_eq!((msgs[0].1.as_str(), msgs[0].2.as_str()), (want[0].prompt.as_str(), "transcript"));
    assert_eq!((msgs[1].1.as_str(), msgs[1].2.as_str()), (want[0].reply.as_deref().unwrap(), "transcript"));
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(&tid).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "completed");
}

/// 壓縮過的檔：重寫進來的那一問照記；派工那一問還在跑，回合留在飛，不拿畫面收。
#[tokio::test]
async fn a_prompt_still_running_in_the_transcript_stays_in_flight() {
    let c = grok_child(Some(COMPACTED)).await;
    let app = c.env.app.clone();
    let want = parse_chat_history(COMPACTED);
    let parent = tt::claude_bot(&app, &c.env.project_id, "parent").await;
    let run = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
    let tid = super::super::relay_watch::open_turn(&app, &run, &parent.id, &want[1].prompt).await.unwrap();

    assert!(!super::super::poller::try_fallback(&app, &c.run_id, Some(&tid)).await.unwrap(), "還沒答完：不收");
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(&tid).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "in_flight");
    let replies: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'assistant'")
        .bind(&c.conv)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, vec![want[0].reply.clone().unwrap()], "只有壓縮前那一問的回覆，畫面上的選單沒被存");
}

/// 沒有 `prompt_index` 的鑰匙必須跨行程穩定。`DefaultHasher` 每趟程序一把種子，daemon 重啟會把同一句當成新回合。
#[test]
fn a_prompt_without_an_index_keeps_the_same_key_across_processes() {
    let ex = parse_chat_history(COMPACTED);
    let expect = format!("q{}", crate::supervisor::cli_refresh::short_hash(ex[0].prompt.trim().as_bytes()));
    assert_eq!(ex[0].key(), expect);
}

fn history_file(home: &Path, cwd: &str, sid: &str) -> PathBuf {
    home.join("sessions").join(cwd.replace('/', "%2F")).join(sid).join("chat_history.jsonl")
}

fn one_exchange(index: Option<u64>, prompt: &str, reply: &str) -> String {
    let idx = index.map(|i| format!(",\"prompt_index\":{i}")).unwrap_or_default();
    format!(
        "{{\"type\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"<user_query>\\n{prompt}\\n</user_query>\"}}]{idx}}}\n{{\"type\":\"assistant\",\"content\":{reply:?}}}\n"
    )
}

/// 壓縮把已經記過的那一問重寫成沒有 `prompt_index`：同一句不能再記一份。
#[tokio::test]
async fn a_compacted_replay_does_not_record_the_same_prompt_again() {
    let prompt = "第一問的原文，壓縮後不帶 index";
    let reply = "第一問的回覆";
    let c = grok_child(Some(&one_exchange(Some(0), prompt, reply))).await;
    let app = c.env.app.clone();
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
    let cwd = c.env.repo.to_string_lossy().into_owned();
    std::fs::write(history_file(&c.home, &cwd, SID), one_exchange(None, prompt, reply)).unwrap();
    assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 0, pending: None });
    assert_eq!(messages(&app, &c.conv).await.len(), 2, "壓縮重播不能再插一組 user/assistant");
}

/// 壓縮後 grok 可能把 `prompt_index` 從頭用。舊的 `p0` 還在，新的另一句也是 0，不能被去重吞掉。
#[tokio::test]
async fn a_reused_prompt_index_still_records_the_new_question() {
    let c = grok_child(Some(&one_exchange(Some(0), "舊的第一問", "舊回覆"))).await;
    let app = c.env.app.clone();
    sync_locked(&app, &c.run_id).await.unwrap();
    let cwd = c.env.repo.to_string_lossy().into_owned();
    std::fs::write(history_file(&c.home, &cwd, SID), one_exchange(Some(0), "壓縮後全新的一問", "新回覆")).unwrap();
    let synced = sync_locked(&app, &c.run_id).await.unwrap();
    assert_eq!(synced, Synced::Read { imported: 1, pending: None });
    let texts: Vec<String> = messages(&app, &c.conv).await.into_iter().map(|(_, t, _, _)| t).collect();
    assert!(texts.iter().any(|t| t == "壓縮後全新的一問"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "舊的第一問"), "{texts:?}");
}

/// pid 被重用、active_sessions 還指著別顆 run 已綁的 session：不能把那顆的對話記到這顆。
#[tokio::test]
async fn a_reused_pid_does_not_import_a_session_another_run_holds() {
    let c = grok_child(None).await;
    let app = c.env.app.clone();
    let cwd = c.env.repo.to_string_lossy().into_owned();
    let pane = format!("pane-{}", c.bot.id);
    let mut proc = std::process::Command::new("sleep").arg("60").env("HERDR_PANE_ID", &pane).env("HERDR_SESSION", "test").spawn().unwrap();
    write_session(&c.home, &cwd, SID, i64::from(proc.id() as i32), FRESH);
    let other_bot = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, managed_by, hook_token, env_json, cwd, created_at)
         VALUES (?,?,'other','grok','[]',0,0,'child','tok','{}',?,?)",
    )
    .bind(&other_bot)
    .bind(&c.env.project_id)
    .bind(&cwd)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let other = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, adopted, native_session_id, started_at) VALUES (?,?,'running','idle',1,?,?)",
    )
    .bind(&other)
    .bind(&other_bot)
    .bind(SID)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let synced = sync_locked(&app, &c.run_id).await;
    let _ = proc.kill();
    let _ = proc.wait();
    assert_eq!(synced.unwrap(), Synced::Unavailable);
    assert!(messages(&app, &c.conv).await.is_empty(), "別顆 grok 的對話不能記到這顆");
    let run = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
    assert_ne!(run.native_session_id.as_deref(), Some(SID));
}
