//! issue #92 的端到端情境：身分 A 跑到一半撞額度 → 換成身分 B → `--resume` 接回**同一段** session。
//!
//! 走真的路徑（`restart_bot_with`、`hookrecv::process`、`prompt_*`、`flush_queued_locked`），herdr 是 mock：
//! 1. 身分 A 起來、`SessionStart` 回報 session `S`；一回合在飛時撞額度，`StopFailure` 把它收成失敗。
//! 2. 換成身分 B、`?resume=native` 重啟：對話檔搬進 B 的 `CLAUDE_CONFIG_DIR`，argv 帶 `--resume S`，pane env 是 B 的目錄
//!    與新 run 自己的 `AM_RUN_ID`。
//! 3. 身分 A 那個行程遲到的 `SessionStart`（同一個 `S`）不算驗證：世代圍籬認得出它不是新行程送的。
//! 4. 驗證前：AGM 派工排進佇列、使用者 409、flush 不動；`SessionStart`（B 的行程、同一個 `S`）一到就放行，
//!    派工真的打進 B 的 pane。
//! 5. B 的第一回合在飛時，A 遲到的撞額度 `StopFailure` 與半截回覆才到——擋下；B 自己的 `Stop` 照常收尾。
//! 6. 撞額度那一則**從頭到尾沒有被重送**：換身分、接回、放行都不會把它再打進 pane。

use super::*;
use crate::testing as tt;

const S: &str = "sess-continuous";
const CWD_KEY: &str = "-Users-m4p-project-quota";

async fn add_identity(app: &Arc<App>, name: &str, dir: &std::path::Path) {
    let (name, dir) = (name.to_string(), dir.to_string_lossy().into_owned());
    app.cfg
        .update(move |c| {
            c.identities.push(crate::config::IdentityCfg {
                name: name.clone(),
                kind: "claude".into(),
                host: None,
                env: [("CLAUDE_CONFIG_DIR".to_string(), dir.clone())].into(),
                args: vec![],
            });
            Ok(())
        })
        .await
        .unwrap();
}

fn hook(bot_id: &str, run_id: &str, payload: Value) -> crate::hookrecv::HookBody {
    crate::hookrecv::HookBody {
        bot_id: bot_id.to_string(),
        provider: "claude".into(),
        payload,
        received_at: None,
        truncated: false,
        run_id: Some(run_id.to_string()),
    }
}

/// herdr 收過的每一段要打進 pane 的字（`pane.send_text` 與 `agent.prompt`）。
fn typed(e: &tt::Env) -> Vec<String> {
    let mut out = Vec::new();
    for m in ["pane.send_text", "agent.prompt"] {
        for p in e.herdr.calls_to(m) {
            out.push(p.to_string());
        }
    }
    out
}

/// 最後一次開 pane（`workspace.create` 或 `tab.create`，照呼叫順序）帶的 env。
fn last_env(e: &tt::Env) -> Value {
    e.herdr
        .calls
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(m, _)| m == "tab.create" || m == "workspace.create")
        .and_then(|(_, p)| p.get("env").cloned())
        .expect("a pane was created with an env")
}

fn last_start_args(e: &tt::Env) -> Vec<String> {
    let p = e.herdr.calls_to("agent.start").pop().expect("agent.start was called");
    p["args"].as_array().unwrap().iter().filter_map(Value::as_str).map(String::from).collect()
}

