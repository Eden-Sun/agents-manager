//! 登入失效的主動提示（daemon 那一半）：回合授權失敗記下、host 快照帶出、各種情況清掉。

use super::*;
use crate::hookrecv::{process, HookBody};
use crate::testing as tt;
use serde_json::{json, Value};

const HOST: &str = "local";

fn identity(name: &str, logged_in: Option<bool>) -> crate::tools::IdentityInfo {
    crate::tools::IdentityInfo {
        name: name.into(),
        kind: "claude".into(),
        logged_in,
        reason: None,
        account: Some("fake@example.test".into()),
        plan: None,
        source: crate::tools::SOURCE_CONFIG,
        config_dir: None,
    }
}

async fn with_identities(app: &Arc<App>, list: &[(&str, Option<bool>)]) {
    app.tools.lock().await.insert(
        HOST.to_string(),
        crate::tools::HostTools {
            tools: Default::default(),
            identities: list.iter().map(|(n, li)| (n.to_string(), identity(n, *li))).collect(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: db::now(),
        },
    );
}

/// 綁著 `identity` 的 claude bot＋一個在跑的 run。
async fn bound_bot(env: &tt::Env, name: &str, identity: &str) -> db::Bot {
    let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
    sqlx::query("UPDATE bots SET identity = ? WHERE id = ?").bind(identity).bind(&bot.id).execute(&env.app.db).await.unwrap();
    tt::fake_run(&env.app, &bot.id).await;
    db::bot(&env.app.db, &bot.id).await.unwrap().unwrap()
}

fn hook(bot: &db::Bot, payload: Value) -> HookBody {
    HookBody { bot_id: bot.id.clone(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }
}

fn auth_failure(bot: &db::Bot, pid: &str) -> HookBody {
    hook(bot, json!({"hook_event_name": "StopFailure", "session_id": "s1", "prompt_id": pid, "reason": "authentication_error: 401 invalid credentials"}))
}

fn stop(bot: &db::Bot, pid: &str, text: &str) -> HookBody {
    hook(bot, json!({"hook_event_name": "Stop", "session_id": "s1", "prompt_id": pid, "last_assistant_message": text}))
}

async fn json_of(app: &Arc<App>, name: &str) -> Value {
    let all = app.tools.lock().await;
    identities_json(app, HOST, &all.get(HOST).unwrap().identities)[name].clone()
}

#[test]
fn the_not_logged_in_line_must_be_the_whole_message() {
    for yes in [
        "Not logged in · Please run /login",
        "Not logged in · Run /login",
        "  ⎿  Not logged in · Please run /login.",
        "not logged in · please run /login",
    ] {
        assert!(is_not_logged_in_line(yes), "{yes:?}");
    }
    for no in [
        "",
        "我看到 Not logged in · Please run /login 這行",
        "Not logged in · Please run /login\n然後呢",
        "Logged in as someone",
        "Not logged in",
    ] {
        assert!(!is_not_logged_in_line(no), "{no:?}");
    }
}

#[tokio::test]
async fn the_snapshot_carries_login_needed_only_for_marked_identities() {
    let env = tt::env().await;
    with_identities(&env.app, &[("cc1", Some(true)), ("cc2", Some(true))]).await;
    assert!(json_of(&env.app, "cc1").await.get("login_needed").is_none(), "沒記：不帶");
    assert!(mark(&env.app, HOST, "cc1", VIA_TURN));
    assert!(!mark(&env.app, HOST, "cc1", VIA_TURN), "已經有：沒變");
    let n = json_of(&env.app, "cc1").await["login_needed"].clone();
    assert_eq!(n["via"], VIA_TURN);
    assert!(n["since"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(json_of(&env.app, "cc2").await.get("login_needed").is_none(), "別的身分不受影響");
    // 別台主機同名的身分不共用。
    assert_eq!(get(&env.app, "m4p", "cc1"), None);
    assert!(clear(&env.app, HOST, "cc1"));
    assert!(!clear(&env.app, HOST, "cc1"));
}

/// `StopFailure` 被分類成授權失敗：記下、推 `host_changed`（帶 `login_needed`），重送同一則不再推。
#[tokio::test]
async fn an_auth_stop_failure_marks_the_identity_and_pushes_the_snapshot() {
    let env = tt::env().await;
    with_identities(&env.app, &[("cc1", Some(true))]).await;
    let bot = bound_bot(&env, "logout-a", "cc1").await;
    let mut rx = env.app.subscribe();

    process(&env.app, &auth_failure(&bot, "p1")).await.unwrap();
    assert!(get(&env.app, HOST, "cc1").is_some());
    let frames: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).filter(|f| f.kind == "host_changed").collect();
    assert_eq!(frames.len(), 1, "變了才推一次");
    assert_eq!(frames[0].data["identities"]["cc1"]["login_needed"]["via"], VIA_TURN);

    let since = get(&env.app, HOST, "cc1").unwrap().since;
    process(&env.app, &auth_failure(&bot, "p1")).await.unwrap();
    assert_eq!(get(&env.app, HOST, "cc1").unwrap().since, since, "同一次登出：since 不變，網頁不會當成新的一次");
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).all(|f| f.kind != "host_changed"), "不重複推");
}

/// 其他種類的失敗、沒綁身分的 bot、不是 claude：都不記。
#[tokio::test]
async fn other_failures_unbound_bots_and_other_kinds_are_ignored() {
    let env = tt::env().await;
    with_identities(&env.app, &[("cc1", Some(true))]).await;
    let bot = bound_bot(&env, "logout-b", "cc1").await;
    process(&env.app, &hook(&bot, json!({"hook_event_name": "StopFailure", "session_id": "s", "prompt_id": "p", "reason": "API Error: 500"}))).await.unwrap();
    process(&env.app, &hook(&bot, json!({"hook_event_name": "StopFailure", "session_id": "s", "prompt_id": "p2", "reason": "rate_limit_error 429"}))).await.unwrap();
    assert_eq!(get(&env.app, HOST, "cc1"), None, "API／額度失敗不是登出");

    let unbound = tt::claude_bot(&env.app, &env.project_id, "logout-unbound").await;
    tt::fake_run(&env.app, &unbound.id).await;
    process(&env.app, &auth_failure(&unbound, "p3")).await.unwrap();
    assert!(env.app.login_needed.lock().unwrap().is_empty(), "沒綁身分：沒有哪個身分可以提示");

    // 身分不在這台主機的表裡（改名、刪了）：不記。
    let ghost = bound_bot(&env, "logout-ghost", "cc-gone").await;
    process(&env.app, &auth_failure(&ghost, "p4")).await.unwrap();
    assert!(env.app.login_needed.lock().unwrap().is_empty());
}

/// 沒登入時回合只回 `Not logged in · Please run /login`（走 `Stop`）也算；一般答覆、引用那句話的回報不算，而且一般答覆會清掉。
#[tokio::test]
async fn a_not_logged_in_reply_marks_and_a_normal_reply_clears() {
    let env = tt::env().await;
    with_identities(&env.app, &[("cc1", Some(true))]).await;
    let bot = bound_bot(&env, "logout-c", "cc1").await;

    process(&env.app, &stop(&bot, "p1", "我讀到的錯誤是 Not logged in · Please run /login，所以先停下來")).await.unwrap();
    assert_eq!(get(&env.app, HOST, "cc1"), None, "引用那句話不算");

    process(&env.app, &stop(&bot, "p2", "Not logged in · Please run /login")).await.unwrap();
    assert!(get(&env.app, HOST, "cc1").is_some());

    let mut rx = env.app.subscribe();
    process(&env.app, &stop(&bot, "p3", "好了，測試都過了")).await.unwrap();
    assert_eq!(get(&env.app, HOST, "cc1"), None, "又能正常答完一回合：通了");
    let pushed: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).filter(|f| f.kind == "host_changed").collect();
    assert_eq!(pushed.len(), 1);
    assert!(pushed[0].data["identities"]["cc1"].get("login_needed").is_none());
}

