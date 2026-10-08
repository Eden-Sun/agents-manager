use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde_json::{json, Value};

use crate::lifecycle::LcError;
use crate::panes::{announce_if_changed, classify, facts_from, row_json};
use crate::state::App;

/// `GET /api/projects/{id}/panes`：這個專案的非 agent pane。**沒歸屬的不在這裡**（SPEC §6.5e：它不屬於任何專案，
/// 掛在每個專案底下會重複出現、看起來像那個專案的東西）；它們走 `GET /api/panes?unowned=1`。
pub async fn list_for_project(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, LcError> {
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let project = crate::db::project(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    let rows = sqlx::query(
        "SELECT * FROM panes WHERE host=? AND project_id=? ORDER BY kind, pane_id",
    )
    .bind(&project.host)
    .bind(&id)
    .fetch_all(&app.db)
    .await
    .map_err(sql)?;
    let panes: Vec<Value> = rows.iter().map(row_json).collect();
    Ok(Json(json!({"project_id": id, "host": project.host, "panes": panes})))
}

#[derive(serde::Deserialize, Default)]
pub struct AdoptIn {
    pub owner_bot_id: Option<String>,
    pub purpose: Option<String>,
    /// 使用者手開的 pane 只有這個明確帶 true 才會變成可自動關（§6.5e）。
    #[serde(default)]
    pub allow_gc: bool,
}

/// `GET /api/panes?unowned=1`：全機的非 agent pane；`unowned=1` 只回「連專案都對不到」的那些（§6.5e）。
/// 哪一顆是 scratch 由 daemon 標（`scratch`），前端不自己重算。
pub async fn list_all(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, LcError> {
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let only_unowned = q.get("unowned").map(|v| v == "1" || v == "true").unwrap_or(false);
    let rows = if only_unowned {
        sqlx::query("SELECT * FROM panes WHERE owned_by='none' ORDER BY host, scratch DESC, pane_id").fetch_all(&app.db).await
    } else {
        sqlx::query("SELECT * FROM panes ORDER BY host, kind, pane_id").fetch_all(&app.db).await
    }
    .map_err(sql)?;
    Ok(Json(json!({"panes": rows.iter().map(row_json).collect::<Vec<_>>()})))
}

/// `POST /api/panes/{id}/focus?host=local`：把 herdr 的焦點切到這顆 pane。只動焦點，不改內容。
pub async fn focus(
    State(app): State<Arc<App>>,
    Path(pane_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let (client, _) = crate::api::shell::client_for(&app, &host).await?;
    client.pane_focus(&pane_id).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok(Json(json!({"focused": true, "pane_id": pane_id})))
}

/// `POST /api/panes/{id}/adopt`：補 owner／purpose。不會偷偷讓使用者的 pane 變成可 GC。
pub async fn adopt(
    State(app): State<Arc<App>>,
    Path(pane_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Option<Json<AdoptIn>>,
) -> Result<Json<Value>, LcError> {
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let project = match b.owner_bot_id.as_deref() {
        Some(bot) => Some(
            crate::db::bot(&app.db, bot)
                .await
                .map_err(up)?
                .ok_or_else(|| LcError::NotFound("bot".into()))?
                .project_id,
        ),
        None => None,
    };
    let n = sqlx::query(
        "UPDATE panes SET owner_bot_id=COALESCE(?, owner_bot_id), project_id=COALESCE(?, project_id),
                          owner_adopted=CASE WHEN ? IS NULL THEN owner_adopted ELSE 1 END,
                          purpose=COALESCE(?, purpose), gc_optin=CASE WHEN ? THEN 1 ELSE gc_optin END,
                          orphan_notified_at=NULL
          WHERE host=? AND pane_id=?",
    )
    .bind(b.owner_bot_id.as_deref())
    .bind(project.as_deref())
    .bind(b.owner_bot_id.as_deref())
    .bind(b.purpose.as_deref())
    .bind(b.allow_gc)
    .bind(&host)
    .bind(&pane_id)
    .execute(&app.db)
    .await
    .map_err(sql)?
    .rows_affected();
    if n == 0 {
        return Err(LcError::NotFound("pane".into()));
    }
    let row = sqlx::query("SELECT * FROM panes WHERE host=? AND pane_id=?")
        .bind(&host)
        .bind(&pane_id)
        .fetch_one(&app.db)
        .await
        .map_err(sql)?;
    tracing::info!(host, pane_id, owner = ?b.owner_bot_id, purpose = ?b.purpose, allow_gc = b.allow_gc, "pane adopted");
    announce_if_changed(&app, &host).await;
    Ok(Json(row_json(&row)))
}

/// `POST /api/panes/{id}/close`：人按的關閉。服務 pane 要帶 `confirm=true`（UI 會先顯示 port）。
pub async fn close(
    State(app): State<Arc<App>>,
    Path(pane_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let confirmed = q.get("confirm").map(|v| v == "true" || v == "1").unwrap_or(false);
    close_tracked(&app, &host, &pane_id, confirmed).await.map(Json)
}

/// [`close`] 的本體；`DELETE /api/hosts/{name}/shells/{pane_id}` 找不到記憶體那份時也走這裡（web review M2）。
pub(crate) async fn close_tracked(app: &Arc<App>, host: &str, pane_id: &str, confirmed: bool) -> Result<Value, LcError> {
    let (app, host, pane_id) = (app.clone(), host.to_string(), pane_id.to_string());
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let up = |e: anyhow::Error| LcError::Upstream(format!("{e:#}"));
    let row = sqlx::query("SELECT * FROM panes WHERE host=? AND pane_id=?")
        .bind(&host)
        .bind(&pane_id)
        .fetch_optional(&app.db)
        .await
        .map_err(sql)?
        .ok_or_else(|| LcError::NotFound("pane".into()))?;
    let mut info = row_json(&row);
    let agent_pane = || LcError::Forbidden(json!({"error": "agent_pane", "message": "這顆 pane 正在跑 agent，請從 bot 停掉"}));
    let session = app.session_for_host(&host).await.unwrap_or_default();
    if !crate::db::active_runs_for_pane(&app.db, &host, &pane_id, &session, &session).await.map_err(up)?.is_empty() {
        return Err(agent_pane());
    }
    let (client, _) = crate::runners::app_ports_p11::client_for(&app, &host).await?;
    match client.pane_get(&pane_id).await.map_err(up)? {
        None => {
            sqlx::query("DELETE FROM panes WHERE host=? AND pane_id=?").bind(&host).bind(&pane_id).execute(&app.db).await.map_err(sql)?;
            crate::drafts::clear_shell(&app, &host, &pane_id).await;
            announce_if_changed(&app, &host).await;
            return Err(LcError::NotFound("pane".into()));
        }
        Some(p) if p.agent.as_deref().is_some_and(|a| !a.is_empty()) => return Err(agent_pane()),
        Some(_) => {}
    }
    let probe = app.probe();
    let live = match (probe.dump(&app, &host).await, client.pane_shell(&pane_id).await) {
        (Ok(dump), Ok(shell)) => match facts_from(&shell, &dump, &pane_id) {
            Some(f) => probe.listen_ports(&host, &f.listen_pids).await.map(|ports| (f, ports)),
            None => None,
        },
        _ => None,
    };
    let needs_confirm = match &live {
        Some((f, ports)) => {
            let kind = classify(f.foreground.as_deref(), ports);
            info["kind"] = json!(kind);
            info["foreground"] = json!(f.foreground);
            info["read_only"] = json!(!ports.is_empty());
            info["listen_ports"] = json!(ports);
            kind == "service"
        }
        None => true,
    };
    if needs_confirm && !confirmed {
        return Err(LcError::conflict(
            "service pane needs confirm=true",
            json!({"reason": "service_pane", "pane": info, "unverified": live.is_none()}),
        ));
    }
    crate::runners::app_ports_p11::close_pane_and_tab(
        &client,
        info["workspace_id"].as_str(),
        info["tab_id"].as_str(),
        &pane_id,
    )
    .await;
    sqlx::query("DELETE FROM panes WHERE host=? AND pane_id=?")
        .bind(&host)
        .bind(&pane_id)
        .execute(&app.db)
        .await
        .map_err(sql)?;
    crate::drafts::clear_shell(&app, &host, &pane_id).await;
    app.pane_live.lock().await.remove(&(host.clone(), pane_id.clone()));
    announce_if_changed(&app, &host).await;
    tracing::info!(host, pane_id, kind = %info["kind"], "pane closed by request");
    Ok(json!({"closed": true, "pane": info}))
}
