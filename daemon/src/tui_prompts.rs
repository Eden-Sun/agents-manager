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

/// 單獨一行壓成好比對的樣子：框線字元當空白、行首的游標／項目符號（`❯`、`>`、`•`…）丟掉，
/// 其餘小寫、空白收成一個。
///
/// 對話框要**逐行**認，不能把整段畫面壓成一串再 `contains`：只要三段字都出現在最底幾行就中，
/// agent 自己在回報裡引了「switch model?」「yes, switch to」「no, go back」也會被當成框開著，
/// 之後每則 prompt 都被擋成 409 `dialog_open`（2026-09-18 實測）。真的框一定是標題、`1. …`、
/// `2. …` 各自成行，正文引文則在句子中間。
fn norm_line(line: &str) -> String {
    let spaced: String = line
        .chars()
        .map(|c| if c.is_whitespace() || "│┌┐└┘─├┤┬┴┼╭╮╯╰▎▔".contains(c) { ' ' } else { c.to_ascii_lowercase() })
        .collect();
    let joined = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.trim_start_matches(|c| "❯›»>*●•⏺⎿✻-— ".contains(c)).trim().to_string()
}

/// 有沒有哪一行（照 [`norm_line`] 正規化後）以這段字開頭。
fn line_starts_with(lines: &[String], prefix: &str) -> bool {
    lines.iter().any(|l| l.starts_with(prefix))
}

/// 畫面尾端有沒有哪一行是**空的**輸入列游標（框線／空白之外只剩一個 `❯`／`›`，同
/// `capture::claude::ClaudeCapture::awaits_input` 認的形狀）。
///
/// 真正的確認框會佔用輸入列那一行（游標旁還跟著 `1. …`／選項文字），不可能同時讓輸入列
/// 空著等打字；回覆裡逐行引用對話框原文——就算連編號、選項都照抄成單獨一行，跟真的框長得
/// 一模一樣——那一輪的畫面稍後一定會再印出一行空的輸入列（`is_switch_model_dialog`／
/// `is_grok_trust_dialog` 各自的 tail 範圍內），因為那時候根本沒有框在擋。用「輸入列還空著」
/// 這個結構性事實去分辨，不必再猜引文的排版像不像框（2026-09-18：只認「標題／選項各自成行」
/// 擋不住刻意排成一行一句的引文，見 issue #114）。
pub(crate) fn composer_is_idle(lines: &[&str]) -> bool {
    lines.iter().any(|l| {
        let mut chars = l.chars().filter(|c| !"│┃╭╮╰╯─━▔ \t".contains(*c));
        matches!(chars.next(), Some('❯') | Some('›')) && chars.next().is_none()
    })
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
    // 標題允許窄 pane 折行（`Select login` / ` method:`），但選項一定要自己成行——句子裡提到
    // 「select login method」的正文不算（[`norm_line`]）。只看最底 [`MENU_TAIL_LINES`] 行、而且輸入列不能空著：
    // bot 在回報裡逐行引用這個選單（2026-09-22 triage bot 貼了 2.1.280 的 onboarding 原文），正文在上面、
    // 底下是空的輸入列，那不是選單（同 [`is_switch_model_dialog`] 2026-09-18 的教訓）。
    let Some(tail_raw) = menu_tail(screen) else { return false };
    let lines: Vec<String> = tail_raw.iter().map(|l| norm_line(l)).collect();
    flatten(&tail_raw.join("\n")).contains("select login method")
        && (line_starts_with(&lines, "1. claude account with subscription") || line_starts_with(&lines, "2. anthropic console account"))
}

/// 開場選單（登入／onboarding 主題）連同底下的預覽與提示最多這麼高；再往上是正文。
const MENU_TAIL_LINES: usize = 20;

