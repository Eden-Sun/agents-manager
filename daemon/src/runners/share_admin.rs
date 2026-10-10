//! 分享連結管理端點的 axum handler（`share::admin` 的組裝層）：吃 `App` 與 `RequestPrincipal` 的部分放在這裡，
//! `share::admin` 只留 `Cfg`／`Db` 這類窄 trait 的核心邏輯（step 6a）。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::RequestPrincipal;
use crate::lifecycle::LcError;
use crate::share::admin;
use crate::share::store;
use crate::state::App;

#[derive(Deserialize)]
pub(crate) struct ShareIn {
    enabled: bool,
    /// 只收信任分享（SPEC §20.1a）：`true`＝允許 `<iframe>` 嵌入這條連結；沒帶＝沿用現況。受限分享帶了一律 400。
    #[serde(default)]
    allow_embed: Option<bool>,
}

fn user_only(principal: &RequestPrincipal) -> Result<(), LcError> {
    if *principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    Ok(())
}

pub(crate) async fn get_share(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    let lock = app.bot_lock(&id).await;
    let _guard = lock.lock_owned().await;
    if !admin::shareable_bot(&app, &id).await? {
        return Ok(Json(json!({"shareable": false, "enabled": false, "url": null, "needs_rotate": false, "token_hint": null, "created_at": null, "last_used_at": null, "allow_embed": false})));
    }
    Ok(Json(admin::state(&app, &id).await?))
}

pub(crate) async fn post_share(
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
    if !admin::shareable_bot(&app, &id).await? {
        return Err(admin::not_shareable(&id));
    }
    if body.allow_embed.is_some() && !store::is_trusted(&app.db, &id).await.map_err(admin::db_err)? {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "share_embed_trusted_only", "message": "「允許 iframe 嵌入」只給信任分享的 bot"})));
    }
    if !body.enabled {
        store::disable(&app.db, &id).await.map_err(admin::db_err)?;
        crate::share::portal::kick(&id);
        app.emit("bot_share_changed", json!({"bot_id": id, "enabled": false})).await;
        return Ok(Json(admin::state(&app, &id).await?));
    }
    admin::base_url(&app).await?;
    // 已經開著：不換 token（已經發出去的連結照樣能用），回的是同一條網址。
    if store::enable(&app.db, &id).await.map_err(admin::db_err)?.is_some() {
        app.emit("bot_share_changed", json!({"bot_id": id, "enabled": true})).await;
    }
    if let Some(allow) = body.allow_embed {
        store::set_allow_embed(&app.db, &id, allow).await.map_err(admin::db_err)?;
    }
    Ok(Json(admin::state(&app, &id).await?))
}

pub(crate) async fn post_rotate(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    #[cfg(test)]
    crate::lifecycle::race_point::hit("share_admin_before_lock", &id).await;
    let lock = app.bot_lock(&id).await;
    let _guard = lock.lock_owned().await;
    if !admin::shareable_bot(&app, &id).await? {
        return Err(admin::not_shareable(&id));
    }
    admin::base_url(&app).await?;
    if store::rotate(&app.db, &id).await.map_err(admin::db_err)?.is_none() {
        return Err(LcError::conflict("share_disabled", json!({"bot_id": id, "message": "分享沒開著，先開分享"})));
    }
    crate::share::portal::kick(&id);
    app.emit("bot_share_changed", json!({"bot_id": id, "enabled": true})).await;
    Ok(Json(admin::state(&app, &id).await?))
}
