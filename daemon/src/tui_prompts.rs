//! 認得出來、不該讓使用者操心的 TUI 對話框（主要是 Claude Code 的滿意度問卷）。
//!
//! 問卷會讓 agent 卡在 `blocked`；使用者的決定是**一律選 `0: Dismiss`**，daemon 自己按掉。
//! 事件（[`crate::events`]）與巡邏（[`spawn_survey_watcher`]）兩條都接：訂閱前跳出、事件漏掉、
//! herdr 沒判成 blocked 時只剩巡邏。[`crate::quota_claude`] 探測 pane 也用：它會按 Enter，
//! 落在問卷上等於替使用者打分數。
//! 認畫面不認狀態：權限確認、trust 對話框等原封不動留給使用者。

use crate::db;
use crate::state::App;
use std::sync::Arc;
use std::time::Duration;

const SWEEP: Duration = Duration::from_secs(10);

/// 按下 `0` 之後等多久再看一眼。
const SETTLE: Duration = Duration::from_millis(700);

/// 窄 pane 會把同一句話折成好幾行，逐行比對認不出來；框線字元一併當空白。
fn flatten(screen: &str) -> String {
    let spaced: String = screen
        .chars()
        .map(|c| if c.is_whitespace() || "│┌┐└┘─├┤┬┴┼▎▔".contains(c) { ' ' } else { c.to_ascii_lowercase() })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 問卷在輸入框上方，所以只看畫面尾端；正文就算提到問句和 `dismiss` 也不算。
const SURVEY_TAIL_LINES: usize = 14;
const SURVEY_QUESTION_LINES: usize = 3;
const SURVEY_OPTION_GAP: usize = 3;

fn has_survey_option(line: &str, number: usize, label: &str) -> bool {
    let needle = format!("{number}:");
    let mut offset = 0;
    while let Some(found) = line[offset..].find(&needle) {
        let start = offset + found;
        let boundary = line[..start].chars().next_back().map_or(true, |c| !c.is_ascii_alphanumeric());
        let rest = line[start + needle.len()..].trim_start();
        let label_end = rest
            .char_indices()
            .nth(label.chars().count())
            .map_or(rest.len(), |(i, _)| i);
        let label_boundary = rest[label_end..].chars().next().map_or(true, |c| !c.is_ascii_alphabetic());
        if boundary && rest[..label_end].eq_ignore_ascii_case(label) && label_boundary {
            return true;
        }
        offset = start + needle.len();
    }
    false
}

fn survey_options(line: &str) -> [bool; 4] {
    let line = flatten(line);
    [
        has_survey_option(&line, 0, "dismiss"),
        has_survey_option(&line, 1, "bad"),
        has_survey_option(&line, 2, "fine"),
        has_survey_option(&line, 3, "good"),
    ]
}

/// 問句要由 `●` / `>` 開頭且相鄰幾行內有至少三個選項，避免 agent 引用這些字串時誤認。
pub fn is_feedback_survey(screen: &str) -> bool {
    let lines: Vec<String> = screen
        .lines()
        .map(flatten)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(SURVEY_TAIL_LINES)..];
    let question = "how is claude doing";

    for start in 0..tail.len() {
        let marker = tail[start].strip_prefix("● ").or_else(|| tail[start].strip_prefix("> "));
        if marker.is_none() {
            continue;
        }
        for end in (start + 1)..=tail.len().min(start + SURVEY_QUESTION_LINES) {
            if !tail[start..end].join(" ").contains(question) {
                continue;
            }
            let option_end = tail.len().min(end + SURVEY_OPTION_GAP);
            let mut found = [false; 4];
            for line in &tail[start..option_end] {
                for (slot, present) in survey_options(line).into_iter().enumerate() {
                    found[slot] |= present;
                }
            }
            if found.into_iter().filter(|present| *present).count() >= 3 {
                return true;
            }
        }
    }
    false
}

