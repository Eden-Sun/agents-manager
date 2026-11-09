//! 排程的後端測試：真的用測試 App（SQLite＋假 herdr），驗證帳本、綁定、停用撤回、冪等與 unknown 暫停。

use super::api::{self, CreateIn, RunIn, SpecIn};
use super::runner;
use super::store::{self, Occurrence, Schedule};
use crate::db;
use crate::lifecycle::{self, LcError};
use crate::testing;
use axum::extract::{Path, State};
use axum::Json;

async fn setup() -> (testing::Env, db::Bot) {
    let e = testing::env().await;
    let bot = dedicated(&e, "sched-bot").await;
    (e, bot)
}

/// 排程專屬 bot（role = scheduler）：新綁定一律要這種。
async fn dedicated(e: &testing::Env, name: &str) -> db::Bot {
    let bot = testing::claude_bot(&e.app, &e.project_id, name).await;
    sqlx::query("UPDATE bots SET role = 'scheduler' WHERE id = ?").bind(&bot.id).execute(&e.app.db).await.unwrap();
    db::bot(&e.app.db, &bot.id).await.unwrap().unwrap()
}

fn daily(bot: &str, prompt: &str) -> CreateIn {
    CreateIn {
        client_request_id: "crid-1".into(),
        name: "每日".into(),
        prompt: prompt.into(),
        bot_id: bot.into(),
        tz: "UTC".into(),
        spec: SpecIn { mode: "daily".into(), at: None, every_secs: None, time: Some("09:00".into()), weekdays: None, slots: None },
        notify: "failure".into(),
        clear_after: false,
        from_base: false,
        retry_count: 0, retry_interval_mins: 3, retry_deadline_mins: 15,
    }
}

/// 直接插一筆排程（不經 API），next_due 設在一小時後。
async fn schedule(app: &std::sync::Arc<crate::state::App>, project: &str, bot: &str) -> Schedule {
    let now = db::now();
    let s = Schedule {
        id: db::ulid(),
        project_id: project.into(),
        bot_id: bot.into(),
        name: "排程".into(),
        prompt: "請整理進度".into(),
        mode: "daily".into(),
        tz: "UTC".into(),
        spec_json: r#"{"mode":"daily","time":"09:00"}"#.into(),
        next_due: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        enabled: 1,
        generation: 1,
        notify: "failure".into(),
        clear_after: 0,
        from_base: 0,
        retry_count: 0, retry_interval_mins: 3, retry_deadline_mins: 15,
        create_crid: None,
        create_fp: None,
        created_at: now.clone(),
        updated_at: now,
        deleted_at: None,
    };
    let mut tx = db::begin_write(&app.db).await.unwrap();
    store::insert_schedule_tx(&mut tx, &s).await.unwrap();
    tx.commit().await.unwrap();
    s
}

/// 一筆 pending 的 occurrence（模擬到期落帳）。
async fn pending(app: &std::sync::Arc<crate::state::App>, s: &Schedule) -> String {
    let now = db::now();
    let o = Occurrence {
        id: db::ulid(),
        schedule_id: s.id.clone(),
        source: "scheduled".into(),
        scheduled_for: now.clone(),
        generation: s.generation,
        bot_id: s.bot_id.clone(),
        prompt: s.prompt.clone(),
        slot_label: None,
        client_request_id: None,
        status: "pending".into(),
        waiting_reason: None,
        merged_count: 0,
        conversation_id: None,
        turn_id: None,
        error: None,
        outcome: None, outcome_summary: None, reply_message_id: None, attempt_no: 1, next_retry_at: None,
        created_at: now.clone(),
        updated_at: now,
    };
    let mut tx = db::begin_write(&app.db).await.unwrap();
    store::insert_occurrence_tx(&mut tx, &o).await.unwrap();
    tx.commit().await.unwrap();
    o.id
}

async fn count(app: &std::sync::Arc<crate::state::App>, sql: &str, bind: &str) -> i64 {
    sqlx::query_scalar(sql).bind(bind).fetch_one(&app.db).await.unwrap()
}

async fn turn_status(app: &std::sync::Arc<crate::state::App>, turn: &str) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(turn).fetch_one(&app.db).await.unwrap()
}

#[tokio::test]
async fn occurrence_view_exposes_the_existing_turn_start_error_without_changing_it() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let id = pending(&e.app, &s).await;
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let turn = db::ulid();
    let reason = "remote grok hooks file is not a safe regular file: AM_GROK_UNTRUSTED";
    sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at,awaits_start,start_error) VALUES (?,?,'web','queued','pending',?,1,?)")
        .bind(&turn).bind(&conv).bind(db::now()).bind(reason).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE schedule_occurrences SET status='queued', turn_id=? WHERE id=?")
        .bind(&turn).bind(&id).execute(&e.app.db).await.unwrap();
    let occ = store::get_occurrence(&e.app.db, &id).await.unwrap().unwrap();
    let view = store::occurrence_view(&e.app.db, &occ).await.unwrap();
    assert_eq!(view["turn_start_error"], reason);
    assert_eq!(view["turn_awaits_start"], 1);
    let turn_row: (Option<String>, i64) = sqlx::query_as("SELECT start_error, awaits_start FROM turns WHERE id=?")
        .bind(&turn).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(turn_row, (Some(reason.to_string()), 1));
    let without_turn = Occurrence { turn_id: None, ..occ };
    let view = store::occurrence_view(&e.app.db, &without_turn).await.unwrap();
    assert!(view["turn_start_error"].is_null() && view["turn_awaits_start"].is_null());
}

#[tokio::test]
async fn enqueue_waits_while_the_bot_is_stopping_and_creates_no_turn() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let occ = pending(&e.app, &s).await;
    testing::fake_run(&e.app, &bot.id).await;
    sqlx::query("UPDATE runs SET state = 'stopping' WHERE bot_id = ?").bind(&bot.id).execute(&e.app.db).await.unwrap();
    assert!(!runner::enqueue(&e.app, &occ).await.unwrap());
    let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
    assert_eq!(o.status, "pending");
    assert_eq!(o.waiting_reason.as_deref(), Some("stopping"));
    assert_eq!(count(&e.app, "SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE c.bot_id = ?", &bot.id).await, 0);
}

#[tokio::test]
async fn enqueue_does_not_overwrite_a_turn_the_user_already_queued() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let occ = pending(&e.app, &s).await;
    testing::fake_run(&e.app, &bot.id).await;
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let mine = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, awaits_idle) VALUES (?,?,'web','queued','pending',?,1)")
        .bind(&mine)
        .bind(&conv)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    assert!(!runner::enqueue(&e.app, &occ).await.unwrap());
    let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
    assert_eq!(o.waiting_reason.as_deref(), Some("bot_busy"));
    assert_eq!(turn_status(&e.app, &mine).await, "queued", "使用者排的那筆不被覆蓋或撤回");
}

#[tokio::test]
async fn enqueue_binds_turn_and_message_in_one_step_and_marks_the_occurrence_queued() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let occ = pending(&e.app, &s).await;
    assert!(runner::enqueue(&e.app, &occ).await.unwrap());
    let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
    assert_eq!(o.status, "queued");
    let turn = o.turn_id.clone().expect("綁定 turn");
    assert!(o.conversation_id.is_some());
    let (awaits_start, status): (i64, String) = sqlx::query_as("SELECT awaits_start, status FROM turns WHERE id = ?").bind(&turn).fetch_one(&e.app.db).await.unwrap();
    assert_eq!((awaits_start, status.as_str()), (1, "queued"), "沒有 run：等啟動路徑認領");
    let relay: String = sqlx::query_scalar("SELECT relay_from FROM messages WHERE turn_id = ? AND role = 'user'").bind(&turn).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(relay, "daemon");
}

