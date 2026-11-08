//! `?confirm=` 的判斷必須跟 handler 的解碼一致（review L2 #1）：編碼過的 `%63onfirm` 不能讓非 AGM 的管理者刪受保護的 bot。
use crate::{api, db, state::App};
use axum::extract::{Path, Query, State};
use axum::Extension;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn raw(app: Arc<App>, request: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = api::router(app);
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).await.unwrap();
    server.abort();
    response
}

async fn bot_with_token(e: &crate::testing::Env, name: &str, role: &str) -> db::Bot {
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
    sqlx::query("UPDATE bots SET hook_token=?, role=? WHERE id=?")
        .bind(format!("confirm-{}", bot.id))
        .bind(role)
        .bind(&bot.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    db::bot(&e.app.db, &bot.id).await.unwrap().unwrap()
}

/// 專家模式的管理者 bot（非 AGM 角色）＋同專案的執行者。
async fn expert_setup(e: &crate::testing::Env) -> (db::Bot, db::Bot) {
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&e.project_id).execute(&e.app.db).await.unwrap();
    let manager = bot_with_token(e, "expert-manager", "manager").await;
    let executor = bot_with_token(e, "protected-executor", "executor").await;
    (manager, executor)
}

fn deleted(row: Option<db::Bot>) -> bool {
    row.is_none_or(|b| b.deleted_at.is_some())
}

#[tokio::test]
async fn encoded_confirm_from_expert_manager_is_refused_and_nothing_is_deleted() {
    let e = crate::testing::env().await;
    let (manager, executor) = expert_setup(&e).await;
    for query in ["confirm=supervisor", "%63onfirm=supervisor", "con%66irm=supervisor", "confirm=", "%63onfirm"] {
        let request = format!(
            "DELETE /api/bots/{}?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nConnection: close\r\n\r\n",
            executor.id, manager.id, manager.hook_token
        );
        let response = raw(e.app.clone(), request).await;
        assert!(response.starts_with("HTTP/1.1 403"), "{query} must be refused: {response}");
        assert!(!deleted(db::bot(&e.app.db, &executor.id).await.unwrap()), "{query} must not delete the executor");
    }
}

#[tokio::test]
async fn delete_route_rechecks_the_principal_when_confirm_is_present() {
    let e = crate::testing::env().await;
    let (manager, executor) = expert_setup(&e).await;
    let confirm = Query(api::DeleteQuery { confirm: Some("supervisor".into()) });
    let manager_principal = Some(Extension(api::RequestPrincipal::Bot(manager.id.clone())));
    let denied = api::delete_bot_route_for_test(State(e.app.clone()), Path(executor.id.clone()), confirm, manager_principal).await;
    assert!(denied.is_err(), "non-AGM Bot principal with confirm must be refused in the handler");
    assert!(!deleted(db::bot(&e.app.db, &executor.id).await.unwrap()));
    // 使用者帶 confirm 不被 principal 閘擋（這個假資料的 bot 不在 config.toml，所以後面會以 not_in_config 拒絕，但不是 403）。
    let user = Some(Extension(api::RequestPrincipal::User));
    let passed = api::delete_bot_route_for_test(State(e.app.clone()), Path(executor.id.clone()), Query(api::DeleteQuery { confirm: Some("supervisor".into()) }), user).await;
    assert!(!matches!(passed, Err(crate::lc_error::LcError::Forbidden(_))), "User principal must pass the confirm gate");
}
