//! claude 在跑的中途被 `/model`、`/effort` 換掉（SPEC §4.4a，2026-09-29 使用者：console-pm 把子 agent requote
//! 從 Opus 換成 Sonnet 5.5，側欄還寫 Opus）。
//!
//! 沒掛 hook 的 run（收編的子 agent）沒有 statusLine 回報，AG Man 只在收編時從 argv 讀過一次模型。claude 執行這兩個
//! slash 指令後會在對話裡印一行確認（2.1.283 真機）：
//!
//! ```text
//!   ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions
//!   ⎿  Set effort level to high (saved as your default for new sessions): Comprehensive implementation …
//! ```
//!
//! 畫面巡邏（`update_watch`，30 秒一輪）順便找最後一次出現的這兩行，寫進 `runs.runtime_*`；子 agent（`managed_by=child`）
//! 的設定本來就是從 argv 抄來的，一併改 `bots.model`／`effort`，側欄才跟著變。
//!
//! 只認 `⎿` 開頭的行（slash 指令的輸出），對話內容裡引用到的同一句不算。一般 bot 可能是 `--resume` 接回來的，
//! 畫面上那行可能是上一個 session 的：**同一個 run 第一次看到的只當基準**，之後變了才採用。子 agent 不會被 AG Man
//! 接回，看到就採用。

use crate::db;
use crate::state::App;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Switch {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// 畫面上最後一次的 `Set model to …`／`Set effort level to …`（各自取最後一次）。
///
/// `⎿` 不是 slash 指令專屬：工具結果（`⏺ Bash(…)` 底下的 stdout）也用它縮排（#741），所以只認**緊接在使用者打的
/// `❯ /model …`／`❯ /effort …` 之後**的那一行 `⎿`；`⏺` 一出現（工具、回覆）就清掉，一行確認只配一個指令。
/// 指令行必須頂格——工具輸出一律縮排，裡面長得像 `❯ /model` 的行不算。
pub fn parse(screen: &str) -> Switch {
    let mut s = Switch::default();
    let mut after_slash = false;
    for line in screen.lines() {
        if let Some(rest) = line.strip_prefix('❯') {
            let cmd = rest.split_whitespace().next().unwrap_or("");
            after_slash = matches!(cmd, "/model" | "/effort");
            continue;
        }
        if line.starts_with('⏺') {
            after_slash = false;
            continue;
        }
        let Some(rest) = line.trim_start().strip_prefix('⎿') else { continue };
        if !std::mem::take(&mut after_slash) {
            continue;
        }
        let rest = rest.trim_start();
        if let Some(name) = rest.strip_prefix("Set model to ") {
            let name = name.split(" and saved").next().unwrap_or(name);
            let name = name.split(" (").next().unwrap_or(name).trim();
            if let Some(id) = model_id(name) {
                s.model = Some(id);
            }
        } else if let Some(level) = rest.strip_prefix("Set effort level to ") {
            let level: String = level.chars().take_while(char::is_ascii_alphabetic).collect();
            if !level.is_empty() {
                s.effort = Some(level.to_ascii_lowercase());
            }
        }
    }
    s
}

/// 顯示名 → model id：`Sonnet 5.5` → `claude-sonnet-5-5`（`bots.model` 與 argv 用的就是這種）。
fn model_id(display: &str) -> Option<String> {
    let mut it = display.split_whitespace();
    let family = it.next()?.to_ascii_lowercase();
    if !matches!(family.as_str(), "opus" | "sonnet" | "haiku" | "fable") {
        return None;
    }
    let version = it.next().filter(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))?;
    Some(format!("claude-{family}-{}", version.replace('.', "-")))
}

/// run id → 這個 run 第一次看到的切換（基準）。只有一般 bot 會記。
fn baselines() -> &'static Mutex<HashMap<String, Switch>> {
    static B: OnceLock<Mutex<HashMap<String, Switch>>> = OnceLock::new();
    B.get_or_init(Default::default)
}

/// 這一輪沒看到的 run（結束了）不留帳。
pub fn retain_runs(active: &[String]) {
    baselines().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
}