#[tokio::test]
async fn materialize_twice_from_a_stale_snapshot_creates_one_occurrence() {
    let (e, bot) = setup().await;
    let mut s = schedule(&e.app, &e.project_id, &bot.id).await;
    // 讓它已經到期。
    let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE schedules SET next_due = ? WHERE id = ?").bind(&past).bind(&s.id).execute(&e.app.db).await.unwrap();
    s.next_due = Some(past);
    let now = db::now();
    runner::materialize(&e.app.db, &s, &now).await.unwrap();
    runner::materialize(&e.app.db, &s, &now).await.unwrap();
    assert_eq!(count(&e.app, "SELECT COUNT(*) FROM schedule_occurrences WHERE schedule_id = ?", &s.id).await, 1);
}

async fn interval_with_submillisecond_anchor(app: &std::sync::Arc<crate::state::App>, project: &str, bot: &str) -> Schedule {
    let mut s = schedule(app, project, bot).await;
    let anchor = "2026-10-17T12:00:00.123456789Z";
    let due = "2026-10-17T12:01:00.123Z";
    s.mode = "interval".into();
    s.spec_json = format!(r#"{{"mode":"interval","every_secs":60,"anchor":"{anchor}"}}"#);
    s.next_due = Some(due.into());
    sqlx::query("UPDATE schedules SET mode = ?, spec_json = ?, next_due = ? WHERE id = ?")
        .bind(&s.mode)
        .bind(&s.spec_json)
        .bind(&s.next_due)
        .bind(&s.id)
        .execute(&app.db)
        .await
        .unwrap();
    s
}

#[tokio::test]
async fn interval_materialize_advances_past_a_millisecond_due_with_submillisecond_anchor() {
    let (e, bot) = setup().await;
    let s = interval_with_submillisecond_anchor(&e.app, &e.project_id, &bot.id).await;
    let before_due = "2026-10-17T12:01:00.122Z";
    let due = "2026-10-17T12:01:00.123Z";
    assert!(store::list_due(&e.app.db, before_due).await.unwrap().is_empty(), "到期前 1 毫秒不可列為到期");
    let listed = store::list_due(&e.app.db, due).await.unwrap();
    assert_eq!(listed.len(), 1, "到期毫秒應列為到期");

    runner::materialize(&e.app.db, &listed[0], due).await.unwrap();
    let next_due: String = sqlx::query_scalar("SELECT next_due FROM schedules WHERE id = ?").bind(&s.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(next_due, "2026-10-17T12:02:00.123Z", "到期當毫秒和晚 1 毫秒都必須越過已落帳 occurrence");
    assert!(store::list_due(&e.app.db, "2026-10-17T12:01:00.124Z").await.unwrap().is_empty());
}

#[tokio::test]
async fn interval_pending_occurrence_merges_on_the_next_due_without_repeating_due() {
    let (e, bot) = setup().await;
    let s = interval_with_submillisecond_anchor(&e.app, &e.project_id, &bot.id).await;
    let due = "2026-10-17T12:01:00.123Z";
    runner::materialize(&e.app.db, &s, due).await.unwrap();

    let next = store::get_schedule(&e.app.db, &s.id).await.unwrap().unwrap();
    let next_due = next.next_due.clone().unwrap();
    runner::materialize(&e.app.db, &next, &next_due).await.unwrap();
    assert_eq!(count(&e.app, "SELECT COUNT(*) FROM schedule_occurrences WHERE schedule_id = ?", &s.id).await, 1);
    assert_eq!(count(&e.app, "SELECT merged_count FROM schedule_occurrences WHERE schedule_id = ?", &s.id).await, 1);
}

#[tokio::test]
async fn failed_interval_occurrence_does_not_retry_the_same_millisecond_due() {
    let (e, bot) = setup().await;
    let s = interval_with_submillisecond_anchor(&e.app, &e.project_id, &bot.id).await;
    let due = "2026-10-17T12:01:00.123Z";
    runner::materialize(&e.app.db, &s, due).await.unwrap();
    let occurrence_id: String = sqlx::query_scalar("SELECT id FROM schedule_occurrences WHERE schedule_id = ?").bind(&s.id).fetch_one(&e.app.db).await.unwrap();
    let mut tx = db::begin_write(&e.app.db).await.unwrap();
    store::finish_open_tx(&mut tx, &occurrence_id, "failed", "test", due).await.unwrap();
    tx.commit().await.unwrap();

    assert!(store::list_due(&e.app.db, "2026-10-17T12:01:20.123Z").await.unwrap().is_empty(), "下一輪不能再次列出剛失敗 occurrence 的 due");
}

#[tokio::test]
async fn delete_withdraws_a_queued_turn_before_it_is_sent() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let occ = pending(&e.app, &s).await;
    assert!(runner::enqueue(&e.app, &occ).await.unwrap());
    let turn = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap().turn_id.unwrap();
    let _ = api::delete(State(e.app.clone()), Path(s.id.clone())).await.unwrap();
    assert_eq!(turn_status(&e.app, &turn).await, "failed", "未送出的排隊要撤回");
    let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
    assert_eq!(o.status, "cancelled");
}

#[tokio::test]
async fn disable_does_not_touch_a_turn_that_is_already_in_flight() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let occ = pending(&e.app, &s).await;
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let inflight = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','in_flight','ok',?)")
        .bind(&inflight)
        .bind(&conv)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE schedule_occurrences SET status='queued', turn_id=?, conversation_id=? WHERE id=?").bind(&inflight).bind(&conv).bind(&occ).execute(&e.app.db).await.unwrap();
    let _ = api::patch(State(e.app.clone()), Path(s.id.clone()), Json(api::PatchIn { name: None, prompt: None, bot_id: None, tz: None, spec: None, notify: None, clear_after: None, from_base: None, retry_count: None, retry_interval_mins: None, retry_deadline_mins: None, enabled: Some(false) }))
        .await
        .unwrap();
    assert_eq!(turn_status(&e.app, &inflight).await, "in_flight", "已送出的回合照跑");
    let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
    assert_eq!(o.status, "queued", "等 turn 進終態才收斂");
}

#[tokio::test]
async fn create_with_the_same_key_and_content_returns_the_original_and_different_content_conflicts() {
    let (e, bot) = setup().await;
    let pid = e.project_id.clone();
    let first = api::create(State(e.app.clone()), Path(pid.clone()), Json(daily(&bot.id, "一"))).await.unwrap().0;
    let again = api::create(State(e.app.clone()), Path(pid.clone()), Json(daily(&bot.id, "一"))).await.unwrap().0;
    assert_eq!(first["id"], again["id"]);
    assert_eq!(count(&e.app, "SELECT COUNT(*) FROM schedules WHERE project_id = ?", &pid).await, 1);
    let conflict = api::create(State(e.app.clone()), Path(pid.clone()), Json(daily(&bot.id, "二"))).await;
    assert!(matches!(conflict, Err(LcError::Conflict(_))));
}

#[tokio::test]
async fn create_refuses_a_bot_from_another_project() {
    let (e, bot) = setup().await;
    sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p-other', '/other', 'other', ?)").bind(db::now()).execute(&e.app.db).await.unwrap();
    let r = api::create(State(e.app.clone()), Path("p-other".into()), Json(daily(&bot.id, "x"))).await;
    assert!(matches!(r, Err(LcError::Bad(_))), "跨專案的 Bot 不能接收排程");
}

#[tokio::test]
async fn run_replay_with_the_same_key_returns_the_same_occurrence() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let run = || api::run_now(State(e.app.clone()), Path(s.id.clone()), Json(RunIn { client_request_id: Some("run-1".into()), dry_run: false }));
    let a = run().await.unwrap().0;
    let b = run().await.unwrap().0;
    assert_eq!(a["occurrence"]["id"], b["occurrence"]["id"]);
    assert_eq!(count(&e.app, "SELECT COUNT(*) FROM schedule_occurrences WHERE schedule_id = ?", &s.id).await, 1);
}

