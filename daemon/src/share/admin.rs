//! 分享連結的管理端點，在主 API（7788）上，只收 UI token（SPEC「分享 bot」、API.md）：
//!
//! - `GET  /api/bots/{id}/share` → `{shareable, enabled, url, needs_rotate, token_hint, created_at, last_used_at}`：開著就回完整 `url`
//!   （舊資料只有 hash：`url:null`、`needs_rotate:true`，重產一次就有）
//! - `POST /api/bots/{id}/share` `{"enabled":true|false}` → 開（已經開著就沿用同一條）／關（清掉 token）
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
    /// 受限 bot「新資料夾」的根目錄（可用 `~/`）；沒設＝`~/shared-bots`。要在 daemon 資料目錄之外。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folders_root: Option<String>,
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

/// 活著、而且是分享用 bot（受限或信任分享）。不存在 404；不是分享用 bot 409 `not_shareable`。
async fn shareable_bot(app: &Arc<App>, id: &str) -> Result<bool, LcError> {
    let bot = crate::db::bot(&app.db, id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project = crate::db::project(&app.db, &bot.project_id).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if project.is_none_or(|p| p.deleted_at.is_some()) {
        return Err(LcError::NotFound("bot".into()));
    }
    store::is_share_bot(&app.db, id).await.map_err(db_err)
}

fn not_shareable(id: &str) -> LcError {
    LcError::conflict(
        "not_shareable",
        json!({"bot_id": id, "message": "只有建立時選「分享用（受限）」或「信任分享」的 bot 能分享；既有 bot 不能切換，要分享請新建一顆"}),
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

/// 目前的分享狀態。開著就從 DB 存的 token 組完整網址；`base_url` 沒設時 `url:null`（開不了新的，但舊列照樣報狀態）。
async fn state(app: &Arc<App>, id: &str) -> Result<Value, LcError> {
    let row = store::share(&app.db, id).await.map_err(db_err)?;
    let base = app.cfg.get().await.share.base();
    let url = row.as_ref().and_then(|r| r.token.as_deref()).zip(base).map(|(t, b)| format!("{b}/s/{t}"));
    Ok(json!({
        "shareable": true,
        "enabled": row.is_some(),
        "url": url,
        // 加 `token` 欄之前開的分享只有 hash：連結照樣能用，但拿不回完整網址，重產一次就有。
        "needs_rotate": row.as_ref().is_some_and(|r| r.token.is_none()),
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
    let lock = app.bot_lock(&id).await;
    let _guard = lock.lock_owned().await;
    if !shareable_bot(&app, &id).await? {
        return Ok(Json(json!({"shareable": false, "enabled": false, "url": null, "needs_rotate": false, "token_hint": null, "created_at": null, "last_used_at": null})));
    }
    Ok(Json(state(&app, &id).await?))
}

pub async fn post_share(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<ShareIn>,
) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    #[cfg(test)]
    crate::lifecycle::race_point::hit("share_admin_before_lock", &id).await;
    let lock = app.bot_lock(&id).await;
    let _guard = lock.lock_owned().await;
    if !shareable_bot(&app, &id).await? {
        return Err(not_shareable(&id));
    }
    if !body.enabled {
        store::disable(&app.db, &id).await.map_err(db_err)?;
        crate::share::portal::kick(&id);
        app.emit("bot_share_changed", json!({"bot_id": id, "enabled": false})).await;
        return Ok(Json(state(&app, &id).await?));
    }
    base_url(&app).await?;
    // 已經開著：不換 token（已經發出去的連結照樣能用），回的是同一條網址。
    if store::enable(&app.db, &id).await.map_err(db_err)?.is_some() {
        app.emit("bot_share_changed", json!({"bot_id": id, "enabled": true})).await;
    }
    Ok(Json(state(&app, &id).await?))
}

pub async fn post_rotate(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    #[cfg(test)]
    crate::lifecycle::race_point::hit("share_admin_before_lock", &id).await;
    let lock = app.bot_lock(&id).await;
    let _guard = lock.lock_owned().await;
    if !shareable_bot(&app, &id).await? {
        return Err(not_shareable(&id));
    }
    base_url(&app).await?;
    if store::rotate(&app.db, &id).await.map_err(db_err)?.is_none() {
        return Err(LcError::conflict("share_disabled", json!({"bot_id": id, "message": "分享沒開著，先開分享"})));
    }
    crate::share::portal::kick(&id);
    app.emit("bot_share_changed", json!({"bot_id": id, "enabled": true})).await;
    Ok(Json(state(&app, &id).await?))
}

/// 建分享用 bot（受限或信任分享）的前半：決定資料夾（新資料夾就建出來）、`shared_bots` 記下來（在寫 config 之前）。
/// 回 `(資料夾, 是不是這次建的)`；`replay`＝config 裡已經有同一個 `client_request_id` 的 bot：新資料夾已經在了就照用，不回 409。
pub(crate) async fn reserve_share_bot(
    app: &Arc<App>,
    bot_id: &str,
    profile: &str,
    folder: &crate::share::folder::ShareFolderIn,
    replay: bool,
) -> Result<(String, bool), LcError> {
    use crate::share::folder;
    let home = crate::share::cage::local_home();
    let home_p = std::path::Path::new(&home);
    let (dir, created) = match folder {
        folder::ShareFolderIn::Existing { path } => (folder::check_existing(path, home_p, &app.data_dir)?, false),
        folder::ShareFolderIn::New { name } => {
            let root = folder::root(app.cfg.get().await.share.folders_root.as_deref(), &home);
            match folder::create_new(&root, name, home_p, &app.data_dir) {
                Ok(d) => (d, true),
                Err(LcError::Conflict(_)) if replay => (std::fs::canonicalize(root.join(name)).unwrap_or_else(|_| root.join(name)), false),
                Err(e) => return Err(e),
            }
        }
    };
    if let Err(e) = folder::ensure_inbox(&dir) {
        if created {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Err(LcError::Upstream(format!("share folder inbox: {e}")));
    }
    let ws = dir.to_string_lossy().into_owned();
    if let Err(e) = store::insert_share_bot(&app.db, bot_id, profile, &ws).await {
        if created {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Err(db_err(e));
    }
    Ok((ws, created))
}

#[cfg(test)]
pub(crate) async fn reserve_restricted(app: &Arc<App>, bot_id: &str, folder: &crate::share::folder::ShareFolderIn, replay: bool) -> Result<(String, bool), LcError> {
    reserve_share_bot(app, bot_id, store::PROFILE_RESTRICTED, folder, replay).await
}

/// 建受限 bot 的後半：建成了就把 `bots.cwd` 指到資料夾；沒建成（失敗、重送拿回舊的那顆）就把前半收回。
/// 資料夾只有「這次新建的」才刪：既有資料夾、重送時已經在的，一律不動。
pub(crate) async fn finish_restricted(app: &Arc<App>, bot_id: &str, workspace: &str, created_folder: bool, created: bool) {
    if created {
        if let Err(e) = sqlx::query("UPDATE bots SET cwd = ? WHERE id = ?").bind(workspace).bind(bot_id).execute(&app.db).await {
            // 啟動時以 `shared_bots.workspace` 為準（`cage::prepare`），這裡寫不進去只影響側欄顯示的目錄。
            tracing::warn!(bot = bot_id, error = %e, "could not point the restricted bot's cwd at its folder");
        }
        crate::outbox::mark_share_keep(&app.data_dir, bot_id);
        return;
    }
    if let Err(e) = store::delete_restricted(&app.db, bot_id).await {
        tracing::warn!(bot = bot_id, error = %e, "could not roll back a restricted bot reservation");
    }
    if created_folder {
        let _ = std::fs::remove_dir_all(workspace);
    }
}