/// 畫面最底 [`MENU_TAIL_LINES`] 個非空行；輸入列空著（[`composer_is_idle`]）就回 `None`——那時畫面上不可能有選單在擋。
fn menu_tail(screen: &str) -> Option<Vec<&str>> {
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = raw[raw.len().saturating_sub(MENU_TAIL_LINES)..].to_vec();
    (!composer_is_idle(&tail)).then_some(tail)
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
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail_raw = &raw[raw.len().saturating_sub(DIALOG_TAIL_LINES)..];
    if composer_is_idle(tail_raw) {
        return false; // 輸入列還空著，不可能有框擋著它（見 [`composer_is_idle`]）。
    }
    let tail: Vec<String> = tail_raw.iter().map(|l| norm_line(l)).collect();
    let titled = tail.iter().any(|l| l.starts_with("switch model?") || l.starts_with("change effort level?"));
    titled && line_starts_with(&tail, "1. yes, switch to") && line_starts_with(&tail, "2. no, go back")
}

/// Claude Code 2.1.278 首次啟動的「Auto mode」推銷框（2026-09-22 build child 卡在這裡半小時：herdr 判 idle、
/// 交辦 queued 不送，排隊逾時被撤回、租約過期）。兩個選項各自成行才算；同 [`is_switch_model_dialog`]，
/// 輸入列空著就不是框。第二個選項「keep bypass permissions」是我們要的，呼叫端送 Down＋Enter。
pub fn is_auto_mode_offer(screen: &str) -> bool {
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail_raw = &raw[raw.len().saturating_sub(DIALOG_TAIL_LINES)..];
    if composer_is_idle(tail_raw) {
        return false;
    }
    let tail: Vec<String> = tail_raw.iter().map(|l| norm_line(l)).collect();
    line_starts_with(&tail, "yes, set auto mode as my default permission mode") && line_starts_with(&tail, "no, keep bypass permissions")
}

/// onboarding 第一頁（`hasCompletedOnboarding` 被清掉、或全新的 `CLAUDE_CONFIG_DIR`）：「Choose the text style…」
/// 七個主題選項。跟登入選單同一類：交給人處理（409 `needs_login`），不自動按——按了下一頁就是登入選單，
/// 一樣要人。真畫面在 `lifecycle/fixtures/claude-2.1.278-onboarding-theme.txt`（2026-09-22）。
pub fn is_onboarding_theme(screen: &str) -> bool {
    // 同 [`is_login_menu`]：只看最底幾行、輸入列不能空著（2026-09-22 triage bot 在回報裡引了整頁原文，每次送交辦都被判 needs_login）。
    let Some(tail_raw) = menu_tail(screen) else { return false };
    let lines: Vec<String> = tail_raw.iter().map(|l| norm_line(l)).collect();
    flatten(&tail_raw.join("\n")).contains("choose the text style that looks best with your terminal")
        && line_starts_with(&lines, "1. auto (match terminal)")
        && (line_starts_with(&lines, "2. dark mode") || line_starts_with(&lines, "3. light mode"))
}

/// grok 1.0.34 開在沒信任過的目錄時跳「Do you trust the contents of this directory?」（y／n），
/// herdr 看不出是對話框，prompt 打進去會被吃掉（2026-09-17 使用者截圖，報 composer_unreadable）。
pub fn is_grok_trust_dialog(screen: &str) -> bool {
    // 同 [`is_switch_model_dialog`]：標題與兩個選項各自成行，句子裡提到不算；只看最底
    // [`DIALOG_TAIL_LINES`] 行；而且要求輸入列不是空的（[`composer_is_idle`]）——grok 回覆裡
    // 就算把這三段字逐行照抄（連「各自成行」這個形狀都模仿了），畫面上其實沒有真的框，
    // 稍後照樣會印出一行空的輸入列，用這個結構性事實分辨，不必再猜引文的排版像不像框
    // （2026-09-18，issue #114：`is_switch_model_dialog` 已經修過同一種誤判，這裡補齊）。
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail_raw = &lines[lines.len().saturating_sub(DIALOG_TAIL_LINES)..];
    if composer_is_idle(tail_raw) {
        return false;
    }
    let tail: Vec<String> = tail_raw.iter().map(|l| norm_line(l)).collect();
    line_starts_with(&tail, "do you trust the contents of this directory")
        && line_starts_with(&tail, "yes, proceed")
        && line_starts_with(&tail, "no, quit")
}

/// 確認框連同框線與 statusLine 的最大高度；再往上是正文。
const DIALOG_TAIL_LINES: usize = 12;