#[tokio::test]
async fn run_now_refuses_a_disabled_schedule_and_a_bad_key() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    sqlx::query("UPDATE schedules SET enabled = 0 WHERE id = ?").bind(&s.id).execute(&e.app.db).await.unwrap();
    let r = api::run_now(State(e.app.clone()), Path(s.id.clone()), Json(RunIn::default())).await;
    assert!(matches!(r, Err(LcError::Conflict(_))));
    sqlx::query("UPDATE schedules SET enabled = 1 WHERE id = ?").bind(&s.id).execute(&e.app.db).await.unwrap();
    let bad = api::run_now(State(e.app.clone()), Path(s.id.clone()), Json(RunIn { client_request_id: Some("has space".into()), dry_run: false })).await;
    assert!(matches!(bad, Err(LcError::Bad(_))));
}

#[tokio::test]
async fn unknown_outcome_pauses_the_schedule_in_the_same_step() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let occ = pending(&e.app, &s).await;
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let t = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','failed','unknown',?)")
        .bind(&t)
        .bind(&conv)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE schedule_occurrences SET status='queued', turn_id=?, conversation_id=? WHERE id=?").bind(&t).bind(&conv).bind(&occ).execute(&e.app.db).await.unwrap();
    let transitions = store::settle_queued(&e.app.db, &db::now()).await.unwrap();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].status, "unknown");
    let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
    assert_eq!(o.status, "unknown");
    let enabled: i64 = sqlx::query_scalar("SELECT enabled FROM schedules WHERE id = ?").bind(&s.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(enabled, 0, "同一步暫停，下次到期不會自動重跑");
    // 之後再 settle 不會重複通知（notified 去重）。
    assert!(store::settle_queued(&e.app.db, &db::now()).await.unwrap().is_empty());
}

async fn run_with(app: &std::sync::Arc<crate::state::App>, sid: &str, crid: &str) -> String {
    let v = api::run_now(State(app.clone()), Path(sid.into()), Json(RunIn { client_request_id: Some(crid.into()), dry_run: false })).await.unwrap().0;
    v["occurrence"]["id"].as_str().expect("回應要有 occurrence.id").to_string()
}

async fn turns_for(app: &std::sync::Arc<crate::state::App>, bot: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE c.bot_id = ?").bind(bot).fetch_one(&app.db).await.unwrap()
}

/// 把綁定的 turn 推到終態（測試用：排隊中的 turn 撤回成 failed，是合法的 queued→failed）。
async fn finish_turn(app: &std::sync::Arc<crate::state::App>, occ: &str) {
    let turn = store::get_occurrence(&app.db, occ).await.unwrap().unwrap().turn_id.unwrap();
    sqlx::query("UPDATE turns SET status = 'failed', delivery = 'failed' WHERE id = ?").bind(&turn).execute(&app.db).await.unwrap();
    store::settle_queued(&app.db, &db::now()).await.unwrap();
}

#[tokio::test]
async fn manual_key_merged_into_open_occurrence_replays_the_same_occurrence_after_it_finishes() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let k1 = run_with(&e.app, &s.id, "k1").await;
    // K2 在 K1 的 occurrence 還沒結束時送來：併進去，但 key 也要記下來。
    let k2 = run_with(&e.app, &s.id, "k2").await;
    assert_eq!(k1, k2, "未結束的同一筆：合併");
    assert_eq!(turns_for(&e.app, &bot.id).await, 1);
    finish_turn(&e.app, &k1).await;
    assert_eq!(store::get_occurrence(&e.app.db, &k1).await.unwrap().unwrap().status, "failed");
    // 回應遺失後的重送（K2）：回同一筆，不多排 turn、不多開 occurrence。
    assert_eq!(run_with(&e.app, &s.id, "k2").await, k1, "K2 重放回原來那一筆");
    assert_eq!(run_with(&e.app, &s.id, "k1").await, k1, "K1 重放也回原來那一筆");
    assert_eq!(count(&e.app, "SELECT COUNT(*) FROM schedule_occurrences WHERE schedule_id = ?", &s.id).await, 1);
    assert_eq!(turns_for(&e.app, &bot.id).await, 1, "重放不多排 turn");
}

#[tokio::test]
async fn manual_key_absorbed_by_a_scheduled_occurrence_with_no_key_replays_that_occurrence() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let scheduled = pending(&e.app, &s).await;
    assert!(runner::enqueue(&e.app, &scheduled).await.unwrap());
    // 排程來的 occurrence（client_request_id 是 NULL）還沒結束時，手動 key 併進去。
    assert_eq!(run_with(&e.app, &s.id, "m1").await, scheduled);
    assert_eq!(turns_for(&e.app, &bot.id).await, 1);
    finish_turn(&e.app, &scheduled).await;
    assert_eq!(run_with(&e.app, &s.id, "m1").await, scheduled, "重放回同一筆");
    assert_eq!(count(&e.app, "SELECT COUNT(*) FROM schedule_occurrences WHERE schedule_id = ?", &s.id).await, 1);
    assert_eq!(turns_for(&e.app, &bot.id).await, 1, "不多排 turn");
}

fn patch_in() -> api::PatchIn {
    api::PatchIn { name: None, prompt: None, bot_id: None, tz: None, spec: None, notify: None, clear_after: None, from_base: None, retry_count: None, retry_interval_mins: None, retry_deadline_mins: None, enabled: None }
}

#[tokio::test]
async fn clear_after_round_trips_through_create_get_and_patch() {
    let (e, bot) = setup().await;
    let pid = e.project_id.clone();
    let mut input = daily(&bot.id, "一");
    input.clear_after = true;
    let created = api::create(State(e.app.clone()), Path(pid.clone()), Json(input)).await.unwrap().0;
    assert_eq!(created["clear_after"], true, "POST 回應要帶 clear_after");
    let sid = created["id"].as_str().unwrap().to_string();
    let got = api::get_one(State(e.app.clone()), Path(sid.clone())).await.unwrap().0;
    assert_eq!(got["clear_after"], true, "GET 回 true");
    // 只帶 enabled 的部分更新不碰 clear_after。
    let stopped = api::patch(State(e.app.clone()), Path(sid.clone()), Json(api::PatchIn { enabled: Some(false), ..patch_in() })).await.unwrap().0;
    assert_eq!(stopped["clear_after"], true, "沒帶就保留原值");
    let edited = api::patch(State(e.app.clone()), Path(sid.clone()), Json(api::PatchIn { clear_after: Some(false), ..patch_in() })).await.unwrap().0;
    assert_eq!(edited["clear_after"], false, "PATCH 改成 false");
    let got = api::get_one(State(e.app.clone()), Path(sid)).await.unwrap().0;
    assert_eq!(got["clear_after"], false);
}

#[tokio::test]
async fn clear_after_is_part_of_the_create_key_fingerprint() {
    let (e, bot) = setup().await;
    let pid = e.project_id.clone();
    let plain = daily(&bot.id, "一");
    let legacy = api::create_fingerprint(&plain);
    assert!(!legacy.contains("clear_after"), "沒勾的 fingerprint 維持升版前的樣子，原樣重送仍回原筆");
    api::create(State(e.app.clone()), Path(pid.clone()), Json(plain)).await.unwrap();
    let mut other = daily(&bot.id, "一");
    other.clear_after = true;
    let r = api::create(State(e.app.clone()), Path(pid), Json(other)).await;
    assert!(matches!(r, Err(LcError::Conflict(_))), "同 key 只差 clear_after：要 409，不默默回舊的");
}

#[test]
fn only_done_clears_the_bot_conversation() {
    let t = |status: &str, clear_after: bool| store::Transition {
        schedule_id: "s".into(),
        schedule_name: "n".into(),
        project_id: "p".into(),
        occurrence_id: "o".into(),
        status: status.into(),
        notify: "failure".into(),
        error: None,
        bot_id: "b".into(),
        clear_after,
        outcome: if status == "done" { "success" } else { "unknown" }.into(),
        attempt_no: 1,
    };
    assert!(runner::should_clear_after(&t("done", true)));
    assert!(!runner::should_clear_after(&t("done", false)), "沒勾就不清");
    for status in ["failed", "unknown", "cancelled"] {
        assert!(!runner::should_clear_after(&t(status, true)), "{status} 留著，讓使用者看得出出了什麼事");
    }
}