/// Claude Code 開場登入選單（`CLAUDE_CONFIG_DIR` 未登入）。herdr 看起來是活的，prompt 卻只打在
/// 選單上、回合永遠掛著，所以 [`crate::lifecycle`] 送前先看，中了回 409 `needs_login`。
pub fn is_login_menu(screen: &str) -> bool {
    let t = flatten(screen);
    t.contains("select login method") && (t.contains("claude account with subscription") || t.contains("anthropic console account"))
}

/// claude 狀態列靠右的 `✔ Update installed · Restart to update`。回固定字而非整行：左半是每回合
/// 都變的 statusLine，存 DB 會一直 emit。只看最底 [`TAIL_LINES`] 行：正文引文也會中（2026-09-08 實測）。
pub fn update_notice(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    let t = flatten(&tail);
    (t.contains("update installed") && t.contains("restart to update")).then(|| UPDATE_NOTICE.to_string())
}

/// statusLine 那行加底下 `⏵⏵ bypass permissions` 一行，窄 pane 折行也在範圍內。
const TAIL_LINES: usize = 6;

/// 存進 `runs.update_notice` 的字，也是 UI tooltip 原句。
pub const UPDATE_NOTICE: &str = "Update installed · Restart to update";

/// 有對話紀錄時 `/model <別名>` 會跳「Switch model?」確認框（2.1.268 實測），herdr 判成 `idle`，
/// 下一則 prompt 被打進框裡、Enter 替使用者按 Yes、回合 stall（2026-09-11 AGM）。
/// 只看最底幾行，理由同 [`update_notice`]。
/// `/effort` 是同一個框，標題換成「Change effort level?」（2026-09-18 實測）：兩個都要認，
/// 漏掉哪一個，那個框就留在畫面上吃掉下一則 prompt。
pub fn is_switch_model_dialog(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(DIALOG_TAIL_LINES)..].join("\n");
    let t = flatten(&tail);
    (t.contains("switch model?") || t.contains("change effort level?"))
        && t.contains("yes, switch to")
        && t.contains("no, go back")
}

/// grok 1.0.34 開在沒信任過的目錄時跳「Do you trust the contents of this directory?」（y／n），
/// herdr 看不出是對話框，prompt 打進去會被吃掉（2026-09-17 使用者截圖，報 composer_unreadable）。
pub fn is_grok_trust_dialog(screen: &str) -> bool {
    let t = flatten(screen);
    t.contains("do you trust the contents of this directory") && t.contains("yes, proceed") && t.contains("no, quit")
}

/// 確認框連同框線與 statusLine 的最大高度；再往上是正文。
const DIALOG_TAIL_LINES: usize = 12;

/// 讀不到畫面就當不是——那不是這裡要擋的事。
pub async fn stuck_at_login(app: &Arc<App>, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.clone() else { return false };
    let Some(client) = app.herdr_for_run(run).await else { return false };
    match client.pane_read(&pane, "visible", 80).await {
        Ok(r) => is_login_menu(&r.text),
        Err(_) => false,
    }
}

/// 停在問卷上就按 `0`；回傳是否真的按了。
pub async fn dismiss_if_survey(app: &Arc<App>, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.clone() else { return false };
    let Some(client) = app.herdr_for_run(run).await else { return false };
    let Ok(read) = client.pane_read(&pane, "visible", 80).await else { return false };
    if !is_feedback_survey(&read.text) {
        return false;
    }
    {
        let mut revisions = app.survey_revisions.lock().await;
        if revisions.get(&run.id) == Some(&read.revision) {
            return false;
        }
        revisions.insert(run.id.clone(), read.revision);
    }
    tracing::info!(run = %run.id, bot = %run.bot_id, "claude 滿意度問卷：自動選 0（Dismiss）");
    if let Err(e) = client.pane_send_keys(&pane, &["0"]).await {
        let mut revisions = app.survey_revisions.lock().await;
        if revisions.get(&run.id) == Some(&read.revision) {
            revisions.remove(&run.id);
        }
        tracing::warn!(run = %run.id, error = %e, "問卷送 0 失敗");
        return false;
    }
    tokio::time::sleep(SETTLE).await;
    // 有的版本要再按 Enter；只在仍是問卷時補，免得落進後面真正等人回答的框。
    // 別用 run.agent_status 守衛：事件路徑的 Run 是 DB 更新前的複本（過期的 idle）。
    if matches!(client.pane_read(&pane, "visible", 80).await, Ok(r) if is_feedback_survey(&r.text)) {
        let _ = client.pane_send_keys(&pane, &["enter"]).await;
    }
    true
}

