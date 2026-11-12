use super::*;
use crate::quota::{quota_key, LimitHit, Quota};

const SESSION: &str = "01a15c0b-4e13-7aa0-99bd-11db39c00879";

fn at(secs_ago: i64) -> String {
    db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(secs_ago))
}

fn hit(secs_ago: i64) -> Quota {
    Quota {
        five_hour: None,
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: Some(LimitHit { message: "You hit your weekly limit.".into(), until: None, at: at(secs_ago), bucket: None }),
        plan: None,
        updated_at: db::now(),
        source: "grok-limit-hit".into(),
        account: None,
        host: "local".into(),
    }
}

/// 一顆 Grok bot、它的 running run，以及一個 `delivered_secs_ago` 秒前送出、還在飛的回合（`None`＝沒有 `delivered_at`）。
async fn grok_bot(e: &crate::testing::Env, project: &str, delivered_secs_ago: Option<i64>) -> (String, String) {
    let bot = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
         VALUES (?,?,?,'grok','[]',0,1,'tok','user',?)",
    )
    .bind(&bot)
    .bind(project)
    .bind(format!("grok-{}", &bot[bot.len() - 8..]))
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    let run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','working','ws-1','pane-1','agent','test',?)",
    )
    .bind(&run)
    .bind(&bot)
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&e.app.db, &bot).await.unwrap();
    let turn = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at, delivered_at)
         VALUES (?,?,?,'web','in_flight','ok','跑一次測試',?,?)",
    )
    .bind(&turn)
    .bind(&conv)
    .bind(&run)
    .bind(at(delivered_secs_ago.unwrap_or(0) + 1))
    .bind(delivered_secs_ago.map(at))
    .execute(&e.app.db)
    .await
    .unwrap();
    (bot, run)
}

/// 在 in-flight 回合之前再送出過一個回合（`secs_ago` 秒前），狀態與 native id 可選。
async fn previous_turn(e: &crate::testing::Env, run: &str, bot: &str, secs_ago: i64, status: &str, native: Option<&str>) {
    let conv = db::conversation_id(&e.app.db, bot).await.unwrap();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, native_session_id, native_turn_id, created_at, delivered_at, completed_at)
         VALUES (?,?,?,'web',?,'ok','前一句',?,?,?,?,?)",
    )
    .bind(db::ulid())
    .bind(&conv)
    .bind(run)
    .bind(status)
    .bind(native.map(|_| SESSION))
    .bind(native)
    .bind(at(secs_ago + 1))
    .bind(at(secs_ago))
    .bind(at(secs_ago - 5))
    .execute(&e.app.db)
    .await
    .unwrap();
}

/// 一則 Grok 的 Stop，欄位都可以換。
struct Stop {
    reply: &'static str,
    received_at: Option<String>,
    reason: &'static str,
    active: bool,
    run_id: Option<String>,
    provider: &'static str,
}

impl Stop {
    fn ok() -> Stop {
        Stop { reply: "好", received_at: Some(at(1)), reason: "end_turn", active: false, run_id: None, provider: "grok" }
    }
    fn body(&self, bot: &str) -> HookBody {
        HookBody {
            bot_id: bot.to_string(),
            provider: self.provider.into(),
            payload: serde_json::json!({"hookEventName": "stop", "sessionId": SESSION, "promptId": db::ulid(), "reason": self.reason,
                                        "stopHookActive": self.active, "lastAssistantMessage": self.reply}),
            received_at: self.received_at.clone(),
            truncated: false,
            run_id: self.run_id.clone(),
        }
    }
}

async fn still_hit(e: &crate::testing::Env, key: &str) -> bool {
    e.app.quotas.lock().await.get(key).unwrap().limit_hit.is_some()
}

/// 本機 `grok` 那格 600 秒前撞限；回合 `delivered` 秒前送出；Stop 照 `change` 改。回傳撞限有沒有被清掉。
async fn cleared_by(delivered: Option<i64>, change: impl FnOnce(&mut Stop)) -> bool {
    let e = crate::testing::env().await;
    let (bot, _run) = grok_bot(&e, &e.project_id, delivered).await;
    e.app.quotas.lock().await.insert(quota_key("local", "grok"), hit(600));
    let mut stop = Stop::ok();
    change(&mut stop);
    let _ = process_locked(&e.app, &stop.body(&bot)).await;
    !still_hit(&e, "grok").await
}