/// 排一筆已綁定、已完成（completed＋delivery ok）的 occurrence，回傳 settle 出來的終態轉移。
async fn finished_occurrence(app: &std::sync::Arc<crate::state::App>, s: &Schedule) -> store::Transition {
    let occ = pending(app, s).await;
    let conv = db::conversation_id(&app.db, &s.bot_id).await.unwrap();
    let t = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','completed','ok',?)")
        .bind(&t)
        .bind(&conv)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE schedule_occurrences SET status='queued', turn_id=?, conversation_id=? WHERE id=?").bind(&t).bind(&conv).bind(&occ).execute(&app.db).await.unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'assistant','已完成','transcript',?)")
        .bind(db::ulid()).bind(&conv).bind(&t).bind(db::now()).execute(&app.db).await.unwrap();
    let mut transitions = store::settle_queued(&app.db, &db::now()).await.unwrap();
    assert_eq!(transitions.len(), 1);
    transitions.remove(0)
}

#[tokio::test]
async fn a_done_clear_after_schedule_opens_a_fresh_conversation_on_a_running_bot() {
    let (e, bot) = setup().await;
    let run1 = lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    sqlx::query("UPDATE schedules SET clear_after = 1 WHERE id = ?").bind(&s.id).execute(&e.app.db).await.unwrap();
    let t = finished_occurrence(&e.app, &s).await;
    assert_eq!(t.status, "done");
    assert!(runner::should_clear_after(&t));
    let run2 = runner::clear_bot_after(&e.app, &t.bot_id).await.unwrap().expect("done 且條件成立：要重啟");
    assert_ne!(run2, run1, "新的 run（新對話）");
    assert_eq!(db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap().id, run2);
}

#[tokio::test]
async fn clear_does_not_start_a_bot_that_is_not_running() {
    let (e, bot) = setup().await;
    assert_eq!(runner::clear_bot_after(&e.app, &bot.id).await.unwrap(), None, "沒在跑就不替它啟動");
    assert!(db::active_run(&e.app.db, &bot.id).await.unwrap().is_none());
}

#[tokio::test]
async fn clear_waits_when_the_bot_has_another_queued_turn_or_schedule() {
    let (e, bot) = setup().await;
    let run1 = lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    // 使用者另外排的回合還在佇列裡：不清（不覆蓋、不撤回別人的）。
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, awaits_idle) VALUES (?,?,'web','queued','pending',?,1)")
        .bind(db::ulid())
        .bind(&conv)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    assert_eq!(runner::clear_bot_after(&e.app, &bot.id).await.unwrap(), None);
    assert_eq!(db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap().id, run1, "沒有重啟");
    // 另一筆排程的 occurrence 還沒結束：也不清。
    // 使用者那筆撤掉（queued→failed 是合法轉移）；之後只剩排程的未結束 occurrence 擋著。
    sqlx::query("UPDATE turns SET status = 'failed', delivery = 'failed' WHERE conversation_id = ? AND status = 'queued'").bind(&conv).execute(&e.app.db).await.unwrap();
    let _other = pending(&e.app, &s).await;
    assert_eq!(runner::clear_bot_after(&e.app, &bot.id).await.unwrap(), None);
    assert_eq!(db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap().id, run1);
}

#[test]
fn task_outcomes_control_failure_notices() {
    for outcome in ["failed", "refused", "unknown"] { assert!(store::wants_notice("failure", outcome)); }
    assert!(!store::wants_notice("failure", "success"));
    assert!(store::wants_notice("all", "success"));
    assert!(!store::wants_notice("none", "failed"));
}

#[tokio::test]
async fn retries_keep_attempt_evidence_obey_deadline_and_never_repeat_refusals() {
    let (e, bot) = setup().await;
    let mut s = schedule(&e.app, &e.project_id, &bot.id).await;
    s.retry_count = 2;
    sqlx::query("UPDATE schedules SET retry_count=2 WHERE id=?").bind(&s.id).execute(&e.app.db).await.unwrap();
    let occ_id = pending(&e.app, &s).await;
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let turn = db::ulid();
    let reply = db::ulid();
    let now = db::now();
    sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES (?,?,'web','completed','ok',?)")
        .bind(&turn).bind(&conv).bind(&now).execute(&e.app.db).await.unwrap();
    sqlx::query("INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at) VALUES (?,?,?,'assistant','操作失敗 password=synthetic-secret','transcript',?)")
        .bind(&reply).bind(&conv).bind(&turn).bind(&now).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE schedule_occurrences SET status='queued',turn_id=?,conversation_id=? WHERE id=?")
        .bind(&turn).bind(&conv).bind(&occ_id).execute(&e.app.db).await.unwrap();
    let ts = store::settle_queued(&e.app.db, &now).await.unwrap();
    assert_eq!(ts.len(), 1);
    assert_eq!(ts[0].outcome, "failed");
    let mut occ = store::get_occurrence(&e.app.db, &occ_id).await.unwrap().unwrap();
    assert_eq!((occ.status.as_str(), occ.attempt_no), ("pending", 2));
    assert!(occ.next_retry_at.is_some());
    assert!(!occ.outcome_summary.as_ref().unwrap().contains("synthetic-secret"));
    assert!(!runner::enqueue(&e.app, &occ_id).await.unwrap(), "not due yet");
    let view = store::occurrence_view(&e.app.db, &occ).await.unwrap();
    assert_eq!(view["attempts"][0]["reply_message_id"], reply);
    assert_eq!(view["attempts"][0]["turn_id"], turn);
    assert!(store::settle_queued(&e.app.db, &now).await.unwrap().is_empty());
    for outcome in ["refused", "unknown", "success"] { assert!(store::retry_at(&s, &occ, outcome, &now).is_none()); }
    occ.attempt_no = 3;
    assert!(store::retry_at(&s, &occ, "failed", &now).is_none());
    occ.attempt_no = 1;
    let due = chrono::DateTime::parse_from_rfc3339(&occ.scheduled_for).unwrap();
    let at = |mins| (due + chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    assert!(store::retry_at(&s, &occ, "failed", &at(12)).is_some());
    assert!(store::retry_at(&s, &occ, "failed", &at(13)).is_none());
    sqlx::query("UPDATE schedule_occurrences SET scheduled_for=?,next_retry_at=? WHERE id=?").bind(at(-20)).bind(at(-1)).bind(&occ_id).execute(&e.app.db).await.unwrap();
    assert!(!runner::enqueue(&e.app, &occ_id).await.unwrap());
    assert_eq!(store::get_occurrence(&e.app.db, &occ_id).await.unwrap().unwrap().status, "failed");
    assert_eq!(count(&e.app, "SELECT COUNT(*) FROM turns WHERE conversation_id=?", &conv).await, 1);
}

#[tokio::test]
async fn refusal_is_persisted_without_retry_even_with_retry_enabled() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    sqlx::query("UPDATE schedules SET retry_count=5 WHERE id=?").bind(&s.id).execute(&e.app.db).await.unwrap();
    let occ = pending(&e.app, &s).await;
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let turn = db::ulid();
    sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES (?,?,'web','completed','ok',?)")
        .bind(&turn).bind(&conv).bind(db::now()).execute(&e.app.db).await.unwrap();
    sqlx::query("INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at) VALUES (?,?,?,'assistant','無法協助這個要求','transcript',?)")
        .bind(db::ulid()).bind(&conv).bind(&turn).bind(db::now()).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE schedule_occurrences SET status='queued',turn_id=? WHERE id=?").bind(&turn).bind(&occ).execute(&e.app.db).await.unwrap();
    let ts = store::settle_queued(&e.app.db, &db::now()).await.unwrap();
    assert_eq!(ts[0].outcome, "refused");
    let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
    assert_eq!(o.status, "done");
    assert!(o.next_retry_at.is_none());
    assert!(!runner::should_clear_after(&ts[0]));
}