#[tokio::test]
async fn quota_on_identity_a_then_switching_to_b_resumes_the_exact_same_session() {
    let e = tt::env().await;
    let app = e.app.clone();
    let dir_a = e.dir.join("cc-a");
    let dir_b = e.dir.join("cc-b");
    add_identity(&app, "cc-a", &dir_a).await;
    add_identity(&app, "cc-b", &dir_b).await;
    let bot = tt::claude_bot(&app, &e.project_id, "quota").await;
    sqlx::query("UPDATE bots SET identity='cc-a' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();

    // ---- 1. 身分 A：起來、回報 session、一回合在飛時撞額度
    let run_a = start_bot(&app, &bot.id).await.unwrap();
    assert_eq!(last_env(&e)["CLAUDE_CONFIG_DIR"], json!(dir_a.to_string_lossy()));
    let transcript_a = dir_a.join("projects").join(CWD_KEY).join(format!("{S}.jsonl"));
    std::fs::create_dir_all(transcript_a.parent().unwrap()).unwrap();
    std::fs::write(&transcript_a, "{\"type\":\"user\",\"message\":\"先做 X\"}\n").unwrap();
    crate::hookrecv::process(
        &app,
        &hook(&bot.id, &run_a, json!({"hook_event_name": "SessionStart", "session_id": S, "source": "startup",
                                      "transcript_path": transcript_a.to_string_lossy()})),
    )
    .await
    .unwrap();
    let quota_turn = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','先做 X（撞額度的那一句）',?)",
    )
    .bind(&quota_turn)
    .bind(&conv)
    .bind(&run_a)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let limit = json!({"hook_event_name": "StopFailure", "session_id": S, "prompt_id": "p-a",
                       "reason": "You've hit your usage limit · resets 5pm"});
    crate::hookrecv::process(&app, &hook(&bot.id, &run_a, limit.clone())).await.unwrap();
    let status = |id: String| {
        let app = app.clone();
        async move { sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap() }
    };
    assert_eq!(status(quota_turn.clone()).await, "failed", "撞額度的回合收成失敗，不是掛著等重送");

    // ---- 2. 換身分 B、要求接回原對話
    sqlx::query("UPDATE bots SET identity='cc-b' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
    let run_b = restart_bot_with(&app, &bot.id, strict).await.unwrap();
    assert_ne!(run_b, run_a);
    let args = last_start_args(&e);
    assert!(args.windows(2).any(|w| w == ["--resume", S]), "明確帶 DB 記的那個 session，不靠「最近一段」：{args:?}");
    let env_b = last_env(&e);
    assert_eq!(env_b["CLAUDE_CONFIG_DIR"], json!(dir_b.to_string_lossy()), "新行程跑在 B 的帳號底下");
    assert_eq!(env_b["AM_RUN_ID"], json!(run_b), "新行程的 hook 會帶自己的 run id");
    let transcript_b = dir_b.join("projects").join(CWD_KEY).join(format!("{S}.jsonl"));
    assert_eq!(std::fs::read_to_string(&transcript_b).unwrap(), std::fs::read_to_string(&transcript_a).unwrap(), "對話檔搬進 B 的目錄");
    let (resume_sid, outcome): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT resume_session_id, resume_outcome FROM runs WHERE id=?").bind(&run_b).fetch_one(&app.db).await.unwrap();
    assert_eq!((resume_sid.as_deref(), outcome), (Some(S), None), "要接的 session 記下來了，還沒有結論");

    // B 的 pane 是活的、能打字（沒有閘門的話，下面的 flush 就會真的送出去）。
    let pane_b: String = sqlx::query_scalar("SELECT pane_id FROM runs WHERE id=?").bind(&run_b).fetch_one(&app.db).await.unwrap();
    e.herdr.live_pane(&pane_b, tt::LivePane { width: Some(120), transcript_file: Some(transcript_b.clone()), ..Default::default() });
    db::set_pane_typed(&app.db, &run_b).await.unwrap();
    let in_pane = |text: &str| e.herdr.pane(&pane_b).map_or(0, |p| p.transcript.iter().filter(|l| l.contains(text)).count());
    let outcome_now = || {
        let app = app.clone();
        let run_b = run_b.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, Option<String>)>("SELECT resume_session_id, resume_outcome FROM runs WHERE id=?")
                .bind(run_b)
                .fetch_one(&app.db)
                .await
                .unwrap()
        }
    };

    // ---- 3. 身分 A 那個行程遲到的 SessionStart（spool 重播）：同一個 S，但不是新行程的回報，不算驗證
    crate::hookrecv::process(
        &app,
        &hook(&bot.id, &run_a, json!({"hook_event_name": "SessionStart", "session_id": S, "source": "startup",
                                      "transcript_path": transcript_a.to_string_lossy()})),
    )
    .await
    .unwrap();
    assert_eq!(outcome_now().await, (Some(S.to_string()), None), "舊行程的回報不能冒充新行程接回了");

    // ---- 4. 驗證前：派工排隊、使用者 409、flush 不動（一個字都沒進 B 的 pane）
    let dispatch = prompt_relayed_queueable(&app, &bot.id, "換身分之後的派工", "agm-after-switch", Some(crate::agent_relay::DAEMON_SENDER))
        .await
        .unwrap();
    assert_eq!(dispatch.delivery, "queued");
    assert!(matches!(prompt(&app, &bot.id, "使用者插一句", "user-after-switch").await,
                     Err(LcError::Conflict(ref v)) if v["reason"] == "resume_unverified"));
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&dispatch.turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!((t.status.as_str(), t.flush_retries, t.run_id.as_deref()), ("queued", 0, None), "驗證前不 claim、不花重試");
    assert_eq!(in_pane("換身分之後的派工"), 0);

    // B 的行程回報：同一個 session → 接回了
    crate::hookrecv::process(
        &app,
        &hook(&bot.id, &run_b, json!({"hook_event_name": "SessionStart", "session_id": S, "source": "resume",
                                      "transcript_path": transcript_b.to_string_lossy()})),
    )
    .await
    .unwrap();
    assert_eq!(outcome_now().await, (None, Some("verified".to_string())), "同一段 session 接回來了");
    let native: Option<String> = sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run_b).fetch_one(&app.db).await.unwrap();
    assert_eq!(native.as_deref(), Some(S));
    let lost: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='system' AND content LIKE '%接不回%'")
        .bind(&conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(lost, 0, "接回了就沒有 context_lost");

    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&dispatch.turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!((t.status.as_str(), t.delivery.as_str(), t.run_id.as_deref()), ("in_flight", "ok", Some(run_b.as_str())), "驗證過了才送，送進 B 這一代");
    assert_eq!(in_pane("換身分之後的派工"), 1);

    // ---- 5. B 的第一回合在飛時，A 那個行程的撞額度與半截回覆才到（遠端 spool 晚到）：同一個 S，全部擋下
    crate::hookrecv::process(&app, &hook(&bot.id, &run_a, limit)).await.unwrap();
    let late_reply = json!({"hook_event_name": "Stop", "session_id": S, "prompt_id": "p-a",
                            "last_assistant_message": "身分 A 那一回合的半截回覆"});
    crate::hookrecv::process(&app, &hook(&bot.id, &run_a, late_reply)).await.unwrap();
    assert_eq!(status(dispatch.turn_id.clone()).await, "in_flight", "A 遲到的撞額度／回覆不准收掉 B 的回合");
    // B 自己答完：照常收尾。
    let done = json!({"hook_event_name": "Stop", "session_id": S, "prompt_id": "p-b",
                      "last_assistant_message": "B 接著原本的脈絡做完了"});
    crate::hookrecv::process(&app, &hook(&bot.id, &run_b, done)).await.unwrap();
    assert_eq!(status(dispatch.turn_id.clone()).await, "completed");
    let replies: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='assistant' ORDER BY created_at")
        .bind(&conv)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, vec!["B 接著原本的脈絡做完了".to_string()], "A 的半截回覆沒有混進來");

    // ---- 6. 撞額度那一句沒有被重送；DB 裡也只有那一筆
    assert_eq!(in_pane("撞額度的那一句"), 0, "換身分、接回、放行都不可以把撞額度那一句再打一次");
    assert!(typed(&e).iter().all(|s| !s.contains("撞額度的那一句")), "{:?}", typed(&e));
    let copies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE prompt_text LIKE '%撞額度的那一句%'")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(copies, 1);
    assert_eq!(status(quota_turn).await, "failed", "沒有被翻回進行中");
    forget_queue_retry_timer(&bot.id);
    stop_bot(&app, &bot.id).await.unwrap();
}

