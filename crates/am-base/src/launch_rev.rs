//! 啟動版本（#355 機制 B／#353）：`needs_restart`（「載入的 ≠ 設定的」）是**衍生事實**，該從資料算，不是只放在 PATCH 的 HTTP 回應裡
//! （回應掉了、daemon 在 config commit 後死掉，那個 `true` 就永遠沒人知道，執行中的 CLI 還拿著舊的 argv／env／persona／settings）。
//!
//! `of(bot)`＝啟動相關設定正規化後的雜湊；run 啟動時（`start_bot_locked_with`）把當時的版本記在 `runs.launch_rev`；
//! 之後 config 改了，bot 目前的版本跟 active run 記的不同＝過期＝要重啟。`runs.launch_rev` 是 NULL（不是 daemon 起的、adopt 來的、
//! 升版前的舊 run）＝沒記，**不誤報**；PATCH 改設定的那一刻若 active run 沒記，先用「改之前」的版本補記，之後才看得出過期。
//! （`bots.launch_rev` 欄位 v13 一併加了但不用：目前的版本隨時從 bot 那一列算得出來，存起來反而多一份會不一致的資料。）

use crate::db;
use serde_json::json;
use sqlx::SqlitePool;

pub fn fnv1a64(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 啟動相關設定的版本：改任何一個都要重啟才生效的那組（`PATCH` 的 `restart_relevant`）。env 用有序 map，順序不影響結果。
///
/// 第五格（`Value::Null`）是 2026-10-02 移除的 `instruction_files` 留下的位置：已移除的設定不再參與版本，但格子留著、值固定 null，
/// 這樣沒設過它的 bot（NULL，也就是幾乎全部）算出來的版本跟移除前一模一樣——升級 daemon 不會把所有執行中、沒有任何設定變動的
/// bot（含 codex／grok）一次標成「需重啟」。代價：設過非預設值的 bot 版本會變（設定檔內容確實不同了），照實標成需重啟。
pub fn of(bot: &db::Bot) -> String {
    let canon = json!([
        bot.model, bot.effort, bot.fast, bot.persona, serde_json::Value::Null, bot.args_json,
        bot.identity, bot.env(), bot.inject_hooks, bot.auto_approve,
    ]);
    format!("{:016x}", fnv1a64(&canon.to_string()))
}

/// active run 載入的版本跟 bot 現在的設定不一樣＝過期。run 沒記版本＝不知道，不誤報。
pub fn is_stale(bot: &db::Bot, run: &db::Run) -> bool {
    let current = of(bot);
    if run.live_rev.as_deref() == Some(current.as_str()) {
        return false;
    }
    run.launch_rev.as_deref().is_some_and(|r| r != current)
}

/// 記下這個 run 載入的版本（PATCH 當場套用成功、或補記舊 run）。
pub async fn stamp(pool: &SqlitePool, run_id: &str, rev: &str) -> Result<(), sqlx::Error> {
    let updated = sqlx::query("UPDATE runs SET launch_rev = ? WHERE id = ?")
        .bind(rev)
        .bind(run_id)
        .execute(pool)
        .await?;
    if updated.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

/// Finish a persisted live-apply receipt. `live_rev` was written in the same transaction as the
/// runtime snapshot and is scoped to this exact run. If this UPDATE fails, the marker stays so a
/// later state read or startup recovery can retry without touching the TUI.
pub async fn stamp_live_revision(pool: &SqlitePool, run_id: &str) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE runs SET launch_rev = live_rev, live_rev = NULL WHERE id = ? AND live_rev IS NOT NULL",
    )
    .bind(run_id)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 1 {
        return Ok(true);
    }
    let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE id = ?)")
        .bind(run_id)
        .fetch_one(pool)
        .await?;
    if exists == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    // Another retry may have atomically consumed the marker first.
    Ok(false)
}

/// 改設定之前呼叫：active run 沒記版本就用「改之前」的 bot 補記，之後才算得出過期。
pub async fn stamp_if_missing(pool: &SqlitePool, run: &db::Run, bot_before: &db::Bot) -> Result<(), sqlx::Error> {
    if run.launch_rev.is_none() {
        stamp(pool, &run.id, &of(bot_before)).await?;
    }
    Ok(())
}