#[tokio::test]
async fn due_retry_binds_a_new_turn_and_keeps_the_prior_attempt_link() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let occ_id = pending(&e.app, &s).await;
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let old = db::ulid();
    sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES (?,?,'web','failed','ok',?)")
        .bind(&old).bind(&conv).bind(db::now()).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE schedule_occurrences SET status='queued',turn_id=?,conversation_id=? WHERE id=?").bind(&old).bind(&conv).bind(&occ_id).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE schedules SET retry_count=1 WHERE id=?").bind(&s.id).execute(&e.app.db).await.unwrap();
    store::settle_queued(&e.app.db, &db::now()).await.unwrap();
    sqlx::query("UPDATE schedule_occurrences SET next_retry_at=? WHERE id=?").bind(db::now()).bind(&occ_id).execute(&e.app.db).await.unwrap();
    assert!(runner::enqueue(&e.app, &occ_id).await.unwrap());
    let o = store::get_occurrence(&e.app.db, &occ_id).await.unwrap().unwrap();
    assert_ne!(o.turn_id.as_ref(), Some(&old));
    assert_eq!(o.attempt_no, 2);
    assert!(o.next_retry_at.is_none());
    assert_eq!(turn_status(&e.app, &old).await, "failed");
    let prior: String = sqlx::query_scalar("SELECT turn_id FROM schedule_attempts WHERE occurrence_id=? AND attempt_no=1").bind(&occ_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(prior, old);
}

#[tokio::test]
async fn a_retry_waiting_in_the_queue_cannot_be_claimed_past_its_deadline() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    sqlx::query("UPDATE schedules SET retry_count=1 WHERE id=?").bind(&s.id).execute(&e.app.db).await.unwrap();
    let occ = pending(&e.app, &s).await;
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let turn = db::ulid();
    sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES (?,?,'web','queued','pending',?)")
        .bind(&turn).bind(&conv).bind(db::now()).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE schedule_occurrences SET status='queued',attempt_no=2,turn_id=?,scheduled_for='2020-01-01T00:00:00.000Z' WHERE id=?")
        .bind(&turn).bind(&occ).execute(&e.app.db).await.unwrap();
    assert!(sqlx::query("UPDATE turns SET status='in_flight' WHERE id=?").bind(&turn).execute(&e.app.db).await.is_err());
    assert_eq!(turn_status(&e.app, &turn).await, "queued");
    runner::expire_queued_retries(&e.app, &db::now()).await.unwrap();
    assert_eq!(turn_status(&e.app, &turn).await, "failed");
    assert_eq!(store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap().status, "failed");
    assert_eq!(store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap().outcome.as_deref(), Some("failed"));
    let normal = db::ulid();
    sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES (?,?,'web','queued','pending',?)")
        .bind(&normal).bind(&conv).bind(db::now()).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE turns SET status='in_flight' WHERE id=?").bind(&normal).execute(&e.app.db).await.unwrap();
    let reduced = db::ulid();
    sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES (?,?,'web','queued','pending',?)")
        .bind(&reduced).bind(&conv).bind(db::now()).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE schedule_occurrences SET status='queued',turn_id=?,scheduled_for=? WHERE id=?").bind(&reduced).bind(db::now()).bind(&occ).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE schedules SET retry_count=0 WHERE id=?").bind(&s.id).execute(&e.app.db).await.unwrap();
    assert!(sqlx::query("UPDATE turns SET status='in_flight' WHERE id=?").bind(&reduced).execute(&e.app.db).await.is_err());
    runner::expire_queued_retries(&e.app, &db::now()).await.unwrap();
    assert_eq!(turn_status(&e.app, &reduced).await, "failed");
}

#[tokio::test]
async fn unavailable_bot_waits_once_and_batch_rebind_withdraws_that_occurrence() {
    let (e, bot) = setup().await;
    let target = dedicated(&e, "replacement").await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let occurrence = pending(&e.app, &s).await;
    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&bot.id).execute(&e.app.db).await.unwrap();
    assert!(!runner::enqueue(&e.app, &occurrence).await.unwrap());
    assert!(!runner::enqueue(&e.app, &occurrence).await.unwrap());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM schedule_occurrences WHERE schedule_id = ? AND status = 'pending' AND waiting_reason = 'bot_unavailable'").bind(&s.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(count, 1);
    let _ = api::rebind(State(e.app.clone()), Path(e.project_id.clone()), Json(api::RebindIn { schedule_ids: vec![s.id.clone()], bot_id: target.id.clone() })).await.unwrap();
    let updated = store::get_schedule(&e.app.db, &s.id).await.unwrap().unwrap();
    assert_eq!(updated.bot_id, target.id);
    assert_eq!(updated.next_due, s.next_due);
    let status: String = sqlx::query_scalar("SELECT status FROM schedule_occurrences WHERE id = ?").bind(&occurrence).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(status, "cancelled");
}

#[tokio::test]
async fn batch_rebind_is_atomic_when_any_schedule_has_an_open_execution() {
    let (e, bot) = setup().await;
    let target = dedicated(&e, "replacement").await;
    let first = schedule(&e.app, &e.project_id, &bot.id).await;
    let second = schedule(&e.app, &e.project_id, &bot.id).await;
    pending(&e.app, &second).await;
    let result = api::rebind(State(e.app.clone()), Path(e.project_id.clone()), Json(api::RebindIn { schedule_ids: vec![first.id.clone(), second.id.clone()], bot_id: target.id })).await;
    assert!(result.is_err());
    for id in [&first.id, &second.id] {
        assert_eq!(store::get_schedule(&e.app.db, id).await.unwrap().unwrap().bot_id, bot.id);
    }
}

#[tokio::test]
async fn batch_rebind_rejects_missing_target_without_mutating_schedules() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let result = api::rebind(State(e.app.clone()), Path(e.project_id.clone()), Json(api::RebindIn { schedule_ids: vec![s.id.clone()], bot_id: "missing-bot".into() })).await;
    assert!(result.is_err());
    let updated = store::get_schedule(&e.app.db, &s.id).await.unwrap().unwrap();
    assert_eq!(updated.bot_id, bot.id);
    assert_eq!(updated.generation, s.generation);
}

#[tokio::test]
async fn dry_run_has_distinct_source_key_and_never_merges_with_a_formal_run() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    let run = |dry| api::run_now(State(e.app.clone()), Path(s.id.clone()), Json(RunIn { client_request_id: Some("same".into()), dry_run: dry }));
    let first = run(false).await.unwrap().0;
    let formal = first["occurrence"]["id"].as_str().unwrap().to_string();
    assert!(run(true).await.is_err(), "dry run must not merge with the formal open occurrence");
    finish_turn(&e.app, &formal).await;
    let dry = run(true).await.unwrap().0;
    let dry_id = dry["occurrence"]["id"].as_str().unwrap().to_string();
    assert_ne!(formal, dry_id);
    assert_eq!(dry["occurrence"]["source"], "dry_run");
    let occ = store::get_occurrence(&e.app.db, &dry_id).await.unwrap().unwrap();
    assert!(occ.prompt.starts_with("試跑："));
    assert!(occ.prompt.ends_with(&s.prompt));
    assert_eq!(run(true).await.unwrap().0["occurrence"]["id"], dry_id);
    // An already accepted formal key always replays its own occurrence, not the dry run.
    assert_eq!(run(false).await.unwrap().0["occurrence"]["id"], formal);
    let new_formal = api::run_now(State(e.app.clone()), Path(s.id.clone()), Json(RunIn { client_request_id: Some("another".into()), dry_run: false })).await;
    assert!(new_formal.is_err(), "formal run must not merge with the open dry run");
    let turn = store::get_occurrence(&e.app.db, &dry_id).await.unwrap().unwrap().turn_id.unwrap();
    sqlx::query("UPDATE turns SET status='failed', delivery='failed' WHERE id=?").bind(turn).execute(&e.app.db).await.unwrap();
    let transitions = store::settle_queued(&e.app.db, &db::now()).await.unwrap();
    let outcome = transitions.iter().find(|t| t.occurrence_id == dry_id).unwrap();
    assert_eq!(outcome.status, "failed");
    assert_eq!(outcome.notify, "none", "dry failures do not emit schedule failure notices");
}

