//! 計畫中的 herdr server 重啟（SPEC §6.5.2，AGM 2026-09-17 herdr 0.9.0 升級）。
//!
//! 平常 reconcile 看到子 agent 的 agent 不在，就把那顆子 bot 軟刪：子 agent 只活在父開的 pane 裡。
//! 但 herdr server 一重啟，**所有** pane 同時消失——那不是「子 agent 做完了」，照平常的規則會把每一顆
//! 子 agent 一次刪光，事後接回原對話也叫不回來。
//!
//! 所以要有一個**明確、有時限、有稽核**的維護狀態：只有 AGM 角色開得了、上限 30 分鐘、逾時自動結束，
//! 開／關／逾時都寫 `supervisor_notes`。期間 reconcile 照樣把 run 標成 exited，只是不刪子 bot；
//! 維護結束（或逾時）時，這段期間被標 exited、到現在仍沒接回的子 agent 才照原規則退休。

use crate::lifecycle::LcError;
use crate::state::App;
use crate::supervisor::store;
use anyhow::Result;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

/// 維護窗口的上限（分鐘）。升級實測一輪不到 5 分鐘；30 分鐘給回滾留空間，又不會讓「忘了關」變成常態。
pub const MAX_MINUTES: i64 = 30;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS herdr_maintenance (
           id INTEGER PRIMARY KEY CHECK (id = 1),
           opened_at TEXT NOT NULL,
           until TEXT NOT NULL,
           opened_by TEXT NOT NULL,
           reason TEXT
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Window {
    pub opened_at: String,
    pub until: String,
    pub opened_by: String,
    pub reason: Option<String>,
}

async fn row(pool: &SqlitePool) -> Result<Option<Window>> {
    Ok(sqlx::query_as::<_, Window>("SELECT opened_at, until, opened_by, reason FROM herdr_maintenance WHERE id = 1")
        .fetch_optional(pool)
        .await?)
}

/// 現在是不是在維護中。過了截止時間的窗口在這裡就地結束（寫 note、退休沒接回的子 agent），回 `None`。
pub async fn active(app: &Arc<App>) -> Result<Option<Window>> {
    let Some(w) = row(&app.db).await? else { return Ok(None) };
    if w.until.as_str() > crate::db::now().as_str() {
        return Ok(Some(w));
    }
    close(app, &w, "herdr_maintenance_expired", "system", Some("截止時間到了，自動結束")).await?;
    Ok(None)
}

async fn close(app: &Arc<App>, w: &Window, kind: &str, actor: &str, reason: Option<&str>) -> Result<Vec<String>> {
    let gone = sqlx::query("DELETE FROM herdr_maintenance WHERE id = 1 AND opened_at = ?")
        .bind(&w.opened_at)
        .execute(&app.db)
        .await?
        .rows_affected();
    if gone == 0 {
        return Ok(vec![]); // 別人先關了
    }
    let retired = retire_unreturned_children(app, &w.opened_at).await?;
    store::add_note(
        &app.db,
        kind,
        &json!({"opened_at": w.opened_at, "until": w.until, "opened_by": w.opened_by, "closed_by": actor,
                "reason": reason, "retired_children": retired}),
    )
    .await?;
    tracing::info!(kind, actor, retired = retired.len(), "herdr maintenance closed");
    Ok(retired)
}

/// 維護期間被標 exited、到現在還沒有 active run 的子 agent：照原規則退休（跟 reconcile 平常做的一樣）。
async fn retire_unreturned_children(app: &Arc<App>, since: &str) -> Result<Vec<String>> {
    let kids: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT b.id, b.name, b.project_id FROM bots b
          WHERE b.managed_by = 'child' AND b.deleted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = b.id AND r.state IN ('starting','running','stopping'))
            AND EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = b.id AND r.ended_at >= ?)",
    )
    .bind(since)
    .fetch_all(&app.db)
    .await?;
    let mut names = Vec::new();
    for (id, name, project_id) in kids {
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL").bind(crate::db::now()).bind(&id).execute(&app.db).await?;
        app.emit("project_changed", json!({"project_id": project_id})).await;
        tracing::info!(bot = %name, "herdr maintenance over: child never came back, retired");
        names.push(name);
    }
    Ok(names)
}