/// ③ 撞限之後送出、之後 Stop 成功：清。
#[tokio::test]
async fn a_turn_sent_after_the_hit_and_answered_clears_it() {
    assert!(cleared_by(Some(300), |_| {}).await);
}

/// ① 撞限之前送出、撞限之後才 Stop：不清（回合跨過撞限，證明不了撞限之後帳號能跑）。
#[tokio::test]
async fn a_turn_sent_before_the_hit_keeps_it_even_if_its_stop_comes_after() {
    assert!(!cleared_by(Some(900), |_| {}).await);
}

/// ④ 另一顆 bot 同帳號撞限、這顆 bot 的回合跨過那一刻：不清；同一個撞限，之後才送出的別顆 bot 才清。
#[tokio::test]
async fn another_bots_limit_in_the_middle_of_this_bots_turn_keeps_it() {
    let e = crate::testing::env().await;
    let (spanning, _) = grok_bot(&e, &e.project_id, Some(900)).await;
    let (later, _) = grok_bot(&e, &e.project_id, Some(300)).await;
    e.app.quotas.lock().await.insert(quota_key("local", "grok"), hit(600));
    let _ = process_locked(&e.app, &Stop::ok().body(&spanning)).await;
    assert!(still_hit(&e, "grok").await, "回合跨過撞限，不清");
    let _ = process_locked(&e.app, &Stop::ok().body(&later)).await;
    assert!(!still_hit(&e, "grok").await, "撞限之後送出的回合答完，清");
}

/// ② 舊回合晚到的 Stop：觸發時間早於新回合的 `delivered_at`，不清。
#[tokio::test]
async fn a_stop_fired_before_the_new_turn_was_sent_keeps_it() {
    assert!(!cleared_by(Some(300), |s| s.received_at = Some(at(400))).await);
}

/// 前一個回合怎麼收的都不影響這一個：後續新回合自己的 Stop 照清（AGM 裁示，不做前序回合／整 run 的守衛）。
#[tokio::test]
async fn the_turns_before_this_one_do_not_block_it() {
    for (status, native) in [("completed_fallback", None), ("failed", None), ("completed", Some("p-prev")), ("completed", None)] {
        let e = crate::testing::env().await;
        let (bot, run) = grok_bot(&e, &e.project_id, Some(300)).await;
        previous_turn(&e, &run, &bot, 500, status, native).await;
        e.app.quotas.lock().await.insert(quota_key("local", "grok"), hit(600));
        let _ = process_locked(&e.app, &Stop::ok().body(&bot)).await;
        assert!(!still_hit(&e, "grok").await, "{status} {native:?}");
    }
}

/// 被備援收成 `completed_fallback` 的那一筆本身（沒有 in-flight 回合）：晚到的 Stop 補回它，不清。
#[tokio::test]
async fn the_fallback_closed_turn_itself_never_clears() {
    let e = crate::testing::env().await;
    let (bot, _run) = grok_bot(&e, &e.project_id, Some(300)).await;
    sqlx::query("UPDATE turns SET status = 'completed_fallback', completed_at = ?").bind(at(10)).execute(&e.app.db).await.unwrap();
    e.app.quotas.lock().await.insert(quota_key("local", "grok"), hit(600));
    let _ = process_locked(&e.app, &Stop::ok().body(&bot)).await;
    assert!(still_hit(&e, "grok").await);
}

