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
        argon_weekly: None,
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

    crate::runners::quota_agy::record_probe_result(&env.app, HOST, &new, &Err(ProbeFail::AuthRequired)).await;
    assert_eq!(logged_in(&env.app).await, Some(false));
    assert!(auth_denied_active(&key, &new));
}

/// #1024：這一則是在換代之前放行的（捕獲的是舊世代）：它的授權失敗不能把新世代標成未登入、不能清它的讀數。
#[tokio::test]
async fn an_auth_failure_admitted_under_a_replaced_generation_changes_nothing() {
    let _globals = agy_globals().await;
    let env = tt::env().await;
    seed_agy(&env.app).await;
    env.app.quotas.lock().await.insert("agy".into(), quota());
    let bot = agy_bot(&env, "agy-auth-old-gen").await;
    let old = env.app.hosts.fence(HOST).await.unwrap();
    env.app.hosts.get(HOST).await.unwrap().bump_generation_for_test();

    on_turn_auth_failure(&env.app, &bot, &old).await;

    assert_eq!(logged_in(&env.app).await, Some(true), "舊世代的授權失敗不翻新世代的旗標");
    assert!(env.app.quotas.lock().await.contains_key("agy"), "新世代的讀數不清");
    let new = env.app.hosts.fence(HOST).await.unwrap();
    assert!(!crate::runners::quota_agy::auth_denied_active(&crate::quota::quota_key(HOST, "agy"), &new), "不記新世代的冷卻");
}

/// #1024：同一代的授權失敗照常翻成未登入（上一個測試的對照組）。
#[tokio::test]
async fn an_auth_failure_under_the_current_generation_still_marks_logged_out() {
    let _globals = agy_globals().await;
    let env = tt::env().await;
    seed_agy(&env.app).await;
    let bot = agy_bot(&env, "agy-auth-cur-gen").await;
    let fence = env.app.hosts.fence(HOST).await.unwrap();

    on_turn_auth_failure(&env.app, &bot, &fence).await;

    assert_eq!(logged_in(&env.app).await, Some(false));
}

/// #2237：綁 ag1（自己的 `HOME`）的 bot 授權失敗：只標 ag1 未登入、清 ag1 那一格額度；主機預設帳號與裸 `agy` 額度不動。
/// 綁 ag0（不換 `HOME`＝同一個帳號）照舊標預設帳號。
#[tokio::test]
async fn an_auth_failure_marks_the_bots_own_agy_identity_not_the_default_account() {
    let _globals = agy_globals().await;
    let env = tt::env().await;
    seed_agy(&env.app).await;
    let ag1 = crate::config::IdentityCfg { name: "ag1".into(), kind: "agy".into(), host: Some(HOST.into()), env: [("HOME".into(), "/tmp/agy-ag1".into())].into(), args: vec![] };
    env.app.cfg.update(move |cfg| { cfg.identities.push(ag1); Ok(()) }).await.unwrap();
    let mut info = crate::tools::IdentityInfo::unknown("ag1", "agy", crate::tools::SOURCE_CONFIG, None);
    info.logged_in = Some(true);
    env.app.tools.lock().await.get_mut(HOST).unwrap().identities.insert("ag1".into(), info);
    let ag1_key = crate::quota::quota_key(HOST, &crate::quota::quota_base_default_aware("agy", Some("ag1"), false));
    assert_ne!(ag1_key, crate::quota::quota_key(HOST, "agy"));
    env.app.quotas.lock().await.insert("agy".into(), quota());
    env.app.quotas.lock().await.insert(ag1_key.clone(), quota());

    let bot = agy_bot(&env, "agy-auth-ag1").await;
    sqlx::query("UPDATE bots SET identity='ag1' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE runs SET runtime_identity='ag1' WHERE bot_id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    process(&env.app, &stop(&bot, "401 UNAUTHENTICATED: invalid credentials")).await.unwrap();

    assert_eq!(logged_in(&env.app).await, Some(true), "預設帳號沒事");
    assert!(env.app.quotas.lock().await.contains_key("agy"), "預設帳號的額度留著");
    assert!(!crate::runners::quota_agy::auth_denied().lock().unwrap().contains_key(&crate::quota::quota_key(HOST, "agy")), "預設帳號不進探測冷卻");
    let ag1_in = env.app.tools.lock().await.get(HOST).and_then(|h| h.identities.get("ag1")).and_then(|i| i.logged_in);
    assert_eq!(ag1_in, Some(false), "真正失效的 ag1 標成未登入");
    assert!(!env.app.quotas.lock().await.contains_key(&ag1_key), "ag1 那一格清掉");

    let bot0 = agy_bot(&env, "agy-auth-ag0").await;
    sqlx::query("UPDATE bots SET identity='ag0' WHERE id=?").bind(&bot0.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE runs SET runtime_identity='ag0' WHERE bot_id=?").bind(&bot0.id).execute(&env.app.db).await.unwrap();
    process(&env.app, &stop(&bot0, "401 UNAUTHENTICATED: invalid credentials")).await.unwrap();
    assert_eq!(logged_in(&env.app).await, Some(false), "ag0 就是預設帳號");
}
