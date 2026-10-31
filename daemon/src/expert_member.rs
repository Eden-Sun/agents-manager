//! 專家模式只有成員、沒有 child（使用者 2026-10-31）：把 expert 專案裡留下的 child 原地轉成成員。
//! 同一個 bot id 寫進 config.toml、清掉 parent 關係；pane、run 與對話都不動，所以不斷線。
//! 先寫 config（投影的 upsert 不改 `managed_by`），再改 DB；DB 改失敗就把 config 那筆收回。
use std::sync::Arc;
use axum::{extract::{Path, State}, http::StatusCode, response::{IntoResponse, Response}, Extension, Json};
use serde::Deserialize;
use serde_json::json;
use crate::{api::RequestPrincipal, config::BotCfg, db, lifecycle::LcError, state::App};

#[derive(Deserialize, Default)]
pub(crate) struct AdoptMember {
    pub role: Option<String>,
    pub effort: Option<String>,
    /// 下次 native resume 要接回的原生對話（例：codex session id）；不帶就沿用 run 上記的。
    pub session_id: Option<String>,
}

fn up(e: impl std::fmt::Display) -> LcError { LcError::Upstream(e.to_string()) }

pub(crate) async fn adopt(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<AdoptMember>,
) -> Result<Response, LcError> {
    let child = db::bot(&app.db, &id).await.map_err(up)?.filter(|c| c.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project = db::project(&app.db, &child.project_id).await.map_err(up)?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("project".into()))?;
    if child.managed_by != "child" || project.mode != "expert" {
        return Err(LcError::conflict("only a child in an expert project becomes a member", json!({"reason": "not_expert_child"})));
    }
    let role = b.role.filter(|r| !r.trim().is_empty());
    match &principal {
        RequestPrincipal::User => {
            if !matches!(role.as_deref(), None | Some("executor" | "advisor")) { return Err(LcError::Bad("role must be executor, advisor or empty".into())); }
        }
        RequestPrincipal::Bot(caller) => {
            let manager = db::bot(&app.db, caller).await.map_err(up)?.filter(|m| m.deleted_at.is_none());
            let is_manager = manager.as_ref().is_some_and(|m| m.role.as_deref() == Some("manager") && m.parent_bot_id.is_none() && m.project_id == child.project_id);
            if !is_manager || !matches!(role.as_deref(), None | Some("executor" | "advisor")) {
                return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "manager_role_scope"})));
            }
        }
        _ => return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"}))),
    }
    if !crate::config::valid_bot_name(&child.name) {
        return Err(LcError::Bad(format!("bot name: {}", crate::config::BOT_NAME_RE)));
    }
    if db::live_bots(&app.db).await.map_err(up)?.iter().any(|x| x.id != id && x.project_id == child.project_id && x.name == child.name && x.managed_by == "user") {
        return Err(LcError::conflict("a member with this name already exists in the project", json!({"name": child.name})));
    }
    let effort = b.effort.clone().or_else(|| child.effort.clone());
    let cfg = BotCfg {
        role: role.clone(),
        id: Some(id.clone()),
        name: child.name.clone(),
        kind: child.kind.clone(),
        model: child.model.clone(),
        effort,
        fast: child.fast != 0,
        persona: child.persona.clone(),
        args: child.args(),
        autostart: false,
        inject_hooks: true,
        auto_approve: child.auto_approve != 0,
        identity: child.identity.clone(),
        env: child.env(),
        herdr_session: None,
        create_request_id: None,
        create_fingerprint: None,
    };
    let pid = child.project_id.clone();
    crate::projection::update_and_project(&app.cfg, &app.db, move |c| {
        let p = c.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).ok_or_else(|| anyhow::anyhow!("project not in config"))?;
        p.bots.push(cfg);
        Ok(())
    })
    .await
    .map_err(|e| LcError::conflict("config.toml was not changed", json!({"reason": "member_config_failed", "message": e.to_string()})))?;
    let detached = async {
        let mut tx = app.db.begin().await?;
        sqlx::query("UPDATE bots SET managed_by='user', parent_bot_id=NULL WHERE id=?").bind(&id).execute(&mut *tx).await?;
        if let Some(session) = b.session_id.as_deref().filter(|s| !s.trim().is_empty()) {
            sqlx::query("UPDATE runs SET native_session_id=? WHERE id=(SELECT id FROM runs WHERE bot_id=? ORDER BY rowid DESC LIMIT 1)").bind(session.trim()).bind(&id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = detached {
        let pid = child.project_id.clone();
        let rid = id.clone();
        let _ = crate::projection::update_and_project(&app.cfg, &app.db, move |c| {
            if let Some(p) = c.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())) { p.bots.retain(|x| x.id.as_deref() != Some(rid.as_str())); }
            Ok(())
        })
        .await;
        return Err(up(e));
    }
    app.emit("bot_changed", json!({"bot_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"bot_id": id, "member": true, "needs_restart": true}))).into_response())
}
