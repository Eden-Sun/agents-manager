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
    crate::runners::quota_agy::auth_denied().lock().unwrap().remove(&crate::quota::quota_key(HOST, "agy"));
}

/// 這個檔的測試都碰同一份行程全域狀態：`auth_denied()` 的 `agy` 冷卻（key 是本機）、`probe_errors()` 的 `local`，
/// 以及 `tools.agy.logged_in` 與假 HOME 裡的憑證檔（`quota_agy` 的登入／登出測試同樣用這些）。平行跑時任何一條
/// 清掉或寫入冷卻，別條的斷言就看到別人的狀態（冷卻測試被另一條的 `clear_denied()` 清掉後，watcher 把旗標翻回已登入）。
/// 所以每個測試開頭先拿 `token_test_lock`（與 `quota_agy`／`api` 那幾條共用同一把），進場與離場（含 panic）都把冷卻清乾淨。
struct AgyGlobals {
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for AgyGlobals {
    fn drop(&mut self) {
        clear_denied();
        crate::quota_agy::set_probe_error(HOST, None);
        let _ = std::fs::remove_file(tt::fake_home().join(crate::quota_agy::TOKEN_FILE));
    }
}

async fn agy_globals() -> AgyGlobals {
    let lock = crate::quota_agy::token_test_lock().await;
    clear_denied();
    crate::quota_agy::set_probe_error(HOST, None);
    AgyGlobals { _lock: lock }
}

#[tokio::test]
async fn an_agy_auth_stop_failure_marks_agy_logged_out_and_pushes_host_changed() {
    let _globals = agy_globals().await;
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
}

#[tokio::test]
async fn an_agy_rate_limit_or_api_error_does_not_mark_logged_out() {
    let _globals = agy_globals().await;
    let env = tt::env().await;
    seed_agy(&env.app).await;
    let bot = agy_bot(&env, "agy-auth-b").await;
    for err in ["429 RESOURCE_EXHAUSTED", "500 internal", "network error: connection reset", "request timeout"] {
        process(&env.app, &stop(&bot, err)).await.unwrap();
        assert_eq!(logged_in(&env.app).await, Some(true), "{err}");
    }
}

#[tokio::test]
async fn a_repeated_agy_auth_failure_pushes_once() {
    let _globals = agy_globals().await;
    let env = tt::env().await;
    seed_agy(&env.app).await;
    let bot = agy_bot(&env, "agy-auth-c").await;
    let mut rx = env.app.subscribe();
    process(&env.app, &stop(&bot, "401 UNAUTHENTICATED")).await.unwrap();
    assert_eq!(drain(&mut rx).iter().filter(|f| f.kind == "host_changed").count(), 1);
    process(&env.app, &stop(&bot, "401 UNAUTHENTICATED")).await.unwrap();
    assert!(drain(&mut rx).iter().all(|f| f.kind != "host_changed"), "已是未登入：不重複推");
}

/// 憑證檔還在（只是被撤銷）：冷卻期內登入偵測不把旗標翻回去。
#[tokio::test]
async fn the_watcher_does_not_flip_back_during_the_auth_denied_cooldown() {
    let _globals = agy_globals().await;
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
}

#[tokio::test]
async fn an_unadmitted_agy_hook_does_not_change_login_state() {
    let _globals = agy_globals().await;
    let env = tt::env().await;
    seed_agy(&env.app).await;
    // 有 bot 但沒有 run：沒有放行證明。
    let bot = tt::claude_bot(&env.app, &env.project_id, "agy-auth-e").await;
    sqlx::query("UPDATE bots SET kind='agy' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
    let _ = process(&env.app, &stop(&bot, "401 UNAUTHENTICATED")).await;
    assert_eq!(logged_in(&env.app).await, Some(true));
}

/// #1023：探測途中換了世代（重連、改指）：舊世代的 AuthRequired 不能寫冷卻、不能翻新世代的旗標、不能留探測錯誤。
#[tokio::test]
async fn a_probe_result_from_a_replaced_generation_writes_nothing() {
    use crate::quota_agy::ProbeFail;
    use crate::runners::quota_agy::auth_denied_active;
    let _globals = agy_globals().await;
    let env = tt::env().await;
    seed_agy(&env.app).await;
    env.app.quotas.lock().await.insert("agy".into(), quota());
    let key = crate::quota::quota_key(HOST, "agy");
    let old = env.app.hosts.fence(HOST).await.unwrap();
    env.app.hosts.get(HOST).await.unwrap().bump_generation_for_test();
    let new = env.app.hosts.fence(HOST).await.unwrap();
    let mut rx = env.app.subscribe();

    crate::runners::quota_agy::record_probe_result(&env.app, HOST, &old, &Err(ProbeFail::AuthRequired)).await;
    assert_eq!(logged_in(&env.app).await, Some(true), "舊世代的未登入不翻新世代的旗標");
    assert!(!auth_denied_active(&key, &new), "舊世代不寫冷卻");
    crate::runners::quota_agy::record_probe_result(&env.app, HOST, &old, &Err(ProbeFail::other("timeout", "old".to_string()))).await;
    assert!(!crate::quota_agy::set_probe_error(HOST, None), "舊世代的失敗不留在新主機上（沒有東西可清）");
    assert!(drain(&mut rx).iter().all(|f| f.kind != "host_changed"), "什麼都沒寫就不推");
}

/// #1023：新世代自己的冷卻不能被舊世代的成功清掉。
#[tokio::test]
async fn a_replaced_generations_success_does_not_clear_the_new_cooldown() {
    use crate::quota_agy::ProbeFail;
    use crate::runners::quota_agy::auth_denied_active;
    let _globals = agy_globals().await;
    let env = tt::env().await;
    seed_agy(&env.app).await;
    let key = crate::quota::quota_key(HOST, "agy");
    let old = env.app.hosts.fence(HOST).await.unwrap();
    env.app.hosts.get(HOST).await.unwrap().bump_generation_for_test();
    let new = env.app.hosts.fence(HOST).await.unwrap();

    crate::runners::quota_agy::note_auth_denied(key.clone(), &new);
    crate::runners::quota_agy::record_probe_result(&env.app, HOST, &old, &Ok(())).await;
    assert!(auth_denied_active(&key, &new), "舊世代的成功不清新世代的冷卻");

    // 新世代自己的結果照常生效：AuthRequired 翻未登入、記冷卻。
    crate::runners::quota_agy::record_probe_result(&env.app, HOST, &new, &Err(ProbeFail::AuthRequired)).await;
    assert_eq!(logged_in(&env.app).await, Some(false));
    assert!(auth_denied_active(&key, &new));
}
