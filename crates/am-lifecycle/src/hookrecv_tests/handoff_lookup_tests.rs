use super::*;
use crate::testing::{claude_bot, env, fake_run, Env};

fn stop_body(bot_id: &str, prompt_id: &str) -> HookBody {
    HookBody {
        bot_id: bot_id.into(),
        provider: "claude".into(),
        payload: json!({
            "hook_event_name": "Stop",
            "session_id": "sess-1",
            "prompt_id": prompt_id,
            "last_assistant_message": "做完了",
        }),
        received_at: Some("2026-10-11T12:00:00.000Z".into()),
        truncated: false,
        run_id: None,
    }
}

async fn hand_off(e: &Env, to: Option<&str>) {
    sqlx::query("UPDATE projects SET handed_off_to = ? WHERE id = ?").bind(to).bind(&e.project_id).execute(&e.app.db).await.unwrap();
}

/// BLOB 解不成 `Option<String>`，所以 `bot_handed_off_to` 會讀失敗；`bots`、`runs`、`turns` 等其他查詢都不碰這一欄。
async fn break_the_handoff_lookup(e: &Env) {
    sqlx::query("UPDATE projects SET handed_off_to = X'00' WHERE id = ?").bind(&e.project_id).execute(&e.app.db).await.unwrap();
}

/// 回合、訊息、run 的狀態：Stop 真的處理過，這幾樣至少會有一樣變。
async fn footprint(e: &Env, run: &str) -> (i64, i64, String, String) {
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&e.app.db).await.unwrap();
    let msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages").fetch_one(&e.app.db).await.unwrap();
    let (state, status): (String, String) =
        sqlx::query_as("SELECT state, agent_status FROM runs WHERE id = ?").bind(run).fetch_one(&e.app.db).await.unwrap();
    (turns, msgs, state, status)
}

/// 查詢讀失敗：回錯（收件匣會退避重試），而且完全不做 hook 副作用。查詢恢復後，移交的專案照樣不處理。
#[tokio::test]
async fn an_unreadable_handoff_lookup_fails_the_hook_and_recovery_still_ignores_a_handed_off_project() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hook-lookup-err").await;
    let run = fake_run(&e.app, &bot.id).await;
    hand_off(&e, Some("agm-host")).await;
    break_the_handoff_lookup(&e).await;
    let before = footprint(&e, &run).await;

    let err = process_for(&e.app, &stop_body(&bot.id, "p1"), None).await.expect_err("讀不到移交狀態就得回錯，不能當成沒移交");
    assert!(format!("{err:#}").contains("handed off"), "錯誤要說清楚是移交查詢失敗：{err:#}");
    assert_eq!(footprint(&e, &run).await, before, "查詢失敗時不准有任何 hook 副作用");

    // 查詢恢復：移交的專案照樣丟掉，而且不留下任何東西。
    hand_off(&e, Some("agm-host")).await;
    process_for(&e.app, &stop_body(&bot.id, "p1"), None).await.unwrap();
    assert_eq!(footprint(&e, &run).await, before, "恢復後移交的專案仍然不處理");
}

/// 對照組：沒有移交、查詢正常的專案，hook 照常處理（副作用確實發生）。
#[tokio::test]
async fn an_owned_project_hook_is_still_processed() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hook-lookup-owned").await;
    let run = fake_run(&e.app, &bot.id).await;
    let before = footprint(&e, &run).await;

    process_for(&e.app, &stop_body(&bot.id, "p1"), None).await.unwrap();
    assert_ne!(footprint(&e, &run).await, before, "自己管的專案，Stop 要真的處理到");
}

/// #1259 × #108：移交查詢讀不出來、這一則是撞額度的 StopFailure：撞限照記（排著的派工才擋得住），
/// 其餘副作用一樣都不做（回合不收、不寫訊息），然後回錯讓收件匣重試。
#[tokio::test]
async fn a_quota_stop_failure_while_the_handoff_lookup_is_unreadable_records_only_the_limit() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hook-lookup-quota").await;
    let run = fake_run(&e.app, &bot.id).await;
    break_the_handoff_lookup(&e).await;
    let before = footprint(&e, &run).await;
    let fail = HookBody {
        bot_id: bot.id.clone(),
        provider: "claude".into(),
        payload: json!({
            "hook_event_name": "StopFailure",
            "session_id": "s-q",
            "prompt_id": "p-q",
            "reason": "You've hit your session limit · resets 5pm",
        }),
        received_at: Some("2026-10-11T12:00:00.000Z".into()),
        truncated: false,
        run_id: None,
    };

    let err = process_for(&e.app, &fail, None).await.expect_err("讀不到移交狀態就得回錯，由收件匣重試");
    assert!(format!("{err:#}").contains("handed off"), "錯誤要說清楚是移交查詢失敗：{err:#}");
    assert_eq!(footprint(&e, &run).await, before, "除了撞限，不准有任何 hook 副作用");
    let limit_recorded = e.app.quotas.lock().await.values().any(|q| q.limit_hit.is_some());
    assert!(limit_recorded, "撞額度是帳號的事實，移交查詢讀不出來也要照記");
}
