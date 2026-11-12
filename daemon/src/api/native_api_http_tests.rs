use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

async fn get(addr: std::net::SocketAddr, path: &str, auth: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_to_string(&mut response)).await.unwrap().unwrap();
    response
}

async fn post(addr: std::net::SocketAddr, path: &str, auth: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_to_string(&mut response)).await.unwrap().unwrap();
    response
}

#[tokio::test]
async fn native_fresh_target_for_cli_survives_absent_private_tables() {
    let env=crate::testing::env().await;
    let bot=crate::testing::claude_bot(&env.app,&env.project_id,"native-descriptor-cli").await;
    for table in ["api_bot_join_receipts","api_bot_attempts","api_bot_sessions"] {
        sqlx::query(&format!("DROP TABLE {table}")).execute(&env.app.db).await.unwrap();
    }
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(); let addr=listener.local_addr().unwrap();
    let shutdown=CancellationToken::new(); let stop=shutdown.clone(); let router=super::router(env.app.clone());
    let task=tokio::spawn(async move { axum::serve(listener,router.into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(async move { stop.cancelled().await }).await.unwrap() });
    let user=format!("X-AM-Token: {}\r\n",env.app.ui_token);
    let response=get(addr,&format!("/api/bots/{}/api_fresh_target",bot.id),&user).await;
    let state=get(addr,"/api/state",&user).await;
    shutdown.cancel(); tokio::time::timeout(std::time::Duration::from_secs(5),task).await.unwrap().unwrap();
    assert!(response.starts_with("HTTP/1.1 200") && response.contains("unavailable"));
    assert!(state.starts_with("HTTP/1.1 200"));
}

#[tokio::test]
async fn api_usage_sums_attempts_and_is_user_only() {
    let env = crate::testing::env().await;
    let (bot, token, cli) = (crate::db::ulid(), crate::db::ulid(), crate::testing::claude_bot(&env.app, &env.project_id, "usage-cli").await);
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api',?,?)")
        .bind(&bot).bind(&env.project_id).bind(&bot).bind(&token).bind(&now).execute(&env.app.db).await.unwrap();
    let ins = |n: i64, state: &'static str, speed: Option<&'static str>, usage: Option<&'static str>, reserved: i64, est: Option<i64>| {
        let (db, bot, now) = (env.app.db.clone(), bot.clone(), now.clone());
        async move {
            sqlx::query("INSERT INTO api_bot_attempts(id,bot_id,generation,ordinal,request_hash,requested_speed,actual_speed,state,usage_json,reserved_microusd,estimated_microusd,pricing_version,created_at) VALUES(?,?,1,?,'h','standard',?,?,?,?,?,'v',?)")
                .bind(crate::db::ulid()).bind(&bot).bind(n).bind(speed).bind(state).bind(usage).bind(reserved).bind(est).bind(&now).execute(&db).await.unwrap();
        }
    };
    ins(0, "recorded", Some("standard"), Some(r#"{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":300,"cache_creation_input_tokens":100}"#), 4_000_000, Some(2_000)).await;
    ins(1, "reserved", None, None, 4_100_000, None).await;
    ins(2, "unknown", None, None, 4_200_000, None).await;
    ins(3, "failed", None, None, 4_300_000, None).await;
    let bot2 = crate::db::ulid();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api',?,?)")
        .bind(&bot2).bind(&env.project_id).bind(&bot2).bind(crate::db::ulid()).bind(&now).execute(&env.app.db).await.unwrap();
    let conv = crate::db::ulid();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)").bind(&conv).bind(&bot2).bind(&now).execute(&env.app.db).await.unwrap();
    for turn in ["t1", "t2"] {
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,created_at) VALUES(?,?,'web','completed',?)").bind(turn).bind(&conv).bind(&now).execute(&env.app.db).await.unwrap();
    }
    for (n, turn, state, usage, reserved, est) in [(10, "t1", "recorded", Some(r#"{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":20,"cache_creation_input_tokens":7}"#), 100, Some(119_000)), (11, "t1", "recorded", Some(r#"{"input_tokens":1,"output_tokens":2}"#), 100, Some(1_000)), (12, "t2", "reserved", None, 500_000, None)] {
        sqlx::query("INSERT INTO api_bot_attempts(id,bot_id,turn_id,generation,ordinal,request_hash,requested_speed,state,usage_json,reserved_microusd,estimated_microusd,pricing_version,created_at) VALUES(?,?,?,1,?,'h','standard',?,?,?,?,'v',?)")
            .bind(crate::db::ulid()).bind(&bot2).bind(turn).bind(n).bind(state).bind(usage).bind(reserved).bind(est).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(); let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new(); let stop = shutdown.clone();
    let router = super::router(env.app.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(async move { stop.cancelled().await }).await.unwrap() });
    let user = format!("X-AM-Token: {}\r\n", env.app.ui_token);
    let bot_auth = format!("X-AM-Bot-Id: {bot}\r\nX-AM-Bot-Token: {token}\r\n");
    let ok = get(addr, &format!("/api/bots/{bot}/api-usage"), &user).await;
    let denied = get(addr, &format!("/api/bots/{bot}/api-usage"), &bot_auth).await;
    let cli_resp = get(addr, &format!("/api/bots/{}/api-usage", cli.id), &user).await;
    let turns_resp = get(addr, &format!("/api/bots/{bot2}/api-usage"), &user).await;
    shutdown.cancel(); tokio::time::timeout(std::time::Duration::from_secs(5), task).await.unwrap().unwrap();
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    let body: Value = serde_json::from_str(ok.split("\r\n\r\n").nth(1).unwrap().trim()).unwrap();
    assert_eq!(body["attempts"], json!({"total":4,"recorded":1,"reserved":1,"unknown":1,"failed":1}));
    assert_eq!(body["cost_microusd"], json!({"recorded":2000,"reserved":4_100_000,"unknown":4_200_000}));
    assert_eq!(body["tokens"], json!({"input":100,"output":50,"cache_read":300,"cache_creation":100}));
    assert_eq!(body["cache_hit_rate"], json!(0.6));
    assert_eq!(body["last"]["speed"], "standard");
    assert_eq!(body["turns"], json!([]), "沒有 turn_id 的 attempt 不算 turn");
    let body2: Value = serde_json::from_str(turns_resp.split("\r\n\r\n").nth(1).unwrap().trim()).unwrap();
    let turns = body2["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    let t1 = turns.iter().find(|t| t["turn_id"] == "t1").unwrap();
    assert_eq!((t1["requests"].as_i64(), t1["cost_usd"].as_f64(), t1["in_flight"].as_bool()), (Some(2), Some(0.12), Some(false)));
    assert_eq!((t1["input_tokens"].as_i64(), t1["output_tokens"].as_i64(), t1["cache_read_tokens"].as_i64(), t1["cache_write_tokens"].as_i64()), (Some(11), Some(7), Some(20), Some(7)));
    let t2 = turns.iter().find(|t| t["turn_id"] == "t2").unwrap();
    assert_eq!((t2["in_flight"].as_bool(), t2["reserved_usd"].as_f64(), t2["cost_usd"].as_f64()), (Some(true), Some(0.5), Some(0.0)));
    assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");
    assert!(cli_resp.starts_with("HTTP/1.1 400"), "{cli_resp}");
}

#[tokio::test]
async fn native_fresh_target_is_user_only_and_unavailable_on_storage_error() {
    let env = crate::testing::env().await;
    let bot = crate::db::ulid(); let session = crate::db::ulid(); let token = crate::db::ulid();
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api',?,?)")
        .bind(&bot).bind(&env.project_id).bind(&bot).bind(&token).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,1,1,'{}',?,?,?)")
        .bind(&session).bind(&bot).bind(&now).bind(&now).bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(); let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new(); let stop = shutdown.clone();
    let router = super::router(env.app.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(async move { stop.cancelled().await }).await.unwrap() });
    let user = format!("X-AM-Token: {}\r\n", env.app.ui_token);
    let bot_auth = format!("X-AM-Bot-Id: {bot}\r\nX-AM-Bot-Token: {token}\r\n");
    let path = format!("/api/bots/{bot}/api_fresh_target");
    let visible = get(addr, &path, &user).await;
    let bot_state = get(addr, "/api/state", &bot_auth).await;
    let user_state = get(addr, "/api/state", &user).await;
    let denied = get(addr, &path, &bot_auth).await;
    crate::supervisor::store::get_or_init(&env.app.db).await.unwrap();
    crate::supervisor::store::set_env(&env.app.db, &bot, &env.project_id, "/tmp").await.unwrap();
    let agm_denied = get(addr, &path, &bot_auth).await;
    let fresh_path = format!("/api/bots/{bot}/start?resume=fresh&api_expected_session_id={session}&api_expected_generation=1");
    let agm_fresh = post(addr, &fresh_path, &bot_auth).await;
    let session_after_agm: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap();
    let svc_token = crate::db::ulid();
    env.app.service_tokens.write().unwrap().insert(crate::service_auth::DAEMON_SWAP.into(), svc_token.clone());
    let svc_auth = format!("X-AM-Service-Id: {}\r\nX-AM-Service-Token: {}\r\n", crate::service_auth::DAEMON_SWAP, svc_token);
    let svc_probe = get(addr, "/api/supervisor/health", &svc_auth).await;
    let svc_denied = get(addr, &path, &svc_auth).await;
    sqlx::query("DROP TABLE api_bot_sessions").execute(&env.app.db).await.unwrap();
    let unavailable = get(addr, &path, &user).await;
    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), task).await.unwrap().unwrap();
    assert!(visible.starts_with("HTTP/1.1 200"));
    let value: Value = serde_json::from_str(visible.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(value, json!({"availability":"available","session_id":session,"generation":1}));
    assert!(bot_state.starts_with("HTTP/1.1 200"));
    assert!(user_state.starts_with("HTTP/1.1 200"));
    assert!(!bot_state.contains(&session) && !user_state.contains(&session));
    assert!(denied.starts_with("HTTP/1.1 403"));
    assert!(agm_denied.starts_with("HTTP/1.1 403"));
    assert!(agm_fresh.starts_with("HTTP/1.1 403"));
    assert_eq!(session_after_agm, 1);
    assert!(!svc_probe.starts_with("HTTP/1.1 401") && !svc_probe.starts_with("HTTP/1.1 403"));
    assert!(svc_denied.starts_with("HTTP/1.1 403"));
    assert!(unavailable.starts_with("HTTP/1.1 200"));
    let value: Value = serde_json::from_str(unavailable.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(value, json!({"availability":"unavailable"}));
}
