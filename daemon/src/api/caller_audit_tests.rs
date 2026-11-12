//! review d77434c0 #3：`X-AM-Caller` 是自由文字，任何拿得到 UI token 的人都能寫 `caller=agm`。
//! 身份只認 `X-AM-Bot-Id` ＋ 對得上的 `X-AM-Bot-Token`；自稱記在另一格，而且要看得出是自稱。
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 一顆進了 config.toml 的 user bot（刪除才找得到目標）。
async fn a_user_bot(e: &crate::testing::Env, name: &str) -> db::Bot {
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
    let (pid, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
    let (bid, bname) = (bot.id.clone(), name.to_string());
    e.app
        .cfg
        .update(move |cfg| {
            let entry: crate::config::BotCfg =
                toml::from_str(&format!("id = '{bid}'\nname = '{bname}'\nkind = 'claude'\n")).unwrap();
            match cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())) {
                Some(p) => p.bots.push(entry),
                None => cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                    handed_off_to: None,
                    id: Some(pid),
                    path: repo,
                    label: "proj".into(),
                    host: LOCAL_HOST.into(),
                    bots: vec![entry],
                }),
            }
            Ok(())
        })
        .await
        .unwrap();
    bot
}

/// 真的走一次 HTTP（含 `auth` 中介層），回 `(狀態行, intent 記下的呼叫端)`。
/// `DELETE /api/bots/{id}` with exactly these headers (no UI token added); the raw response.
async fn delete_raw(app: &Arc<App>, bot_id: &str, headers: &[(&str, &str)], confirm_supervisor: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(app.clone());
    let server =
        tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let confirm = if confirm_supervisor { "?confirm=supervisor" } else { "" };
    let req = format!("DELETE /api/bots/{bot_id}{confirm} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\nContent-Length: 0\r\n\r\n");
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    server.abort();
    out
}

/// Delete with these headers, then read who the delete intent says asked for it.
async fn delete_as(app: &Arc<App>, bot_id: &str, headers: &[(&str, &str)], confirm_supervisor: bool) -> (String, String) {
    let out = delete_raw(app, bot_id, headers, confirm_supervisor).await;
    let payload: String = sqlx::query_scalar("SELECT payload_json FROM intents WHERE kind = 'delete_bot' AND subject_id = ?")
        .bind(bot_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&payload).unwrap();
    (out.lines().next().unwrap_or_default().to_string(), v["requested_by"].as_str().unwrap_or_default().to_string())
}

/// As the User (shared UI token) plus these extra headers.
async fn delete_over_http(app: &Arc<App>, bot_id: &str, headers: &[(&str, &str)], confirm_supervisor: bool) -> (String, String) {
    let mut all = vec![("X-AM-Token", app.ui_token.as_str())];
    all.extend_from_slice(headers);
    delete_as(app, bot_id, &all, confirm_supervisor).await
}

/// 只有 hook token 對得上才填 `bot=`；`X-AM-Caller` 不管寫什麼都只進 `caller_self_reported=`。
#[tokio::test]
async fn a_self_reported_caller_is_never_recorded_as_a_verified_bot() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let liar = a_user_bot(&e, "alfa").await;
    let impostor = a_user_bot(&e, "bravo").await;
    let real = a_user_bot(&e, "charlie").await;

    // (1) 只會自稱：記成自稱，`bot=` 空著。
    let (status, by) = delete_over_http(&app, &liar.id, &[("X-AM-Caller", "agm"), ("User-Agent", "curl/8")], false).await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(by.contains("caller_self_reported=agm"), "{by}");
    assert!(by.contains(" bot=-"), "沒有 hook token 就不能有身份：{by}");
    assert!(by.contains("peer=127.0.0.1:") && by.contains("ua=curl/8"), "{by}");

    // (2) 自稱是某顆 bot 但 token 對不上：#556 起中介層直接 401，不降級成 User，也就刪不掉、不留 intent。
    let refused = delete_raw(&app, &impostor.id, &[("X-AM-Bot-Id", &real.id), ("X-AM-Bot-Token", "not-the-token"), ("X-AM-Caller", "agm")], false).await;
    assert!(refused.starts_with("HTTP/1.1 401"), "token 對不上不能算驗過：{refused}");
    assert!(db::bot(&app.db, &impostor.id).await.unwrap().unwrap().deleted_at.is_none(), "被拒的請求不能刪掉 bot");

    // (3) 一般 Bot 不可呼叫管理刪除；註冊成 AGM role 後，帶對 token 可用原權限走完刪除，來源自稱照樣分欄記錄。
    crate::supervisor::store::get_or_init(&app.db).await.unwrap();
    crate::supervisor::store::set_env(&app.db, &real.id, &e.project_id, "/tmp").await.unwrap();
    let token: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id = ?").bind(&real.id).fetch_one(&app.db).await.unwrap();
    let (_, by) = delete_as(&app, &real.id, &[("X-AM-Bot-Id", &real.id), ("X-AM-Bot-Token", &token), ("X-AM-Caller", "agm")], true).await;
    assert!(by.contains(&format!("bot={}(charlie)", real.id)), "{by}");
    assert!(by.contains("caller_self_reported=agm"), "{by}");
}

/// 軟刪的 bot 拿自己的 token 也不算驗過（token 外流之後還能冒名記帳）。
#[tokio::test]
async fn a_deleted_bots_token_no_longer_proves_identity() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let bot = crate::testing::claude_bot(&app, &e.project_id, "gone").await;
    let token: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id = ?").bind(&bot.id).fetch_one(&app.db).await.unwrap();
    let mut h = HeaderMap::new();
    h.insert("X-AM-Bot-Id", bot.id.parse().unwrap());
    h.insert("X-AM-Bot-Token", token.parse().unwrap());
    assert!(verified_caller_bot(&app, &h).await.is_some(), "前提：活著時驗得過");

    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&bot.id).execute(&app.db).await.unwrap();
    assert_eq!(verified_caller_bot(&app, &h).await, None);
}
