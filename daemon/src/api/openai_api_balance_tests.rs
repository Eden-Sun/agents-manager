//! OpenAI API 全域餘額（`GET /api/openai-api/balance`、`PUT /api/openai-api/balance-anchor`）：
//! 錨點之後才算、只加 openai 的花費（API 帳本 + spend.jsonl）、Claude 的不算、沒估價的進 uncertain、
//! 未啟用／沒錨點的回應、PUT 驗證與寫回 config、bot 呼叫 403。
use super::*;
use crate::openai_api_balance::{get_openai_api_balance, put_openai_api_balance_anchor, PutBalanceAnchorRequest};
use crate::testing::{env, Env};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ANCHOR_AT: &str = "2026-11-10T00:00:00Z";

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

async fn enable(e: &Env) {
    e.app.cfg.update(|cfg| {
        cfg.openai_api.enabled = true;
        Ok(())
    }).await.unwrap();
}

async fn attempt(e: &Env, bot: &str, provider: &str, state: &str, estimated: Option<i64>, reserved: i64, created_at: &str) {
    sqlx::query("INSERT INTO api_bot_attempts (id, bot_id, generation, ordinal, request_hash, requested_speed, state, estimated_microusd, reserved_microusd, pricing_version, provider, created_at) VALUES (?, ?, 1, 0, 'h', 'standard', ?, ?, ?, 'v1', ?, ?)")
        .bind(db::ulid()).bind(bot).bind(state).bind(estimated).bind(reserved).bind(provider).bind(created_at)
        .execute(&e.app.db).await.unwrap();
}

/// 直接寫 spend.jsonl（同一份檔，provider 欄決定歸屬）。
fn spend_file(e: &Env, lines: &[(&str, &str, i64)]) {
    let dir = e.app.data_dir.join("claude-api");
    std::fs::create_dir_all(&dir).unwrap();
    let body: String = lines
        .iter()
        .map(|(at, provider, microusd)| format!("{}\n", json!({"at": at, "provider": provider, "microusd": microusd, "use_case": "test", "model": "m"})))
        .collect();
    std::fs::write(dir.join("spend.jsonl"), body).unwrap();
}

async fn anchor(e: &Env, usd: f64, at: Option<&str>) -> Result<Value, LcError> {
    put_openai_api_balance_anchor(&e.app, PutBalanceAnchorRequest { usd, at: at.map(str::to_string) }).await
}

#[tokio::test]
async fn disabled_or_unanchored_reports_configured_false_with_the_enabled_flag() {
    let e = env().await;
    let off = get_openai_api_balance(&e.app).await.unwrap();
    assert_eq!((off["enabled"].clone(), off["configured"].clone()), (json!(false), json!(false)), "預設未啟用");
    enable(&e).await;
    let unset = get_openai_api_balance(&e.app).await.unwrap();
    assert_eq!((unset["enabled"].clone(), unset["configured"].clone()), (json!(true), json!(false)), "啟用但沒錨點");
    assert!(unset.get("remaining_usd").is_none());
}

#[tokio::test]
async fn spend_counts_only_openai_after_the_anchor_and_puts_unpriced_unknowns_in_uncertain() {
    let e = env().await;
    enable(&e).await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "oa-balance").await.id;
    anchor(&e, 20.0, Some(ANCHOR_AT)).await.unwrap();

    attempt(&e, &bot, "openai", "recorded", Some(9_000_000), 9_000_000, "2026-11-02T00:00:00Z").await;
    attempt(&e, &bot, "openai", "recorded", Some(1_500_000), 1_500_000, "2026-11-12T01:00:00Z").await;
    attempt(&e, &bot, "openai", "unknown", None, 2_000_000, "2026-11-12T02:00:00Z").await;
    attempt(&e, &bot, "anthropic", "recorded", Some(5_000_000), 5_000_000, "2026-11-12T03:00:00Z").await;
    spend_file(&e, &[
        ("2026-11-09T12:00:00Z", "openai", 8_000_000),
        ("2026-11-12T04:00:00Z", "openai", 500_000),
        ("2026-11-12T05:00:00Z", "anthropic", 7_000_000),
    ]);

    let v = get_openai_api_balance(&e.app).await.unwrap();
    assert_eq!(v["configured"], json!(true), "{v}");
    assert_eq!(v["anchor"]["usd"], json!(20.0));
    assert!((v["spent_usd"].as_f64().unwrap() - 2.0).abs() < 1e-9, "{v}");
    assert!((v["remaining_usd"].as_f64().unwrap() - 18.0).abs() < 1e-9, "{v}");
    assert!((v["uncertain_usd"].as_f64().unwrap() - 2.0).abs() < 1e-9, "{v}");
    assert!((v["remaining_floor_usd"].as_f64().unwrap() - 16.0).abs() < 1e-9, "{v}");
}

