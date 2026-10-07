use std::sync::Arc;
use anyhow::Result;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};
use crate::herdr_maintenance::{row, CloseIn, OpenIn, Window, MAX_MINUTES};
use crate::lc_error::LcError;
use crate::state::App;

fn forbidden() -> LcError {
    LcError::Forbidden(json!({
        "error": "forbidden", "reason": "herdr_maintenance_forbidden",
        "message": "只有 AGM 角色可以開關 herdr 維護狀態"
    }))
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// 現在是不是在維護中。過了截止時間的窗口在這裡就地結束（寫 note、退休沒接回的子 agent），回 `None`。
pub async fn active(app: &Arc<App>) -> Result<Option<Window>> {
    let Some(w) = row(&app.db).await? else { return Ok(None) };
    if crate::db::cmp_ts(w.until.as_str(), crate::db::now().as_str()).is_gt() {
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
    crate::supervisor_inbox::add_note(
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
        "SELECT b.id, b.name, p.host FROM bots b JOIN projects p ON p.id = b.project_id
          WHERE b.managed_by = 'child' AND b.deleted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = b.id AND r.state IN ('starting','running','stopping'))
            AND EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = b.id AND r.ended_at >= ?)",
    )
    .bind(since)
    .fetch_all(&app.db)
    .await?;
    let mut names = Vec::new();
    for (id, name, host) in kids {
        match crate::child_reconcile_safety::retirement_block(&app.db, &id).await {
            Ok(Some(reason)) => {
                tracing::info!(host = %host, bot = %name, reason, "herdr maintenance: child kept by a persisted retirement guard");
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(host = %host, bot = %name, error = ?e, "herdr maintenance: cannot read child retirement guard; child kept and reconciliation deferred");
                crate::runners::reconcile::schedule_deferred_pass(app, &host);
                continue;
            }
        }
        // 走退役的唯一入口（#413）：記呼叫端；AGM 的 child 不在這裡被隱式退役，擋下來、推一則給巡檢。
        use crate::child_retire::{retire, Mode, Outcome};
        if retire(app, &id, "herdr_maintenance_closed", Mode::Implicit).await? == Outcome::Retired {
            tracing::info!(bot = %name, "herdr maintenance over: child never came back, retired");
            names.push(name);
        }
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

pub async fn get(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let w = active(&app).await.map_err(up)?;
    Ok(Json(json!({"active": w.is_some(), "window": w, "max_minutes": MAX_MINUTES})))
}

pub async fn open(State(app): State<Arc<App>>, headers: HeaderMap, body: Option<Json<OpenIn>>) -> Result<Json<Value>, LcError> {
    let role = crate::app_ports_p12::actor_role_name(&app, &headers).await?.ok_or_else(forbidden)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let minutes = b.minutes.unwrap_or(MAX_MINUTES);
    if !(1..=MAX_MINUTES).contains(&minutes) {
        return Err(LcError::Bad(format!("minutes 要在 1..={MAX_MINUTES}")));
    }
    let reason = b.reason.as_deref().map(str::trim).filter(|r| !r.is_empty()).ok_or_else(|| LcError::Bad("reason 必填（會寫進稽核紀錄）".into()))?;
    if let Some(w) = active(&app).await.map_err(up)? {
        return Err(LcError::conflict("herdr maintenance is already open", json!({"window": w})));
    }
    let Some(w) = open_as(&app, minutes, role, reason).await.map_err(up)? else {
        return Err(LcError::conflict("herdr maintenance is already open", json!({})));
    };
    Ok(Json(json!({"active": true, "window": w})))
}

/// 開窗口（API 與 daemon 自己的 herdr 一鍵更新共用，同一份稽核）。已經有人開著就回 `None`，不疊、不搶。
pub async fn open_as(app: &Arc<App>, minutes: i64, actor: &str, reason: &str) -> Result<Option<Window>> {
    let opened_at = crate::db::now();
    let until = (chrono::Utc::now() + chrono::Duration::minutes(minutes)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let inserted = sqlx::query("INSERT OR IGNORE INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,?,?,?)")
        .bind(&opened_at)
        .bind(&until)
        .bind(actor)
        .bind(reason)
        .execute(&app.db)
        .await?
        .rows_affected();
    if inserted == 0 {
        return Ok(None);
    }
    crate::supervisor_inbox::add_note(&app.db, "herdr_maintenance_start", &json!({"opened_at": opened_at, "until": until, "opened_by": actor, "reason": reason})).await?;
    arm_expiry(app, &until);
    tracing::info!(actor, until, reason, "herdr maintenance opened");
    row(&app.db).await
}

/// 關掉 `open_as` 開的那個窗口（`opened_at` 對得上才關，別人的不動）；回退休的子 agent 名。
pub async fn close_as(app: &Arc<App>, w: &Window, actor: &str, reason: Option<&str>) -> Result<Vec<String>> {
    close(app, w, "herdr_maintenance_end", actor, reason).await
}

pub async fn end(State(app): State<Arc<App>>, headers: HeaderMap, body: Option<Json<CloseIn>>) -> Result<Json<Value>, LcError> {
    let role = crate::app_ports_p12::actor_role_name(&app, &headers).await?.ok_or_else(forbidden)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let Some(w) = active(&app).await.map_err(up)? else {
        return Ok(Json(json!({"active": false, "closed": false})));
    };
    let retired = close(&app, &w, "herdr_maintenance_end", role, b.reason.as_deref()).await.map_err(up)?;
    Ok(Json(json!({"active": false, "closed": true, "retired_children": retired})))
}
