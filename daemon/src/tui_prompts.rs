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
}