/// issue #106：換身分**之前**就排著的 AGM 派工，重啟之後還在，接回驗證完才送，而且只送一次。
/// 以前 `stop_bot_locked` 在新 run 建立前就 `revoke_orphaned_queued_turns`，那一刻沒有 active run，
/// 派工被當成孤兒撤掉——`resume_gate` 再完美也沒有東西可以放行。
///
/// 情境：A 撞額度的那一回合還掛著（StopFailure 還在遠端 spool 裡沒到），AGM 的派工排在它後面；
/// 這時候換身分重啟，重啟收掉那一回合、派工留下來給 B。
#[tokio::test]
async fn a_dispatch_queued_before_the_switch_survives_the_restart_and_goes_out_exactly_once() {
    let e = tt::env().await;
    let app = e.app.clone();
    let dir_a = e.dir.join("cc-a");
    let dir_b = e.dir.join("cc-b");
    add_identity(&app, "cc-a", &dir_a).await;
    add_identity(&app, "cc-b", &dir_b).await;
    let bot = tt::claude_bot(&app, &e.project_id, "queued-switch").await;
    sqlx::query("UPDATE bots SET identity='cc-a' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let turn = |id: String| {
        let app = app.clone();
        async move { sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap() }
    };

    let run_a = start_bot(&app, &bot.id).await.unwrap();
    let transcript_a = dir_a.join("projects").join(CWD_KEY).join(format!("{S}.jsonl"));
    std::fs::create_dir_all(transcript_a.parent().unwrap()).unwrap();
    std::fs::write(&transcript_a, "{\"type\":\"user\",\"message\":\"先做 X\"}\n").unwrap();
    crate::hookrecv::process(
        &app,
        &hook(&bot.id, &run_a, json!({"hook_event_name": "SessionStart", "session_id": S, "source": "startup",
                                      "transcript_path": transcript_a.to_string_lossy()})),
    )
    .await
    .unwrap();
    let quota_turn = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','先做 X（撞額度的那一句）',?)",
    )
    .bind(&quota_turn)
    .bind(&conv)
    .bind(&run_a)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    // A 還在回合中：AGM 的派工排在後面。
    let dispatch = prompt_relayed_queueable(&app, &bot.id, "撞額度之前就排好的派工", "agm-before-switch", Some(crate::agent_relay::DAEMON_SENDER))
        .await
        .unwrap();
    assert_eq!(dispatch.delivery, "queued");

    // ---- 換身分重啟（回合中）：撞額度那一回合被收掉，派工留下來
    sqlx::query("UPDATE bots SET identity='cc-b' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
    let run_b = restart_bot_with(&app, &bot.id, strict).await.unwrap();
    assert_eq!(turn(quota_turn.clone()).await.status, "failed", "被重啟打斷的那一回合收成失敗，不會被重送");
    let t = turn(dispatch.turn_id.clone()).await;
    assert_eq!((t.status.as_str(), t.run_id.as_deref()), ("queued", None), "派工不是孤兒：重啟之後還排著");
    let revoked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'")
        .bind(&dispatch.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(revoked, 0, "沒有「一併撤銷」的說明");

    // ---- 新 run 接回、驗證完才送
    let transcript_b = dir_b.join("projects").join(CWD_KEY).join(format!("{S}.jsonl"));
    let pane_b: String = sqlx::query_scalar("SELECT pane_id FROM runs WHERE id=?").bind(&run_b).fetch_one(&app.db).await.unwrap();
    e.herdr.live_pane(&pane_b, tt::LivePane { width: Some(120), transcript_file: Some(transcript_b.clone()), ..Default::default() });
    db::set_pane_typed(&app.db, &run_b).await.unwrap();
    // 真的送進 B 的 pane 幾次（mock 的 pane 只在字被送出時記一筆）。
    let sent = || e.herdr.pane(&pane_b).map_or(0, |p| p.transcript.iter().filter(|l| l.contains("撞額度之前就排好的派工")).count());
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    let t = turn(dispatch.turn_id.clone()).await;
    assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 0), "還沒驗證：等");
    assert_eq!(sent(), 0);

    crate::hookrecv::process(
        &app,
        &hook(&bot.id, &run_b, json!({"hook_event_name": "SessionStart", "session_id": S, "source": "resume",
                                      "transcript_path": transcript_b.to_string_lossy()})),
    )
    .await
    .unwrap();
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    let t = turn(dispatch.turn_id.clone()).await;
    assert_eq!((t.status.as_str(), t.delivery.as_str(), t.run_id.as_deref()), ("in_flight", "ok", Some(run_b.as_str())));
    assert_eq!(sent(), 1);

    // 多叫醒幾次、A 遲到的撞額度也到了、B 答完——都不會再送一次。
    flush_queued_locked(&app, &bot.id).await.unwrap();
    let late = json!({"hook_event_name": "StopFailure", "session_id": S, "prompt_id": "p-a",
                      "reason": "You've hit your usage limit · resets 5pm"});
    crate::hookrecv::process(&app, &hook(&bot.id, &run_a, late)).await.unwrap();
    assert_eq!(turn(dispatch.turn_id.clone()).await.status, "in_flight");
    let done = json!({"hook_event_name": "Stop", "session_id": S, "prompt_id": "p-b", "last_assistant_message": "派工做完了"});
    crate::hookrecv::process(&app, &hook(&bot.id, &run_b, done)).await.unwrap();
    assert_eq!(turn(dispatch.turn_id.clone()).await.status, "completed");
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!(sent(), 1, "只送一次");
    let copies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE prompt_text = '撞額度之前就排好的派工'")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(copies, 1);
    assert!(typed(&e).iter().all(|s| !s.contains("撞額度的那一句")));
    forget_queue_retry_timer(&bot.id);
    stop_bot(&app, &bot.id).await.unwrap();
}