/// 到截止時間自己收尾，不必等下一次 reconcile 剛好來查。
fn arm_expiry(app: &Arc<App>, until: &str) {
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(until) else { return };
    let wait = (at.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().unwrap_or_default() + std::time::Duration::from_secs(1);
    let app = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        if let Err(e) = active(&app).await {
            tracing::warn!(error = ?e, "herdr maintenance expiry check failed");
        }
    });
}

/// 開機時接手：上一顆 daemon 開的窗口還沒到期就重新排截止，已經過期就當場結束。
///
/// 讀不到不算接手過（#75 重開）：窗口可能還開著，沒排截止就只能等哪一輪對帳剛好來查——期間被標 exited 的子 agent
/// 一直掛著。背景照開機恢復的退避再讀，讀到為止。
pub async fn arm_on_startup(app: &Arc<App>) {
    match active(app).await {
        Ok(Some(w)) => arm_expiry(app, &w.until),
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = ?e, "could not read herdr maintenance state; retrying in the background");
            let app = app.clone();
            tokio::spawn(async move {
                for attempt in 0.. {
                    tokio::time::sleep(crate::reconcile::recovery_retry_delay(attempt)).await;
                    match active(&app).await {
                        Ok(Some(w)) => {
                            arm_expiry(&app, &w.until);
                            return;
                        }
                        Ok(None) => return,
                        Err(e) => tracing::warn!(error = ?e, "still cannot read herdr maintenance state"),
                    }
                }
            });
        }
    }
}

// ---------------------------------------------------------------- API

