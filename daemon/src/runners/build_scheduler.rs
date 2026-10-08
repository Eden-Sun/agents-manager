use std::sync::Arc;
use axum::extract::{Form, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};
use crate::build_scheduler::{
    acquire, authenticate, field_limit_error, has_oversized_fields, release_as, renew, status,
    sweep, up, AcquireIn, Acquired, ReleaseIn, RenewErr, RenewIn, MAX_FIELD_CHARS,
    MAX_ROWS_PER_BOT, SWEEP_EVERY,
};
use crate::lc_error::LcError;
use crate::state::App;

/// 測試裡縮短週期，才看得到「第一輪 panic 之後下一輪照常」。
const TICK_EVERY: std::time::Duration = if cfg!(test) { std::time::Duration::from_millis(50) } else { SWEEP_EVERY };

pub fn spawn_sweeper(app: Arc<App>) {
    crate::background_loop::spawn_periodic(&app, "build scheduler sweeper", TICK_EVERY, TICK_EVERY, |app| async move {
        #[cfg(test)]
        {
            if PANIC_NEXT_TICK.swap(false, std::sync::atomic::Ordering::SeqCst) {
                panic!("test-injected build scheduler sweep panic");
            }
            TICKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let (held, waiting) = sweep(app.as_ref()).await;
        if held > 0 || waiting > 0 {
            tracing::info!(held, waiting, "build scheduler: 收回沒人續約／沒人再 poll 的名額");
        }
    });
}

/// 測試用：讓下一輪 sweep 一開始就 panic 一次，以及數 tick 次數。
#[cfg(test)]
pub(crate) static PANIC_NEXT_TICK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
pub(crate) static TICKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub async fn get_status(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(status(app.as_ref()).await.map_err(up)?))
}

pub async fn post_acquire(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(body): Form<AcquireIn>,
) -> Result<Json<Value>, LcError> {
    if body.holder.trim().is_empty() {
        return Err(LcError::Bad("holder 不能是空的".into()));
    }
    if has_oversized_fields(&[&body.holder, &body.purpose, &body.host]) {
        return Err(field_limit_error());
    }
    let bot_id = authenticate(app.as_ref(), &headers, body.bot_id.as_deref()).await?;
    match acquire(
        app.as_ref(),
        body.holder.trim(),
        bot_id.as_deref(),
        body.purpose.trim(),
        body.host.trim(),
    )
    .await
    .map_err(up)?
    {
        Acquired::Granted { token, expires_at } => {
            let cfg = app.cfg.build_fresh().await;
            Ok(Json(json!({
                "granted": true,
                "token": token,
                "expires_at": expires_at,
                "cargo_jobs": cfg.cargo_jobs,
                "test_threads": cfg.test_threads(),
                "lease_ttl_secs": cfg.lease_ttl().map_err(LcError::Bad)?
            })))
        }
        Acquired::Waiting { active, since } => {
            let cfg = app.cfg.build_fresh().await;
            Ok(Json(json!({
                "granted": false,
                "active": active,
                "max_concurrent": cfg.max_concurrent(),
                "since": since,
                "retry_after_secs": 5
            })))
        }
        Acquired::TooManyForBot => Err(LcError::conflict(
            "too_many_build_slots",
            json!({
                "reason": "too_many_build_slots",
                "max_per_bot": MAX_ROWS_PER_BOT,
                "message": "這顆 bot 同時佔著或排著的名額太多了；等手上的 cargo 跑完（或放掉）再要"
            }),
        )),
        Acquired::HolderOwnedByAnotherBot => Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "holder_bot_mismatch",
            "message": "a bot may only reuse its own build slot holder",
        }))),
    }
}

pub async fn post_renew(
    State(app): State<Arc<App>>,
    Form(body): Form<RenewIn>,
) -> Result<Json<Value>, LcError> {
    if has_oversized_fields(&[&body.holder, &body.token]) {
        return Err(field_limit_error());
    }
    match renew(app.as_ref(), body.holder.trim(), &body.token).await.map_err(up)? {
        Ok(expires_at) => Ok(Json(json!({"renewed": true, "expires_at": expires_at}))),
        Err(RenewErr::NotFound) => Err(LcError::NotFound("build_slot".into())),
        Err(RenewErr::TokenMismatch) => Err(LcError::Forbidden(json!({"error": "token_mismatch"}))),
    }
}

pub async fn post_release(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(body): Form<ReleaseIn>,
) -> (axum::http::StatusCode, Json<Value>) {
    if has_oversized_fields(&[&body.holder, &body.token]) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({
                "released": false,
                "error": "bad_request",
                "message": format!("build-slot 欄位最多 {MAX_FIELD_CHARS} 個字元")
            })),
        );
    }
    // 空 token＝取消等待（#913）：沒有 token 可以證明這個 holder 是你的，所以要驗身分，而且只能取消自己的號碼牌。
    // 帶 token 的放名額維持原樣（token 本身就是憑證，shim 的 `trap … EXIT` 不用帶標頭）。
    let bot_id = if body.token.is_empty() {
        match authenticate(app.as_ref(), &headers, body.bot_id.as_deref()).await {
            Ok(id) => id,
            Err(_) => {
                return (
                    axum::http::StatusCode::FORBIDDEN,
                    Json(json!({"released": false, "error": "unauthorized", "message": "取消等待要帶 bot 身分或 UI token"})),
                )
            }
        }
    } else {
        None
    };
    match release_as(app.as_ref(), body.holder.trim(), &body.token, bot_id.as_deref()).await {
        Ok(()) => (axum::http::StatusCode::OK, Json(json!({"released": true}))),
        Err(e) => {
            tracing::error!(holder = %body.holder, error = ?e, "build scheduler: could not release a slot; it stays held until its lease expires");
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({"released": false})))
        }
    }
}