/// `idle` 也掃：問卷在回合結束後插入，herdr 可能只判成 idle，下一句話就會打進問卷裡。
pub fn spawn_survey_watcher(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP).await;
            let runs = db::all_active_runs(&app.db).await.unwrap_or_default();
            for run in runs.into_iter().filter(|r| r.agent_status == "blocked" || r.agent_status == "idle") {
                dismiss_if_survey(&app, &run).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn grok_trust_dialog_is_recognised_even_when_centred_and_wrapped() {
        let screen = "  main ~/p/h/projects/rt\n\n⠀⠀⠀⠀⠀⠀⣀⣀⡀\nDo you trust the contents of this directory?\n                /Users/m4p/project/hermes-agents/projects/rt\n\nGrok Build may run or modify contents in this directory,\n              posing security risks.\n\nYes, proceed                 y\n                  No, quit                     n\n\nGrok Build  1.0.34 [stable]\n";
        assert!(super::is_grok_trust_dialog(screen));
        assert!(!super::is_grok_trust_dialog("> Do you trust the contents of this directory? I asked grok that yesterday."));
    }

    #[test]
    fn recognises_the_login_menu_even_when_wrapped() {
        let screen = "Welcome to Claude Code v2.1.263\n\nSelect login\n method:\n\n❯ 1. Claude account with subscription · Pro, Max\n   2. Anthropic Console account · API usage billing\n";
        assert!(super::is_login_menu(screen));
        assert!(!super::is_login_menu("❯ 1. Yes, proceed\n  2. No, exit\nIs this a project you trust?"));
        assert!(!super::is_login_menu("the user asked about 'select login method' in the docs"));
    }

    use super::*;

    const SURVEY: &str = r#"
 ● How is Claude doing this session? (optional)
   1: Bad    2: Fine   3: Good   0: Dismiss

 > │
"#;

    /// 窄 pane 把同一段折成碎片之後，還是同一份問卷。
    const SURVEY_WRAPPED: &str = r#"
│ ● How is Claude doing   │
│ this session?           │
│ (optional)              │
│   1: Bad    2: Fine     │
│   3: Good   0: Dismiss  │
│ ╭──────────────────────╮ │
│ │ >                    │ │
│ ╰──────────────────────╯ │
│ claude | model | 42%      │
│ ⏵⏵ bypass permissions on │
"#;

    const PERMISSION: &str = r#"
 ⏺ Bash(rm -rf ./target/debug)
 ╭──────────────────────────────────────╮
 │  Do you want to proceed?             │
 │  ❯ 1. Yes                            │
 │    2. No, and tell Claude what to do │
 ╰──────────────────────────────────────╯
"#;

    #[test]
    fn survey_is_recognised_even_when_wrapped() {
        assert!(is_feedback_survey(SURVEY));
        assert!(is_feedback_survey(SURVEY_WRAPPED));
    }

    #[test]
    fn other_dialogs_are_left_alone() {
        assert!(!is_feedback_survey(PERMISSION));
        assert!(!is_feedback_survey(""));
        // 只提到 dismiss 的畫面不算——那個字到處都是。
        assert!(!is_feedback_survey("Press 0 to dismiss this notice"));
    }

    /// 2.1.268 在一個有對話紀錄的 session 裡打 `/model haiku` 之後的真畫面（2026-09-11）。
    const SWITCH_MODEL: &str = "  ⎿  Interrupted · What should Claude do instead?\n▔▔▔▔▔▔▔▔▔▔\n   Switch model?\n   Your next response will be slower and use more tokens\n   This conversation is cached for the current model. Switching to Haiku 4.5 means the full history gets re-read on your next message.\n   ❯ 1. Yes, switch to Haiku 4.5\n     2. No, go back\n";

    /// `/effort low` 在同一種 session 裡跳的框（2026-09-18 實測，標題是「Change effort level?」）。
    const CHANGE_EFFORT: &str = "  ✻ Cooked for 2m 16s · done 08:17\n▔▔▔▔▔▔▔▔▔▔\n   Change effort level?\n   Your next response will be slower and use more tokens\n\nThis conversation is cached for the current effort level. Switching to low means the full history gets re-read on your next message.\n\n❯ 1. Yes, switch to low\n  2. No, go back\n";

    #[test]
    fn change_effort_confirmation_is_recognised() {
        assert!(is_switch_model_dialog(CHANGE_EFFORT));
        // 同樣不能被正文裡的引文帶偏。
        let quoted = format!("{}\n{}\n─────\n❯\n─────\n  tony. | agents-manager | Opus 5 | 5h:96%\n", CHANGE_EFFORT, "⏺ Bash(cargo test)\n  ⎿  ok\n".repeat(8));
        assert!(!is_switch_model_dialog(&quoted));
    }

    #[test]
    fn switch_model_confirmation_is_recognised() {
        assert!(is_switch_model_dialog(SWITCH_MODEL));
        // 同一段字出現在正文裡（例如 agent 正在讀這份原始碼），底下還有輸入列與 statusLine——不算。
        let quoted = format!(
            "{}\n{}\n─────\n❯\n─────\n  tony. | agents-manager | Fable 5.1 31% | 5h:96%\n  ⏵⏵ bypass permissions on\n",
            SWITCH_MODEL,
            "⏺ Bash(cargo test)\n  ⎿  ok\n".repeat(8)
        );
        assert!(!is_switch_model_dialog(&quoted));
        assert!(!is_switch_model_dialog(SURVEY));
        assert!(!is_switch_model_dialog(PERMISSION));
        assert!(!is_switch_model_dialog(""));
    }

    #[test]
    fn quoted_survey_text_is_not_a_survey() {
        assert!(!is_feedback_survey(include_str!("tui_prompts.rs")));
        let source = r#"
pub fn is_feedback_survey(screen: &str) -> bool {
    let t = flatten(screen);
    t.contains("how is claude doing") && t.contains("dismiss")
}
"#;
        assert!(!is_feedback_survey(source));
        assert!(!is_feedback_survey("The transcript quoted how is claude doing and the dismiss option."));
    }

    const UPDATE: &str = " hunta | amber | OP5 10% | 3.2k                    ✔ Update installed · Restart to update\n";

    #[test]
    fn update_notice_is_read_off_the_status_line() {
        assert_eq!(update_notice(UPDATE).as_deref(), Some(UPDATE_NOTICE));
        // 窄 pane 折行。
        assert_eq!(update_notice("│ ✔ Update      │\n│ installed ·   │\n│ Restart to    │\n│ update        │\n").as_deref(), Some(UPDATE_NOTICE));
        // 只中一半不算——例如 agent 正在讀這份原始碼。
        assert!(update_notice("✔ Update installed").is_none());
        assert!(update_notice("restart to update the docs").is_none());
        assert!(update_notice(SURVEY).is_none());
        // 正文裡引到這兩句不算——寫這個功能的 agent 的畫面上就是這樣（2026-09-08 實測）。
        let quoted = format!(
            "  tooltip 寫原句 `Update installed · Restart to update` 與…\n{}\n{}\n❯\n{}\n  tony. | agents-manager | Fable 5.1 31% | 5h:96%\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
            "⏺ Bash(git commit -m \"…\")\n  ⎿  cff77fc docs(goals): plan…\n".repeat(4),
            "─".repeat(20),
            "─".repeat(20)
        );
        assert!(update_notice(&quoted).is_none());
        assert!(update_notice("").is_none());
    }
}