/// 身分 A 起來、pane 可以打字，一回合在飛時 AGM 的派工排在後面。回傳 `(bot, run_a, pane_a, 派工的 turn)`。
async fn dispatch_queued_behind_a_turn(e: &tt::Env, name: &str) -> (db::Bot, String, String, String) {
    let app = e.app.clone();
    add_identity(&app, "cc-a", &e.dir.join("cc-a")).await;
    add_identity(&app, "cc-b", &e.dir.join("cc-b")).await;
    let bot = tt::claude_bot(&app, &e.project_id, name).await;
    sqlx::query("UPDATE bots SET identity='cc-a' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let run_a = start_bot(&app, &bot.id).await.unwrap();
    let transcript_a = e.dir.join("cc-a").join("projects").join(CWD_KEY).join(format!("{S}.jsonl"));
    std::fs::create_dir_all(transcript_a.parent().unwrap()).unwrap();
    std::fs::write(&transcript_a, "{\"type\":\"user\",\"message\":\"先做 X\"}\n").unwrap();
    crate::hookrecv::process(
        &app,
        &hook(&bot.id, &run_a, json!({"hook_event_name": "SessionStart", "session_id": S, "source": "startup",
                                      "transcript_path": transcript_a.to_string_lossy()})),
    )
    .await
    .unwrap();
    // A 的 pane 是活的、能打字：沒有額度閘門的話，下面的 flush 會真的把派工送進 A。
    let pane_a: String = sqlx::query_scalar("SELECT pane_id FROM runs WHERE id=?").bind(&run_a).fetch_one(&app.db).await.unwrap();
    e.herdr.live_pane(&pane_a, tt::LivePane { width: Some(120), transcript_file: Some(transcript_a.clone()), ..Default::default() });
    db::set_pane_typed(&app.db, &run_a).await.unwrap();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','先做 X（撞額度的那一句）',?)",
    )
    .bind(db::ulid())
    .bind(&conv)
    .bind(&run_a)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let dispatch = prompt_relayed_queueable(&app, &bot.id, "撞額度後排著的派工", &format!("agm-{name}"), Some(crate::agent_relay::DAEMON_SENDER))
        .await
        .unwrap();
    assert_eq!(dispatch.delivery, "queued");
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    (bot, run_a, pane_a, dispatch.turn_id)
}

/// A 撞額度的那一則 `StopFailure`。
fn a_limit(bot_id: &str, run_a: &str) -> crate::hookrecv::HookBody {
    hook(bot_id, run_a, json!({"hook_event_name": "StopFailure", "session_id": S, "prompt_id": "p-a",
                               "reason": "You've hit your session limit · resets 5pm"}))
}

