//! agy 實際在跑的模型（含 effort）。
//!
//! agy 沒有 hook 回報模型；沒帶 `--model` 啟動時跑的是 agy 自己的預設（1.3.x 是 Gemini 3.8 Flash **high**，
//! 跟 AG Man 的預設 `gemini-3.8-flash-medium` 不同）。parent 用 `herdr agent start --kind agy` 開的 child 沒帶模型，
//! 設定就一直是「預設」、畫面卻是 high（2026-10-16 使用者：wCCA:p4「parent 沒有指定 effort，而 cmd 裡面是 high，與設定不一致」）。
//! 唯一的現況是輸入框下面那一行（agy 1.3.5 真畫面，2026-10-16）：
//!
//! ```text
//! ───────────────────────────────────────────────────────────────
//! >
//! ───────────────────────────────────────────────────────────────
//!   Press up to edit queued messages               Gemini 3.8 Flash · high
//! ```
//!
//! 畫面巡邏（`update_watch`，30 秒一輪）讀這一行，換成 slug（`gemini-3.8-flash-high`，effort 在 slug 裡）校正
//! `runs.runtime_model`；child 的設定一併跟著（[`crate::child_runtime`]）。規則同 [`crate::grok_live`]：
//! 只認最後一條分隔線底下那幾行；讀不到、或換出來的 slug 不是 agy 收的模型，什麼都不動。

use crate::db;
use crate::herdr::HerdrClient;

/// 最後一條分隔線底下最多幾行算框底（再多就是選單之類蓋住了）。
const FOOTER_LINES: usize = 3;

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 10 && t.chars().all(|c| c == '─')
}

/// 框底的 `Gemini 3.8 Flash · high` → `gemini-3.8-flash-high`。讀不到、或不是 [`crate::models::agy_model_allowed`] 收的就是 `None`。
pub fn parse_footer(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let end = lines.iter().rposition(|l| !l.trim().is_empty())? + 1;
    let rule = lines[..end].iter().rposition(|l| is_rule(l))?;
    let tail = &lines[rule + 1..end];
    if tail.is_empty() || tail.len() > FOOTER_LINES {
        return None;
    }
    tail.iter().rev().find_map(|line| {
        let at = line.rfind("Gemini ")?;
        let (name, effort) = line[at..].trim_end().split_once(" · ")?;
        let effort = effort.trim();
        if effort.is_empty() || !effort.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let slug = format!("{}-{}", name.split_whitespace().collect::<Vec<_>>().join("-"), effort).to_ascii_lowercase();
        crate::models::agy_model_allowed(&slug).then_some(slug)
    })
}

/// 畫面讀到的模型跟記著的不一樣才回它。一般 bot 只校正**已知**的 runtime（`NULL`＝沒指定、CLI 預設，補上會多一條
/// 重啟也改不掉的假 drift，同 grok）；child 的設定跟著 runtime，未知也補。
fn moved(seen: String, child: bool, cur: Option<&str>) -> Option<String> {
    ((child || cur.is_some()) && cur != Some(seen.as_str())).then_some(seen)
}

/// 以框底校正 `runs.runtime_model`。回傳 `true` = 有改動。
pub async fn correct_runtime_from_screen(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::Emit), run_id: &str, screen: &str) -> bool {
    let Some(seen) = parse_footer(screen) else { return false };
    let Ok(Some(run)) = db::run(app.db(), run_id).await else { return false };
    let Ok(Some(bot)) = db::bot(app.db(), &run.bot_id).await else { return false };
    let child = bot.managed_by == "child";
    let Some(model) = moved(seen, child, run.runtime_model.as_deref()) else { return false };
    if child {
        if let Err(e) = crate::app_ports_p13::child_runtime_follow(app, &bot.id, Some(&model), None).await {
            tracing::warn!(run = %run_id, bot = %bot.name, error = %e, "could not follow the agy model in the child's settings, retrying next sweep");
            return false;
        }
    }
    if let Err(e) = sqlx::query("UPDATE runs SET runtime_model = ? WHERE id = ?").bind(&model).bind(run_id).execute(app.db()).await {
        tracing::warn!(run = %run_id, bot = %bot.name, error = %e, "could not record the agy runtime model, retrying next sweep");
        return false;
    }
    tracing::info!(run = %run_id, bot = %bot.name, %model, child, "agy runtime corrected from the footer");
    app.emit_bot_status(&bot.id).await;
    true
}