/// claude 開得起來、但憑證讀不到（`CLAUDE_CONFIG_DIR` 沒登入、Keychain 鎖著）時，每個回合都只回一行
/// `⎿  Not logged in · Please run /login`（畫面見測試用的 `screens::NOT_LOGGED_IN`）。跟登入選單不同，
/// 輸入列是空的、herdr 判 idle，所以送交辦的閘擋不到它（issue #420：協調者這樣停了 9 小時）。
/// 只認最底 [`MENU_TAIL_LINES`] 行裡、以 `⎿` 開頭的那一行：正文或引文裡提到這句不算。
/// 畫面在人從別的終端 `security unlock-keychain` 之後不會變，所以這只拿來**標狀態**，不拿來擋送出。
pub fn is_not_logged_in_reply(screen: &str) -> bool {
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    raw[raw.len().saturating_sub(MENU_TAIL_LINES)..]
        .iter()
        .any(|l| l.trim_start().starts_with('⎿') && norm_line(l).starts_with("not logged in") && flatten(l).contains("/login"))
}

/// 畫面上看得出這個 pane 要人登入（登入選單、onboarding、或回合只回 `Not logged in`）。讀不到畫面就當不是。
pub async fn shows_login_problem(app: &Arc<App>, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.clone() else { return false };
    let Some(client) = app.herdr_for_run(run).await else { return false };
    match client.pane_read(&pane, "visible", 80).await {
        Ok(r) => is_login_menu(&r.text) || is_onboarding_theme(&r.text) || is_not_logged_in_reply(&r.text),
        Err(_) => false,
    }
}