/// issue #108 的起點：[`dispatch_queued_behind_a_turn`]，接著撞額度的 `StopFailure` **先到**（比換身分早）。
/// 回傳 `(bot, run_a, pane_a, 派工的 turn)`。
async fn quota_hit_with_a_dispatch_queued(e: &tt::Env, name: &str) -> (db::Bot, String, String, String) {
    let app = e.app.clone();
    let (bot, run_a, pane_a, dispatch) = dispatch_queued_behind_a_turn(e, name).await;

    // StopFailure 先到：回合收成失敗，而且 A 的撞限在推回合結束**之前**就記下來了。
    crate::hookrecv::process(&app, &a_limit(&bot.id, &run_a)).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "StopFailure 就記下撞限，不等讀畫面");

    // 回合結束叫醒的 flush（測試裡 `schedule_flush_queued` 是 no-op，直接叫）：派工留在佇列，沒有送進 A。
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&dispatch).fetch_one(&app.db).await.unwrap();
    assert_eq!((t.status.as_str(), t.flush_retries, t.run_id.as_deref()), ("queued", 0, None), "A 沒額度：不 claim、不花重試");
    assert_eq!(e.herdr.pane(&pane_a).map_or(0, |p| p.transcript.iter().filter(|l| l.contains("撞額度後排著的派工")).count()), 0);
    assert!(queue_retry_timer_armed(&bot.id), "掛了 timer 回來再看");
    forget_queue_retry_timer(&bot.id);
    (bot, run_a, pane_a, dispatch)
}

fn sent_to(e: &tt::Env, pane: &str) -> usize {
    e.herdr.pane(pane).map_or(0, |p| p.transcript.iter().filter(|l| l.contains("撞額度後排著的派工")).count())
}

/// issue #108 驗收（換身分那條）：A 撞額度 → StopFailure 先到 → 派工沒送進 A → 換到 B → 接回驗證完送出，只送一次。
#[tokio::test]
async fn a_dispatch_held_for_quota_goes_to_identity_b_after_the_switch_exactly_once() {
    let e = tt::env().await;
    let app = e.app.clone();
    let (bot, _run_a, pane_a, dispatch) = quota_hit_with_a_dispatch_queued(&e, "quota-to-b").await;

    sqlx::query("UPDATE bots SET identity='cc-b' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
    let run_b = restart_bot_with(&app, &bot.id, strict).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "B 這個身分沒有撞限");
    let transcript_b = e.dir.join("cc-b").join("projects").join(CWD_KEY).join(format!("{S}.jsonl"));
    let pane_b: String = sqlx::query_scalar("SELECT pane_id FROM runs WHERE id=?").bind(&run_b).fetch_one(&app.db).await.unwrap();
    e.herdr.live_pane(&pane_b, tt::LivePane { width: Some(120), transcript_file: Some(transcript_b.clone()), ..Default::default() });
    db::set_pane_typed(&app.db, &run_b).await.unwrap();
    crate::hookrecv::process(
        &app,
        &hook(&bot.id, &run_b, json!({"hook_event_name": "SessionStart", "session_id": S, "source": "resume",
                                      "transcript_path": transcript_b.to_string_lossy()})),
    )
    .await
    .unwrap();
    for _ in 0..3 {
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
    }
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&dispatch).fetch_one(&app.db).await.unwrap();
    assert_eq!((t.status.as_str(), t.delivery.as_str(), t.run_id.as_deref()), ("in_flight", "ok", Some(run_b.as_str())));
    assert_eq!((sent_to(&e, &pane_b), sent_to(&e, &pane_a)), (1, 0), "送進 B、只送一次；A 一次都沒有");
    forget_queue_retry_timer(&bot.id);
    stop_bot(&app, &bot.id).await.unwrap();
}

/// issue #108 驗收（額度回來那條）：同一個身分 A，新的讀數顯示 5 小時窗在撞限之後重開了（撞限被校正作廢）
/// → 派工送出，只送一次。
#[tokio::test]
async fn a_dispatch_held_for_quota_goes_out_once_when_identity_a_gets_its_quota_back() {
    let e = tt::env().await;
    let app = e.app.clone();
    let (bot, run_a, pane_a, dispatch) = quota_hit_with_a_dispatch_queued(&e, "quota-back").await;

    // 還在擋的時候多叫醒幾次也不會送。
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!(sent_to(&e, &pane_a), 0);

    // A 的新讀數：5 小時窗是撞限之後才開的（剛重置、用了 3%）——撞限作廢（`quota::recalibrate_limit_hit`）。
    let base = crate::quota::quota_base_for_host(&app, LOCAL_HOST, "claude", Some("cc-a")).await;
    let reopened = crate::quota::Quota {
        five_hour: Some(crate::quota::Window { observed_at: None, used_pct: 3.0, resets_at: Some(db::iso_at(chrono::Utc::now() + chrono::Duration::hours(5) + chrono::Duration::seconds(5))) }),
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: db::now(),
        source: "statusline".into(),
        account: Some("cc-a".into()),
        host: LOCAL_HOST.into(),
    };
    crate::quota::set(&app, LOCAL_HOST, &base, reopened).await;
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "前提：額度回來了");

    for _ in 0..3 {
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
    }
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&dispatch).fetch_one(&app.db).await.unwrap();
    assert_eq!((t.status.as_str(), t.delivery.as_str(), t.run_id.as_deref()), ("in_flight", "ok", Some(run_a.as_str())));
    assert_eq!(sent_to(&e, &pane_a), 1, "只送一次");
    forget_queue_retry_timer(&bot.id);
    stop_bot(&app, &bot.id).await.unwrap();
}

