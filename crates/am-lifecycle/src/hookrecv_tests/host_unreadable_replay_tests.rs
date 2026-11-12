use super::*;
use crate::testing as tt;

async fn spooled(env: &tt::Env, name: &str) -> (db::Bot, std::path::PathBuf) {
    let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
    let dir = env.app.bot_dir(&bot.id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let line = serde_json::json!({"bot_id": bot.id, "provider": "claude", "payload": {"hook_event_name": "Stop", "session_id": "s1"}});
    let spool = dir.join("hook-spool.jsonl");
    std::fs::write(&spool, format!("{line}\n")).unwrap();
    (bot, spool)
}

async fn inbox_rows(env: &tt::Env) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM hook_events").fetch_one(&env.app.db).await.unwrap()
}

/// #302：上一輪崩在收一半，留下 `.replaying`、沒有新的 spool——它是唯一的副本，這一輪要收進來。
#[tokio::test]
async fn a_leftover_replaying_file_with_no_new_spool_is_still_replayed() {
    let env = tt::env().await;
    let (bot, spool) = spooled(&env, "alfa").await;
    let staging = spool.with_extension("jsonl.replaying");
    std::fs::rename(&spool, &staging).unwrap();
    assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 1, "只剩 .replaying 也要收");
    assert!(!staging.exists());
    assert_eq!(inbox_rows(&env).await, 1);
}

/// 合併走位元組：`.replaying` 尾巴是崩掉時寫到一半的多位元組字元，舊事件不能因此被新的 spool 蓋掉。
#[tokio::test]
async fn a_leftover_replaying_with_a_torn_utf8_tail_is_merged_not_overwritten() {
    let env = tt::env().await;
    let (bot, spool) = spooled(&env, "alfa").await;
    let staging = spool.with_extension("jsonl.replaying");
    let mut old = std::fs::read(&spool).unwrap();
    old.extend_from_slice(&"{\"bot_id\":\"x\",\"note\":\"中".as_bytes()[..24]);
    std::fs::write(&staging, old).unwrap();
    let line = serde_json::json!({"bot_id": bot.id, "provider": "claude", "payload": {"hook_event_name": "Stop", "session_id": "s2"}});
    std::fs::write(&spool, format!("{line}\n")).unwrap();
    assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 2, "舊的一則加新的一則都要收");
    assert_eq!(inbox_rows(&env).await, 2);
}

/// 每顆 bot 的 host 讀不到：修前退回 local，把（遠端 bot 的）本機 spool 當成它的收下；修後回錯、檔案原封不動。
#[tokio::test]
async fn a_bot_whose_host_cannot_be_read_is_not_replayed_as_local() {
    let env = tt::env().await;
    let (bot, spool) = spooled(&env, "alfa").await;
    tt::make_table_unreadable(&env.app, "projects").await;
    let r = replay_spool(&env.app, &bot.id).await;
    tt::make_table_readable(&env.app, "projects").await;
    assert!(r.is_err(), "host 讀不到要回錯：{r:?}");
    assert!(spool.exists(), "spool 不能被當成本機的吃掉");
    assert_eq!(inbox_rows(&env).await, 0);
    assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 1, "DB 好了補回來");
}

/// 列舉那一層：`live_bots_on_host` 讀不到，以前變成「沒有 bot」；現在背景重試，DB 好了不需要任何新事件就補齊。
#[tokio::test]
async fn a_host_pass_that_cannot_enumerate_bots_retries_until_the_spool_is_drained() {
    let env = tt::env().await;
    let (bot, spool) = spooled(&env, "alfa").await;
    let claim = spool.with_extension("jsonl.claim");
    let staging = spool.with_extension("jsonl.replaying");
    // 背景重試把 spool 摘成 `.claim` 之後、併進 `.replaying` 之前停住：這一瞬 spool 與 `.replaying` 都不在，只剩 `.claim`。
    let (paused_tx, paused_rx) = tokio::sync::oneshot::channel::<()>();
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    crate::lifecycle::race_point::arm("spool_claimed", &bot.id, move || async move {
        let _ = paused_tx.send(());
        let _ = go_rx.await;
    });
    tt::make_table_unreadable(&env.app, "projects").await;
    replay_host(&env.app, crate::config::LOCAL_HOST).await;
    assert!(spool.exists(), "讀不到時什麼都不能動");
    tt::make_table_readable(&env.app, "projects").await;
    paused_rx.await.unwrap();
    assert!(claim.exists() && !spool.exists() && !staging.exists(), "停在認領與併入之間");
    go_tx.send(()).unwrap();
    // 等的是背景重試這個任務結束，不是檔案消失的那一瞬（#255、#1662）：重試任務是 tracked 的，
    // 關掉 tracker 後 `wait` 只在它收完（或放棄）時才回來；它先認領、後 commit、最後才刪 `.replaying`，
    // 所以結束時三個檔都不在、列數就是已收的那批。逾時只防卡死，不是等待的條件。
    env.app.background_tasks.close();
    tokio::time::timeout(std::time::Duration::from_secs(60), env.app.background_tasks.wait())
        .await
        .expect("背景重試任務應該在 DB 恢復後結束");
    assert!(!spool.exists(), "DB 恢復後背景重試要把 spool 收進來");
    assert!(!claim.exists() && !staging.exists(), "收完才刪 .claim／.replaying：{staging:?}");
    assert_eq!(inbox_rows(&env).await, 1, "恰好收一次");
}