#[tokio::test]
async fn an_unparsable_stored_anchor_time_is_reported_not_treated_as_zero_spend() {
    let e = env().await;
    enable(&e).await;
    e.app.cfg.update(|cfg| {
        cfg.openai_api.balance_anchor_usd = Some(5.0);
        cfg.openai_api.balance_anchor_at = Some("not-a-time".into());
        Ok(())
    }).await.unwrap();
    let v = get_openai_api_balance(&e.app).await.unwrap();
    assert_eq!(v["configured"], json!(false));
    assert!(v["error"].as_str().is_some_and(|m| m.contains("balance_anchor_at")), "{v}");
}

#[tokio::test]
async fn put_rejects_negative_nan_and_bad_timestamps_and_writes_valid_ones_to_config() {
    let e = env().await;
    for usd in [-1.0, f64::NAN, f64::INFINITY] {
        let err = anchor(&e, usd, None).await.unwrap_err();
        assert!(matches!(&err, LcError::BadValue(v) if v["reason"] == "invalid_usd"), "{usd}: {err:?}");
    }
    let err = anchor(&e, 1.0, Some("2026-11-10 00:00")).await.unwrap_err();
    assert!(matches!(&err, LcError::BadValue(v) if v["error"] == "bad_request"), "{err:?}");
    assert!(e.app.cfg.get().await.openai_api.balance_anchor_usd.is_none(), "拒絕的請求不寫 config");

    let ok = anchor(&e, 12.5, Some(ANCHOR_AT)).await.unwrap();
    assert_eq!((ok["ok"].clone(), ok["anchor"]["usd"].clone(), ok["anchor"]["at"].clone()), (json!(true), json!(12.5), json!(ANCHOR_AT)));
    let cfg = e.app.cfg.get().await;
    assert_eq!((cfg.openai_api.balance_anchor_usd, cfg.openai_api.balance_anchor_at.clone()), (Some(12.5), Some(ANCHOR_AT.to_string())));
    let now = anchor(&e, 3.0, None).await.unwrap();
    assert!(now["anchor"]["at"].as_str().is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok()), "{now}");
}

#[tokio::test]
async fn http_routes_answer_and_a_bot_gets_403_user_only_and_changes_nothing() {
    let e = env().await;
    enable(&e).await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "oa-balance-bot").await;
    sqlx::query("UPDATE bots SET hook_token=? WHERE id=?").bind("tok-own").bind(&bot.id).execute(&e.app.db).await.unwrap();
    let as_bot = vec![("X-AM-Bot-Id", bot.id.clone()), ("X-AM-Bot-Token", "tok-own".to_string())];
    let as_user = vec![("X-AM-Token", e.app.ui_token.clone())];

    for (method, path, body) in [
        ("GET", "/api/openai-api/balance", None),
        ("PUT", "/api/openai-api/balance-anchor", Some(json!({"usd": 99.0}))),
    ] {
        let (status, text) = http(&e, method, path, body.clone(), &as_bot).await;
        assert_eq!(status, 403, "{method} {path}: {text}");
        assert!(text.contains("user_only"), "{text}");
    }
    assert!(e.app.cfg.get().await.openai_api.balance_anchor_usd.is_none(), "bot 的 PUT 什麼都沒寫");

    let (status, text) = http(&e, "PUT", "/api/openai-api/balance-anchor", Some(json!({"usd": 7.25, "at": ANCHOR_AT})), &as_user).await;
    assert_eq!(status, 200, "{text}");
    let (status, text) = http(&e, "GET", "/api/openai-api/balance", None, &as_user).await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!((v["enabled"].clone(), v["configured"].clone(), v["anchor"]["usd"].clone()), (json!(true), json!(true), json!(7.25)));
}