/// daemon 重啟：行程內的東西（`app.quotas`、重看的 timer、`recently_held`）全沒了，DB 與活著的 pane 還在。
async fn restarted(e: &tt::Env, bot_id: &str) -> Arc<App> {
    forget_queue_retry_timer(bot_id);
    super::quota_hold::forget_held(bot_id);
    tt::restart_app(e).await
}

/// 開機後那台主機的身分偵測寫完（`tools::detect` → `install_host_tools`，回填掛在這之後）。
async fn identities_detected(app: &Arc<App>) {
    let ht = crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![], utc_offset_secs: None, herdr_cli: None, checked_at: db::now() };
    crate::tools::install_host_tools(app, LOCAL_HOST, ht).await;
}

fn held(t: &db::Turn) -> (&str, i64, Option<&str>) {
    (t.status.as_str(), t.flush_retries, t.run_id.as_deref())
}

async fn turn_row(app: &Arc<App>, id: &str) -> db::Turn {
    sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
}

/// issue #108 重開（縮小範圍那則）：只有 queued Turn、**沒有** `quota_blocked` 交辦，撞限只在記憶體。
/// A 撞限 → 派工被擋 → daemon 重啟 → 開機叫醒 flush：不能送進 A（身分偵測還沒完成也一樣）；偵測完的回填
/// 把撞限種回記憶體，派送前也看得到 → 換 B → 送出，只送一次，A 一次都沒有。
#[tokio::test]
async fn a_quota_hold_survives_a_daemon_restart_then_goes_to_identity_b_exactly_once() {
    let e = tt::env().await;
    let (bot, _run_a, pane_a, dispatch) = quota_hit_with_a_dispatch_queued(&e, "quota-restart-b").await;
    let app = restarted(&e, &bot.id).await;
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "前提：新行程的記憶體是空的");

    // 開機的 `rearm_queue_retries` 立刻叫醒 flush（測試裡 `schedule_flush_queued` 是 no-op，直接叫）——這時身分偵測還沒完成。
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!(held(&turn_row(&app, &dispatch).await), ("queued", 0, None), "重啟後仍擋：不 claim、不花重試");
    assert_eq!(sent_to(&e, &pane_a), 0, "沒有送進還沒額度的 A");
    assert!(queue_retry_timer_armed(&bot.id), "掛了 timer 回來再看");

    identities_detected(&app).await;
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "回填：記憶體又知道 A 還在擋（派送前、額度列都讀這裡）");
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!(held(&turn_row(&app, &dispatch).await), ("queued", 0, None), "回填之後照樣擋");
    assert_eq!(sent_to(&e, &pane_a), 0);

    // 換到 B：接回驗證完就送。
    sqlx::query("UPDATE bots SET identity='cc-b' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
    let run_b = restart_bot_with(&app, &bot.id, strict).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "B 這個身分沒有撞限");
    let transcript_b = e.dir.join("cc-b").join("projects").join(CWD_KEY).join(format!("{S}.jsonl"));
    let pane_b: String = sqlx::query_scalar("SELECT pane_id FROM runs WHERE id=?").bind(&run_b).fetch_one(&app.db).await.unwrap();
    e.herdr.live_pane(&pane_b, tt::LivePane { width: Some(120), transcript_file: Some(transcript_b.clone()), ..Default::default() });
    db::set_pane_typed(&app.db, &run_b).await.unwrap();
    crate::hookrecv::process(
        &app,
        &hook(&bot.id, &run_b, json!({"hook_event_name": "SessionStart", "session_id": S, "source": "resume",
                                      "transcript_path": transcript_b.to_string_lossy()})),
    )
    .await
    .unwrap();
    for _ in 0..3 {
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
    }
    let t = turn_row(&app, &dispatch).await;
    assert_eq!((t.status.as_str(), t.delivery.as_str(), t.run_id.as_deref()), ("in_flight", "ok", Some(run_b.as_str())));
    assert_eq!((sent_to(&e, &pane_b), sent_to(&e, &pane_a)), (1, 0), "送進 B、只送一次；A 一次都沒有");
    forget_queue_retry_timer(&bot.id);
    stop_bot(&app, &bot.id).await.unwrap();
}

