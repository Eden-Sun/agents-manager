//! 畫面上認得出來、而且不該讓使用者操心的 TUI 對話框。
//!
//! 目前只有一種：Claude Code 每隔一陣子插進來的滿意度問卷
//!
//! ```text
//! ● How is Claude doing this session? (optional)
//!   1: Bad    2: Fine   3: Good   0: Dismiss
//! ```
//!
//! 它跟手上的工作無關，卻會讓 agent 停在 `blocked` 等人回答——回合卡住、輸入框鎖住，UI 還會
//! 把整個終端彈到眼前（`BlockedModal`）。使用者的決定是**一律選 `0: Dismiss`**，所以 daemon
//! 自己按掉，不驚動任何人。
//!
//! 兩條路徑都接上，因為兩條都可能是唯一的機會：
//! - 事件：`pane.agent_status_changed` 一報 `blocked` 就看一眼（[`crate::events`]），最快；
//! - 巡邏：[`spawn_survey_watcher`] 每 10 秒掃一次停著（`blocked` / `idle`）的 Run，補上問卷在
//!   pane 訂閱建立之前就跳出來、事件漏掉、或 herdr 根本沒把它當成 `blocked` 的情形。
//!
//! [`crate::quota_claude`] 的探測 pane 也走同一個判斷：那裡本來就會對擋路的對話框按 Enter，
//! 而 Enter 落在這份問卷上等於**替使用者打了一個分數**。
//!
//! 認畫面而不是認狀態：只有畫面上真的是那份問卷才會按鍵，其他等人回答的東西（權限確認、
//! trust 對話框）原封不動留給使用者。

use crate::db;
use crate::state::App;
use std::sync::Arc;
use std::time::Duration;

/// 巡邏間隔：blocked 的 Run 通常是 0 個或 1 個，一次 `pane.read` 很便宜。
const SWEEP: Duration = Duration::from_secs(10);

/// 按下 `0` 之後等多久再看一眼。
const SETTLE: Duration = Duration::from_millis(700);

/// 終端畫面壓成一行小寫、單一空白的字。
///
/// 窄 pane 會把同一句話折成好幾行（`is_shredded` 那個老問題），逐行比對認不出來；框線字元也
/// 一併當成空白丟掉。
fn flatten(screen: &str) -> String {
    let spaced: String = screen
        .chars()
        .map(|c| if c.is_whitespace() || "│┌┐└┘─├┤┬┴┼▎▔".contains(c) { ' ' } else { c.to_ascii_lowercase() })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 這個畫面是不是那份滿意度問卷。
///
/// 兩個條件都要中：問句本身，加上 `dismiss` 這個選項——只有問句可能是別人引用了這段文字
/// （例如 agent 正在讀這份原始碼），只有 `dismiss` 則太常見。
pub fn is_feedback_survey(screen: &str) -> bool {
    let t = flatten(screen);
    t.contains("how is claude doing") && t.contains("dismiss")
}

/// 這個畫面是不是 Claude Code 開場的登入選單（`CLAUDE_CONFIG_DIR` 指到一個還沒登入的目錄）：
///
/// ```text
/// Select login method:
/// ❯ 1. Claude account with subscription · Pro, Max, Team, or Enterprise
///   2. Anthropic Console account · API usage billing
/// ```
///
/// 停在這裡的 agent 對 herdr 看起來是活的、也收得下字，但送進去的 prompt 只是在選單上打字，
/// 回合會一直掛著。所以 [`crate::lifecycle`] 送 prompt 前先看一眼，中了就直接 409 `needs_login`。
pub fn is_login_menu(screen: &str) -> bool {
    let t = flatten(screen);
    t.contains("select login method") && (t.contains("claude account with subscription") || t.contains("anthropic console account"))
}

/// claude 已經把新版下載好、等重啟才會換過去時，畫面最底下那行（跟使用者的 statusLine 同一
/// 行、靠右）印的：
///
/// ```text
/// ✔ Update installed · Restart to update
/// ```
///
/// 中了就回一句固定的字，而不是整行——那行左半邊還有使用者 statusLine 的內容（模型、用量…），
/// 每回合都在變，存進 DB 只會一直 emit。
///
/// 兩段字都要中，理由同 [`is_feedback_survey`]。但這裡光是「兩段都中」還不夠：2026-09-08 實測，
/// 正在寫這個功能的那個 agent 的畫面上同時有這兩句**引文**，照樣中。所以只看畫面**最下面**
/// [`TAIL_LINES`] 行非空白的——真正的通知就印在那條狀態列上，正文捲不到那裡。
pub fn update_notice(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    let t = flatten(&tail);
    (t.contains("update installed") && t.contains("restart to update")).then(|| UPDATE_NOTICE.to_string())
}

/// 只認畫面最底下這幾行。那句印在使用者 statusLine 那一行（靠右），底下最多再一行
/// `⏵⏵ bypass permissions on …`；窄 pane 折行也還在這個範圍內。
const TAIL_LINES: usize = 6;

/// [`update_notice`] 中了以後存進 `runs.update_notice` 的字，也是 UI tooltip 上的原句。
pub const UPDATE_NOTICE: &str = "Update installed · Restart to update";

/// claude 的 `/model <別名>` 在**已經有對話紀錄**時不會直接換，而是先跳一個確認框：
///
/// ```text
///  Switch model?
///  Your next response will be slower and use more tokens
///  This conversation is cached for the current model. Switching to Haiku 4.5 means …
///  ❯ 1. Yes, switch to Haiku 4.5
///    2. No, go back
/// ```
///
/// （2.1.268 實測；空的 session 沒有快取可失效，就直接換、不問。）herdr 把這個框判成 `idle`，
/// 所以 daemon 以為指令已經套用，下一則 prompt 被打進框裡：字被丟掉、Enter 替使用者按了
/// 「Yes」，回合在 12 秒後以 stall 失敗（2026-09-11 AGM：`/model fable` 04:45:09 送出，
/// transcript 裡直到 04:45:21.98 使用者的「go」按下 Enter 才真的執行）。
///
/// 只看畫面**最下面**幾行：這個框畫在輸入列的位置，正文裡引用到這幾個字（例如 agent 正在讀
/// 這份原始碼）不算——同 [`update_notice`] 踩過的坑。
pub fn is_switch_model_dialog(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(DIALOG_TAIL_LINES)..].join("\n");
    let t = flatten(&tail);
    t.contains("switch model?") && t.contains("yes, switch to") && t.contains("no, go back")
}

