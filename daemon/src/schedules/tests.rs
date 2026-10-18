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
    let bot = testing::claude_bot(&e.app, &e.project_id, "sched-bot").await;
    (e, bot)
}

fn daily(bot: &str, prompt: &str) -> CreateIn {
    CreateIn {
        client_request_id: "crid-1".into(),
        name: "每日".into(),
        prompt: prompt.into(),
        bot_id: bot.into(),
        tz: "UTC".into(),
        spec: SpecIn { mode: "daily".into(), at: None, every_secs: None, time: Some("09:00".into()), weekdays: None },
        notify: "failure".into(),
        clear_after: false,
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
        client_request_id: None,
        status: "pending".into(),
        waiting_reason: None,
        notified: 0,
        merged_count: 0,
        conversation_id: None,
        turn_id: None,
        error: None,
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
    let _ = api::patch(State(e.app.clone()), Path(s.id.clone()), Json(api::PatchIn { name: None, prompt: None, bot_id: None, tz: None, spec: None, notify: None, clear_after: None, enabled: Some(false) }))
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
    let run = || api::run_now(State(e.app.clone()), Path(s.id.clone()), Json(RunIn { client_request_id: Some("run-1".into()) }));
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
    let bad = api::run_now(State(e.app.clone()), Path(s.id.clone()), Json(RunIn { client_request_id: Some("has space".into()) })).await;
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
    let v = api::run_now(State(app.clone()), Path(sid.into()), Json(RunIn { client_request_id: Some(crid.into()) })).await.unwrap().0;
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
    api::PatchIn { name: None, prompt: None, bot_id: None, tz: None, spec: None, notify: None, clear_after: None, enabled: None }
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