/// 同上，但額度回來的那條：重啟、回填之後，A 的新讀數顯示 5 小時窗在撞限之後重開了（撞限被校正作廢）→ 送出一次。
#[tokio::test]
async fn a_quota_hold_survives_a_daemon_restart_then_goes_out_once_when_a_new_reading_clears_it() {
    let e = tt::env().await;
    let (bot, run_a, pane_a, dispatch) = quota_hit_with_a_dispatch_queued(&e, "quota-restart-back").await;
    let app = restarted(&e, &bot.id).await;
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!(held(&turn_row(&app, &dispatch).await), ("queued", 0, None), "重啟後仍擋");
    identities_detected(&app).await;
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!((held(&turn_row(&app, &dispatch).await), sent_to(&e, &pane_a)), (("queued", 0, None), 0), "回填之後照樣擋");

    let base = crate::quota::quota_base_for_host(&app, LOCAL_HOST, "claude", Some("cc-a")).await;
    let reopened = crate::quota::Quota {
        five_hour: Some(crate::quota::Window { observed_at: None, used_pct: 3.0, resets_at: Some(db::iso_at(chrono::Utc::now() + chrono::Duration::hours(5) + chrono::Duration::seconds(5))) }),
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: db::now(),
        source: "statusline".into(),
        account: Some("cc-a".into()),
        host: LOCAL_HOST.into(),
    };
    crate::quota::set(&app, LOCAL_HOST, &base, reopened).await;
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "前提：新讀數把回填的撞限校正掉了");
    for _ in 0..3 {
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
    }
    let t = turn_row(&app, &dispatch).await;
    assert_eq!((t.status.as_str(), t.delivery.as_str(), t.run_id.as_deref()), ("in_flight", "ok", Some(run_a.as_str())));
    assert_eq!(sent_to(&e, &pane_a), 1, "只送一次");
    forget_queue_retry_timer(&bot.id);
    stop_bot(&app, &bot.id).await.unwrap();
}

/// 換到 B、接回驗證完 flush：派工送進 B 一次，A 一次都沒有。
async fn goes_to_b_exactly_once(e: &tt::Env, app: &Arc<App>, bot: &db::Bot, pane_a: &str, dispatch: &str) {
    sqlx::query("UPDATE bots SET identity='cc-b' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
    let run_b = restart_bot_with(app, &bot.id, strict).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    assert!(crate::quota::limit_hit_for_bot(app, &bot).await.is_none(), "B 這個身分沒有撞限");
    let transcript_b = e.dir.join("cc-b").join("projects").join(CWD_KEY).join(format!("{S}.jsonl"));
    let pane_b: String = sqlx::query_scalar("SELECT pane_id FROM runs WHERE id=?").bind(&run_b).fetch_one(&app.db).await.unwrap();
    e.herdr.live_pane(&pane_b, tt::LivePane { width: Some(120), transcript_file: Some(transcript_b.clone()), ..Default::default() });
    db::set_pane_typed(&app.db, &run_b).await.unwrap();
    crate::hookrecv::process(
        app,
        &hook(&bot.id, &run_b, json!({"hook_event_name": "SessionStart", "session_id": S, "source": "resume",
                                      "transcript_path": transcript_b.to_string_lossy()})),
    )
    .await
    .unwrap();
    for _ in 0..3 {
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(app, &bot.id).await.unwrap();
    }
    let t = turn_row(app, dispatch).await;
    assert_eq!((t.status.as_str(), t.delivery.as_str(), t.run_id.as_deref()), ("in_flight", "ok", Some(run_b.as_str())));
    assert_eq!((sent_to(e, &pane_b), sent_to(e, pane_a)), (1, 0), "送進 B、只送一次；A 一次都沒有");
    forget_queue_retry_timer(&bot.id);
    stop_bot(app, &bot.id).await.unwrap();
}

async fn limit_hits(app: &Arc<App>) -> Vec<String> {
    app.quotas.lock().await.iter().filter(|(_, q)| q.limit_hit.is_some()).map(|(k, _)| k.clone()).collect()
}

/// **issue #522**：憑據在、但**內容壞了**（未來加欄位沒帶 `serde(default)`、寫到一半、有人動過 DB）。
/// 以前回填把那一列跳過卻照樣把整台主機標成「回填完了」，於是 flush 走「回填完了只看記憶體」那條：
/// 記憶體是空的 → 判定不擋 → `forget` 把唯一的證據清掉 → 重啟後第一拍就送進還沒額度的身分，
/// 正是這個模組存在的理由。現在解不開的那顆 bot 不算回填過，照舊問那一列自己的憑據（那裡對解不開是回錯、
/// 呼叫端照擋，跟 `held_on_turn` 同方向）。
#[tokio::test]
async fn a_queued_prompt_whose_quota_hold_is_unreadable_is_not_released_after_a_restart() {
    let e = tt::env().await;
    let (bot, _run_a, pane_a, dispatch) = quota_hit_with_a_dispatch_queued(&e, "quota-hold-corrupt").await;
    let app = restarted(&e, &bot.id).await;
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "前提：新行程的記憶體是空的");
    sqlx::query("UPDATE turns SET quota_hold='{' WHERE id=?").bind(&dispatch).execute(&app.db).await.unwrap();

    // 回填跑完：那一列解不開，撞限沒被種回記憶體。
    identities_detected(&app).await;
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "前提：解不開的憑據種不回去");

    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();

    assert_eq!(held(&turn_row(&app, &dispatch).await), ("queued", 0, None), "證明不了有額度就照擋：不 claim、不花重試");
    assert_eq!(sent_to(&e, &pane_a), 0, "沒有送進還沒額度的 A");
    let raw: Option<String> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?")
        .bind(&dispatch)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(raw.as_deref(), Some("{"), "唯一的證據不准被 forget 掉");
    assert!(queue_retry_timer_armed(&bot.id), "掛了 timer 回來再看");
    forget_queue_retry_timer(&bot.id);
    stop_bot(&app, &bot.id).await.unwrap();
}

