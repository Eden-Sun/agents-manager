//! Claude API Bot 的 explicit retire HTTP 入口（issue #1944 S2）：`POST /api/bots/{id}/api-session/retire`。
//!
//! 只給使用者（User）。退役本身在 [`crate::api_bot::explicit_retirement`]（經 `ApiBotPort::api_explicit_retire`），
//! 這裡只做：輸入檢查、預查（找不到 404、fence 不符 409）、把核心的錯誤型別對應成 HTTP。
//! 任何不能證明安全的錯誤一律 fail closed 成 502，不捏造成功。
use std::sync::Arc;

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::api::RequestPrincipal;
use crate::lifecycle::api_bot_port::{ApiBotPort, ApiFreshError, ApiFreshResult, ApiFreshTarget};
use crate::lifecycle::LcError;
use crate::state::App;

/// 同 `start` 的 `api_expected_session_id` 規則：只收短的英數、`-`、`_`。
fn parse_target(body: &Value) -> Result<ApiFreshTarget, LcError> {
    let bad = || LcError::Bad("api_fresh_target_required".into());
    let session_id = body.get("session_id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))).ok_or_else(bad)?;
    let generation = body.get("generation").and_then(Value::as_i64).filter(|g| *g > 0).ok_or_else(bad)?;
    Ok(ApiFreshTarget { session_id: session_id.to_owned(), generation })
}

/// 預查：目標不存在（或不是這顆 bot 的未退役 claude-api session）回 404；generation 不符回 409。
/// 核心只回 `Unconfirmed`、分不出這兩種，所以在進核心前先分。核心仍會在交易裡重新驗證，這一步只是分類。
async fn preflight(app: &App, bot: &str, target: &ApiFreshTarget) -> Result<(), LcError> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT s.generation FROM api_bot_sessions s JOIN bots b ON b.id=s.bot_id \
         WHERE s.id=? AND s.bot_id=? AND s.retired_at IS NULL AND b.kind='claude-api' AND b.deleted_at IS NULL",
    )
    .bind(&target.session_id)
    .bind(bot)
    .fetch_optional(&app.db)
    .await
    .map_err(|_| LcError::Upstream("api_retire_storage".into()))?;
    match row {
        None => Err(LcError::NotFoundValue(json!({"error": "not_found", "reason": "api_session_not_found"}))),
        Some((generation,)) if generation != target.generation => Err(fence_mismatch()),
        Some(_) => Ok(()),
    }
}

fn fence_mismatch() -> LcError {
    LcError::conflict("api_fresh_target_mismatch", json!({}))
}

/// 核心錯誤 → HTTP。`IdentityMismatch` 是 fence 不符（409）；其餘（忙碌、待決、儲存、未確認）都 502，
/// 因為我們證明不了沒有副作用，不能當成失敗或成功回。
fn map_error(error: ApiFreshError) -> LcError {
    match error {
        ApiFreshError::IdentityMismatch => fence_mismatch(),
        ApiFreshError::Busy => LcError::Upstream("api_retire_busy".into()),
        ApiFreshError::Unresolved => LcError::Upstream("api_retire_unresolved".into()),
        ApiFreshError::Storage => LcError::Upstream("api_retire_storage".into()),
        ApiFreshError::Unconfirmed => LcError::Upstream("api_retire_unconfirmed".into()),
    }
}

fn bot_user_only() -> LcError {
    LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"}))
}