#[tokio::test]
async fn scheduled_due_is_not_consumed_or_merged_while_a_dry_run_is_open() {
    let (e, bot) = setup().await;
    let mut s = schedule(&e.app, &e.project_id, &bot.id).await;
    let due = db::now();
    sqlx::query("UPDATE schedules SET next_due=? WHERE id=?").bind(&due).bind(&s.id).execute(&e.app.db).await.unwrap();
    s.next_due = Some(due.clone());
    let o = pending(&e.app, &s).await;
    sqlx::query("UPDATE schedule_occurrences SET source='dry_run' WHERE id=?").bind(&o).execute(&e.app.db).await.unwrap();
    runner::materialize(&e.app.db, &s, &due).await.unwrap();
    assert_eq!(store::get_schedule(&e.app.db, &s.id).await.unwrap().unwrap().next_due, Some(due));
    assert_eq!(store::get_occurrence(&e.app.db, &o).await.unwrap().unwrap().merged_count, 0);
}

#[tokio::test]
async fn dry_run_never_retries_and_never_sends_failure_notices() {
    let (e, bot) = setup().await;
    let mut s = schedule(&e.app, &e.project_id, &bot.id).await;
    s.retry_count = 5;
    s.retry_deadline_mins = 60;
    let occ_id = pending(&e.app, &s).await;
    let mut o = store::get_occurrence(&e.app.db, &occ_id).await.unwrap().unwrap();
    assert!(store::retry_at(&s, &o, "failed", &o.scheduled_for).is_some(), "正式執行會重試");
    sqlx::query("UPDATE schedule_occurrences SET source='dry_run' WHERE id=?").bind(&occ_id).execute(&e.app.db).await.unwrap();
    o = store::get_occurrence(&e.app.db, &occ_id).await.unwrap().unwrap();
    assert!(store::retry_at(&s, &o, "failed", &o.scheduled_for).is_none(), "試跑不重試");
    assert!(!store::wants_notice("none", "failed"), "試跑的 notify 是 none，不發失敗通知");
}

