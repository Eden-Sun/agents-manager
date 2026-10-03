//! 分享連結的管理端點，在主 API（7788）上，只收 UI token（SPEC「分享 bot」、API.md）：
//!
//! - `GET  /api/bots/{id}/share` → `{shareable, enabled, url:null, token_hint, created_at, last_used_at}`
//! - `POST /api/bots/{id}/share` `{"enabled":true|false}` → 開（回完整 `url`，只有這一次）／關（清掉 token）
//! - `POST /api/bots/{id}/share/rotate` → 換新 token，舊連結當下失效，回新的 `url`

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::RequestPrincipal;
use crate::lifecycle::LcError;
use crate::share::store;
use crate::state::App;

/// `[share]`：`listen` 是分享入口的獨立 listener（Tailscale Funnel 指過來的那個 port），`base_url` 是對外網址。
/// 兩個都可以不寫：沒 `listen` 就不開入口，沒 `base_url` 就不能開分享連結（409 `share_not_configured`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareCfg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl ShareCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// `https://…`／`http://…`，去掉結尾的 `/`；其他形狀當沒設。
    pub fn base(&self) -> Option<String> {
        let b = self.base_url.as_deref()?.trim().trim_end_matches('/');
        let rest = b.strip_prefix("https://").or_else(|| b.strip_prefix("http://"))?;
        (!rest.is_empty() && !rest.contains(char::is_whitespace)).then(|| b.to_string())
    }
}

#[derive(Deserialize)]
pub struct ShareIn {
    enabled: bool,
}

fn user_only(principal: &RequestPrincipal) -> Result<(), LcError> {
    if *principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    Ok(())
}

fn db_err(e: sqlx::Error) -> LcError {
    LcError::Upstream(format!("share store: {e}"))
}

/// 活著、而且是受限的分享用 bot。不存在 404；不是受限 bot 409 `not_shareable`。
async fn shareable_bot(app: &Arc<App>, id: &str) -> Result<bool, LcError> {
    let bot = crate::db::bot(&app.db, id).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if bot.as_ref().is_none_or(|b| b.deleted_at.is_some()) {
        return Err(LcError::NotFound("bot".into()));
    }
    store::is_restricted(&app.db, id).await.map_err(db_err)
}

fn not_shareable(id: &str) -> LcError {
    LcError::conflict(
        "not_shareable",
        json!({"bot_id": id, "message": "只有建立時選「分享用（受限）」的 bot 能分享；既有 bot 不能切換，要分享請新建一顆"}),
    )
}

async fn base_url(app: &Arc<App>) -> Result<String, LcError> {
    app.cfg.get().await.share.base().ok_or_else(|| {
        LcError::conflict(
            "share_not_configured",
            json!({"message": "config.toml 的 [share] base_url（Tailscale Funnel 的 https 網址）還沒設定"}),
        )
    })
}

async fn state(app: &Arc<App>, id: &str, url: Option<String>) -> Result<Value, LcError> {
    let row = store::share(&app.db, id).await.map_err(db_err)?;
    Ok(json!({
        "shareable": true,
        "enabled": row.is_some(),
        "url": url,
        "token_hint": row.as_ref().map(|r| r.token_hint.clone()),
        "created_at": row.as_ref().map(|r| r.created_at.clone()),
        "last_used_at": row.as_ref().and_then(|r| r.last_used_at.clone()),
    }))
}

pub async fn get_share(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    if !shareable_bot(&app, &id).await? {
        return Ok(Json(json!({"shareable": false, "enabled": false, "url": null, "token_hint": null, "created_at": null, "last_used_at": null})));
    }
    Ok(Json(state(&app, &id, None).await?))
}

pub async fn post_share(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<ShareIn>,
) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    if !shareable_bot(&app, &id).await? {
        return Err(not_shareable(&id));
    }
    if !body.enabled {
        store::disable(&app.db, &id).await.map_err(db_err)?;
        crate::share::portal::kick(&id);
        app.emit("bot_share_changed", json!({"bot_id": id, "enabled": false})).await;
        return Ok(Json(state(&app, &id, None).await?));
    }
    let base = base_url(&app).await?;
    let token = store::enable(&app.db, &id).await.map_err(db_err)?;
    if token.is_some() {
        app.emit("bot_share_changed", json!({"bot_id": id, "enabled": true})).await;
    }
    // 已經開著：不換 token（已經發出去的連結照樣能用），完整網址也拿不回來，要新連結請 rotate。
    Ok(Json(state(&app, &id, token.map(|t| format!("{base}/s/{t}"))).await?))
}

pub async fn post_rotate(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    if !shareable_bot(&app, &id).await? {
        return Err(not_shareable(&id));
    }
    let base = base_url(&app).await?;
    let Some(token) = store::rotate(&app.db, &id).await.map_err(db_err)? else {
        return Err(LcError::conflict("share_disabled", json!({"bot_id": id, "message": "分享沒開著，先開分享"})));
    };
    crate::share::portal::kick(&id);
    app.emit("bot_share_changed", json!({"bot_id": id, "enabled": true})).await;
    Ok(Json(state(&app, &id, Some(format!("{base}/s/{token}"))).await?))
}

/// 建受限 bot 的前半：工作目錄建好、`shared_bots` 記下來（在寫 config 之前）。
pub(crate) async fn reserve_restricted(app: &Arc<App>, bot_id: &str) -> Result<String, LcError> {
    let ws = crate::share::cage::ensure_workspace(&app.data_dir, bot_id).map_err(|e| LcError::Upstream(format!("restricted workspace: {e}")))?;
    let ws = ws.to_string_lossy().into_owned();
    store::insert_restricted(&app.db, bot_id, &ws).await.map_err(db_err)?;
    Ok(ws)
}

/// 建受限 bot 的後半：建成了就把 `bots.cwd` 指到工作目錄；沒建成（失敗、重送拿回舊的那顆）就把前半收回。
pub(crate) async fn finish_restricted(app: &Arc<App>, bot_id: &str, workspace: &str, created: bool) {
    if created {
        if let Err(e) = sqlx::query("UPDATE bots SET cwd = ? WHERE id = ?").bind(workspace).bind(bot_id).execute(&app.db).await {
            // 啟動時以 `shared_bots.workspace` 為準（`cage::prepare`），這裡寫不進去只影響側欄顯示的目錄。
            tracing::warn!(bot = bot_id, error = %e, "could not point the restricted bot's cwd at its workspace");
        }
        return;
    }
    if let Err(e) = store::delete_restricted(&app.db, bot_id).await {
        tracing::warn!(bot = bot_id, error = %e, "could not roll back a restricted bot reservation");
    }
    if let Some(dir) = crate::share::cage::workspace_dir(&app.data_dir, bot_id).and_then(|w| w.parent().map(|p| p.to_path_buf())) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
