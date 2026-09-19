//! #187：`purge_deleted_bot_dirs` 只在「確定是軟刪」而且「確定沒有 active run」時才有權刪 `bots/<id>/`。
//! 讀不到（SQLite 一時忙、I/O 錯）不等於沒有：留著、下一次啟動再判斷。
use super::*;
use crate::testing as tt;

/// 一顆軟刪的 bot 與它的目錄（裡面放著 hook 設定，刪掉就補不回來）。
async fn deleted_bot(env: &tt::Env, name: &str) -> (db::Bot, std::path::PathBuf) {
    let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&bot.id).execute(&env.app.db).await.unwrap();
    let dir = env.app.data_dir.join("bots").join(&bot.id);
    std::fs::create_dir_all(dir.join("hooks")).unwrap();
    std::fs::write(dir.join("hooks/settings.json"), "{}").unwrap();
    (bot, dir)
}

/// 驗收一、三：軟刪的 bot 還有 active run，`active_run` 讀不到 → 目錄必須留著；DB 好了但 run 還在 → 仍留著；
/// run 終止之後才刪。
#[tokio::test]
async fn a_live_run_whose_state_cannot_be_read_keeps_its_dir() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot, dir) = deleted_bot(&env, "alfa").await;
    let run = tt::fake_run(&app, &bot.id).await;

    tt::make_table_unreadable(&app, "runs").await;
    assert!(db::active_run(&app.db, &bot.id).await.is_err(), "前提：active_run 真的讀不到");
    assert_eq!(purge_deleted_bot_dirs(&app).await, 0);
    assert!(dir.join("hooks/settings.json").exists(), "讀不到 run 的狀態就不能刪它的目錄");

    tt::make_table_readable(&app, "runs").await;
    assert_eq!(purge_deleted_bot_dirs(&app).await, 0, "讀得到了：run 還活著，仍不刪");
    assert!(dir.exists());

    sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE id=?").bind(db::now()).bind(&run).execute(&app.db).await.unwrap();
    assert_eq!(purge_deleted_bot_dirs(&app).await, 1, "run 確定終止了才刪");
    assert!(!dir.exists());
}

/// 驗收二、四：確定沒有 run 的軟刪 bot 照常刪；活著的 bot、沒有任何 bot 認領的目錄一律不碰。
#[tokio::test]
async fn only_a_confirmed_deleted_bot_without_a_run_is_purged() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (_, gone) = deleted_bot(&env, "alfa").await;
    let live = tt::claude_bot(&app, &env.project_id, "bravo").await;
    let live_dir = app.data_dir.join("bots").join(&live.id);
    std::fs::create_dir_all(&live_dir).unwrap();
    let orphan = app.data_dir.join("bots").join("nobody-claims-me");
    std::fs::create_dir_all(&orphan).unwrap();

    assert_eq!(purge_deleted_bot_dirs(&app).await, 1);
    assert!(!gone.exists());
    assert!(live_dir.exists(), "沒刪的 bot 不動");
    assert!(orphan.exists(), "沒有 bot 認領的目錄不動");
}

/// 連「這顆是不是軟刪」都讀不到時，也一樣一個目錄都不刪（以前就是 fail closed，只是完全靜默）。
#[tokio::test]
async fn nothing_is_purged_while_the_bot_rows_cannot_be_read() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (_, dir) = deleted_bot(&env, "alfa").await;

    tt::make_table_unreadable(&app, "bots").await;
    assert_eq!(purge_deleted_bot_dirs(&app).await, 0);
    assert!(dir.exists());

    tt::make_table_readable(&app, "bots").await;
    assert_eq!(purge_deleted_bot_dirs(&app).await, 1, "讀得到了，沒有 run 的軟刪 bot 才刪");
}
