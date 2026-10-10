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

/// 順序（issue #890）：先確認窗口還是同一個 → 退役沒接回的子 agent（失敗＝整個回錯、窗口保留，下次重試）→
/// 同一個交易刪窗口與寫 note。退役是冪等的（已退役的不會再被選到），所以中途失敗、窗口留著重來不會重複。
async fn close(app: &Arc<App>, w: &Window, kind: &str, actor: &str, reason: Option<&str>) -> Result<Vec<String>> {
    let same: Option<i64> = sqlx::query_scalar("SELECT 1 FROM herdr_maintenance WHERE id = 1 AND opened_at = ?")
        .bind(&w.opened_at)
        .fetch_optional(&app.db)
        .await?;
    if same.is_none() {
        return Ok(vec![]); // 別人先關了
    }
    let retired = retire_unreturned_children(app, &w.opened_at).await?;
    let mut tx = app.db.begin().await?;
    let gone = sqlx::query("DELETE FROM herdr_maintenance WHERE id = 1 AND opened_at = ?")
        .bind(&w.opened_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if gone == 0 {
        tx.rollback().await?;
        return Ok(retired); // 別人先關了；退役的結果照回
    }
    crate::supervisor_inbox::add_note_tx(
        &mut tx,
        kind,
        &json!({"opened_at": w.opened_at, "until": w.until, "opened_by": w.opened_by, "closed_by": actor,
                "reason": reason, "retired_children": retired}),
    )
    .await?;
    tx.commit().await?;
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
    // 一顆退役失敗不擋後面的：記下第一個錯、跑完整輪再回錯（窗口保留、下次重試；已退役的不會再被選到）。
    let mut first_err: Option<anyhow::Error> = None;
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
        match retire(app, &id, "herdr_maintenance_closed", Mode::Implicit).await {
            Ok(Outcome::Retired) => {
                tracing::info!(bot = %name, "herdr maintenance over: child never came back, retired");
                names.push(name);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(host = %host, bot = %name, error = ?e, "herdr maintenance: could not retire a child; the window stays open for a retry");
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(names),
    }
}

/// 到截止時間自己收尾，不必等下一次 reconcile 剛好來查。
fn arm_expiry(app: &Arc<App>, until: &str) {
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(until) else { return };
    let wait = (at.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().unwrap_or_default() + std::time::Duration::from_secs(1);
    let app = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        // 讀不到或收尾失敗（窗口還在）就退避重試，直到 `active` 成功。
        for attempt in 0.. {
            match active(&app).await {
                Ok(_) => return,
                Err(e) => {
                    tracing::warn!(error = ?e, attempt, "herdr maintenance expiry check failed; retrying");
                    tokio::time::sleep(crate::reconcile::recovery_retry_delay(attempt)).await;
                }
            }
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
    // 窗口與開窗的稽核寫在同一個交易（同 `close_as`，#890）：note 寫不進去就整個回滾，
    // 不留下「窗口開著、沒有稽核、也沒排到期」的狀態（issue #1140）。
    let mut tx = app.db.begin().await?;
    let inserted = sqlx::query("INSERT OR IGNORE INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,?,?,?)")
        .bind(&opened_at)
        .bind(&until)
        .bind(actor)
        .bind(reason)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if inserted == 0 {
        tx.rollback().await?;
        return Ok(None);
    }
    crate::supervisor_inbox::add_note_tx(&mut tx, "herdr_maintenance_start", &json!({"opened_at": opened_at, "until": until, "opened_by": actor, "reason": reason})).await?;
    tx.commit().await?;
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
