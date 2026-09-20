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

fn fnv1a64(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 啟動相關設定的版本：改任何一個都要重啟才生效的那組（`PATCH` 的 `restart_relevant`）。env 用有序 map，順序不影響結果。
pub fn of(bot: &db::Bot) -> String {
    let canon = json!([
        bot.model, bot.effort, bot.fast, bot.persona, bot.instruction_files, bot.args_json,
        bot.identity, bot.env(), bot.inject_hooks, bot.auto_approve,
    ]);
    format!("{:016x}", fnv1a64(&canon.to_string()))
}

/// active run 載入的版本跟 bot 現在的設定不一樣＝過期。run 沒記版本＝不知道，不誤報。
pub fn is_stale(bot: &db::Bot, run: &db::Run) -> bool {
    run.launch_rev.as_deref().is_some_and(|r| r != of(bot))
}

/// 記下這個 run 載入的版本（PATCH 當場套用成功、或補記舊 run）。
pub async fn stamp(pool: &SqlitePool, run_id: &str, rev: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE runs SET launch_rev = ? WHERE id = ?").bind(rev).bind(run_id).execute(pool).await?;
    Ok(())
}

/// 改設定之前呼叫：active run 沒記版本就用「改之前」的 bot 補記，之後才算得出過期。
pub async fn stamp_if_missing(pool: &SqlitePool, run: &db::Run, bot_before: &db::Bot) -> Result<(), sqlx::Error> {
    if run.launch_rev.is_none() {
        stamp(pool, &run.id, &of(bot_before)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn the_revision_changes_with_every_launch_relevant_field_and_ignores_the_rest() {
        let e = tt::env().await;
        let base = tt::claude_bot(&e.app, &e.project_id, "rev").await;
        let rev = of(&base);
        assert_eq!(rev, of(&base.clone()), "同樣的設定同一個版本");
        let mut b = base.clone();
        b.persona = Some("x".into());
        assert_ne!(of(&b), rev);
        let mut b = base.clone();
        b.env_json = r#"{"A":"1"}"#.into();
        assert_ne!(of(&b), rev);
        let mut b = base.clone();
        b.args_json = r#"["--x"]"#.into();
        assert_ne!(of(&b), rev);
        let mut b = base.clone();
        b.name = "renamed".into();
        b.autostart = 1;
        b.is_primary = 1;
        assert_eq!(of(&b), rev, "名字、autostart、釘選不需要重啟");
    }
}