#[tokio::test]
async fn without_usable_times_or_a_delivered_in_flight_turn_nothing_clears() {
    assert!(!cleared_by(None, |_| {}).await, "回合沒有 delivered_at");
    assert!(!cleared_by(Some(300), |s| s.received_at = None).await);
    assert!(!cleared_by(Some(300), |s| s.received_at = Some("not-a-time".into())).await);
    assert!(!cleared_by(Some(300), |s| s.received_at = Some(at(-3600))).await, "比現在晚的觸發時間不可信");

    // 沒有 in-flight 回合（這一則已被收過、或根本沒有回合）／送達結果不明。
    for sql in ["UPDATE turns SET status = 'completed'", "UPDATE turns SET delivery = 'unknown'", "UPDATE turns SET origin = 'external'"] {
        let e = crate::testing::env().await;
        let (bot, _run) = grok_bot(&e, &e.project_id, Some(300)).await;
        sqlx::query(sql).execute(&e.app.db).await.unwrap();
        e.app.quotas.lock().await.insert(quota_key("local", "grok"), hit(600));
        let _ = process_locked(&e.app, &Stop::ok().body(&bot)).await;
        assert!(still_hit(&e, "grok").await, "{sql}");
    }
}

#[tokio::test]
async fn an_empty_or_failure_reply_never_clears() {
    for reply in [
        "   ",
        "You hit your weekly limit.",
        "Turn failed: Request failed (402): Grok Build usage balance exhausted",
        "API error (status 402 Payment Required): Grok Build usage balance exhausted",
        "sorry\nYou've hit your usage limit",
    ] {
        assert!(!cleared_by(Some(300), |s| s.reply = reply).await, "{reply:?}");
    }
}

/// 只有 end_turn 的 Stop 算：結束 session 的 shutdown、stop hook 自己再觸發的都不是。
#[tokio::test]
async fn only_an_end_turn_stop_counts() {
    assert!(!cleared_by(Some(300), |s| s.reason = "shutdown").await);
    assert!(!cleared_by(Some(300), |s| s.active = true).await);
}

/// 世代圍籬：別的 run 送來的、別的 provider 的 Stop 都不清。
#[tokio::test]
async fn a_stop_from_another_run_or_provider_keeps_it() {
    assert!(!cleared_by(Some(300), |s| s.run_id = Some("some-other-run".into())).await, "圍籬不放行");
    assert!(!cleared_by(Some(300), |s| s.provider = "claude").await);
}

/// 這個 session 是上一代 run 的（現在的 run 綁著另一個）：晚到的 Stop 被世代圍籬擋下。
#[tokio::test]
async fn a_late_stop_of_a_previous_runs_session_keeps_it() {
    let e = crate::testing::env().await;
    let (bot, _run) = grok_bot(&e, &e.project_id, Some(300)).await;
    sqlx::query("UPDATE runs SET state = 'stopped', native_session_id = ? WHERE bot_id = ?").bind(SESSION).bind(&bot).execute(&e.app.db).await.unwrap();
    let current = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, native_session_id)
         VALUES (?,?,'running','working','ws-1','pane-2','agent','test',?,'the-current-session')",
    )
    .bind(&current)
    .bind(&bot)
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    // 現在這代 run 也有一個送出之後還在飛的回合：圍籬不放行時它不能被拿來當證據。
    let conv = db::conversation_id(&e.app.db, &bot).await.unwrap();
    sqlx::query("UPDATE turns SET status = 'completed' WHERE conversation_id = ?").bind(&conv).execute(&e.app.db).await.unwrap();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at, delivered_at)
         VALUES (?,?,?,'web','in_flight','ok','x',?,?)",
    )
    .bind(db::ulid())
    .bind(&conv)
    .bind(&current)
    .bind(at(301))
    .bind(at(300))
    .execute(&e.app.db)
    .await
    .unwrap();
    e.app.quotas.lock().await.insert(quota_key("local", "grok"), hit(600));
    let _ = process_locked(&e.app, &Stop::ok().body(&bot)).await;
    assert!(still_hit(&e, "grok").await);
}

/// 遠端主機：時鐘沒有可驗的界線，不清。
#[tokio::test]
async fn a_remote_host_keeps_it() {
    let e = crate::testing::env().await;
    let remote = db::ulid();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'remote1', ?)")
        .bind(&remote)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    let (bot, _run) = grok_bot(&e, &remote, Some(300)).await;
    let key = quota_key("remote1", "grok");
    e.app.quotas.lock().await.insert(key.clone(), hit(600));
    let _ = process_locked(&e.app, &Stop::ok().body(&bot)).await;
    assert!(still_hit(&e, &key).await);
}