/// 隨機區間：到期後下一次落在隔天，不會同一天再抽；抽到的時間存在 next_due，重算（materialize 以外）不變。
#[tokio::test]
async fn random_window_materializes_once_and_stores_the_next_draw_on_the_following_day() {
    let (e, bot) = setup().await;
    let now = chrono::Utc::now();
    // 用 UTC 時區、窗口＝「現在往前 1 分鐘到往後 1 分鐘」，next_due 設成已到期的一秒（窗口內）。
    let (lo, hi) = (now - chrono::Duration::minutes(1), now + chrono::Duration::minutes(1));
    if lo.date_naive() != hi.date_naive() {
        return; // 跨午夜的瞬間，窗口不成立；下次再跑就好
    }
    let (start, end) = (lo.format("%H:%M:%S").to_string(), hi.format("%H:%M:%S").to_string());
    let mut s = schedule(&e.app, &e.project_id, &bot.id).await;
    s.spec_json = format!(r#"{{"mode":"daily","time":"{start}","slots":[{{"time":"{start}","label":"","until":"{end}"}}]}}"#);
    let due = (now - chrono::Duration::seconds(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE schedules SET spec_json = ?, next_due = ? WHERE id = ?").bind(&s.spec_json).bind(&due).bind(&s.id).execute(&e.app.db).await.unwrap();
    s.next_due = Some(due);
    runner::materialize(&e.app.db, &s, &db::now()).await.unwrap();
    assert_eq!(count(&e.app, "SELECT COUNT(*) FROM schedule_occurrences WHERE schedule_id = ?", &s.id).await, 1);
    let next = store::get_schedule(&e.app.db, &s.id).await.unwrap().unwrap().next_due.unwrap();
    let next = chrono::DateTime::parse_from_rfc3339(&next).unwrap();
    // 下一次一定在窗口結束之後（隔天的窗口），不是同一天再抽。
    assert!(next.with_timezone(&chrono::Utc) > now + chrono::Duration::hours(12), "{next}");
}

#[tokio::test]
async fn random_window_create_rejects_bad_ranges_with_a_code_and_stores_the_draw() {
    let (e, bot) = setup().await;
    let mut bad = daily(&bot.id, "p");
    bad.spec.slots = Some(vec![super::time::Slot { time: "09:00".into(), label: String::new(), until: Some("08:00".into()) }]);
    match api::create(State(e.app.clone()), Path(e.project_id.clone()), Json(bad)).await {
        Err(LcError::BadValue(v)) => assert_eq!(v["error"], "slot_range_not_after_start"),
        other => panic!("{other:?}"),
    }
    let mut ok = daily(&bot.id, "p");
    ok.client_request_id = "crid-range".into();
    ok.spec.slots = Some(vec![super::time::Slot { time: "09:00".into(), label: String::new(), until: Some("09:05".into()) }]);
    let Json(v) = api::create(State(e.app.clone()), Path(e.project_id.clone()), Json(ok)).await.unwrap();
    let due = chrono::DateTime::parse_from_rfc3339(v["next_due"].as_str().unwrap()).unwrap().with_timezone(&chrono::Utc);
    let secs = chrono::Timelike::num_seconds_from_midnight(&due.time());
    assert!((9 * 3600..=9 * 3600 + 300).contains(&secs), "{due}");
    // 同一筆再讀：next_due 不變（存下後不重抽）。
    let id = v["id"].as_str().unwrap().to_string();
    let Json(again) = api::get_one(State(e.app.clone()), Path(id)).await.unwrap();
    assert_eq!(again["next_due"], v["next_due"]);
}

// ── 基準快照與「每次從基準快照開始」 ──────────────────────────────────────────

mod from_base {
    use super::*;
    use crate::api::RequestPrincipal;
    use crate::schedules::base;
    use serde_json::{json, Value};

    async fn codex_bot(e: &testing::Env, name: &str) -> db::Bot {
        let bot = super::dedicated(e, name).await;
        sqlx::query("UPDATE bots SET kind = 'codex' WHERE id = ?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        db::bot(&e.app.db, &bot.id).await.unwrap().unwrap()
    }

    async fn ended_session(e: &testing::Env, bot: &str, sid: &str, transcript: Option<&str>) {
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at) VALUES (?,?,'stopped','idle',?,?,?,?)")
            .bind(db::ulid()).bind(bot).bind(sid).bind(transcript).bind("2026-09-14T00:00:00Z").bind("2026-09-14T00:01:00Z")
            .execute(&e.app.db).await.unwrap();
    }

    async fn call(e: &testing::Env, f: &str, bot: &str, who: RequestPrincipal) -> Result<Value, LcError> {
        let (s, p, x) = (axum::extract::State(e.app.clone()), axum::extract::Path(bot.to_string()), axum::Extension(who));
        let res = match f {
            "get" => base::get_snapshot(s, p, x).await,
            "lock" => base::lock(s, p, x).await,
            _ => base::clear(s, p, x).await,
        }?;
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    fn started_args(e: &testing::Env) -> Vec<Vec<String>> {
        e.herdr.calls.lock().unwrap().iter().filter(|(m, _)| m == "agent.start")
            .filter_map(|(_, p)| p.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())).collect()
    }

    async fn from_base_schedule(e: &testing::Env, bot: &str, retry: i64) -> Schedule {
        let s = schedule(&e.app, &e.project_id, bot).await;
        sqlx::query("UPDATE schedules SET from_base = 1, retry_count = ? WHERE id = ?").bind(retry).bind(&s.id).execute(&e.app.db).await.unwrap();
        store::get_schedule(&e.app.db, &s.id).await.unwrap().unwrap()
    }

    async fn status_and_error(e: &testing::Env, occ: &str) -> (String, Option<String>) {
        sqlx::query_as("SELECT status, error FROM schedule_occurrences WHERE id = ?").bind(occ).fetch_one(&e.app.db).await.unwrap()
    }

    #[tokio::test]
    async fn lock_query_and_clear_round_trip() {
        let e = testing::env().await;
        let bot = codex_bot(&e, "cx").await;
        assert_eq!(call(&e, "get", &bot.id, RequestPrincipal::User).await.unwrap()["locked"], false);
        let no_session = call(&e, "lock", &bot.id, RequestPrincipal::User).await.unwrap_err();
        assert!(matches!(no_session, LcError::Conflict(ref v) if v["reason"] == "base_snapshot_no_session"), "{no_session:?}");
        ended_session(&e, &bot.id, "sid-1", None).await;
        let locked = call(&e, "lock", &bot.id, RequestPrincipal::User).await.unwrap();
        assert_eq!((locked["locked"].as_bool(), locked["session_id"].as_str(), locked["locked_by"].as_str()), (Some(true), Some("sid-1"), Some("user")));
        assert!(locked["locked_at"].is_string());
        ended_session(&e, &bot.id, "sid-2", None).await;
        assert_eq!(call(&e, "lock", &bot.id, RequestPrincipal::User).await.unwrap()["session_id"], "sid-2", "再鎖一次換成目前的");
        assert_eq!(call(&e, "clear", &bot.id, RequestPrincipal::User).await.unwrap()["locked"], false);
        assert_eq!(call(&e, "get", &bot.id, RequestPrincipal::User).await.unwrap()["locked"], false);
    }

    #[tokio::test]
    async fn only_codex_bots_can_lock() {
        let e = testing::env().await;
        let bot = testing::claude_bot(&e.app, &e.project_id, "cl").await;
        ended_session(&e, &bot.id, "sid-1", None).await;
        let err = call(&e, "lock", &bot.id, RequestPrincipal::User).await.unwrap_err();
        assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "base_snapshot_not_codex"), "{err:?}");
    }

    #[tokio::test]
    async fn only_the_user_and_the_expert_manager_may_use_it() {
        let e = testing::env().await;
        let target = codex_bot(&e, "cx").await;
        ended_session(&e, &target.id, "sid-1", None).await;
        let peer = testing::claude_bot(&e.app, &e.project_id, "peer").await;
        for who in [peer.id.clone(), target.id.clone()] {
            for f in ["get", "lock", "clear"] {
                let err = call(&e, f, &target.id, RequestPrincipal::Bot(who.clone())).await.unwrap_err();
                assert!(matches!(err, LcError::Forbidden(_)), "{f} by {who}: {err:?}");
            }
        }
        // 同專案 expert 管理者可以；專案不是 expert 就不行。
        sqlx::query("UPDATE bots SET role = 'manager' WHERE id = ?").bind(&peer.id).execute(&e.app.db).await.unwrap();
        assert!(call(&e, "lock", &target.id, RequestPrincipal::Bot(peer.id.clone())).await.is_err(), "非 expert 專案的 manager 不行");
        sqlx::query("UPDATE projects SET mode = 'expert' WHERE id = ?").bind(&e.project_id).execute(&e.app.db).await.unwrap();
        let ok = call(&e, "lock", &target.id, RequestPrincipal::Bot(peer.id.clone())).await.unwrap();
        assert_eq!(ok["locked_by"], peer.id.as_str());
        assert_eq!(call(&e, "clear", &target.id, RequestPrincipal::Bot(peer.id.clone())).await.unwrap()["locked"], false);
    }

    #[tokio::test]
    async fn the_runner_forks_the_base_before_sending() {
        let e = testing::env().await;
        let bot = codex_bot(&e, "cx").await;
        ended_session(&e, &bot.id, "sid-base", None).await;
        call(&e, "lock", &bot.id, RequestPrincipal::User).await.unwrap();
        let run1 = lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
        let before = started_args(&e).len();
        let s = from_base_schedule(&e, &bot.id, 0).await;
        let occ = pending(&e.app, &s).await;
        assert!(base::dispatch(&e.app, &occ).await.unwrap(), "fork 重啟後綁成 turn");
        let args = started_args(&e);
        assert_eq!(args.len(), before + 1, "多一次重啟");
        assert_eq!(&args.last().unwrap()[..2], ["fork", "sid-base"], "{args:?}");
        let run2 = db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap().id;
        assert_ne!(run2, run1, "新的 run");
        let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
        assert_eq!(o.status, "queued");
        assert!(o.turn_id.is_some());
        let snap = base::get(&e.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(snap.session_id, "sid-base", "基準本身不被改動");
    }

    #[tokio::test]
    async fn a_busy_bot_waits_and_is_not_restarted() {
        let e = testing::env().await;
        let bot = codex_bot(&e, "cx").await;
        ended_session(&e, &bot.id, "sid-base", None).await;
        call(&e, "lock", &bot.id, RequestPrincipal::User).await.unwrap();
        lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES (?,?,'web','in_flight','pending',?)")
            .bind(db::ulid()).bind(&conv).bind(db::now()).execute(&e.app.db).await.unwrap();
        let before = started_args(&e).len();
        let s = from_base_schedule(&e, &bot.id, 0).await;
        let occ = pending(&e.app, &s).await;
        assert!(!base::dispatch(&e.app, &occ).await.unwrap());
        assert_eq!(started_args(&e).len(), before, "忙碌時不重啟");
        let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
        assert_eq!((o.status.as_str(), o.waiting_reason.as_deref()), ("pending", Some("bot_busy")));
    }

    #[tokio::test]
    async fn a_missing_base_fails_without_running_in_the_old_context() {
        let e = testing::env().await;
        let bot = codex_bot(&e, "cx").await;
        ended_session(&e, &bot.id, "sid-base", None).await;
        let run1 = lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
        let before = started_args(&e).len();
        let s = from_base_schedule(&e, &bot.id, 3).await;
        let occ = pending(&e.app, &s).await;
        assert!(!base::dispatch(&e.app, &occ).await.unwrap());
        let (status, error) = status_and_error(&e, &occ).await;
        assert_eq!(status, "failed");
        assert!(error.unwrap().contains("base_snapshot_missing"));
        assert_eq!(started_args(&e).len(), before, "不重啟、不在舊 context 送");
        assert_eq!(db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap().id, run1);
        assert_eq!(count(&e.app, "SELECT COUNT(*) FROM turns WHERE conversation_id IN (SELECT id FROM conversations WHERE bot_id = ?)", &bot.id).await, 0);
    }

    #[tokio::test]
    async fn a_deleted_rollout_file_fails_with_its_own_code() {
        let e = testing::env().await;
        let bot = codex_bot(&e, "cx").await;
        let gone = e.dir.join("rollout-gone.jsonl");
        ended_session(&e, &bot.id, "sid-base", Some(gone.to_str().unwrap())).await;
        call(&e, "lock", &bot.id, RequestPrincipal::User).await.unwrap();
        let s = from_base_schedule(&e, &bot.id, 0).await;
        let occ = pending(&e.app, &s).await;
        assert!(!base::dispatch(&e.app, &occ).await.unwrap());
        let (status, error) = status_and_error(&e, &occ).await;
        assert_eq!(status, "failed");
        assert!(error.unwrap().contains("base_snapshot_rollout_missing"));
        assert!(started_args(&e).iter().all(|a| a.first().map(String::as_str) != Some("fork")));
    }

    #[tokio::test]
    async fn a_failed_fork_restart_retries_while_the_schedule_allows_it() {
        let e = testing::env().await;
        let bot = codex_bot(&e, "cx").await;
        let s = from_base_schedule(&e, &bot.id, 1).await;
        let occ = pending(&e.app, &s).await;
        let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
        base::fail(&e.app, &s, &o, "base_fork_timeout", "等不到", true).await.unwrap();
        let o = store::get_occurrence(&e.app.db, &occ).await.unwrap().unwrap();
        assert_eq!((o.status.as_str(), o.attempt_no), ("pending", 2));
        assert!(o.next_retry_at.is_some() && o.error.as_deref().unwrap().contains("base_fork_timeout"));
        base::fail(&e.app, &s, &o, "base_fork_timeout", "等不到", true).await.unwrap();
        let (status, error) = status_and_error(&e, &occ).await;
        assert_eq!(status, "failed", "額度用完就收成 failed");
        assert!(error.unwrap().contains("base_fork_timeout"));
    }

    #[tokio::test]
    async fn without_the_option_nothing_forks() {
        let e = testing::env().await;
        let bot = codex_bot(&e, "cx").await;
        ended_session(&e, &bot.id, "sid-base", None).await;
        call(&e, "lock", &bot.id, RequestPrincipal::User).await.unwrap();
        lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
        let before = started_args(&e).len();
        let s = schedule(&e.app, &e.project_id, &bot.id).await;
        let occ = pending(&e.app, &s).await;
        assert!(runner::enqueue(&e.app, &occ).await.unwrap(), "from_base=0：照舊直接綁 turn");
        assert_eq!(started_args(&e).len(), before);
    }

    #[tokio::test]
    async fn from_base_needs_a_locked_codex_bot_to_turn_on() {
        let e = testing::env().await;
        let cx = codex_bot(&e, "cx").await;
        let mut body = daily(&cx.id, "x");
        body.from_base = true;
        let err = api::create(State(e.app.clone()), Path(e.project_id.clone()), Json(body)).await.err().expect("沒鎖基準不能開");
        assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "base_snapshot_required"), "{err:?}");
        ended_session(&e, &cx.id, "sid-1", None).await;
        call(&e, "lock", &cx.id, RequestPrincipal::User).await.unwrap();
        let mut body = daily(&cx.id, "x");
        body.from_base = true;
        let Json(created) = api::create(State(e.app.clone()), Path(e.project_id.clone()), Json(body)).await.unwrap();
        assert_eq!(created["from_base"], true);
        let cl = super::dedicated(&e, "cl").await;
        let mut body = daily(&cl.id, "x");
        body.client_request_id = "crid-2".into();
        body.from_base = true;
        let err = api::create(State(e.app.clone()), Path(e.project_id.clone()), Json(body)).await.err().expect("claude 不能開");
        assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "base_snapshot_not_codex"), "{err:?}");
        let _ = json!(null);
    }
}