/// 讀不到畫面就當不是——那不是這裡要擋的事。
pub async fn stuck_at_login(app: &Arc<App>, run: &db::Run) -> bool {
    let Some(pane) = run.pane_id.clone() else { return false };
    let Some(client) = app.herdr_for_run(run).await else { return false };
    match client.pane_read(&pane, "visible", 80).await {
        Ok(r) => is_login_menu(&r.text) || is_onboarding_theme(&r.text),
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


/// 測試共用的真畫面／抄錄畫面（`lifecycle::prompt` 的整合測試也用）。
#[cfg(test)]
pub(crate) mod screens {
    /// 2026-09-22 build child 停在這裡的畫面（巡檢抄錄的原文，選項與提示逐字；框線照 2.1.278 的樣子）。
    pub const AUTO_MODE: &str = "\
 ╭──────────────────────────────────────────────────────────────────────────────╮
 │ Auto mode lets Claude handle permission prompts automatically. Claude will   │
 │ check each action against your settings and only ask when something looks    │
 │ risky.                                                                       │
 │                                                                              │
 │ ❯ Yes, set auto mode as my default permission mode                           │
 │   No, keep bypass permissions                                                │
 │                                                                              │
 │ Enter to confirm · Esc to cancel                                             │
 ╰──────────────────────────────────────────────────────────────────────────────╯
  build | agents-manager | Opus 5 | 5h:96%
";

    /// issue #420：協調者 2026-09-23 停了 9 小時的樣子（輸入列空著、herdr 判 idle，每個回合只回這一行）。
    pub const NOT_LOGGED_IN: &str = "\
 ▐▛███▜▌   Claude Code v2.1.280
▝▜█████▛▘  Opus 5 · Claude Max
  ▘▘ ▝▝    ~/.config/agents-manager/supervisor/AGM-responder

❯ [agents-manager daemon] 協調事件 3 則
  ⎿  Not logged in · Please run /login
   · Run in another terminal: security unlock-keychain

────────────────────────────────────────────────────────────────
❯ 
────────────────────────────────────────────────────────────────
  AGM-responder | Opus 5 H | 5h:- | 7d:-
";

    pub const ONBOARDING_THEME: &str = include_str!("lifecycle/fixtures/claude-2.1.278-onboarding-theme.txt");
    /// 2026-09-22 triage bot（2.1.280）的真回報：正文逐行引了 onboarding 主題頁原文，底下是空的輸入列＋statusline。
    /// daemon 對它每次送交辦都回 needs_login，交辦停在 queued。
    pub const REPORT_QUOTING_ONBOARDING: &str = include_str!("lifecycle/fixtures/claude-2.1.280-report-quoting-onboarding.txt");
}

#[cfg(test)]
mod tests {
    /// issue #420：協調者停在「Not logged in」——輸入列空著、herdr 判 idle，但每個回合都只回這一行。
    /// 要認得出來；正文引用這句、或更早的回合出過這句而後面又答過話，都不算。
    #[test]
    fn a_not_logged_in_reply_is_recognised_only_as_the_latest_error_line() {
        let stuck = super::screens::NOT_LOGGED_IN;
        assert!(super::is_not_logged_in_reply(stuck));
        // 正文提到這句（沒有 `⎿`）：不算。
        let quoted = "⏺ 協調者畫面是「Not logged in · Please run /login」，我已經請使用者處理。\n\n❯ \n  AGM | Opus 5 | 5h:80%\n";
        assert!(!super::is_not_logged_in_reply(quoted));
        // 很久以前出過、之後答過很多話（已經捲出底部）：不算。
        let recovered = format!("{stuck}{}", "⏺ 已處理一則申請。\n".repeat(30));
        assert!(!super::is_not_logged_in_reply(&recovered));
    }

    #[test]
    fn grok_trust_dialog_is_recognised_even_when_centred_and_wrapped() {
        let screen = "  main ~/p/h/projects/rt\n\n⠀⠀⠀⠀⠀⠀⣀⣀⡀\nDo you trust the contents of this directory?\n                /Users/m4p/project/hermes-agents/projects/rt\n\nGrok Build may run or modify contents in this directory,\n              posing security risks.\n\nYes, proceed                 y\n                  No, quit                     n\n\nGrok Build  1.0.34 [stable]\n";
        assert!(super::is_grok_trust_dialog(screen));
        assert!(!super::is_grok_trust_dialog("> Do you trust the contents of this directory? I asked grok that yesterday."));
    }

    /// grok bot 在回覆裡逐行引用這個對話框的三段字（例如報告自己怎麼處理這個誤判），輸入列其實還
    /// 空著等打字，不是真的有框；`line_starts_with`（標題／選項各自成行）擋不住這種排版，得靠
    /// [`composer_is_idle`] 這個結構性事實才分得出來（issue #114）。
    #[test]
    fn a_reply_quoting_the_grok_trust_dialog_is_not_the_dialog_itself() {
        let quoted = "\
⏺ grok 的 trust 對話框長這樣，三段字缺一不可：
  Do you trust the contents of this directory?
  Yes, proceed
  No, quit
  我已經在 pane_ready_for_prompt 補上判斷，關掉之後再送 prompt。
────────────────────
❯
────────────────────
  15m2dg | agents-manager | grok | 5h:96%
";
        assert!(!super::is_grok_trust_dialog(quoted), "畫面上沒有真的框，只是回覆引了原文");
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

    /// 這顆 bot 回報完上面那個修正之後的真畫面尾段（2026-09-18，w168:p91）：畫面上沒有框，只是
    /// 正文引了框裡的三段字，舊的整段 `contains` 就中了——之後每則 prompt 都被擋成 409
    /// `dialog_open`，只能用 herdr 直送才進得來。
    const QUOTED_REPORT: &str = "\
  ⏺ 原因：claude 2.1.x 的 /effort 跳的是跟 /model 同一種確認框，但標題是「Change effort level?」。
  - 改動：daemon/src/tui_prompts.rs 的偵測改成 \"switch model?\" || \"change effort level?\"，另兩個條件（yes, switch to / no, go back）不變；加了一條用截圖真畫面的測試。
  - 驗證：cargo test -p agents-managerd tui_prompts → 8 passed 0 failed。
  - 沒動 lifecycle.rs（有別人未提交的改動），所以函式名仍是 is_switch_model_dialog、提示字仍寫「Switch model?」。
  - 要生效需重建 release 並重啟 daemon，這要 AGM 核准，我沒有執行。
────────────────────
❯
────────────────────
  15m2dg | agents-manager | Opus 5 31% | 5h:96%
  ⏵⏵ bypass permissions on
";

    #[test]
    fn a_report_quoting_the_dialog_is_not_a_dialog() {
        assert!(!is_switch_model_dialog(QUOTED_REPORT));
        // 這份原始碼本身也引了那幾段字。
        assert!(!is_switch_model_dialog(include_str!("tui_prompts.rs")));
    }

    /// issue #114：`a_report_quoting_the_dialog_is_not_a_dialog` 擋住的是「正文中間夾雜引文」；
    /// 但只認「標題／選項各自成行」擋不住把三段字逐行照抄成單獨一行（連編號都照抄），跟真的框
    /// 長得一模一樣——這裡故意這樣排版，證明沒有輸入列閒置這個結構性判斷會被騙過去。
    #[test]
    fn quoting_the_dialog_verbatim_line_by_line_is_still_not_a_dialog() {
        let verbatim = "\
⏺ 這個框長這樣，三段字缺一不可：
  Switch model?
  1. Yes, switch to Haiku 4.5
  2. No, go back
  我已經在 pane_ready_for_prompt 補上判斷，關掉之後再送 prompt。
────────────────────
❯
────────────────────
  15m2dg | agents-manager | Opus 5 | 5h:96%
";
        assert!(!is_switch_model_dialog(verbatim), "畫面上沒有真的框，只是回覆逐行引了原文");
    }

    #[test]
    fn composer_is_idle_only_when_the_prompt_cursor_stands_alone() {
        assert!(composer_is_idle(&["❯"]));
        assert!(composer_is_idle(&["│ ❯ │"]), "框線與空白不算內容");
        assert!(composer_is_idle(&["›"]));
        assert!(!composer_is_idle(&["❯ 1. Yes, switch to Haiku 4.5"]), "游標旁還有選項文字＝框還開著");
        assert!(!composer_is_idle(&["some other line"]));
        assert!(!composer_is_idle(&[]));
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

    use super::screens::{AUTO_MODE, ONBOARDING_THEME, REPORT_QUOTING_ONBOARDING};

    #[test]
    fn the_auto_mode_offer_is_recognised_and_quotes_are_not() {
        assert!(is_auto_mode_offer(AUTO_MODE));
        // 正文引了兩個選項、但輸入列空著＝沒有框。
        let quoted = format!("⏺ 2.1.278 的框長這樣：\n  Yes, set auto mode as my default permission mode\n  No, keep bypass permissions\n{}", IDLE_CLAUDE);
        assert!(!is_auto_mode_offer(&quoted));
        assert!(!is_auto_mode_offer(SWITCH_MODEL) && !is_auto_mode_offer(PERMISSION));
    }

    #[test]
    fn the_onboarding_theme_page_counts_as_stuck_at_login_not_a_dialog_to_answer() {
        assert!(is_onboarding_theme(ONBOARDING_THEME), "真畫面（2.1.278，全新 CLAUDE_CONFIG_DIR）");
        assert!(!is_login_menu(ONBOARDING_THEME));
        assert!(!is_onboarding_theme("⏺ run /theme to choose the text style that looks best with your terminal\n❯\n"));
        assert!(!is_auto_mode_offer(ONBOARDING_THEME));
    }

    /// 正文引了整頁原文、輸入列空著：不是選單。登入選單同一條規則。
    #[test]
    fn a_report_that_quotes_the_onboarding_page_is_not_the_page() {
        assert!(!is_onboarding_theme(REPORT_QUOTING_ONBOARDING));
        assert!(!is_login_menu(REPORT_QUOTING_ONBOARDING));
        let quoted_login = format!("⏺ 登入選單長這樣：\n  Select login method:\n  ❯ 1. Claude account with subscription · Pro, Max\n    2. Anthropic Console account · API usage billing\n{}", IDLE_CLAUDE);
        assert!(!is_login_menu(&quoted_login));
        // 引文在正文上方、底下正在跑（沒有空輸入列）：選單不在最底幾行，也不算。
        let far_above = format!("{}\n{}", REPORT_QUOTING_ONBOARDING.split("0 tokens").next().unwrap(), "  ⏺ Bash(cargo test)\n  ⎿  running…\n".repeat(12));
        assert!(!is_onboarding_theme(&far_above));
    }

    const IDLE_CLAUDE: &str = "────────────────────\n❯\n────────────────────\n  15m2dg | agents-manager | Opus 5 31% | 5h:96%\n";
}
