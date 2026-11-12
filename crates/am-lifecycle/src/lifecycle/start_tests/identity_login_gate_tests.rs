use super::*;
use crate::capabilities::Db;
use crate::config::IdentityCfg;
use crate::testing as tt;
use crate::tools::{HostTools, IdentityInfo, ToolInfo, SOURCE_CONFIG};

/// Registers `name` as a known local `claude` identity whose cache says logged out, and points
/// its CLI at a fake script (never the real `claude`) that answers `{"loggedIn": fresh_logged_in}`
/// when `start_bot` rechecks — so the test never depends on what is actually installed or logged
/// in on the machine running it.
async fn identity_with_recheck_answer(app: &Arc<App>, dir: &std::path::Path, name: &str, fresh_logged_in: bool) {
    let config_dir = dir.join(format!("{name}-config"));
    app.cfg
        .update(|c| {
            c.identities.push(IdentityCfg {
                name: name.into(),
                kind: "claude".into(),
                host: None,
                env: [("CLAUDE_CONFIG_DIR".to_string(), config_dir.to_string_lossy().to_string())].into(),
                args: vec![],
            });
            Ok(())
        })
        .await
        .unwrap();
    let script = dir.join(format!("fake-cli-{name}.sh"));
    crate::testing::write_exec(&script, format!("#!/bin/sh\nprintf '{{\"loggedIn\": {fresh_logged_in}}}'\n"));
    app.tools.lock().await.insert(
        crate::config::LOCAL_HOST.to_string(),
        HostTools {
            tools: [("claude".to_string(), ToolInfo { installed: true, path: Some(script.to_string_lossy().to_string()), version: None, logged_in: None })].into(),
            identities: [(
                name.to_string(),
                IdentityInfo {
                    name: name.to_string(),
                    kind: "claude".to_string(),
                    logged_in: Some(false),
                    reason: None,
                    account: None,
                    plan: None,
                    source: SOURCE_CONFIG,
                    config_dir: None,
                },
            )]
            .into(),
            shell_identities: vec![],
            utc_offset_secs: None, herdr_cli: None, checked_at: db::now(),
        },
    );
}

fn conflict(e: LcError) -> Value {
    match e {
        LcError::Conflict(v) => v,
        other => panic!("expected a 409 conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn an_explicit_logged_out_identity_fails_closed_instead_of_falling_back() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "cc-lock").await;
    sqlx::query("UPDATE bots SET identity='cc-lock' WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    identity_with_recheck_answer(&e.app, &e.dir, "cc-lock", false).await;

    let err = start_bot(&e.app, &bot.id).await.expect_err("身份確認未登入：不能啟動");
    let body = conflict(err);
    assert_eq!(body["reason"], "identity_not_logged_in");
    assert_eq!(body["identity"], "cc-lock");
    assert_eq!(body["host"], "local");

    assert!(db::active_run(e.app.db(), &bot.id).await.unwrap().is_none(), "沒有 run 被建起來");
    assert!(e.herdr.methods().is_empty(), "沒有碰 herdr，CLI 沒被啟動：{:?}", e.herdr.methods());

    let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
    let note: String = sqlx::query_scalar(
        "SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&conv)
    .fetch_one(e.app.db())
    .await
    .unwrap();
    assert!(note.contains("cc-lock") && note.contains("沒有登入"), "{note}");
}

/// 快取覺得沒登入，但 CLI 重驗說有登入（例如剛登入、30 分鐘的快取還沒更新）：照樣啟動，不擋。
#[tokio::test]
async fn a_stale_logged_out_cache_does_not_block_a_start_the_cli_confirms() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "cc-fresh").await;
    sqlx::query("UPDATE bots SET identity='cc-fresh' WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    identity_with_recheck_answer(&e.app, &e.dir, "cc-fresh", true).await;

    start_bot(&e.app, &bot.id).await.expect("CLI 重驗說有登入：照常啟動");
    assert!(db::active_run(e.app.db(), &bot.id).await.unwrap().is_some());
}