/// 巡邏用。`hint` 是鎖外已經讀過的畫面：跟 runtime 一樣就停；不一樣才拿 bot 鎖確認還是同一個 run 再校正。
pub async fn sync_runtime(app: &(impl crate::capabilities::BotLocks + crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::Emit), client: &HerdrClient, bot_id: &str, run_id: &str, pane_id: &str, hint: &str) {
    let Some(seen) = parse_footer(hint) else { return };
    let Ok(Some(run)) = db::run(app.db(), run_id).await else { return };
    let child = matches!(db::bot(app.db(), bot_id).await, Ok(Some(b)) if b.managed_by == "child");
    if moved(seen, child, run.runtime_model.as_deref()).is_none() {
        return;
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    if !matches!(db::active_run(app.db(), bot_id).await, Ok(Some(r)) if r.id == run_id) {
        return;
    }
    let Ok(read) = client.pane_read(pane_id, "visible", 60).await else { return };
    correct_runtime_from_screen(app, run_id, &read.text).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "───────────────────────────────────────────────────────────────";

    fn screen(footer: &str) -> String {
        format!("● Ran 12 commands\n⣯  Running command...\n{RULE}\n>\n{RULE}\n{footer}\n\n")
    }

    #[test]
    fn reads_the_model_and_effort_off_the_footer() {
        let s = screen("  Press up to edit queued messages                                              Gemini 3.8 Flash · high");
        assert_eq!(parse_footer(&s).as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(parse_footer(&screen("  ? for shortcuts      Gemini 3.8 Flash · low")).as_deref(), Some("gemini-3.8-flash-low"));
    }

    #[test]
    fn conversation_text_and_the_banner_are_not_the_footer() {
        // 啟動橫幅寫的是 `Gemini 3.8 Flash (High)`，在分隔線上面，不算。
        let banner = format!("    ▀▀▀▀▀▀▀▀      Gemini 3.8 Flash (High)\n{RULE}\n> 你好\n");
        assert_eq!(parse_footer(&banner), None);
        let quoted = format!("我們用 Gemini 3.8 Flash · low 試過\n{RULE}\n>\n{RULE}\n  esc to cancel\n");
        assert_eq!(parse_footer(&quoted), None, "框底沒有模型就是讀不到，不往上找");
        assert_eq!(parse_footer("沒有框 Gemini 3.8 Flash · high\n"), None);
    }

    #[test]
    fn a_menu_covering_the_footer_is_unread() {
        let mut s = format!("{RULE}\n  Gemini 3.8 Flash · low\n");
        for i in 0..FOOTER_LINES {
            s.push_str(&format!("  {i}. option\n"));
        }
        assert_eq!(parse_footer(&s), None);
    }

    #[test]
    fn models_agy_does_not_take_are_ignored() {
        assert_eq!(parse_footer(&screen("  Gemini 3.1 Pro · high")), None);
        assert_eq!(parse_footer(&screen("  Gemini 3.8 Flash · 高")), None);
    }

    #[test]
    fn an_unset_runtime_is_filled_only_for_a_child() {
        assert_eq!(moved("gemini-3.8-flash-high".into(), true, None).as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(moved("gemini-3.8-flash-high".into(), false, None), None);
        assert_eq!(moved("gemini-3.8-flash-high".into(), false, Some("gemini-3.8-flash-medium")).as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(moved("gemini-3.8-flash-high".into(), true, Some("gemini-3.8-flash-high")), None);
    }
}
