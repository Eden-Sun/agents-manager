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
