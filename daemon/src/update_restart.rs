//! 「閒下來自動重啟套用更新」（2026-11-18，使用者：「這個更新無法點，不合理」）。
//!
//! 更新晶片在「有更新但相關 Bot 都在忙」時不能點，使用者看不到是哪顆、也沒辦法安排。這裡讓確認框可以把那些 bot 排成
//! 「下次閒置時自動重啟（resume 接回）」：旗標存 `bot_update_restart`（durable，daemon 重啟後保留，可取消）；
//! `pane_agent_status_changed` 的 idle 邊呼叫 [`schedule`]，輪到時走一鍵重啟的同一條安全路徑
//! （`bulk_restart::restart_one_when_due`），不打斷忙的 bot。重啟成功或更新已不需要套用就清旗標。
//!
//! `GET`／`POST`／`DELETE /api/bots/update-restart`：User-only。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use axum::{extract::State, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::runners::bulk_restart::{restart_one_when_due, DueOutcome};
use crate::{api::RequestPrincipal, db, lifecycle::LcError, state::App};

#[derive(Debug, Default, Deserialize)]
pub struct Body {
    #[serde(default)]
    pub bot_ids: Vec<String>,
}

fn user_only(principal: &RequestPrincipal) -> Result<(), LcError> {
    if *principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    Ok(())
}

fn internal(e: impl std::fmt::Display) -> LcError {
    LcError::Upstream(e.to_string())
}

async fn snapshot(app: &Arc<App>) -> Result<Value, LcError> {
    Ok(json!({"bot_ids": db::update_restart::list(&app.db).await.map_err(internal)?}))
}

async fn publish(app: &Arc<App>) {
    if let Ok(list) = db::update_restart::list(&app.db).await {
        app.emit("update_restart_changed", json!({"bot_ids": list})).await;
    }
}

pub(crate) async fn get(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    Ok(Json(snapshot(&app).await?))
}

/// 排上。子 agent 不收（一律由父 bot 重開，SPEC §6.5a）；已經閒置的當場就判一次。
pub(crate) async fn post(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>, Json(body): Json<Body>) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    let mut ids = Vec::new();
    for id in &body.bot_ids {
        let bot = db::bot(&app.db, id).await.map_err(internal)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound(format!("bot {id}")))?;
        if bot.managed_by == "child" || bot.parent_bot_id.as_deref().is_some_and(|p| !p.is_empty()) {
            return Err(LcError::conflict("child_restart_forbidden", json!({"bot_id": id, "message": "子 agent 一律由父 bot 重開，不能排自動重啟"})));
        }
        ids.push(id.clone());
    }
    db::update_restart::schedule(&app.db, &ids).await.map_err(internal)?;
    publish(&app).await;
    for id in ids {
        schedule(&app, &id);
    }
    Ok(Json(snapshot(&app).await?))
}

/// 取消。`bot_ids` 空＝全部取消。
pub(crate) async fn delete(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>, Json(body): Json<Body>) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    db::update_restart::cancel(&app.db, &body.bot_ids).await.map_err(internal)?;
    publish(&app).await;
    Ok(Json(snapshot(&app).await?))
}

/// 使用者強制重啟（`POST /api/bots/{id}/restart`）成功之後呼叫：這顆已經套用了更新，「閒下來自動重啟」的旗標不再有意義，清掉
/// 並通知網頁（沒排著就什麼都不做）。重啟失敗時不呼叫——旗標留著，下一次閒置照樣會試。
pub async fn forced_restart_done(app: &Arc<App>, bot_id: &str) {
    if db::update_restart::clear(&app.db, bot_id).await.unwrap_or(false) {
        publish(app).await;
    }
}