/// 巡邏讀到一份 claude 畫面：有切換而且跟記著的不一樣才寫、才推 `bot_status`。
pub async fn observe(app: &Arc<App>, run: &db::Run, screen: &str) {
    let seen = parse(screen);
    if seen == Switch::default() {
        return;
    }
    let Ok(Some(bot)) = db::bot(&app.db, &run.bot_id).await else { return };
    let child = bot.managed_by == "child";
    if !child {
        let mut b = baselines().lock().unwrap_or_else(|e| e.into_inner());
        match b.get(&run.id) {
            None => {
                b.insert(run.id.clone(), seen);
                return;
            }
            Some(prev) if *prev == seen => return,
            Some(_) => {
                b.insert(run.id.clone(), seen.clone());
            }
        }
    }
    let Ok(Some(run)) = db::run(&app.db, &run.id).await else { return };
    let cur_model = run.runtime_model.clone().or_else(|| bot.model.clone());
    let cur_effort = run.runtime_effort.clone().or_else(|| bot.effort.clone());
    let model = seen.model.filter(|m| cur_model.as_deref() != Some(m.as_str()));
    let effort = seen.effort.filter(|e| cur_effort.as_deref() != Some(e.as_str()));
    let bot_behind = child
        && (model.is_some() && bot.model != model || effort.is_some() && bot.effort != effort);
    if model.is_none() && effort.is_none() && !bot_behind {
        return;
    }
    let wrote = sqlx::query("UPDATE runs SET runtime_model = COALESCE(?, runtime_model), runtime_effort = COALESCE(?, runtime_effort) WHERE id = ?")
        .bind(&model)
        .bind(&effort)
        .bind(&run.id)
        .execute(&app.db)
        .await
        .is_ok();
    if wrote && child {
        let _ = sqlx::query("UPDATE bots SET model = COALESCE(?, model), effort = COALESCE(?, effort) WHERE id = ?")
            .bind(&model)
            .bind(&effort)
            .bind(&bot.id)
            .execute(&app.db)
            .await;
    }
    if wrote {
        tracing::info!(run = %run.id, bot = %bot.name, ?model, ?effort, child, "claude runtime switched in the TUI");
        app.emit_bot_status(&bot.id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: &str = "\
❯ /model sonnet
  ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions

❯ /effort high
  ⎿  Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive
     testing and documentation
";

    #[test]
    fn reads_the_last_model_and_effort_switch() {
        assert_eq!(parse(SCREEN), Switch { model: Some("claude-sonnet-5-5".into()), effort: Some("high".into()) });
        let later = format!("{SCREEN}\n❯ /model opus\n  ⎿  Set model to Opus 5.5 (default)\n");
        assert_eq!(parse(&later).model.as_deref(), Some("claude-opus-5-5"), "取最後一次");
    }

    #[test]
    fn ignores_the_same_words_outside_a_slash_command_output() {
        let quoted = "⏺ 我會執行 /model，畫面會顯示 Set model to Sonnet 5.5\n  Set effort level to max\n";
        assert_eq!(parse(quoted), Switch::default());
        assert_eq!(parse("❯ /model x\n  ⎿  Set model to Something Weird\n"), Switch::default(), "認不出的顯示名不猜");
    }

    #[test]
    fn ignores_tool_output_that_looks_like_model_or_effort_confirmation() {
        let tool_output = "⏺ Bash(cat source.rs)\n  ⎿ Set model to Sonnet 5.5 and saved as your default for new sessions\n\
⏺ Bash(cat tests.rs)\n  ⎿ Set effort level to high (saved as your default for new sessions): output\n";
        assert_eq!(parse(tool_output), Switch::default());
    }

    #[test]
    fn a_real_slash_command_after_tool_output_is_still_recognised() {
        let screen = "⏺ Bash(cat tests.rs)\n  ⎿ Set model to Opus 5.5 and saved as your default\n\n❯ /model sonnet\n  ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions\n\n⏺ Bash(cat tests.rs)\n  ⎿ Set effort level to max: spoof\n";
        assert_eq!(parse(screen), Switch { model: Some("claude-sonnet-5-5".into()), effort: None });
        // 工具輸出裡長得像指令行的（縮排）不算，指令後不接 ⎿ 也不留到後面的工具輸出。
        let spoof = "⏺ Bash(cat log)\n    ❯ /model sonnet\n  ⎿ Set model to Sonnet 5.5 and saved\n❯ /model sonnet\n\n⏺ Bash(x)\n  ⎿ Set model to Opus 5.5\n";
        assert_eq!(parse(spoof), Switch::default());
    }

    #[tokio::test]
    async fn a_child_follows_the_switch_and_a_user_bot_only_after_its_baseline() {
        let env = crate::testing::env().await;
        let app = env.app.clone();

        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-opus-5-5', effort = 'xhigh' WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        observe(&app, &run, SCREEN).await;
        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")), "子 agent 的設定跟著改");
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-sonnet-5-5"));

        let user = crate::testing::claude_bot(&app, &env.project_id, "user").await;
        let run_id = crate::testing::fake_run(&app, &user.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        observe(&app, &run, SCREEN).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model, None, "第一次看到的可能是 --resume 印回來的舊行，只當基準");
        observe(&app, &run, &format!("{SCREEN}\n❯ /model haiku\n  ⎿  Set model to Haiku 4.5 and saved\n")).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-haiku-4-5"), "之後變了才採用");
        let b = db::bot(&app.db, &user.id).await.unwrap().unwrap();
        assert_ne!(b.model.as_deref(), Some("claude-haiku-4-5"), "一般 bot 的設定不動（重啟會回到設定值，畫成 drift）");
    }

    #[tokio::test]
    async fn tool_output_does_not_change_a_child_runtime_or_configured_model() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-opus-5-5', effort = 'xhigh' WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        let tool_output = "⏺ Bash(cat source.rs)\n  ⎿ Set model to Sonnet 5.5 and saved as your default for new sessions\n\
⏺ Bash(cat tests.rs)\n  ⎿ Set effort level to high (saved as your default for new sessions): output\n";

        observe(&app, &run, tool_output).await;

        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-opus-5-5"), Some("xhigh")));
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (None, None));
    }
}