/// 探測把身分從非已登入變成已登入：提示跟著消失；已經是已登入時重複寫不會誤清（由回合／登入完成負責）。
#[tokio::test]
async fn a_probe_that_flips_the_identity_to_logged_in_clears_the_marker() {
    let env = tt::env().await;
    with_identities(&env.app, &[("cc1", Some(false))]).await;
    let fence = env.app.hosts.fence(HOST).await.unwrap();
    mark(&env.app, HOST, "cc1", VIA_TURN);
    let changed = crate::tools::record_identity_login_fenced(&env.app, HOST, "cc1", &fence, Some(true), Some("a@b.test".into()), None).await;
    assert!(changed);
    assert_eq!(get(&env.app, HOST, "cc1"), None);

    mark(&env.app, HOST, "cc1", VIA_TURN);
    let changed = crate::tools::record_identity_login_fenced(&env.app, HOST, "cc1", &fence, Some(true), None, None).await;
    assert!(!changed, "本來就是已登入、沒有新資訊");
    assert!(get(&env.app, HOST, "cc1").is_some(), "探測一直說已登入：不代表授權失敗好了（過期憑證），不清");
}

/// 改了身分還沒重啟：pane 裡跑的是啟動時的帳號（`runs.runtime_identity`），失敗要記在那一個，不是設定的新帳號上（#238）。
#[tokio::test]
async fn an_auth_failure_is_charged_to_the_identity_the_run_started_with() {
    let env = tt::env().await;
    with_identities(&env.app, &[("cc1", Some(true)), ("cc2", Some(true))]).await;
    let bot = bound_bot(&env, "swap-a", "cc2").await;
    sqlx::query("UPDATE runs SET runtime_identity = 'cc1' WHERE bot_id = ?").bind(&bot.id).execute(&env.app.db).await.unwrap();

    process(&env.app, &auth_failure(&bot, "p1")).await.unwrap();
    assert!(get(&env.app, HOST, "cc1").is_some(), "pane 裡跑的是 cc1，壞掉的是它");
    assert!(get(&env.app, HOST, "cc2").is_none(), "設定的新帳號沒有壞");
}

/// 同一個情況下正常答完一回合：清掉的是啟動時那個帳號的記號，不是設定的新帳號。
#[tokio::test]
async fn a_good_turn_clears_the_marker_of_the_identity_the_run_started_with() {
    let env = tt::env().await;
    with_identities(&env.app, &[("cc1", Some(true)), ("cc2", Some(true))]).await;
    let bot = bound_bot(&env, "swap-b", "cc2").await;
    sqlx::query("UPDATE runs SET runtime_identity = 'cc1' WHERE bot_id = ?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    mark(&env.app, HOST, "cc1", VIA_TURN);

    process(&env.app, &stop(&bot, "p2", "ok")).await.unwrap();
    assert_eq!(get(&env.app, HOST, "cc1"), None, "cc1 自己的提示清掉");
}