// ── 排程專屬 bot ─────────────────────────────────────────────────────────────

mod dedicated_bot {
    use super::*;

    #[tokio::test]
    async fn new_bindings_must_be_dedicated_but_existing_ones_keep_running() {
        let e = testing::env().await;
        let plain = testing::claude_bot(&e.app, &e.project_id, "plain").await;
        let err = api::create(State(e.app.clone()), Path(e.project_id.clone()), Json(daily(&plain.id, "x"))).await.err().expect("一般 bot 不能新綁");
        assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "schedule_bot_not_dedicated"), "{err:?}");
        // 舊排程綁一般 bot：照常落帳、綁 turn；詳情帶 bot_dedicated=false 供 UI 提示。
        let s = schedule(&e.app, &e.project_id, &plain.id).await;
        let occ = pending(&e.app, &s).await;
        assert!(runner::enqueue(&e.app, &occ).await.unwrap(), "既有綁定照常執行");
        let Json(v) = api::get_one(State(e.app.clone()), Path(s.id.clone())).await.unwrap();
        assert_eq!(v["bot_dedicated"], false);
        // 轉成專屬（role = scheduler）後提示消失。
        sqlx::query("UPDATE bots SET role = 'scheduler' WHERE id = ?").bind(&plain.id).execute(&e.app.db).await.unwrap();
        let Json(v) = api::get_one(State(e.app.clone()), Path(s.id.clone())).await.unwrap();
        assert_eq!(v["bot_dedicated"], true);
    }

    #[tokio::test]
    async fn rebinding_or_changing_the_bot_needs_a_dedicated_target() {
        let e = testing::env().await;
        let own = dedicated(&e, "own").await;
        let plain = testing::claude_bot(&e.app, &e.project_id, "plain").await;
        let s = schedule(&e.app, &e.project_id, &own.id).await;
        let err = api::patch(State(e.app.clone()), Path(s.id.clone()), Json(api::PatchIn { name: None, prompt: None, bot_id: Some(plain.id.clone()), tz: None, spec: None, notify: None, clear_after: None, from_base: None, retry_count: None, retry_interval_mins: None, retry_deadline_mins: None, enabled: None })).await.err().expect("不能改綁一般 bot");
        assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "schedule_bot_not_dedicated"), "{err:?}");
        let err = api::rebind(State(e.app.clone()), Path(e.project_id.clone()), Json(api::RebindIn { schedule_ids: vec![s.id.clone()], bot_id: plain.id.clone() })).await.err().expect("批次改綁也不行");
        assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "schedule_bot_not_dedicated"), "{err:?}");
        let other = dedicated(&e, "other").await;
        api::rebind(State(e.app.clone()), Path(e.project_id.clone()), Json(api::RebindIn { schedule_ids: vec![s.id.clone()], bot_id: other.id.clone() })).await.unwrap();
    }

    #[tokio::test]
    async fn a_dedicated_bot_can_be_shared_by_several_schedules() {
        let e = testing::env().await;
        let own = dedicated(&e, "shared").await;
        let mut a = daily(&own.id, "上班");
        a.client_request_id = "crid-a".into();
        let mut b = daily(&own.id, "下班");
        b.client_request_id = "crid-b".into();
        api::create(State(e.app.clone()), Path(e.project_id.clone()), Json(a)).await.unwrap();
        api::create(State(e.app.clone()), Path(e.project_id.clone()), Json(b)).await.unwrap();
    }
}

#[tokio::test]
async fn a_create_without_retry_fields_defaults_to_two_retries_one_minute_and_no_deadline() {
    let (e, bot) = setup().await;
    let body: CreateIn = serde_json::from_value(serde_json::json!({
        "client_request_id": "crid-d", "name": "預設", "prompt": "x", "bot_id": bot.id, "tz": "UTC",
        "spec": {"mode": "daily", "slots": [{"time": "09:00"}, {"time": "18:00"}]},
    })).unwrap();
    let Json(v) = api::create(State(e.app.clone()), Path(e.project_id.clone()), Json(body)).await.unwrap();
    assert_eq!((v["retry_count"].as_i64(), v["retry_interval_mins"].as_i64()), (Some(2), Some(1)));
    // 沒帶截止＝不設截止：存欄位允許的最大值（7 天），一分鐘間隔的 2 次重試永遠碰不到它。
    assert_eq!(v["retry_deadline_mins"], 10080);
    // 時段不帶標籤也收（標籤欄位保留相容，舊資料照讀）。
    assert_eq!(v["spec"]["slots"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn an_old_schedule_keeps_its_stored_values_and_its_old_deadline_still_applies() {
    let (e, bot) = setup().await;
    let s = schedule(&e.app, &e.project_id, &bot.id).await;
    // 舊排程（建立於舊預設 0／3／15）：讀回原值，不被新預設改寫。
    let Json(v) = api::get_one(State(e.app.clone()), Path(s.id.clone())).await.unwrap();
    assert_eq!((v["retry_count"].as_i64(), v["retry_interval_mins"].as_i64(), v["retry_deadline_mins"].as_i64()), (Some(0), Some(3), Some(15)));
}
