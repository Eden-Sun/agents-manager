//! grok 在跑的中途被 `/model`、`/effort` 換掉（SPEC §4.4a）。
//!
//! grok 沒有 hook 回報模型，argv（`-m`、`--reasoning-effort`）只在收編／啟動時讀一次，而且 TUI 根本不理
//! `--reasoning-effort`（#215）。唯一的現況是輸入框框底（grok 1.0.x 真畫面，2026-10-03）：
//!
//! ```text
//!   ╭────────────────────────────────────────────────╮
//!   │ ❯                                              │
//!   ╰────────────── Grok 4.7 (low) · always-approve ─╯
//! ```
//!
//! 畫面巡邏（`update_watch`，30 秒一輪）讀這一行校正 `runs.runtime_*`；child 的設定一併跟著（[`crate::child_runtime`]）。
//! 讀不到框底（選單蓋住、畫面清掉）什麼都不動，沿用最後已知值。對話裡的 `Switched to Grok 4.7 (low effort)` 不算：
//! 只認 `╰` 開頭的那一行。

use crate::db;
use crate::herdr::HerdrClient;
use crate::state::App;
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrokRuntime {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// 最下面那個 `╰── Grok <版本> (<強度>) …` 框底；認不出模型也認不出強度就是讀不到。
pub fn parse_footer(screen: &str) -> Option<GrokRuntime> {
    screen.lines().rev().find_map(|line| {
        let line = line.trim();
        if !line.starts_with('╰') {
            return None;
        }
        let idx = line.find("Grok ").or_else(|| line.find("grok "))?;
        let (model, effort) = crate::models::grok_title_model_effort(&line[idx..]);
        (model.is_some() || effort.is_some()).then_some(GrokRuntime { model, effort })
    })
}

/// 以框底校正 `runs.runtime_*`。回傳 `true` = 有改動。
/// * 只寫讀到、而且跟記著的不一樣的欄位；讀不到的欄位沿用。
/// * 一般 bot 只校正**已知**的 runtime：`NULL`＝啟動時沒指定（CLI 預設），拿畫面補上會多一條重啟也改不掉的假 drift。
///   child 沒有這個問題（設定會跟著 runtime），未知也補。
pub async fn correct_runtime_from_screen(app: &App, run_id: &str, screen: &str) -> bool {
    let Some(seen) = parse_footer(screen) else { return false };
    let Ok(Some(run)) = db::run(&app.db, run_id).await else { return false };
    let Ok(Some(bot)) = db::bot(&app.db, &run.bot_id).await else { return false };
    let child = bot.managed_by == "child";
    let moved = |seen: Option<String>, cur: Option<&str>| seen.filter(|v| (child || cur.is_some()) && cur != Some(v.as_str()));
    let model = moved(seen.model, run.runtime_model.as_deref());
    let effort = moved(seen.effort, run.runtime_effort.as_deref());
    if model.is_none() && effort.is_none() {
        return false;
    }
    if child {
        if let Err(e) = crate::child_runtime::follow(app, &bot.id, model.as_deref(), effort.as_deref()).await {
            tracing::warn!(run = %run_id, bot = %bot.name, error = %e, "could not follow the grok switch in the child's settings, retrying next sweep");
            return false;
        }
    }
    let wrote = sqlx::query("UPDATE runs SET runtime_model = COALESCE(?, runtime_model), runtime_effort = COALESCE(?, runtime_effort) WHERE id = ?")
        .bind(&model)
        .bind(&effort)
        .bind(run_id)
        .execute(&app.db)
        .await;
    if let Err(e) = wrote {
        tracing::warn!(run = %run_id, bot = %bot.name, error = %e, "could not record the grok runtime switch, retrying next sweep");
        return false;
    }
    tracing::info!(run = %run_id, bot = %bot.name, ?model, ?effort, child, "grok runtime corrected from the composer footer");
    app.emit_bot_status(&bot.id).await;
    true
}

/// 巡邏用：拿 bot 鎖（啟動補 `/effort`、當場套用握著同一把，不會讀到切到一半的畫面），重讀畫面再校正。
pub async fn sync_runtime(app: &Arc<App>, client: &HerdrClient, bot_id: &str, run_id: &str, pane_id: &str) {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    if !matches!(db::active_run(&app.db, bot_id).await, Ok(Some(r)) if r.id == run_id) {
        return;
    }
    let Ok(read) = client.pane_read(pane_id, "visible", 60).await else { return };
    correct_runtime_from_screen(app, run_id, &read.text).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOOTER_LOW: &str = "\
  ⏺ Switched to Grok 4.7 (low effort)

  ╭────────────────────────────────────────────────╮
  │ ❯                                              │
  ╰────────────── Grok 4.7 (low) · always-approve ─╯

  Shift+Tab:mode  │  Ctrl+.:shortcuts
";

    #[test]
    fn reads_model_and_effort_off_the_composer_footer() {
        assert_eq!(parse_footer(FOOTER_LOW), Some(GrokRuntime { model: Some("grok-4.7".into()), effort: Some("low".into()) }));
    }

    #[test]
    fn conversation_text_is_not_the_footer() {
        assert_eq!(parse_footer("  ⏺ Switched to Grok 4.7 (low effort)\n  我們用 Grok 4.6 (high) 試過\n"), None);
        assert_eq!(parse_footer("  ╰──────────────╯\n"), None, "純線條的框底不是模型");
        let quoted = format!("  ╰── Grok 4.6 (high) · always-approve ─╯\n  ⏺ 舊框\n{FOOTER_LOW}");
        assert_eq!(parse_footer(&quoted).and_then(|r| r.effort).as_deref(), Some("low"), "取最下面那個框底");
    }
}