fn in_progress() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// 幾秒後再判一次：idle 邊那一刻回合常常還沒收尾（`turn_in_flight`）、背景工作剛讀、狀態剛起來；這幾種等一下就會好，
/// 之後不會再有 idle 邊，所以要自己再看幾次。正在跑／等回應（`working`／`blocked`）則等下一個 idle 邊，不輪詢。
const RETRY_AFTER_SECS: [u64; 5] = [3, 10, 30, 60, 120];

fn worth_retrying(why: &str) -> bool {
    matches!(why, "turn_in_flight" | "background_jobs" | "unknown_status" | "not_running" | "state_unreadable")
}

/// idle 邊（以及剛排上時）呼叫：這顆排著的話，背景判一次、到了就重啟。沒排著只是一次索引查詢。
pub fn schedule(app: &Arc<App>, bot_id: &str) {
    let (app, bot_id) = (app.clone(), bot_id.to_string());
    tokio::spawn(async move {
        let mut tries = RETRY_AFTER_SECS.iter();
        loop {
            match run_once(&app, &bot_id).await {
                Some(DueOutcome::NotYet(why)) if worth_retrying(why) => {
                    let Some(secs) = tries.next() else { return };
                    tokio::time::sleep(std::time::Duration::from_secs(*secs)).await;
                }
                _ => return,
            }
        }
    });
}

