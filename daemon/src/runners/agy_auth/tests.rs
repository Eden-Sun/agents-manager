use super::*;
use crate::hookrecv::{process, HookBody};
use crate::testing as tt;
use serde_json::{json, Value};

const HOST: &str = "local";

async fn seed_agy(app: &Arc<App>) {
    let ht = crate::tools::HostTools {
        tools: [("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/h/.local/bin/agy".into()), version: Some("1.3.0".into()), logged_in: Some(true) })].into(),
        identities: Default::default(),
        shell_identities: vec![],
        utc_offset_secs: None,
        herdr_cli: None,
        checked_at: db::now(),
    };
    app.tools.lock().await.insert(HOST.into(), ht);
}

fn quota() -> crate::quota::Quota {
    crate::quota::Quota {
        five_hour: None,
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: db::now(),
        source: "agy-usage".into(),
        account: None,
        host: HOST.into(),
    }
}

async fn logged_in(app: &Arc<App>) -> Option<bool> {
    app.tools.lock().await.get(HOST).and_then(|h| h.tools.get("agy")).and_then(|t| t.logged_in)
}

/// 在跑的 agy bot（`UPDATE bots SET kind='agy'`，`tt::claude_bot` 只會建 claude）。
async fn agy_bot(env: &tt::Env, name: &str) -> db::Bot {
    let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
    sqlx::query("UPDATE bots SET kind='agy' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    tt::fake_run(&env.app, &bot.id).await;
    db::bot(&env.app.db, &bot.id).await.unwrap().unwrap()
}

fn stop(bot: &db::Bot, error: &str) -> HookBody {
    let payload = json!({"hookEventName": "stop", "conversationId": "c1", "terminationReason": "ERROR", "error": error});
    HookBody { bot_id: bot.id.clone(), provider: "agy".into(), payload, received_at: None, truncated: false, run_id: None }
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> Vec<crate::state::WsEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn clear_denied() {
    auth_denied().lock().unwrap().remove(&crate::quota::quota_key(HOST, "agy"));
}

#[tokio::test]
async fn an_agy_auth_stop_failure_marks_agy_logged_out_and_pushes_host_changed() {
    clear_denied();
    let env = tt::env().await;
    seed_agy(&env.app).await;
    env.app.quotas.lock().await.insert("agy".into(), quota());
    assert!(env.app.quotas.lock().await.contains_key("agy"));
    let bot = agy_bot(&env, "agy-auth-a").await;
    let mut rx = env.app.subscribe();

    process(&env.app, &stop(&bot, "401 UNAUTHENTICATED: invalid credentials")).await.unwrap();

    assert_eq!(logged_in(&env.app).await, Some(false));
    let frames = drain(&mut rx);
    let hc: Vec<_> = frames.iter().filter(|f| f.kind == "host_changed").collect();
    assert_eq!(hc.len(), 1, "變了才推一次");
    assert_eq!(hc[0].data["tools"]["agy"]["logged_in"], Value::Bool(false));
    let q: Vec<_> = frames.iter().filter(|f| f.kind == "quota_updated").collect();
    assert!(q.iter().any(|f| f.data["quota"].is_null()), "舊讀數要清掉並廣播");
    assert!(!env.app.quotas.lock().await.contains_key("agy"));
    clear_denied();
}

#[tokio::test]
async fn an_agy_rate_limit_or_api_error_does_not_mark_logged_out() {
    clear_denied();
    let env = tt::env().await;
    seed_agy(&env.app).await;
    let bot = agy_bot(&env, "agy-auth-b").await;
    for err in ["429 RESOURCE_EXHAUSTED", "500 internal", "network error: connection reset", "request timeout"] {
        process(&env.app, &stop(&bot, err)).await.unwrap();
        assert_eq!(logged_in(&env.app).await, Some(true), "{err}");
    }
    clear_denied();
}

#[tokio::test]
async fn a_repeated_agy_auth_failure_pushes_once() {
    clear_denied();
    let env = tt::env().await;
    seed_agy(&env.app).await;
    let bot = agy_bot(&env, "agy-auth-c").await;
    let mut rx = env.app.subscribe();
    process(&env.app, &stop(&bot, "401 UNAUTHENTICATED")).await.unwrap();
    assert_eq!(drain(&mut rx).iter().filter(|f| f.kind == "host_changed").count(), 1);
    process(&env.app, &stop(&bot, "401 UNAUTHENTICATED")).await.unwrap();
    assert!(drain(&mut rx).iter().all(|f| f.kind != "host_changed"), "已是未登入：不重複推");
    clear_denied();
}

/// 憑證檔還在（只是被撤銷）：冷卻期內登入偵測不把旗標翻回去。
#[tokio::test]
async fn the_watcher_does_not_flip_back_during_the_auth_denied_cooldown() {
    let _lock = crate::quota_agy::token_test_lock().await;
    clear_denied();
    let env = tt::env().await;
    seed_agy(&env.app).await;
    let bot = agy_bot(&env, "agy-auth-d").await;
    process(&env.app, &stop(&bot, "401 UNAUTHENTICATED")).await.unwrap();
    assert_eq!(logged_in(&env.app).await, Some(false));

    let token = tt::fake_home().join(crate::quota_agy::TOKEN_FILE);
    std::fs::create_dir_all(token.parent().unwrap()).unwrap();
    std::fs::write(&token, "revoked").unwrap();
    crate::runners::quota_agy::login_watch_once(&env.app, HOST).await;
    let _ = std::fs::remove_file(&token);
    assert_eq!(logged_in(&env.app).await, Some(false), "冷卻期內不翻回已登入");
    clear_denied();
}

#[tokio::test]
async fn an_unadmitted_agy_hook_does_not_change_login_state() {
    clear_denied();
    let env = tt::env().await;
    seed_agy(&env.app).await;
    // 有 bot 但沒有 run：沒有放行證明。
    let bot = tt::claude_bot(&env.app, &env.project_id, "agy-auth-e").await;
    sqlx::query("UPDATE bots SET kind='agy' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
    let _ = process(&env.app, &stop(&bot, "401 UNAUTHENTICATED")).await;
    assert_eq!(logged_in(&env.app).await, Some(true));
    clear_denied();
}