#[derive(Deserialize, Default)]
pub struct OpenIn {
    pub minutes: Option<i64>,
    pub reason: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct CloseIn {
    pub reason: Option<String>,
}

fn forbidden() -> LcError {
    LcError::Forbidden(json!({
        "error": "forbidden", "reason": "herdr_maintenance_forbidden",
        "message": "只有 AGM 角色可以開關 herdr 維護狀態"
    }))
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

pub async fn get(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let w = active(&app).await.map_err(up)?;
    Ok(Json(json!({"active": w.is_some(), "window": w, "max_minutes": MAX_MINUTES})))
}

pub async fn open(State(app): State<Arc<App>>, headers: HeaderMap, body: Option<Json<OpenIn>>) -> Result<Json<Value>, LcError> {
    let role = crate::supervisor::bot_requests::actor_role(&app, &headers).await.ok_or_else(forbidden)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let minutes = b.minutes.unwrap_or(MAX_MINUTES);
    if !(1..=MAX_MINUTES).contains(&minutes) {
        return Err(LcError::Bad(format!("minutes 要在 1..={MAX_MINUTES}")));
    }
    let reason = b.reason.as_deref().map(str::trim).filter(|r| !r.is_empty()).ok_or_else(|| LcError::Bad("reason 必填（會寫進稽核紀錄）".into()))?;
    if let Some(w) = active(&app).await.map_err(up)? {
        return Err(LcError::conflict("herdr maintenance is already open", json!({"window": w})));
    }
    let opened_at = crate::db::now();
    let until = (chrono::Utc::now() + chrono::Duration::minutes(minutes)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let actor = role.as_str();
    let inserted = sqlx::query("INSERT OR IGNORE INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,?,?,?)")
        .bind(&opened_at)
        .bind(&until)
        .bind(actor)
        .bind(reason)
        .execute(&app.db)
        .await
        .map_err(up)?
        .rows_affected();
    if inserted == 0 {
        return Err(LcError::conflict("herdr maintenance is already open", json!({})));
    }
    store::add_note(&app.db, "herdr_maintenance_start", &json!({"opened_at": opened_at, "until": until, "opened_by": actor, "reason": reason}))
        .await
        .map_err(up)?;
    arm_expiry(&app, &until);
    tracing::info!(actor, until, reason, "herdr maintenance opened");
    let w = row(&app.db).await.map_err(up)?;
    Ok(Json(json!({"active": true, "window": w})))
}

pub async fn end(State(app): State<Arc<App>>, headers: HeaderMap, body: Option<Json<CloseIn>>) -> Result<Json<Value>, LcError> {
    let role = crate::supervisor::bot_requests::actor_role(&app, &headers).await.ok_or_else(forbidden)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let Some(w) = active(&app).await.map_err(up)? else {
        return Ok(Json(json!({"active": false, "closed": false})));
    };
    let retired = close(&app, &w, "herdr_maintenance_end", role.as_str(), b.reason.as_deref()).await.map_err(up)?;
    Ok(Json(json!({"active": false, "closed": true, "retired_children": retired})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    async fn agm_headers(app: &Arc<App>) -> HeaderMap {
        let id = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p-agm','/tmp','AGM',?)").bind(crate::db::now()).execute(&app.db).await.ok();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p-agm','AGM','claude','agm-tok',?)")
            .bind(&id)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        store::get_or_init(&app.db).await.unwrap();
        store::set_env(&app.db, &id, "p-agm", "/tmp").await.unwrap();
        crate::supervisor::roles::set_env(&app.db, crate::supervisor::roles::Role::Patrol, &id, "p-agm", "/tmp").await.unwrap();
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Id", id.parse().unwrap());
        h.insert("X-AM-Bot-Token", "agm-tok".parse().unwrap());
        h
    }

    fn open_in(minutes: i64) -> Option<Json<OpenIn>> {
        Some(Json(OpenIn { minutes: Some(minutes), reason: Some("herdr 0.9.0".into()) }))
    }

    /// 子 agent（有一個已結束的 run，模擬 reconcile 剛把它標 exited）。
    pub(crate) async fn child_with_ended_run(env: &tt::Env, ended_at: &str) -> String {
        let id = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok','child',?)",
        )
        .bind(&id)
        .bind(&env.project_id)
        .bind(format!("kid-{id}"))
        .bind(crate::db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, started_at, ended_at) VALUES (?,?,'exited','unknown',?,?)")
            .bind(crate::db::ulid())
            .bind(&id)
            .bind(ended_at)
            .bind(ended_at)
            .execute(&env.app.db)
            .await
            .unwrap();
        id
    }

    async fn deleted(app: &Arc<App>, id: &str) -> bool {
        sqlx::query_scalar::<_, Option<String>>("SELECT deleted_at FROM bots WHERE id = ?").bind(id).fetch_one(&app.db).await.unwrap().is_some()
    }

    async fn notes(app: &Arc<App>, kind: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_notes WHERE kind = ?").bind(kind).fetch_one(&app.db).await.unwrap()
    }

    #[tokio::test]
    async fn only_an_agm_role_can_open_or_close_it() {
        let env = tt::env().await;
        let app = env.app.clone();
        let err = open(State(app.clone()), HeaderMap::new(), open_in(10)).await.unwrap_err();
        assert!(matches!(err, LcError::Forbidden(ref v) if v["reason"] == "herdr_maintenance_forbidden"), "{err:?}");
        // 帶了 id 但 token 對不上也不行。
        let mut forged = agm_headers(&app).await;
        forged.insert("X-AM-Bot-Token", "guess".parse().unwrap());
        assert!(matches!(open(State(app.clone()), forged, open_in(10)).await.unwrap_err(), LcError::Forbidden(_)));
        assert!(active(&app).await.unwrap().is_none());
        assert!(matches!(end(State(app.clone()), HeaderMap::new(), None).await.unwrap_err(), LcError::Forbidden(_)));
    }

    #[tokio::test]
    async fn a_window_has_a_bounded_length_and_an_audit_trail() {
        let env = tt::env().await;
        let app = env.app.clone();
        let h = agm_headers(&app).await;
        assert!(matches!(open(State(app.clone()), h.clone(), open_in(31)).await.unwrap_err(), LcError::Bad(_)));
        assert!(matches!(open(State(app.clone()), h.clone(), Some(Json(OpenIn { minutes: Some(5), reason: None }))).await.unwrap_err(), LcError::Bad(_)));
        let v = open(State(app.clone()), h.clone(), open_in(5)).await.unwrap().0;
        assert_eq!(v["active"], true);
        assert!(matches!(open(State(app.clone()), h.clone(), open_in(5)).await.unwrap_err(), LcError::Conflict(_)), "不重疊開兩個");
        assert_eq!(notes(&app, "herdr_maintenance_start").await, 1);
        let v = end(State(app.clone()), h.clone(), Some(Json(CloseIn { reason: Some("升級完成".into()) }))).await.unwrap().0;
        assert_eq!(v["closed"], true);
        assert_eq!(notes(&app, "herdr_maintenance_end").await, 1);
        assert!(active(&app).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn after_it_ends_children_that_never_came_back_are_retired_and_returned_ones_are_kept() {
        let env = tt::env().await;
        let app = env.app.clone();
        let h = agm_headers(&app).await;
        let before = child_with_ended_run(&env, "2020-01-01T00:00:00.000Z").await; // 維護前就結束的：不是這次的事
        let _ = open(State(app.clone()), h.clone(), open_in(10)).await.unwrap();
        let lost = child_with_ended_run(&env, &crate::db::now()).await;
        let back = child_with_ended_run(&env, &crate::db::now()).await;
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, started_at) VALUES (?,?,'running','idle',?)")
            .bind(crate::db::ulid())
            .bind(&back)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let v = end(State(app.clone()), h, None).await.unwrap().0;
        assert_eq!(v["retired_children"].as_array().unwrap().len(), 1, "{v}");
        assert!(deleted(&app, &lost).await, "維護結束仍沒接回：照原規則退休");
        assert!(!deleted(&app, &back).await, "接回來的留著");
        assert!(!deleted(&app, &before).await, "不回頭清舊帳");
    }

    /// #75 重開：開機接手窗口那一次讀不到，不算接手過。窗口在這之間已經到期：背景重試讀到之後照樣收尾（寫 note、退休
    /// 沒接回的子 agent），不必等哪一輪對帳剛好來查。
    #[tokio::test]
    async fn a_startup_that_cannot_read_the_window_keeps_trying_until_it_can() {
        let env = tt::env().await;
        let app = env.app.clone();
        let h = agm_headers(&app).await;
        let _ = open(State(app.clone()), h, open_in(10)).await.unwrap();
        let lost = child_with_ended_run(&env, &crate::db::now()).await;
        sqlx::query("UPDATE herdr_maintenance SET until = '2020-01-01T00:00:00.000Z'").execute(&app.db).await.unwrap();
        sqlx::query("ALTER TABLE herdr_maintenance RENAME TO herdr_maintenance_unreadable").execute(&app.db).await.unwrap();
        arm_on_startup(&app).await;
        assert!(!deleted(&app, &lost).await, "讀不到：還沒接手");

        sqlx::query("ALTER TABLE herdr_maintenance_unreadable RENAME TO herdr_maintenance").execute(&app.db).await.unwrap();
        // 等的是 `close()` 的**最後一步**（寫 note），不是中間那步（退休子 agent）：`close()` 的順序是
        // 刪窗口 → 退休沒接回的子 agent → 寫 note，兩者之間有 await。只等「子 agent 被退休」的話，慢的
        // runner 上會在寫 note 之前就去數 note，數到 0（#274，跟 #255 同一族）。
        let _ = crate::testing::eventually!(notes(&app, "herdr_maintenance_expired").await == 1);
        assert!(deleted(&app, &lost).await, "讀得到之後自己接手：過期的窗口收尾、沒接回的子 agent 退休");
        assert_eq!(notes(&app, "herdr_maintenance_expired").await, 1);
    }

    #[tokio::test]
    async fn an_expired_window_ends_itself_and_normal_rules_resume() {
        let env = tt::env().await;
        let app = env.app.clone();
        let h = agm_headers(&app).await;
        let _ = open(State(app.clone()), h, open_in(10)).await.unwrap();
        let lost = child_with_ended_run(&env, &crate::db::now()).await;
        sqlx::query("UPDATE herdr_maintenance SET until = '2020-01-01T00:00:00.000Z'").execute(&app.db).await.unwrap();
        assert!(active(&app).await.unwrap().is_none(), "過了截止就不算維護中");
        assert_eq!(notes(&app, "herdr_maintenance_expired").await, 1);
        assert!(deleted(&app, &lost).await);
    }
}