/// #108 重開（故障注入 1、4）：A 撞額度的 `StopFailure` 到的時候讀不到 A 在哪台主機。以前退回 `local` 照記、照收回合、
/// 推回合結束。現在這一則失敗（收件匣重試）、哪一格都不寫、撞限欠著、回合不收——派工留在佇列。讀得到之後收件匣重試：
/// 撞限記下、回合收成失敗、派工照撞限擋；換到 B → 送出，只送一次。
#[tokio::test]
async fn a_quota_stop_failure_that_cannot_find_its_identity_holds_the_dispatch_until_recorded_then_goes_to_b_once() {
    let e = tt::env().await;
    let app = e.app.clone();
    let (bot, run_a, pane_a, dispatch) = dispatch_queued_behind_a_turn(&e, "quota-unrecorded").await;

    sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
    assert!(crate::hookrecv::process(&app, &a_limit(&bot.id, &run_a)).await.is_err(), "記不進去：這一則失敗，收件匣重試");
    assert_eq!(limit_hits(&app).await, Vec::<String>::new(), "沒有退回 local 記在別的帳號上");
    assert!(crate::turn_error::owes_limit_hit(&app, &bot.id));
    assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "派送前也看得到欠著的撞限");
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!(held(&turn_row(&app, &dispatch).await), ("queued", 0, None));
    assert_eq!(sent_to(&e, &pane_a), 0, "沒有送進還沒額度的 A");
    sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();

    crate::hookrecv::process(&app, &a_limit(&bot.id, &run_a)).await.expect("收件匣重試：讀得到了");
    assert!(!crate::turn_error::owes_limit_hit(&app, &bot.id));
    assert_eq!(limit_hits(&app).await.len(), 1);
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!((held(&turn_row(&app, &dispatch).await), sent_to(&e, &pane_a)), (("queued", 0, None), 0), "照撞限擋");
    goes_to_b_exactly_once(&e, &app, &bot, &pane_a, &dispatch).await;
}

/// #108 重開（故障注入 3、4）：撞限記進記憶體了，憑據（`turns.quota_hold`）卻寫不進去。以前只記 warn、照收回合——重啟之後
/// 記憶體沒了、那一列也沒有憑據，派工送進 A。現在沒落地不算記好：這一則失敗、回合不收，重啟前後都不送；寫得進去之後
/// 收件匣重試落地，再重啟一次也照擋（偵測之前看那一列、偵測之後回填），換到 B → 只送一次。
#[tokio::test]
async fn a_quota_hold_that_cannot_be_persisted_is_not_sent_before_or_after_a_restart() {
    let e = tt::env().await;
    let (bot, run_a, pane_a, dispatch) = dispatch_queued_behind_a_turn(&e, "quota-unpersisted").await;
    sqlx::query("CREATE TRIGGER refuse_quota_hold BEFORE UPDATE OF quota_hold ON turns BEGIN SELECT RAISE(ABORT, 'injected: cannot write turns.quota_hold'); END")
        .execute(&e.app.db)
        .await
        .unwrap();
    assert!(crate::hookrecv::process(&e.app, &a_limit(&bot.id, &run_a)).await.is_err(), "憑據沒落地：這一則失敗");
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&e.app, &bot.id).await.unwrap();
    assert_eq!((held(&turn_row(&e.app, &dispatch).await), sent_to(&e, &pane_a)), (("queued", 0, None), 0), "重啟前：不送");

    let app = restarted(&e, &bot.id).await;
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert!(crate::hookrecv::process(&app, &a_limit(&bot.id, &run_a)).await.is_err(), "開機後收件匣重試：還是寫不進去");
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!((held(&turn_row(&app, &dispatch).await), sent_to(&e, &pane_a)), (("queued", 0, None), 0), "重啟後：不送");

    sqlx::query("DROP TRIGGER refuse_quota_hold").execute(&app.db).await.unwrap();
    crate::hookrecv::process(&app, &a_limit(&bot.id, &run_a)).await.expect("寫得進去了：收件匣重試成功");
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!((held(&turn_row(&app, &dispatch).await), sent_to(&e, &pane_a)), (("queued", 0, None), 0), "照撞限擋");
    let hold: Option<String> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?").bind(&dispatch).fetch_one(&app.db).await.unwrap();
    assert!(hold.is_some(), "憑據落地");

    let app = restarted(&e, &bot.id).await;
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!((held(&turn_row(&app, &dispatch).await), sent_to(&e, &pane_a)), (("queued", 0, None), 0), "再重啟：偵測之前看那一列");
    identities_detected(&app).await;
    forget_queue_retry_timer(&bot.id);
    flush_queued_locked(&app, &bot.id).await.unwrap();
    assert_eq!((held(&turn_row(&app, &dispatch).await), sent_to(&e, &pane_a)), (("queued", 0, None), 0), "回填之後照擋");
    goes_to_b_exactly_once(&e, &app, &bot, &pane_a, &dispatch).await;
}
