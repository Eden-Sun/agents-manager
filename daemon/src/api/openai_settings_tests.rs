//! `/api/openai-api/settings`（使用者 2026-11-18）與帳本的供應商篩選：UserOnly、GET 絕不回金鑰、PUT 寫回 config.toml 並熱重載、
//! 累計花費只算 openai。金鑰一律是暫存目錄裡的假 sentinel，不讀不寫任何真 key。
use super::*;
use crate::testing::{env, Env};
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SENTINEL: &str = "sk-test-SENTINEL-XYZ";

async fn http(e: &Env, method: &str, path: &str, body: Option<Value>, headers: &[(&str, String)]) -> (u16, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(e.app.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let payload = body.map(|b| serde_json::to_string(&b).unwrap()).unwrap_or_default();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(request.as_bytes()).await.unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    server.abort();
    let status = out.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

fn user(e: &Env) -> Vec<(&'static str, String)> {
    vec![("X-AM-Token", e.app.ui_token.clone())]
}
async fn get(e: &Env) -> (u16, Value) {
    let (s, b) = http(e, "GET", "/api/openai-api/settings", None, &user(e)).await;
    (s, serde_json::from_str(&b).unwrap_or(Value::Null))
}
async fn put(e: &Env, body: Value) -> (u16, Value) {
    let (s, b) = http(e, "PUT", "/api/openai-api/settings", Some(body), &user(e)).await;
    (s, serde_json::from_str(&b).unwrap_or(Value::Null))
}
fn key_file(dir: &std::path::Path, mode: u32) -> String {
    let p = dir.join("openai-key");
    std::fs::write(&p, format!("{SENTINEL}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p.display().to_string()
}

#[tokio::test]
async fn get_and_put_never_contain_the_key_only_whether_it_is_usable() {
    let e = env().await;
    let dir = crate::testing::scratch_dir("openai-settings-key");
    let path = key_file(&dir, 0o600);
    let (status, first) = get(&e).await;
    assert_eq!(status, 200);
    assert_eq!(first["enabled"], false, "預設關");
    assert_eq!(first["key_present"], false, "預設的 key 檔不存在");
    let (status, v) = put(&e, json!({"enabled": true, "key_file": path})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!((v["enabled"].as_bool(), v["key_present"].as_bool(), v["key_file"].as_str()), (Some(true), Some(true), Some(path.as_str())));
    let (_, raw) = http(&e, "GET", "/api/openai-api/settings", None, &user(&e)).await;
    assert!(!raw.contains(SENTINEL), "GET 不能回金鑰：{raw}");
    // 權限對外開放：讀不到，原因是固定句型，也不含金鑰。
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (_, raw) = http(&e, "GET", "/api/openai-api/settings", None, &user(&e)).await;
    let v: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["key_present"], false);
    assert!(v["key_error"].as_str().is_some_and(|m| m.contains("chmod 600")), "{v}");
    assert!(!raw.contains(SENTINEL), "{raw}");
    // 設定檔裡只有路徑，沒有金鑰。
    let on_disk = std::fs::read_to_string(e.app.data_dir.join("config.toml")).unwrap();
    assert!(on_disk.contains("[openai_api]") && !on_disk.contains(SENTINEL), "{on_disk}");
}

#[tokio::test]
async fn a_bot_gets_403_user_only_and_changes_nothing() {
    let e = env().await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "openai-settings-bot").await;
    sqlx::query("UPDATE bots SET hook_token=? WHERE id=?").bind("tok-own").bind(&bot.id).execute(&e.app.db).await.unwrap();
    let as_bot = vec![("X-AM-Bot-Id", bot.id.clone()), ("X-AM-Bot-Token", "tok-own".to_string())];
    for (method, body) in [("GET", None), ("PUT", Some(json!({"enabled": true, "total_cap_usd": 99.0})))] {
        let (status, text) = http(&e, method, "/api/openai-api/settings", body, &as_bot).await;
        assert_eq!(status, 403, "{method}: {text}");
        assert!(text.contains("user_only"), "{text}");
    }
    let (status, v) = get(&e).await;
    assert_eq!((status, v["enabled"].clone(), v["api_bot"]["total_cap_usd"].clone()), (200, json!(false), Value::Null), "bot 的 PUT 什麼都沒寫");
}

#[tokio::test]
async fn put_writes_the_cap_validates_it_and_a_hand_edit_hot_reloads() {
    let e = env().await;
    let (status, v) = put(&e, json!({"total_cap_usd": 5.0})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["api_bot"]["total_cap_usd"], 5.0);
    let since = v["api_bot"]["total_cap_since"].as_str().expect("設上限沒給起算時間＝從現在起算").to_string();
    assert!(chrono::DateTime::parse_from_rfc3339(&since).is_ok(), "{since}");
    let cfg = e.app.cfg.get().await;
    assert_eq!((cfg.openai_api.api_bot.total_cap_usd, cfg.openai_api.api_bot.total_cap_since.as_deref()), (Some(5.0), Some(since.as_str())));
    // 不合法的輸入：400，而且什麼都沒改。
    for bad in [json!({"total_cap_usd": -1.0}), json!({"total_cap_since": "yesterday"}), json!({"key_file": "relative/key"}), json!({"key_file": ""}), json!({"key_file": "/tmp/a\nb"})] {
        let (status, body) = put(&e, bad.clone()).await;
        assert_eq!(status, 400, "{bad}: {body}");
    }
    assert_eq!(get(&e).await.1["api_bot"]["total_cap_usd"], 5.0);
    // 手改 config.toml → 下一次 GET 就是新值（熱重載），不必重啟。
    let path = e.app.data_dir.join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("total_cap_usd = 5.0"), "{text}");
    std::fs::write(&path, text.replace("total_cap_usd = 5.0", "total_cap_usd = 7.5")).unwrap();
    assert_eq!(get(&e).await.1["api_bot"]["total_cap_usd"], 7.5, "熱重載");
    assert_eq!(e.app.cfg.claude_fresh().await.openai_api.api_bot.total_cap_usd, Some(7.5));
    // null＝清除（起算時間一起清）。
    let (_, v) = put(&e, json!({"total_cap_usd": null})).await;
    assert_eq!((v["api_bot"]["total_cap_usd"].clone(), v["api_bot"]["total_cap_since"].clone()), (Value::Null, Value::Null));
}

#[tokio::test]
async fn the_spent_total_counts_only_openai_from_both_ledgers() {
    let e = env().await;
    let at = am_base::db::now();
    for (id, provider, est) in [("a-openai", "openai", 700_i64), ("a-claude", "anthropic", 9_000)] {
        sqlx::query("INSERT INTO api_bot_attempts(id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,estimated_microusd,pricing_version,created_at,provider) VALUES(?,1,0,?,'standard','recorded',?,?,'v',?,?)")
            .bind(id).bind("0".repeat(64)).bind(est).bind(est).bind(&at).bind(provider).execute(&e.app.db).await.unwrap();
    }
    let dir = e.app.data_dir.join("claude-api");
    std::fs::create_dir_all(&dir).unwrap();
    let line = |provider: Option<&str>, micro: i64| match provider {
        Some(p) => json!({"at": at, "microusd": micro, "provider": p}).to_string(),
        None => json!({"at": at, "microusd": micro}).to_string(),
    };
    std::fs::write(dir.join("spend.jsonl"), [line(Some("openai"), 300), line(None, 5_000), line(Some("anthropic"), 6_000)].join("\n") + "\n").unwrap();
    let (_, v) = get(&e).await;
    assert!((v["spent_bots_usd"].as_f64().unwrap() - 0.0007).abs() < 1e-9, "{v}");
    assert!((v["spent_uses_usd"].as_f64().unwrap() - 0.0003).abs() < 1e-9, "{v}");
    assert!((v["spent_usd"].as_f64().unwrap() - 0.001).abs() < 1e-9, "舊行（沒有 provider）算 Claude：{v}");
}

#[tokio::test]
async fn the_ledger_can_be_filtered_by_provider() {
    let e = env().await;
    let row = |provider: &'static str, model: &str, input: u64| crate::claude_api::Row {
        at: am_base::db::now(),
        use_case: "chat_translate".into(),
        model: model.into(),
        provider,
        elapsed_ms: 10,
        error_kind: None,
        input_tokens: input,
        output_tokens: 1,
        stop_reason: None,
    };
    e.app.claude_api.ledger.record(row("anthropic", "claude-haiku-5-5", 100));
    e.app.claude_api.ledger.record(row("openai", "gpt-6-luna", 7));
    e.app.claude_api.ledger.record(row("openai", "gpt-6-luna", 5));
    let fetch = |q: &'static str| {
        let e = &e;
        async move {
            let (s, b) = http(e, "GET", &format!("/api/claude-api/ledger{q}"), None, &user(e)).await;
            (s, serde_json::from_str::<Value>(&b).unwrap_or(Value::Null))
        }
    };
    let (_, all) = fetch("").await;
    assert_eq!((all["totals"]["calls"].clone(), all["rows"].as_array().unwrap().len()), (json!(3), 3));
    let (_, all2) = fetch("?provider=all").await;
    assert_eq!(all2["totals"]["calls"], 3);
    let (_, openai) = fetch("?provider=openai").await;
    assert_eq!((openai["totals"]["calls"].clone(), openai["totals"]["input_tokens"].clone(), openai["rows"].as_array().unwrap().len()), (json!(2), json!(12), 2));
    assert!(openai["rows"].as_array().unwrap().iter().all(|r| r["provider"] == "openai"));
    assert_eq!(openai["by_use"]["chat_translate"]["calls"], 2);
    let (_, claude) = fetch("?provider=anthropic").await;
    assert_eq!((claude["totals"]["calls"].clone(), claude["rows"][0]["model"].clone()), (json!(1), json!("claude-haiku-5-5")));
    let (status, _) = fetch("?provider=bogus").await;
    assert_eq!(status, 400);
}