pub(crate) async fn retire_api_session(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<Value>,
) -> Result<Response, LcError> {
    if principal != RequestPrincipal::User {
        return Err(bot_user_only());
    }
    let target = parse_target(&body)?;
    preflight(&app, &id, &target).await?;
    match app.api_explicit_retire(&id, &target).await.map_err(map_error)? {
        ApiFreshResult::Cleared => {}
    }
    Ok((StatusCode::OK, Json(json!({"result": "cleared", "session_id": target.session_id, "generation": target.generation}))).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;

    const HOOK: &str = "retire-hook-token";

    /// 與 `api_bot::retention_tests::fixture` 同形（那個是模組私有，這裡自給一份）：一顆 claude-api bot、
    /// 一筆可證明的 session（有 applied 的 join receipt 與對應 transcript），核心才會允許退役。
    async fn fixture() -> crate::testing::Env {
        let env = crate::testing::env().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('rt-bot',?,'rt-bot','claude-api',?,?)")
            .bind(&env.project_id).bind(HOOK).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('rt-run','rt-bot','exited',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('rt-conv','rt-bot',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('rt-turn','rt-conv','rt-run','web','completed','ok',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES('rt-session','rt-bot','rt-run','rt-turn',1,5000,'{}',?,?,?)")
            .bind(&now).bind(&now).bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES('rt-session','rt-bot','rt-run','rt-turn',1,5000,'completed','fixture','applied',?,?)")
            .bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES('rt-message','rt-conv','rt-turn','assistant','fixture','transcript',?)")
            .bind(&now).execute(&env.app.db).await.unwrap();
        env
    }

    async fn sessions(env: &crate::testing::Env) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='rt-session'").fetch_one(&env.app.db).await.unwrap()
    }

    /// 送一個 POST，回整段原始回應（狀態列、標頭、本文）。
    async fn post(addr: SocketAddr, path: &str, headers: &str, body: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await.unwrap().unwrap();
        response
    }

    fn status(response: &str) -> &str {
        response.lines().next().unwrap_or_default()
    }

    fn json_body(response: &str) -> Value {
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    /// 起一台真的 router（含 central Bot policy），回傳 addr 與關閉用的 token。
    async fn serve(env: &crate::testing::Env) -> (SocketAddr, CancellationToken, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let router = crate::api::router(env.app.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async move { stop.cancelled().await })
                .await
                .unwrap()
        });
        (addr, shutdown, task)
    }

    async fn stop(shutdown: CancellationToken, task: tokio::task::JoinHandle<()>) {
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    }

    fn user_headers(env: &crate::testing::Env) -> String {
        format!("X-AM-Token: {}\r\n", env.app.ui_token)
    }

    fn bot_headers() -> String {
        format!("X-AM-Bot-Id: rt-bot\r\nX-AM-Bot-Token: {HOOK}\r\n")
    }

    #[tokio::test]
    async fn user_retires_exact_target_and_row_is_gone() {
        let env = fixture().await;
        let (addr, shutdown, task) = serve(&env).await;
        let response = post(addr, "/api/bots/rt-bot/api-session/retire", &user_headers(&env), r#"{"session_id":"rt-session","generation":1}"#).await;
        stop(shutdown, task).await;
        assert!(status(&response).contains(" 200 "), "{response}");
        assert_eq!(json_body(&response), json!({"result": "cleared", "session_id": "rt-session", "generation": 1}));
        assert_eq!(sessions(&env).await, 0);
    }

    #[tokio::test]
    async fn generation_mismatch_is_409_and_keeps_session() {
        let env = fixture().await;
        let (addr, shutdown, task) = serve(&env).await;
        let response = post(addr, "/api/bots/rt-bot/api-session/retire", &user_headers(&env), r#"{"session_id":"rt-session","generation":2}"#).await;
        stop(shutdown, task).await;
        assert!(status(&response).contains(" 409 "), "{response}");
        assert_eq!(json_body(&response)["reason"], "api_fresh_target_mismatch");
        assert_eq!(sessions(&env).await, 1);
    }

    #[tokio::test]
    async fn unknown_session_is_404_and_keeps_session() {
        let env = fixture().await;
        let (addr, shutdown, task) = serve(&env).await;
        let response = post(addr, "/api/bots/rt-bot/api-session/retire", &user_headers(&env), r#"{"session_id":"nope","generation":1}"#).await;
        stop(shutdown, task).await;
        assert!(status(&response).contains(" 404 "), "{response}");
        assert_eq!(json_body(&response)["reason"], "api_session_not_found");
        assert_eq!(sessions(&env).await, 1);
    }

    #[tokio::test]
    async fn unprovable_evidence_fails_closed_as_502_and_keeps_session() {
        let env = fixture().await;
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='rt-session'").execute(&env.app.db).await.unwrap();
        let (addr, shutdown, task) = serve(&env).await;
        let response = post(addr, "/api/bots/rt-bot/api-session/retire", &user_headers(&env), r#"{"session_id":"rt-session","generation":1}"#).await;
        stop(shutdown, task).await;
        assert!(status(&response).contains(" 502 "), "{response}");
        assert_eq!(sessions(&env).await, 1);
    }

    #[tokio::test]
    async fn bot_principal_is_403_user_only_and_keeps_session() {
        let env = fixture().await;
        let (addr, shutdown, task) = serve(&env).await;
        let response = post(addr, "/api/bots/rt-bot/api-session/retire", &bot_headers(), r#"{"session_id":"rt-session","generation":1}"#).await;
        stop(shutdown, task).await;
        assert!(status(&response).contains(" 403 "), "{response}");
        assert_eq!(json_body(&response)["reason"], "user_only");
        assert_eq!(sessions(&env).await, 1);
    }

    #[tokio::test]
    async fn malformed_body_is_400_before_any_mutation() {
        let env = fixture().await;
        let (addr, shutdown, task) = serve(&env).await;
        let response = post(addr, "/api/bots/rt-bot/api-session/retire", &user_headers(&env), r#"{"session_id":"rt-session"}"#).await;
        stop(shutdown, task).await;
        assert!(status(&response).contains(" 400 "), "{response}");
        assert_eq!(sessions(&env).await, 1);
    }
}