/// 確認框連同上下框線、底下的 statusLine 最多這麼高；再往上就是正文。
const DIALOG_TAIL_LINES: usize = 12;

/// 這個 Run 的 pane 現在是不是停在登入選單上。讀不到畫面就當不是——那不是這裡要擋的事。
pub async fn stuck_at_login(app: &Arc<App>, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.clone() else { return false };
    let Some(client) = app.herdr_for_run(run).await else { return false };
    match client.pane_read(&pane, "visible", 80).await {
        Ok(r) => is_login_menu(&r.text),
        Err(_) => false,
    }
}

/// 這個 Run 的 pane 若正停在那份問卷上就替它按 `0`。回傳是否真的按了。
pub async fn dismiss_if_survey(app: &Arc<App>, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.clone() else { return false };
    let Some(client) = app.herdr_for_run(run).await else { return false };
    let Ok(read) = client.pane_read(&pane, "visible", 80).await else { return false };
    if !is_feedback_survey(&read.text) {
        return false;
    }
    tracing::info!(run = %run.id, bot = %run.bot_id, "claude 滿意度問卷：自動選 0（Dismiss）");
    if let Err(e) = client.pane_send_keys(&pane, &["0"]).await {
        tracing::warn!(run = %run.id, error = %e, "問卷送 0 失敗");
        return false;
    }
    tokio::time::sleep(SETTLE).await;
    // 有的版本要再一個 Enter 才收下選擇。只在畫面**還停在同一份問卷**時才補，免得 Enter 落進
    // 問卷後面那個真正在等人回答的東西。
    if matches!(client.pane_read(&pane, "visible", 80).await, Ok(r) if is_feedback_survey(&r.text)) {
        let _ = client.pane_send_keys(&pane, &["enter"]).await;
    }
    true
}

/// 每 [`SWEEP`] 掃一次停著的 Run，替停在問卷上的按掉。
///
/// `blocked` 與 `idle` 都掃：問卷多半讓 herdr 判成 `blocked`（它就是個等輸入的 UI），但它是在
/// **回合結束後**插進來的，herdr 也可能只當成一般的 idle 畫面——那時使用者送出的下一句話會被
/// 打進問卷裡。`working` 不掃：畫面正在動，問卷不會插在中間。
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