/// 同一顆同時只跑一個；回這一輪的結果（沒排著回 `None`）。測試直接呼叫它。
pub async fn run_once(app: &Arc<App>, bot_id: &str) -> Option<DueOutcome> {
    if !db::update_restart::is_scheduled(&app.db, bot_id).await.unwrap_or(false) {
        return None;
    }
    if !in_progress().lock().unwrap_or_else(|e| e.into_inner()).insert(bot_id.to_string()) {
        return None;
    }
    let outcome = restart_one_when_due(app, bot_id).await;
    in_progress().lock().unwrap_or_else(|e| e.into_inner()).remove(bot_id);
    match &outcome {
        DueOutcome::Restarted | DueOutcome::Dropped(_) => {
            let _ = db::update_restart::clear(&app.db, bot_id).await;
            publish(app).await;
        }
        DueOutcome::NotYet(why) => tracing::debug!(bot = bot_id, why, "scheduled update restart not due yet"),
    }
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTICE: &str = "Update installed · Restart to update";

    /// 一顆帶更新通知的 bot（身份是這台不認得的：重啟會在 start 那一步被擋下、不真的開 pane，同 bulk_restart 的夾具）。
    async fn bot_with_update(env: &crate::testing::Env, name: &str, status: &str, notice: &str) -> String {
        let b = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET identity='nope' WHERE id=?").bind(&b.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &b.id).await;
        sqlx::query("UPDATE runs SET update_notice=?, agent_status=? WHERE id=?").bind(notice).bind(status).bind(run).execute(&env.app.db).await.unwrap();
        b.id
    }

    async fn post_ids(app: &Arc<App>, ids: &[&str]) -> Result<Json<Value>, LcError> {
        post(State(app.clone()), Extension(RequestPrincipal::User), Json(Body { bot_ids: ids.iter().map(|s| s.to_string()).collect() })).await
    }

    #[tokio::test]
    async fn a_scheduled_idle_bot_is_restarted_and_its_flag_cleared() {
        let env = crate::testing::env().await;
        let id = bot_with_update(&env, "idle-one", "idle", NOTICE).await;
        db::update_restart::schedule(&env.app.db, std::slice::from_ref(&id)).await.unwrap();
        let out = run_once(&env.app, &id).await.expect("scheduled bots are evaluated");
        assert!(matches!(out, DueOutcome::Restarted | DueOutcome::Dropped("restart_failed")), "{out:?}");
        assert!(!db::update_restart::is_scheduled(&env.app.db, &id).await.unwrap(), "重啟（或放棄）之後旗標清掉");
    }

    #[tokio::test]
    async fn a_scheduled_bot_that_is_still_working_is_left_alone_and_stays_scheduled() {
        let env = crate::testing::env().await;
        let id = bot_with_update(&env, "busy-one", "working", NOTICE).await;
        db::update_restart::schedule(&env.app.db, std::slice::from_ref(&id)).await.unwrap();
        let out = run_once(&env.app, &id).await.unwrap();
        assert_eq!(out, DueOutcome::NotYet("working"), "忙的不打斷");
        assert!(db::update_restart::is_scheduled(&env.app.db, &id).await.unwrap(), "旗標留著，等下一次閒置");
        assert_eq!(db::update_restart::list(&env.app.db).await.unwrap(), vec![id.clone()], "list 讀得到（旗標在 DB，daemon 重啟後同一份）");
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE bot_id=?").bind(&id).execute(&env.app.db).await.unwrap();
        let again = run_once(&env.app, &id).await.unwrap();
        assert!(matches!(again, DueOutcome::Restarted | DueOutcome::Dropped("restart_failed")), "{again:?}");
        assert!(!db::update_restart::is_scheduled(&env.app.db, &id).await.unwrap());
    }

    #[tokio::test]
    async fn an_update_that_no_longer_needs_applying_drops_the_flag_without_restarting() {
        let env = crate::testing::env().await;
        let id = bot_with_update(&env, "already-new", "idle", "").await;
        db::update_restart::schedule(&env.app.db, std::slice::from_ref(&id)).await.unwrap();
        assert_eq!(run_once(&env.app, &id).await, Some(DueOutcome::Dropped("no_longer_pending")));
        assert!(!db::update_restart::is_scheduled(&env.app.db, &id).await.unwrap());
    }

    #[tokio::test]
    async fn cancelling_removes_the_flag_and_an_unscheduled_bot_is_never_evaluated() {
        let env = crate::testing::env().await;
        let a = bot_with_update(&env, "a", "working", NOTICE).await;
        let b = bot_with_update(&env, "b", "working", NOTICE).await;
        let Json(v) = post_ids(&env.app, &[&a, &b]).await.unwrap();
        assert_eq!(v["bot_ids"].as_array().unwrap().len(), 2);
        let Json(v) = delete(State(env.app.clone()), Extension(RequestPrincipal::User), Json(Body { bot_ids: vec![a.clone()] })).await.unwrap();
        assert_eq!(v["bot_ids"], json!([b.clone()]), "只取消指定的那顆");
        assert_eq!(run_once(&env.app, &a).await, None, "取消後不會被評估、更不會重啟");
        let Json(v) = delete(State(env.app.clone()), Extension(RequestPrincipal::User), Json(Body::default())).await.unwrap();
        assert_eq!(v["bot_ids"], json!([]), "空名單＝全部取消");
    }

    #[tokio::test]
    async fn the_api_is_user_only_and_refuses_unknown_or_child_bots() {
        let env = crate::testing::env().await;
        let id = bot_with_update(&env, "x", "working", NOTICE).await;
        for r in [
            post(State(env.app.clone()), Extension(RequestPrincipal::Bot(id.clone())), Json(Body { bot_ids: vec![id.clone()] })).await.map(|_| ()),
            get(State(env.app.clone()), Extension(RequestPrincipal::Bot(id.clone()))).await.map(|_| ()),
            delete(State(env.app.clone()), Extension(RequestPrincipal::Bot(id.clone())), Json(Body::default())).await.map(|_| ()),
        ] {
            assert!(matches!(r, Err(LcError::Forbidden(_))));
        }
        assert!(matches!(post_ids(&env.app, &["nope"]).await, Err(LcError::NotFound(_))));
        sqlx::query("UPDATE bots SET managed_by='child' WHERE id=?").bind(&id).execute(&env.app.db).await.unwrap();
        assert!(matches!(post_ids(&env.app, &[&id]).await, Err(LcError::Conflict(_))));
        assert!(db::update_restart::list(&env.app.db).await.unwrap().is_empty());
    }
}
