//! 認得出來、不該讓使用者操心的 TUI 對話框（主要是 Claude Code 的滿意度問卷）。
//!
//! 問卷會讓 agent 卡在 `blocked`；使用者的決定是**一律選 `0: Dismiss`**，daemon 自己按掉。
//! 事件（[`crate::events`]）與巡邏（[`spawn_survey_watcher`]）兩條都接：訂閱前跳出、事件漏掉、
//! herdr 沒判成 blocked 時只剩巡邏。[`crate::quota_claude`] 探測 pane 也用：它會按 Enter，
//! 落在問卷上等於替使用者打分數。
//! 認畫面不認狀態：權限確認、trust 對話框等原封不動留給使用者。

use crate::db;
use crate::herdr::{HerdrClient, PaneRead};
use crate::state::App;
use am_core::{PaneReadSource, SessionId};
use am_ports::RunPaneReader;
use std::sync::Arc;
use std::time::Duration;

pub use crate::lifecycle::app_ports_p4::shows_login_problem;

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
        let mut chars = l.chars().filter(|c| !c.is_whitespace() && !"│┃╭╮╰╯─━▔".contains(*c));
        matches!(chars.next(), Some('❯') | Some('›')) && chars.next().is_none()
    })
}

/// 問句要由 `●` / `>` 開頭且相鄰幾行內有至少三個選項，避免 agent 引用這些字串時誤認。
/// 這一行是不是**選項列**：`N: 標籤` 之外幾乎沒有別的字。真的問卷是
/// `1: Bad  2: Fine  3: Good  0: Dismiss` 單獨一列（窄 pane 折成兩列也還是只有選項）；
/// 散文裡提到那些字一定還夾著別的句子。
///
/// 這一條取代了原本「該行要以 `● ` 或 `> ` 開頭」的守衛（#485）：`●` 正是 Claude 自己印助理
/// 訊息的項目符號，用 agent 自己的前綴去排除 agent 的引文等於沒有排除。
fn is_option_row(line: &str) -> bool {
    let present = survey_options(line);
    if !present.iter().any(|p| *p) {
        return false;
    }
    let mut rest = flatten(line);
    for (n, label) in [(0usize, "dismiss"), (1, "bad"), (2, "fine"), (3, "good")] {
        for sep in [" ", ""] {
            rest = rest.replace(&format!("{n}:{sep}{label}"), " ");
        }
    }
    rest.split_whitespace().collect::<String>().chars().count() <= 8
}

/// 問卷的最後一列選項**正下方**（跳過框線）是不是一個**空的**輸入列。
///
/// 2.1.281 真畫面（`claude-2.1.281-feedback-survey.txt`，#485）：問卷畫在輸入框正上方，輸入列空著；
/// 一打字問卷就收掉，所以真的問卷只存在於「底下是空輸入列」的時候。回覆裡照抄問卷時，問卷與輸入框之間
/// 還隔著回合結束那一行 `✻ … for Ns · done`（`claude-2.1.281-feedback-survey-quoted.txt`）；
/// 按下的 `0` 如果其實落進輸入列（`❯ 0`），這一條也就不再成立——不會再補 Enter、下一輪也不會再按。
fn sits_on_empty_composer(below: &[&str]) -> bool {
    below
        .iter()
        .filter(|l| !crate::claude_mode::is_mode_row(l))
        .map(|l| l.chars().filter(|c| !"│┃╭╮╰╯─━▔ \t".contains(*c)).collect::<String>())
        .find(|rest| !rest.is_empty())
        .is_some_and(|rest| matches!(rest.as_str(), "❯" | "›" | ">"))
}

#[cfg(test)]
mod mode_row_tests {
    use super::*;

    /// #788：模式列夾在問卷與空輸入列之間，不能把「底下是空框」判掉。2.1.288 default 真畫面那一行。
    #[test]
    fn a_manual_mode_row_does_not_hide_the_empty_composer_under_a_survey() {
        let row = include_str!("lifecycle/fixtures/claude-2.1.288-manual-mode-finished.txt")
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap();
        assert!(row.contains("manual mode on"), "{row}");
        assert!(sits_on_empty_composer(&[row, "❯"]));
    }
}

pub fn is_feedback_survey(screen: &str) -> bool {
    // 原文與壓平後的版本一起留著：兩個 tail 要對齊同一批行——`composer_is_idle` 要看原文
    // （`❯` 會被 `flatten` 留著，但框線得先去掉），問句與選項看壓平後的。
    let kept: Vec<(&str, String)> = screen.lines().map(|l| (l, flatten(l))).filter(|(_, f)| !f.is_empty()).collect();
    let from = kept.len().saturating_sub(SURVEY_TAIL_LINES);
    // **不能**套其他偵測的「輸入列空著就不是框」（[`composer_is_idle`]）：2.1.281 的真問卷底下就是一個空的
    // 輸入列（`claude-2.1.281-feedback-survey.txt`），那道守衛會把真的問卷全部擋掉。分辨引文改看
    // 問卷正下方是什麼（[`sits_on_empty_composer`]）。
    let raw_tail: Vec<&str> = kept[from..].iter().map(|(l, _)| *l).collect();
    let tail: Vec<String> = kept[from..].iter().map(|(_, f)| f.clone()).collect();
    let tail = &tail[..];
    let question = "how is claude doing";

    for start in 0..tail.len() {
        for end in (start + 1)..=tail.len().min(start + SURVEY_QUESTION_LINES) {
            if !tail[start..end].join(" ").contains(question) {
                continue;
            }
            let option_end = tail.len().min(end + SURVEY_OPTION_GAP);
            let mut found = [false; 4];
            let mut last_row = None;
            // 只採計「看起來就是選項列」的行：散文裡把選項寫進句子中間不算（`is_option_row`）。
            for i in (start..option_end).filter(|&i| is_option_row(&tail[i])) {
                for (slot, present) in survey_options(&tail[i]).into_iter().enumerate() {
                    found[slot] |= present;
                }
                last_row = Some(i);
            }
            if found.into_iter().filter(|present| *present).count() >= 3
                && last_row.is_some_and(|i| sits_on_empty_composer(&raw_tail[i + 1..]))
            {
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
///
/// **40 而不是 20（#483）**：量過 repo 裡的兩張真 fixture——真的 onboarding 頁
/// （`claude-2.1.278-onboarding-theme.txt`）與 bot 逐行引用它的回報
/// （`claude-2.1.280-report-quoting-onboarding.txt`）——在偵測用的每個標記上**幾何完全一樣**：
/// 非空行都是 32、標題都在距底部第 16 行、`1. auto…` 都在第 14 行。也就是說窗口本身
/// **沒有在分辨真假**，擋住引文那張的自始至終是 [`composer_is_idle`]（它的空輸入列在距底部第 4 行，
/// 任何 ≥4 的窗口都看得到，放寬不會讓那個誤判回來）。
///
/// 窗口原本是 20，對真畫面只剩 4 行餘裕：新版多一句提示、窄 pane 把預覽折幾行、或底部多一列
/// statusLine，標題就掉出窗口，`is_onboarding_theme`／`is_login_menu` **靜默**失效——沒有 log、
/// 沒有 health 欄位，只會以「交辦被送進登入畫面、回合掛著」的形式出現（#420 那個形狀）。
const MENU_TAIL_LINES: usize = 40;

/// 對話／工具輸出那幾種行首符號。選單底下不該再有這些——有的話那是正文引文，不是開著的選單。
fn is_agent_output_line(line: &str) -> bool {
    matches!(line.trim_start().chars().next(), Some('⏺') | Some('●') | Some('⎿') | Some('✻'))
}

/// 選單是不是畫面**最底下**那個 UI：最後一個 `N.` 選項之後，不能再有對話／工具輸出。
///
/// 窗口從 20 放寬到 40 之後（[`MENU_TAIL_LINES`]），需要這一條來擋「引文在上面、底下還在跑」的畫面：
/// 那種情況輸入列不是空的（正在跑），[`composer_is_idle`] 幫不上忙，原本純粹是靠 20 行窗口把引文
/// 推出範圍外才沒中——而那個窗口同時也是 #483 的脆弱點。真的選單底下只會有它自己的預覽與提示
/// （2.1.278 真畫面是一段程式碼預覽＋`Syntax theme:` 一行），不會有 `⏺`／`⎿` 這種對話輸出。
fn menu_is_bottom_most(tail: &[&str]) -> bool {
    let is_option = |l: &&str| {
        let n = norm_line(l);
        let mut c = n.chars();
        c.next().is_some_and(|d| d.is_ascii_digit()) && c.next() == Some('.')
    };
    let Some(last) = tail.iter().rposition(is_option) else { return false };
    !tail[last + 1..].iter().any(|l| is_agent_output_line(l))
}

/// 畫面最底 [`MENU_TAIL_LINES`] 個非空行；輸入列空著（[`composer_is_idle`]）、或選單底下還有對話輸出
/// （[`menu_is_bottom_most`]）就回 `None`——那兩種情況畫面上都不可能有選單在擋。
fn menu_tail(screen: &str) -> Option<Vec<&str>> {
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = raw[raw.len().saturating_sub(MENU_TAIL_LINES)..].to_vec();
    (!composer_is_idle(&tail) && menu_is_bottom_most(&tail)).then_some(tail)
}

/// claude 狀態列靠右的 `✔ Update installed · Restart to update`。回固定字而非整行：左半是每回合
/// 都變的 statusLine，存 DB 會一直 emit。只看最底 [`TAIL_LINES`] 行：正文引文也會中（2026-09-08 實測）。
pub fn update_notice(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    let t = flatten(&tail);
    (t.contains("update installed") && t.contains("restart to update")).then(|| UPDATE_NOTICE.to_string())
}

/// statusLine 那行加底下的權限模式列（[`crate::capture::claude::is_mode_row`]）一行，窄 pane 折行也在範圍內。
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

/// Codex 0.157.0's startup upgrade screen for legacy models (including `gpt-5.6-*`).
/// It uses the full-screen migration flow, so the migration copy, both choices, and footer are
/// all visible together. Treat it as a user decision: callers must leave it open and block input.
pub fn is_codex_model_migration_prompt(screen: &str) -> bool {
    const TAIL_LINES: usize = 20;
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail_raw = &raw[raw.len().saturating_sub(TAIL_LINES)..];
    if tail_raw.is_empty() || composer_is_idle(tail_raw) {
        return false;
    }
    let tail: Vec<String> = tail_raw.iter().map(|l| norm_line(l)).collect();
    let copy = tail.iter().any(|l| {
        l.starts_with("meet gpt-6 sol")
            || l.starts_with("meet gpt-6 luna")
            || l.starts_with("codex just got an upgrade. introducing")
            || l.starts_with("gpt-5.4 is no longer available")
    });
    let footer = migration_footer(&tail);
    let has_migration_choices =
        line_starts_with(&tail, "1. try new model") && line_starts_with(&tail, "2. use existing model");
    copy && footer.is_some() && (has_migration_choices || footer == Some("continue"))
}

/// `enter/esc confirm · ctrl+c quit`（或 `continue`）。窄 pane 在 `·` 後面折行時，上一行仍以 `enter/esc …` 開頭、下一行是 `ctrl+c …`。
fn migration_footer(tail: &[String]) -> Option<&'static str> {
    fn kind(line: &str) -> Option<&'static str> {
        if line.starts_with("enter/esc confirm") {
            Some("confirm")
        } else if line.starts_with("enter/esc continue") {
            Some("continue")
        } else {
            None
        }
    }
    if let Some(k) = tail.last().and_then(|l| kind(l)) {
        return Some(k);
    }
    if tail.len() >= 2 {
        let prev = &tail[tail.len() - 2];
        let last = tail.last().unwrap();
        if kind(prev).is_some() && last.starts_with("ctrl+c") {
            return kind(prev);
        }
    }
    None
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

/// 編號選單最多這麼高（含說明、`Details:` 與折行）；最後一個選項之下最多再有 [`MENU_BELOW_LINES`] 行
/// （腳註、`✻ Waiting for API response …`、statusLine）。
const CHOICE_MENU_TAIL_LINES: usize = 18;
const MENU_BELOW_LINES: usize = 6;
/// Pane readers cap `visible` at 80 rows; keep the whole active frame when its title is still visible above a tall body.
const DIALOG_FRAME_TAIL_LINES: usize = 80;

fn is_solid_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 10 && t.chars().all(|c| c == '─')
}

/// 這一行是不是選項列：`N. 標籤`，前面可以有游標（`❯`／`›`／`>`）。回（編號, 有沒有游標）。
fn menu_row(line: &str) -> Option<(u32, bool)> {
    let body = line.trim().trim_start_matches(['│', '┃', '▎']).trim_start();
    let (cursor, body) = match body.chars().next() {
        Some(c @ ('❯' | '›' | '>')) => (true, body[c.len_utf8()..].trim_start()),
        _ => (false, body),
    };
    let digits: String = body.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let rest = &body[digits.len()..];
    let label = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    if !label.starts_with(' ') || label.trim().is_empty() {
        return None;
    }
    Some((digits.parse().ok()?, cursor))
}

/// 畫面最底下是不是一份**開著、等人選**的編號選單；是就回它在 `tail` 裡的（第一個、最後一個選項）位置。
///
/// 開著的選單長這樣：從 `1.` 起連號、至少兩項、**剛好一個**選項帶游標、最後一項底下只剩腳註／spinner／statusLine
/// （沒有 `⏺`／`●`／`⎿` 這種對話輸出），而且輸入列不是空的（[`composer_is_idle`]）——回覆裡照抄一份選單，
/// 回合結束後底下一定還會印出空的輸入列。
fn open_choice_menu(tail: &[&str]) -> Option<(usize, usize)> {
    if tail.is_empty() || composer_is_idle(tail) {
        return None;
    }
    let last = tail.iter().rposition(|l| menu_row(l).is_some())?;
    // 模式列是固定 chrome（#788），不佔「選單底下還有幾行」的額度。
    let below: Vec<&&str> = tail[last + 1..].iter().filter(|l| !crate::claude_mode::is_mode_row(l)).collect();
    if below.len() > MENU_BELOW_LINES || below.iter().any(|l| is_agent_output_line(l) && !l.trim_start().starts_with('✻')) {
        return None;
    }
    let (mut want, mut cursors, mut first, mut gap) = (menu_row(tail[last])?.0, 0, last, 0);
    for i in (0..=last).rev() {
        let Some((n, cursor)) = menu_row(tail[i]) else {
            // 選項之間的說明、折行；撞到對話輸出或夾太多行＝走出選單了。
            gap += 1;
            if is_agent_output_line(tail[i]) || gap > MENU_BELOW_LINES {
                break;
            }
            continue;
        };
        if n != want {
            break;
        }
        cursors += usize::from(cursor);
        (first, gap) = (i, 0);
        if n == 1 {
            break;
        }
        want -= 1;
    }
    let count = menu_row(tail[last])?.0;
    (menu_row(tail[first]).map(|(n, _)| n) == Some(1) && count >= 2 && cursors == 1).then_some((first, last))
}

/// 終端備援（`poller::try_fallback`）用：畫面停在**任何**等人選的編號選單上（權限框、AskUserQuestion、
/// `Session paused`…）。那不是回合結束，選單那幾行更不是回覆（2026-09-25 cf-ox-fork-fork：herdr 判 idle，
/// 備援把 `2. Edit prompt and retry with …` 存成了回覆）。
pub fn awaits_menu_choice(screen: &str) -> bool {
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    open_choice_menu(&raw[raw.len().saturating_sub(CHOICE_MENU_TAIL_LINES)..]).is_some() || is_held_message_prompt(screen)
}

/// 2.1.287 的 held message 框：別的 session 用 SendMessage 送來、但兩邊權限模式不同，訊息被扣住等使用者決定（#775）。
/// 長這樣：實線下一行 `Held message from another session`、來源與說明、`Message body …:`、兩條 `╌` 虛線夾住訊息內文，
/// 最底是**沒有編號**的兩個選項 `❯ Deny — …`／`Deliver this message to Claude`，[`open_choice_menu`] 認不到。
///
/// 要同時有：標題那一行（正上方是實線）、標題之後兩個選項各自成行而且剛好一個帶游標、最後一個選項底下只剩腳註
/// （沒有對話輸出）、輸入列不是空的——回覆裡引用這段原文時底下一定還有空的輸入列。真畫面在
/// `lifecycle/fixtures/claude-2.1.287-held-message.txt`、`claude-2.1.287-bash-with-held-queued.txt`（排在 Bash 框後面時不算開著）。
pub(crate) fn held_message_prompt_start(screen: &str) -> Option<usize> {
    let raw: Vec<(usize, &str)> = screen.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()).collect();
    let tail = &raw[raw.len().saturating_sub(DIALOG_FRAME_TAIL_LINES)..];
    let lines: Vec<&str> = tail.iter().map(|(_, l)| *l).collect();
    if composer_is_idle(&lines) {
        return None;
    }
    // The permission body is untrusted text. Prefer the outer title immediately below its solid rule, not a repeated
    // title inside the `╌`-delimited message body. Its real frame is the first matching title after transcript output.
    let after_output = tail.iter().rposition(|(_, l)| is_agent_output_line(l)).map_or(0, |i| i + 1);
    let Some(title) = (after_output..tail.len()).find(|&i| norm_line(tail[i].1) == "held message from another session" && i > 0 && is_solid_rule(tail[i - 1].1)) else {
        return None;
    };
    let after = &lines[title + 1..];
    // 選項列：游標（可有可無）＋選項字，跟 `norm_line` 不同的是游標要留著數。
    let option = |l: &str, label: &str| -> Option<bool> {
        let body = l.trim();
        let (cursor, body) = match body.chars().next() {
            Some(c @ ('❯' | '›' | '>')) => (true, body[c.len_utf8()..].trim_start()),
            _ => (false, body),
        };
        body.to_lowercase().starts_with(label).then_some(cursor)
    };
    let Some(deny) = after.iter().rposition(|l| option(l, "deny").is_some()) else { return None };
    let Some(deliver) = after.iter().rposition(|l| option(l, "deliver this message").is_some()) else { return None };
    let last = deny.max(deliver);
    let cursors = usize::from(option(after[deny], "deny") == Some(true)) + usize::from(option(after[deliver], "deliver this message") == Some(true));
    if cursors != 1 || after.len() - 1 - last > MENU_BELOW_LINES || after[last + 1..].iter().any(|l| is_agent_output_line(l)) {
        return None;
    }
    Some(tail[title].0)
}

pub fn is_held_message_prompt(screen: &str) -> bool {
    held_message_prompt_start(screen).is_some()
}

/// Claude Code 2.1.281 的「Session paused」選單（API 拒答或額度用完後，問要換模型重試還是改 prompt／改用額度）。
/// herdr 判成 `idle`，網頁不會彈出選項；[`crate::session_paused`] 補標成 `blocked`，**一個鍵都不按**，讓使用者自己選。
/// 標題要自己一行、在選單正上方的同一個框裡（中間不能夾對話輸出）。真畫面（只取選單那段）在
/// `lifecycle/fixtures/claude-2.1.281-session-paused.txt`。
pub fn is_session_paused_menu(screen: &str) -> bool {
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &raw[raw.len().saturating_sub(CHOICE_MENU_TAIL_LINES)..];
    let Some((first, _)) = open_choice_menu(tail) else { return false };
    tail[..first]
        .iter()
        .rev()
        .take_while(|l| !is_agent_output_line(l))
        .any(|l| norm_line(l) == "session paused")
}

/// claude 一般的權限確認選單（Bash／Write／Edit／Fetch／Read／MCP…）停在畫面尾巴等人選：回工具名（`Bash`、`Write`、`Fetch`、`MCP`…）。
/// 給 blocked 的結構化原因用（`blocked_reason::observe`，「等待權限確認：Bash」）；**只是分類，不按任何鍵**。
/// `Tool use` 框分得出 MCP 與其他工具（[`tool_use_name`]）；跨 session 的 held message 框回 `Held message`（[`is_held_message_prompt`]）。
///
/// 要同時有：尾巴是等人選的編號選單（輸入列不是空的）、選單上方有一條實線（`────`）框出來的標題行（`Bash command`、`Create file`、
/// `Fetch`、`Read file  1 of 3`、`Tool use`…）、標題後面有 `Do you want to …` 那句問題。防誤刪框、Session paused、auto mode、
/// 切換模型、問卷是別的選單，不算；回覆裡逐行引用原文時底下有空的輸入列，也不算。真畫面在 `lifecycle/fixtures/claude-2.1.28[67]-*-permission*.txt`。
pub fn permission_prompt(screen: &str) -> Option<String> {
    if is_held_message_prompt(screen) {
        return Some("Held message".to_string());
    }
    if !awaits_menu_choice(screen)
        || dangerous_rm_prompt(screen).is_some()
        || is_session_paused_menu(screen)
        || is_auto_mode_offer(screen)
        || is_switch_model_dialog(screen)
        || is_feedback_survey(screen)
    {
        return None;
    }
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let frame = &raw[raw.len().saturating_sub(DIALOG_FRAME_TAIL_LINES)..];
    let menu_from = frame.len().saturating_sub(CHOICE_MENU_TAIL_LINES);
    let (menu_first, _) = open_choice_menu(&frame[menu_from..])?;
    let menu_start = menu_from + menu_first;
    // 真正的 frame 從最新工具輸出列之後的第一條實線算起（多框時最舊在上）。標題與目前選項之間若還有
    // 另一份連號、恰好一個游標的選單，那條實線是上一份選單的，不能借它的標題；改看下一條實線。
    // 命令預覽裡恰好一行 `1. …` 不夠證明是另一份選單。
    let after_output = frame[..menu_start].iter().rposition(|l| is_agent_output_line(l)).map_or(0, |i| i + 1);
    let rule = (after_output..menu_start).find(|&i| {
        if !is_solid_rule(frame[i]) || i + 1 >= menu_start {
            return false;
        }
        let body = &frame[i + 1..menu_start];
        if !body.iter().any(|l| norm_line(l).starts_with("do you want to")) {
            return false;
        }
        let preceding_rows: Vec<(u32, bool)> = frame[i + 2..menu_start].iter().filter_map(|line| menu_row(line)).collect();
        let has_previous_menu = preceding_rows.windows(2).any(|pair| pair[1].0 == pair[0].0 + 1)
            && preceding_rows.iter().filter(|(_, cursor)| *cursor).count() == 1;
        !has_previous_menu
    })?;
    let title_line = frame.get(rule + 1)?;
    // `Read file                  1 of 3`：標題與計數之間隔著一大段空白，只取前半。
    let title = title_line.trim().split("  ").next().unwrap_or("").trim();
    let low = title.to_lowercase();
    let tool = if low.starts_with("bash") {
        "Bash".to_string()
    } else if low.starts_with("create file") || low.starts_with("write") {
        "Write".to_string()
    } else if low.starts_with("edit") || low.starts_with("update file") {
        "Edit".to_string()
    } else if low.starts_with("read") {
        "Read".to_string()
    } else if low.starts_with("fetch") {
        "Fetch".to_string()
    } else if low == "tool use" {
        tool_use_name(&frame[rule + 2..menu_start])
    } else if !title.is_empty() && title.chars().count() <= 40 {
        title.to_string()
    } else {
        return None;
    };
    Some(tool)
}

/// `Tool use` 框（MCP 與其他沒有專屬畫面的工具，2.1.287 起工具呼叫夾在兩條 `╌` 虛線之間，#775）是哪個工具。
/// MCP：標題下一行是 `demo — Echo Tool: (MCP)`，回 `MCP`。其他工具（如 WebSearch）：虛線之間第一行是
/// `Web Search("…")`，取括號前的名字；認不出來就回 `Tool use`，不再一律當成 MCP。`body` 從標題下一行起算。
fn tool_use_name(body: &[&str]) -> String {
    let dashes = |l: &&str| {
        let t = l.trim();
        !t.is_empty() && t.chars().all(|c| c == '╌')
    };
    let open = body.iter().position(dashes);
    if body[..open.unwrap_or(body.len())].iter().any(|l| l.trim_end().ends_with("(MCP)")) {
        return "MCP".to_string();
    }
    let call = open.and_then(|i| body.get(i + 1)).map(|l| l.trim()).unwrap_or("");
    match call.split_once('(') {
        Some((name, _)) if !name.trim().is_empty() && name.trim().chars().count() <= 40 => name.trim().to_string(),
        _ => "Tool use".to_string(),
    }
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
    // 同 [`is_switch_model_dialog`]：標題與兩個帶鍵位的選項各自成行，句子裡提到不算；只看最底
    // [`DIALOG_TAIL_LINES`] 行；而且要求輸入列不是空的（[`composer_is_idle`]）——grok 回覆裡
    // 就算把這三段字逐行照抄（連「各自成行」這個形狀都模仿了），畫面上其實沒有真的框，
    // 稍後照樣會印出一行空的輸入列，用這個結構性事實分辨，不必再猜引文的排版像不像框
    // （2026-09-18，issue #114：`is_switch_model_dialog` 已經修過同一種誤判，這裡補齊）。
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail_raw = &lines[lines.len().saturating_sub(DIALOG_TAIL_LINES)..];
    if composer_is_idle(tail_raw) {
        return false;
    }
    // Grok marks its own transcript/tool output with these prefixes. They are stripped by
    // `norm_line`, so reject them on the raw rows before an answer that quotes the complete
    // dialog (including the y/n keys and build row) can look like the live trust screen.
    if tail_raw.iter().any(|line| {
        let line = line.trim_start();
        ["⏺", "●", "⎿"].iter().any(|marker| line.starts_with(marker))
    }) {
        return false;
    }
    let tail: Vec<String> = tail_raw.iter().map(|l| norm_line(l)).collect();
    line_starts_with(&tail, "do you trust the contents of this directory")
        && exact_or_wrapped_pair(&tail, "yes, proceed y")
        && exact_or_wrapped_pair(&tail, "no, quit n")
        && tail.iter().any(|l| {
            l.strip_prefix("grok build ")
                .and_then(|v| v.split_whitespace().next())
                .is_some_and(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()))
        })
}

/// 選項整行是 `yes, proceed y`。窄 pane 把鍵位折到下一行時，相鄰兩行接起來仍是那一句。
fn exact_or_wrapped_pair(lines: &[String], exact: &str) -> bool {
    lines.iter().any(|l| l == exact) || lines.windows(2).any(|w| format!("{} {}", w[0], w[1]) == exact)
}

/// 確認框連同框線與 statusLine 的最大高度；再往上是正文。
const DIALOG_TAIL_LINES: usize = 12;

/// claude 開得起來、但憑證讀不到（`CLAUDE_CONFIG_DIR` 沒登入、Keychain 鎖著）時，每個回合都只回一行
/// `⎿  Not logged in · Please run /login`（畫面見測試用的 `screens::NOT_LOGGED_IN`）。跟登入選單不同，
/// 輸入列是空的、herdr 判 idle，所以送交辦的閘擋不到它（issue #420：協調者這樣停了 9 小時）。
/// 只認最底 [`LATEST_REPLY_TAIL_LINES`] 行裡、以 `⎿` 開頭的那一行：正文或引文裡提到這句不算。
/// 畫面在人從別的終端 `security unlock-keychain` 之後不會變，所以這只拿來**標狀態**，不拿來擋送出。
pub fn is_not_logged_in_reply(screen: &str) -> bool {
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    raw[raw.len().saturating_sub(LATEST_REPLY_TAIL_LINES)..]
        .iter()
        .any(|l| l.trim_start().starts_with('⎿') && norm_line(l).starts_with("not logged in") && flatten(l).contains("/login"))
}

/// 「這一行是不是**最近一個**回合印的」的範圍。跟 [`MENU_TAIL_LINES`] 是兩件事：那個問的是
/// 「選單有多高」（#483 把它放寬到 40 以吸收版本長高），這個問的是「多久以前算太久」——
/// 之後又答過很多話就代表人已經處理掉了，不該再標成停在登入問題（issue #420 的測試釘住這一點）。
/// 兩者以前共用同一個常數，放寬選單窗口時會連帶把這裡的「最近」也放寬。
const LATEST_REPLY_TAIL_LINES: usize = 20;

/// `Some(true/false)` means the screen was read; `None` means it is unknown.
pub async fn shows_login_problem_with_reader<R: RunPaneReader>(
    reader: &R,
    bot_id: &am_core::BotId,
    run_session: Option<&SessionId>,
    pane_id: Option<&str>,
) -> Option<bool> {
    let pane = pane_id?.trim();
    if pane.is_empty() {
        return None;
    }
    let Ok(Some(screen)) = reader
        .read_run_pane(bot_id, run_session, pane, PaneReadSource::Visible, 80)
        .await
    else {
        return None;
    };
    Some(is_login_menu(&screen) || is_onboarding_theme(&screen) || is_not_logged_in_reply(&screen))
}

/// Claude Code 2.1.281 起的「Dangerous rm operation」確認框（`rm -rf $(…)`、`$VAR`、頂層目錄這類目標）：
/// 就算帶 `--dangerously-skip-permissions` 也會跳，約 2 分鐘沒人回答就自動拒絕（那個指令不會執行，回合照常往下走）。
/// 它是**防誤刪**的：daemon 只認、只通知，一個鍵都不替它按，送達也不能打進框裡（`pane_ready_for_prompt`、
/// [`crate::dangerous_rm`]）。真畫面 `lifecycle/fixtures/claude-2.1.281-dangerous-rm.txt`（2026-09-24）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerousRm {
    /// 警語原文（`Dangerous rm operation on …: <目標>`），折行接回一行。
    pub warning: String,
    /// 警語冒號後面那段，也就是 claude 認定的目標（路徑、`command substitution output`、變數路徑連同它所在的那段 rm）。
    pub target: String,
    /// 框上方 `Bash command` 裡的指令（逐行），讀不到是 `None`。
    pub command: Option<String>,
    /// 指令很長、pane 又矮時，框的上緣（`Bash command` 標題與上面那條虛線）已經捲出畫面：`command` 只是畫面上看得到的後半段。
    pub command_truncated: bool,
}

/// 警語、倒數、問句與兩個選項最多這麼高（倒數那句在窄 pane 會折兩行、警語也會折）。
const RM_DIALOG_TAIL_LINES: usize = 16;
/// 從警語往上找 `Bash command` 標題最多找這麼多行（長指令、heredoc 可以上百行；2.1.289 的 python heredoc 就超過舊值 24）。
const RM_COMMAND_LOOKBACK: usize = 200;

/// 框線、行首的 `│` 與前後空白拿掉，其餘原樣（大小寫與路徑要留著給人看）。
fn strip_box(line: &str) -> String {
    line.trim().trim_start_matches(['│', '┃', '▎']).trim().trim_end_matches(['│', '┃']).trim().to_string()
}

/// 指令列：只拿掉行首的 `│` 欄與它後面那一個空白，指令自己的縮排（python heredoc 的縮排有意義）留著。
fn strip_gutter(line: &str) -> String {
    let t = line.trim_start().trim_start_matches(['│', '┃', '▎']);
    t.strip_prefix(' ').unwrap_or(t).trim_end().to_string()
}

/// 2.1.286 起權限框夾住指令／內容的虛線（整行只有 `╌`）。
fn is_dash_rule(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| c == '╌')
}

pub fn dangerous_rm_prompt(screen: &str) -> Option<DangerousRm> {
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = raw.len().saturating_sub(RM_DIALOG_TAIL_LINES);
    let tail_raw = &raw[start..];
    if composer_is_idle(tail_raw) {
        return None; // 輸入列空著＝沒有框在擋，回覆裡引了原文而已（見 [`composer_is_idle`]）。
    }
    let norm: Vec<String> = tail_raw.iter().map(|l| norm_line(l)).collect();
    let question = norm.iter().rposition(|l| l.starts_with("do you want to proceed"))?;
    let after = &norm[question + 1..];
    if !(line_starts_with(after, "1. yes") && line_starts_with(after, "2. no")) {
        return None;
    }
    let warn = norm[..question].iter().rposition(|l| l.starts_with("dangerous rm operation"))?;
    // 警語折行：接到倒數那句（`⚠ Claude Code will automatically deny …`）或問句為止。
    let mut warning = strip_box(tail_raw[warn]);
    for (i, l) in norm.iter().enumerate().take(question).skip(warn + 1) {
        if l.starts_with("⚠") || l.contains("will automatically deny") {
            break;
        }
        warning.push(' ');
        warning.push_str(&strip_box(tail_raw[i]));
    }
    let target = warning.split_once(": ").map(|(_, t)| t.trim().to_string()).unwrap_or_default();
    // 指令在框上方的 `Bash command` 區塊：標題下、警語前，`│` 開頭的那幾行。指令只佔一列時 claude 不畫 `│`
    // （2026-09-24 真畫面），那就是標題下第一列；再下一列是說明，不算。
    // 2.1.286 起指令改用兩條 `╌` 虛線夾住、說明移到虛線上方（#746，`claude-2.1.286-dangerous-rm-*.txt`）：
    // 有虛線就只取兩條虛線之間，不然單列指令會把說明當成指令。
    let abs_warn = start + warn;
    let from = abs_warn.saturating_sub(RM_COMMAND_LOOKBACK);
    let above = &raw[from..abs_warn];
    let gutter_or_first = |block: &[&str]| -> String {
        let gutter: Vec<String> = block.iter().filter(|l| l.trim_start().starts_with('│')).map(|l| strip_gutter(l)).collect();
        if gutter.is_empty() { block.first().map(|l| strip_box(l)).unwrap_or_default() } else { gutter.join("\n") }
    };
    let mut command_truncated = false;
    let mut command = above.iter().rposition(|l| norm_line(l) == "bash command").map(|h| {
        let mut block = &above[h + 1..];
        let rules: Vec<usize> = block.iter().enumerate().filter(|(_, l)| is_dash_rule(l)).map(|(i, _)| i).collect();
        if let [open, close, ..] = rules[..] {
            block = &block[open + 1..close];
        }
        gutter_or_first(block)
    });
    if command.is_none() {
        // 標題已經捲出畫面（長指令＋矮 pane）：只剩收尾那條虛線，它上面到畫面頂（或再上一條虛線）都是指令的後半段。
        let rules: Vec<usize> = above.iter().enumerate().filter(|(_, l)| is_dash_rule(l)).map(|(i, _)| i).collect();
        if let Some(&close) = rules.last() {
            let open = rules.len().checked_sub(2).map(|i| rules[i] + 1);
            command_truncated = open.is_none();
            let block = &above[open.unwrap_or(0)..close];
            // 標題都看不到了，沒有 `│` 的列可能是說明或別的東西，只收 `│` 開頭的列。
            let gutter: Vec<String> = block.iter().filter(|l| l.trim_start().starts_with('│')).map(|l| strip_gutter(l)).collect();
            if !gutter.is_empty() {
                command = Some(gutter.join("\n"));
            }
        }
    }
    let command = command.filter(|c| !c.is_empty());
    Some(DangerousRm { warning, target, command_truncated: command_truncated && command.is_some(), command })
}

/// Pure classifier for a screen that the caller has already read successfully.
pub fn stuck_at_login(screen: &str) -> bool {
    is_login_menu(screen) || is_onboarding_theme(screen)
}

/// 認出問卷之後**要不要真的按鍵**（#485）。
///
/// 曾經暫時關掉（1804c9f8）：當時這是唯一會主動按鍵、卻沒有真畫面 fixture 的偵測。2026-09-25 補上
/// 2.1.281 的真畫面（`CLAUDE_FORCE_DISPLAY_SURVEY=1` 在隔離的 herdr session 叫出來、`pane read` 原文）
/// 與 agent 照抄問卷的真畫面，兩張都有對照測試；同一次實測確認單按 `0` 就會收掉、輸入列維持空的。
/// 誤判的最壞情況也收斂了：`0` 若其實落進輸入列，[`sits_on_empty_composer`] 隨即不成立，不補 Enter、
/// 下一輪也不再按。
///
/// 改回 `false` 時，問卷會自動變成「該吵父 agent 的畫面」（[`daemon_dismisses_survey`]，見
/// `child_alerts::alertable_question`），不會變成沒人按也沒人知道的靜默停擺。
const PRESS_KEYS_ON_SURVEY: bool = true;

/// daemon 現在會不會自己把問卷按掉。`child_alerts` 用它決定要不要把問卷算成「該吵父 agent 的畫面」：
/// daemon 會按掉就不吵，不按就要吵，否則問卷會變成沒人知道的靜默停擺。
pub fn daemon_dismisses_survey() -> bool {
    PRESS_KEYS_ON_SURVEY
}

/// The pane and herdr authority that were current when the survey was recognized.
struct SurveyTarget {
    run: db::Run,
    pane: String,
    host: String,
    session: String,
    socket_path: std::path::PathBuf,
    client: HerdrClient,
}

fn same_survey_run(expected: &db::Run, current: &db::Run) -> bool {
    current.state == "running"
        && current.id == expected.id
        && current.bot_id == expected.bot_id
        && current.pane_id == expected.pane_id
        && current.herdr_session == expected.herdr_session
        && current.native_session_id == expected.native_session_id
        && current.transcript_path == expected.transcript_path
}

async fn current_survey_target(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes), expected: &db::Run) -> Option<SurveyTarget> {
    let current = db::active_run(app.db(), &expected.bot_id).await.ok()??;
    if !same_survey_run(expected, &current) {
        return None;
    }
    let pane = current.pane_id.clone()?;
    let host = db::bot_host(app.db(), &current.bot_id).await.ok()?;
    let session = app.session_for_run(&current).await?;
    let client = app.herdr_for_session(&host, &session).await?;
    let socket_path = client.socket_path().to_path_buf();
    Some(SurveyTarget { run: current, pane, host, session, socket_path, client })
}

async fn survey_target_is_current(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes), target: &SurveyTarget) -> bool {
    let Some(current) = current_survey_target(app, &target.run).await else { return false };
    current.host == target.host
        && current.session == target.session
        && current.socket_path == target.socket_path
        && current.pane == target.pane
}

fn survey_composer_is_idle(screen: &str) -> bool {
    let kept: Vec<&str> = screen.lines().filter(|line| !flatten(line).is_empty()).collect();
    let from = kept.len().saturating_sub(SURVEY_TAIL_LINES);
    composer_is_idle(&kept[from..])
}

async fn read_actionable_survey(target: &SurveyTarget, expected: Option<&PaneRead>) -> Option<PaneRead> {
    let read = target.client.pane_read(&target.pane, "visible", 80).await.ok()?;
    if read.pane_id != target.pane || read.source != "visible" || read.revision == 0 || read.truncated {
        return None;
    }
    if !is_feedback_survey(&read.text) || !survey_composer_is_idle(&read.text) {
        return None;
    }
    if let Some(expected) = expected {
        if read.revision != expected.revision || read.text != expected.text {
            return None;
        }
    }
    Some(read)
}

/// 不在 `active` 裡的 run（結束了）不留問卷去重記錄：每個出現過問卷的 run 一格，只記不清的話只增不減。
pub(crate) async fn retain_survey_runs(app: &Arc<App>, active: &[String]) {
    app.survey_revisions.lock().await.retain(|id, _| active.contains(id));
}

async fn release_survey_revision(app: &Arc<App>, run_id: &str, revision: u64) {
    let mut revisions = app.survey_revisions.lock().await;
    if revisions.get(run_id) == Some(&revision) {
        revisions.remove(run_id);
    }
}

/// 停在問卷上就按 `0`；回傳是否真的按了。[`PRESS_KEYS_ON_SURVEY`] 關著時只記一行 warn 就回 `false`。
pub async fn dismiss_if_survey(app: &Arc<App>, run: &db::Run) -> bool {
    let Some(target) = current_survey_target(app, run).await else { return false };
    let Some(read) = read_actionable_survey(&target, None).await else { return false };
    let pane = target.pane.as_str();
    let first_for_this_run = {
        let mut revisions = app.survey_revisions.lock().await;
        if revisions.get(&run.id) == Some(&read.revision) {
            return false;
        }
        let first = !revisions.contains_key(&run.id);
        revisions.insert(run.id.clone(), read.revision);
        first
    };
    if !PRESS_KEYS_ON_SURVEY {
        // 畫面內容不進 log（那是對話內容）；要補 fixture 的人用 pane id 自己抓一份原文。
        //
        // **每個 run 只吵一次**：revision 去重對按鍵版夠用（按下去畫面就變了），但不按鍵時問卷會一直
        // 留在畫面上，每次重繪 revision 就變一次，每 10 秒一輪的巡邏會把同一件事一直寫進 log
        // （i267 審 #485）。第一次 warn，之後降成 debug；真正該吵的人由 `child_alerts` 負責，
        // 那邊本來就有自己的指紋去重。
        if first_for_this_run {
            tracing::warn!(
                run = %run.id,
                bot = %run.bot_id,
                pane = %pane,
                "claude 滿意度問卷：偵測到但不自動按鍵（PRESS_KEYS_ON_SURVEY 關著，#485）。請人處理"
            );
        } else {
            tracing::debug!(run = %run.id, pane = %pane, "claude 滿意度問卷仍在（不自動按鍵）");
        }
        return false;
    }
    tracing::info!(run = %run.id, bot = %run.bot_id, "claude 滿意度問卷：自動選 0（Dismiss）");
    #[cfg(test)]
    crate::lifecycle::race_point::hit("survey_before_first_key", &run.id).await;
    // The classification read only authorizes this exact pane revision and content, under the
    // same run/session authority. There is no conditional send_keys CAS in Herdr yet.
    let still_current = survey_target_is_current(app, &target).await;
    let final_read = if still_current { read_actionable_survey(&target, Some(&read)).await } else { None };
    let still_current = final_read.is_some() && survey_target_is_current(app, &target).await;
    if !still_current {
        release_survey_revision(app, &run.id, read.revision).await;
        return false;
    }
    if let Err(e) = target.client.pane_send_keys(pane, &["0"]).await {
        release_survey_revision(app, &run.id, read.revision).await;
        tracing::warn!(run = %run.id, error = %e, "問卷送 0 失敗");
        return false;
    }
    #[cfg(test)]
    crate::lifecycle::race_point::hit("survey_after_zero", &run.id).await;
    tokio::time::sleep(SETTLE).await;
    // The post-0 read is a new authorization basis for versions that also need Enter.
    if survey_target_is_current(app, &target).await {
        if let Some(second_stage) = read_actionable_survey(&target, None).await {
            app.survey_revisions.lock().await.insert(run.id.clone(), second_stage.revision);
            #[cfg(test)]
            crate::lifecycle::race_point::hit("survey_before_enter", &run.id).await;
            let still_current = survey_target_is_current(app, &target).await;
            let final_read = if still_current {
                read_actionable_survey(&target, Some(&second_stage)).await
            } else {
                None
            };
            if final_read.is_some() && survey_target_is_current(app, &target).await {
                let _ = target.client.pane_send_keys(pane, &["enter"]).await;
            } else {
                release_survey_revision(app, &run.id, second_stage.revision).await;
            }
        }
    }
    true
}

/// `idle` 也掃：問卷在回合結束後插入，herdr 可能只判成 idle，下一句話就會打進問卷裡。
pub fn spawn_survey_watcher(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP).await;
            let runs = db::all_active_runs(&app.db).await.unwrap_or_default();
            crate::session_paused::forget_ended(&app, &runs).await;
            crate::dangerous_rm::forget_ended(&app, &runs).await;
            for run in runs {
                if run.agent_status == "blocked" || run.agent_status == "idle" || crate::dangerous_rm::is_open(&run.id) {
                    dismiss_if_survey(&app, &run).await;
                    // 防誤刪框：herdr 判成 idle 時的安全網，也負責框關掉之後的收尾（不按任何鍵）。
                    crate::dangerous_rm::observe(&app, &run).await;
                }
                // Session paused 選單：herdr 判 idle 時補標 blocked、選單關掉時還原（不按任何鍵）。
                if run.agent_status == "idle" || crate::session_paused::is_forced(&run.id) {
                    crate::session_paused::observe(&app, &run).await;
                }
                // Codex 在 starting／working 狀態也可能停在啟動遷移框；只查畫面，不替使用者選。
                crate::codex_model_migration::observe(&app, &run).await;
                // claude 停在一般權限確認選單：事件那一刻漏掉、或選單換了一種工具，這裡每輪重讀一次（只看、不按鍵）。
                if run.agent_status == "blocked" {
                    crate::blocked_reason::observe(&app, &run).await;
                }
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

    /// 2.1.281 真畫面（2026-09-25，#485）：隔離的 herdr session 裡 `CLAUDE_FORCE_DISPLAY_SURVEY=1 claude` 叫出的問卷，
    /// `pane read --source visible` 原文（路徑縮成 `/…/`）。問卷就在輸入框正上方，**輸入列是空的**。
    pub const FEEDBACK_SURVEY: &str = include_str!("lifecycle/fixtures/claude-2.1.281-feedback-survey.txt");
    /// 同一個 session 問卷收掉之後，請 agent 把問卷兩行原樣印出來的回覆：`⏺` 引文、`✻ … · done`、空輸入列。
    pub const FEEDBACK_SURVEY_QUOTED: &str = include_str!("lifecycle/fixtures/claude-2.1.281-feedback-survey-quoted.txt");
    pub const ONBOARDING_THEME: &str = include_str!("lifecycle/fixtures/claude-2.1.278-onboarding-theme.txt");
    /// 2026-09-22 triage bot（2.1.280）的真回報：正文逐行引了 onboarding 主題頁原文，底下是空的輸入列＋statusline。
    /// daemon 對它每次送交辦都回 needs_login，交辦停在 queued。
    pub const REPORT_QUOTING_ONBOARDING: &str = include_str!("lifecycle/fixtures/claude-2.1.280-report-quoting-onboarding.txt");
    /// 2.1.281 真畫面：bypass 模式下 `rm -rf $(…)/*` 跳的防誤刪框，含自動拒絕的倒數（本機 2.1.281＋假 API 重現，2026-09-24）。
    pub const DANGEROUS_RM: &str = include_str!("lifecycle/fixtures/claude-2.1.281-dangerous-rm.txt");
    /// 2.1.288 default 權限模式回合結束的真畫面（#783）：沒有 statusLine，輸入框底下只有 `⏸ manual mode on · ← for agents`。
    pub const MANUAL_MODE_FINISHED_2288: &str = include_str!("lifecycle/fixtures/claude-2.1.288-manual-mode-finished.txt");
    /// 回合結束、輸入列空著的 claude 底部。
    pub const IDLE_CLAUDE: &str = "────────────────────\n❯\n────────────────────\n  15m2dg | agents-manager | Opus 5 31% | 5h:96%\n";
    /// 2026-09-25 cf-ox-fork-fork（2.1.281）停著的「Session paused」選單：只取選單那段，說明換成假文字。herdr 判 `idle`。
    pub const SESSION_PAUSED: &str = include_str!("lifecycle/fixtures/claude-2.1.281-session-paused.txt");
    /// 同一個 pane 在倒數到 0 之後：框不見了，claude 收到「被內建安全檢查拒絕」的 tool_result、把回合做完、回到輸入列。
    pub const DANGEROUS_RM_AUTO_DENIED: &str = include_str!("lifecycle/fixtures/claude-2.1.281-dangerous-rm-auto-denied.txt");
    /// 2.1.281 真畫面（2026-09-24，真帳號的拋棄式 pane，herdr `pane read` 原文，herdr 判 `blocked`）：指令只佔一列時
    /// `Bash command` 底下**沒有 `│`**（多列或折行才畫），下一列是說明。重現要用 `rm -rf "$(echo tmpdir2)"` 這種目標
    /// **整段都是**替換輸出的；`rm -rf "$(pwd)/tmpdir"` 在 2.1.281 不跳框、直接刪掉。
    pub const DANGEROUS_RM_ONE_ROW: &str = include_str!("lifecycle/fixtures/claude-2.1.281-dangerous-rm-one-row.txt");
    /// 2.1.286 真畫面（2026-09-30，#746：拋棄式安裝＋拋棄式 herdr pane，60 欄，**沒有**帶 skip-permissions，`pane read --source visible`）。
    /// 指令改用兩條 `╌` 虛線夾住、說明移到虛線上方；單列指令一樣沒有 `│`，警語改成 `│` 開頭、沒有倒數。
    pub const DANGEROUS_RM_2286_ONE_ROW: &str = include_str!("lifecycle/fixtures/claude-2.1.286-dangerous-rm-one-row.txt");
    /// 2.1.289 真畫面（2026-10-05，本機 claude 2.1.289＋假 Anthropic API 在 tmux 150x50 重現，拋棄式 HOME／`CLAUDE_CONFIG_DIR`，`--permission-mode default`）：
    /// 14 列 python heredoc 的 Bash 指令＋`rm -f "$S"/{…}` 變數路徑。警語兩列都是 `│` 開頭，沒有倒數、警語與問句之間沒有空白列。
    /// 指令（含標題與兩條虛線）超過舊的 24 列回看範圍。
    pub const DANGEROUS_RM_2289_LONG_HEREDOC: &str = include_str!("lifecycle/fixtures/claude-2.1.289-dangerous-rm-long-heredoc.txt");
    /// 同一個指令在 `--dangerously-skip-permissions`、30 列高的 pane（真畫面，同上方式）：多一行 `⚠ Claude Code will automatically deny …` 倒數，
    /// 而且 pane 太矮，Bash 框的標題與上面那條虛線已經捲出畫面（畫面頂端就是指令中段）。
    pub const DANGEROUS_RM_2289_COUNTDOWN_30ROWS: &str = include_str!("lifecycle/fixtures/claude-2.1.289-dangerous-rm-countdown-30rows.txt");
    /// 同一個 session：兩列指令，虛線之間是 `│` 開頭的指令列。
    pub const DANGEROUS_RM_2286_MULTILINE: &str = include_str!("lifecycle/fixtures/claude-2.1.286-dangerous-rm-multiline.txt");
    /// 同一個 session 的一般權限框：Bash（指令夾在虛線之間，上面多一段 auto mode 提示）、平行 Read 疊起來的
    /// `Read file … 1 of 3`／`2 of 3`（外觀跟檔案編輯框一樣）、Fetch（問句是 `Do you want to allow Claude to fetch this content?`）。
    pub const PERMISSION_2286_BASH: &str = include_str!("lifecycle/fixtures/claude-2.1.286-bash-permission.txt");
    pub const PERMISSION_2286_READ_1_OF_3: &str = include_str!("lifecycle/fixtures/claude-2.1.286-read-permission-1-of-3.txt");
    pub const PERMISSION_2286_READ_2_OF_3: &str = include_str!("lifecycle/fixtures/claude-2.1.286-read-permission-2-of-3.txt");
    pub const PERMISSION_2286_FETCH: &str = include_str!("lifecycle/fixtures/claude-2.1.286-fetch-permission.txt");
    /// 2.1.287 default 權限模式的真畫面（2026-10-02，拋棄式 `CLAUDE_CONFIG_DIR`＋tmux 120x40，Linux，`--permission-mode default`）：
    /// Bash、Write（`Create file`）、Fetch、MCP（`Tool use … (MCP)`，工具呼叫夾在虛線之間）各一張。
    pub const PERMISSION_2287_BASH: &str = include_str!("lifecycle/fixtures/claude-2.1.287-bash-permission.txt");
    pub const PERMISSION_2287_WRITE: &str = include_str!("lifecycle/fixtures/claude-2.1.287-write-permission.txt");
    pub const PERMISSION_2287_FETCH: &str = include_str!("lifecycle/fixtures/claude-2.1.287-fetch-permission.txt");
    pub const PERMISSION_2287_MCP: &str = include_str!("lifecycle/fixtures/claude-2.1.287-mcp-permission.txt");
    /// 2.1.287 同樣方式（2026-10-03，tmux 120x40，`--setting-sources project --permission-mode default`，#775）：
    /// WebSearch 也是 `Tool use` 框，但標題下面沒有 `(MCP)`，虛線之間是 `Web Search("…")`。
    pub const PERMISSION_2287_WEBSEARCH: &str = include_str!("lifecycle/fixtures/claude-2.1.287-websearch-permission.txt");
    /// 三個平行 Bash 一起等權限：2.1.287 起最舊的排最上面（先是 q1，按掉之後換 q2）；Bash 框沒有 `N of M` 計數。
    pub const PERMISSION_2287_BASH_QUEUE_FIRST: &str = include_str!("lifecycle/fixtures/claude-2.1.287-bash-queue-first.txt");
    pub const PERMISSION_2287_BASH_QUEUE_SECOND: &str = include_str!("lifecycle/fixtures/claude-2.1.287-bash-queue-second.txt");
    /// 三個平行 Read：`Read file … 1 of 3` 是 o1（2.1.286 的 `1 of 3` 是最後送出的 o3），按掉後 `2 of 3` 是 o2。
    pub const PERMISSION_2287_READ_1_OF_3: &str = include_str!("lifecycle/fixtures/claude-2.1.287-read-permission-1-of-3.txt");
    pub const PERMISSION_2287_READ_2_OF_3: &str = include_str!("lifecycle/fixtures/claude-2.1.287-read-permission-2-of-3.txt");
    /// 別的 session（權限模式不同）SendMessage 進來，被扣住等使用者決定：選項沒有編號。
    pub const HELD_MESSAGE_2287: &str = include_str!("lifecycle/fixtures/claude-2.1.287-held-message.txt");
    /// Bash 框開著時又來一則 held message：舊的 Bash 框在上，held message 只在對話裡記一行 `● Held peer message …`。
    pub const PERMISSION_2287_BASH_WITH_HELD_QUEUED: &str = include_str!("lifecycle/fixtures/claude-2.1.287-bash-with-held-queued.txt");
    /// 2026-10-02 grok 1.0.46 在沒信任過的目錄的真畫面（拋棄式 tmux；信任框置中，標題與選項各自成行，最底一行是版本）。
    pub const GROK_1046_TRUST: &str = include_str!("lifecycle/fixtures/grok-1.0.46-trust-dialog.txt");
    /// 2026-09-23 m12 的 pane（巡檢交辦時抄的原文，路徑中段被抄錄者省略成 `…`）。
    pub const DANGEROUS_RM_M12: &str = "\
 Dangerous rm operation on statically-unresolvable target: /Users/…/web/docs/screenshots/pin-3rows/*
 Do you want to proceed?
 ❯ 1. Yes
   2. No
 Esc to cancel · Tab to amend
";
    /// 2026-09-20 w16A:pQ 真機（網頁 `tuiChoices.test.ts` 同一份）：變數路徑那一種，上面有 `Bash command` 框。
    pub const DANGEROUS_RM_VARIABLE: &str = r#"
────────────────────────────────────────────────────────────────────────────────
 Bash command

   │ D=$(cat /tmp/.origin_dir); P=robinstech-com-tw
   │ shred -u "$D"/origin.key 2>/dev/null || rm -f "$D"/*; rmdir "$D" 2>/dev/null
   Install origin cert on LB and clean up key

 │ Dangerous rm operation on possibly-empty variable path: "$D"/* in `rm -f "$D"/*`

 Do you want to proceed?
 ❯ 1. Yes
   2. No

 Esc to cancel · Tab to amend
"#;
}

#[cfg(test)]
mod survey_revision_tests {
    use super::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn an_ended_runs_survey_dedupe_record_is_dropped() {
        let env = tt::env().await;
        let app = env.app.clone();
        app.survey_revisions.lock().await.insert("survey-gone".into(), 3);
        app.survey_revisions.lock().await.insert("survey-kept".into(), 4);
        retain_survey_runs(&app, &["survey-kept".to_string()]).await;
        let m = app.survey_revisions.lock().await;
        assert!(!m.contains_key("survey-gone") && m.contains_key("survey-kept"));
    }
}

#[cfg(test)]
mod tests {
    #[derive(Default)]
    struct FakeRunPaneReader {
        screen: Option<String>,
        calls: std::sync::Mutex<Vec<(String, Option<String>, String, am_core::PaneReadSource, u32)>>,
    }

    impl am_ports::RunPaneReader for FakeRunPaneReader {
        fn read_run_pane<'a>(
            &'a self,
            bot: &'a am_core::BotId,
            run_session: Option<&'a am_core::SessionId>,
            pane_id: &'a str,
            source: am_core::PaneReadSource,
            lines: u32,
        ) -> impl std::future::Future<Output = Result<Option<String>, am_core::PortError>> + Send + 'a {
            let call = (
                bot.clone(),
                run_session.cloned(),
                pane_id.to_string(),
                source,
                lines,
            );
            async move {
                self.calls.lock().unwrap().push(call);
                Ok(self.screen.clone())
            }
        }
    }

    #[tokio::test]
    async fn login_problem_probe_uses_run_pane_reader_with_visible_tail() {
        let reader = FakeRunPaneReader {
            screen: Some(super::screens::NOT_LOGGED_IN.to_string()),
            ..Default::default()
        };
        let bot = "bot-1".to_string();
        let session = "session-1".to_string();

        assert_eq!(
            super::shows_login_problem_with_reader(&reader, &bot, Some(&session), Some(" pane-1 ")).await,
            Some(true)
        );
        assert_eq!(
            *reader.calls.lock().unwrap(),
            vec![(bot, Some(session), "pane-1".to_string(), am_core::PaneReadSource::Visible, 80)]
        );
    }

    #[tokio::test]
    async fn login_problem_probe_keeps_unavailable_and_missing_panes_unknown() {
        let reader = FakeRunPaneReader::default();
        let bot = "bot-1".to_string();

        assert_eq!(super::shows_login_problem_with_reader(&reader, &bot, None, None).await, None);
        assert_eq!(super::shows_login_problem_with_reader(&reader, &bot, None, Some("  ")).await, None);
        assert_eq!(super::shows_login_problem_with_reader(&reader, &bot, None, Some("pane-1")).await, None);
        assert_eq!(reader.calls.lock().unwrap().len(), 1);
    }

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

    /// 窄 pane 把 `Yes, proceed                 y` 折成標籤與按鍵兩行時，仍是信任框。
    #[test]
    fn a_wrapped_grok_trust_key_is_still_the_dialog() {
        let screen = "\
Do you trust the contents of this directory?
/Users/m4p/project

Yes, proceed
                 y
No, quit
                 n

Grok Build  1.0.34
";
        assert!(super::is_grok_trust_dialog(screen));
    }

    #[test]
    fn grok_trust_lookalike_text_without_the_live_choice_keys_is_not_a_dialog() {
        let printed_text = "\
⏺ Example output from a previous trust prompt:\n\
Do you trust the contents of this directory?\n\
Yes, proceed\n\
No, quit\n\
Grok Build 1.0.46\n";
        assert!(!super::is_grok_trust_dialog(printed_text), "a lookalike without the y/n controls must never authorize a keypress");
    }

    #[test]
    fn a_reply_printing_the_complete_trust_prompt_is_not_the_live_dialog() {
        let printed_text = "\
⏺ Reference from the Grok trust screen:\n\
Do you trust the contents of this directory?\n\
/home/user/project\n\
Grok Build may run or modify contents in this directory,\n\
posing security risks.\n\
Yes, proceed                 y\n\
No, quit                     n\n\
Grok Build 1.0.46\n\
● That was an example; continuing the explanation now.\n";
        assert!(
            !super::is_grok_trust_dialog(printed_text),
            "a response that reproduces every visible trust control must never authorize a keypress"
        );
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

    async fn survey_test_env() -> (crate::testing::Env, crate::db::Run) {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "survey").await;
        let run_id = crate::testing::fake_run(&env.app, &bot.id).await;
        let run = crate::db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(run.id, run_id);
        env.herdr.set_screen(run.pane_id.as_deref().unwrap(), screens::FEEDBACK_SURVEY);
        (env, run)
    }

    async fn assert_no_survey_key_after_race<F, Fut>(mutate: F)
    where
        F: FnOnce(std::sync::Arc<crate::state::App>, crate::db::Run) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (env, run) = survey_test_env().await;
        let app = env.app.clone();
        let hook_run = run.clone();
        crate::lifecycle::race_point::arm("survey_before_first_key", &run.id, move || mutate(app, hook_run));

        assert!(!dismiss_if_survey(&env.app, &run).await, "changed authority must not count as a dismissal");
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "a changed authority receives no key");
    }

    #[tokio::test]
    async fn a_replaced_dialog_receives_no_survey_zero() {
        let (env, run) = survey_test_env().await;
        let pane = run.pane_id.clone().unwrap();
        let replace = env.herdr.set_screen_later();
        crate::lifecycle::race_point::arm("survey_before_first_key", &run.id, move || async move {
            replace(&pane, PERMISSION);
        });

        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "a permission dialog must receive no stale 0");
        assert!(!env.app.survey_revisions.lock().await.contains_key(&run.id), "uncertain screen must re-arm dedupe");
    }

    #[tokio::test]
    async fn a_revision_change_with_identical_survey_text_receives_no_zero() {
        let (env, run) = survey_test_env().await;
        let pane = run.pane_id.clone().unwrap();
        let bump = env.herdr.bump_screen_revision_later();
        crate::lifecycle::race_point::arm("survey_before_first_key", &run.id, move || async move {
            bump(&pane);
        });

        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "same text at another revision is not the authorized read");
    }

    #[tokio::test]
    async fn an_unreadable_final_survey_read_receives_no_zero() {
        let (env, run) = survey_test_env().await;
        let fail = env.herdr.fail_later();
        crate::lifecycle::race_point::arm("survey_before_first_key", &run.id, move || async move {
            fail("pane.read", crate::testing::Fault::Refuse);
        });

        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "an unreadable last-moment pane is uncertain");
    }

    #[tokio::test]
    async fn classifier_match_without_an_idle_composer_receives_no_zero() {
        let (env, run) = survey_test_env().await;
        env.herdr.set_screen(run.pane_id.as_deref().unwrap(), SURVEY);
        assert!(is_feedback_survey(SURVEY), "the classifier fixture is recognized");
        assert!(!composer_is_idle(&SURVEY.lines().collect::<Vec<_>>()), "the cursor shape is not an idle composer");

        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "composer uncertainty fails closed");
    }

    #[tokio::test]
    async fn an_active_run_replacement_receives_no_survey_zero() {
        assert_no_survey_key_after_race(|app, run| async move {
            sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run.id).execute(&app.db).await.unwrap();
            crate::testing::fake_run(&app, &run.bot_id).await;
        })
        .await;
    }

    #[tokio::test]
    async fn a_pane_replacement_receives_no_survey_zero() {
        assert_no_survey_key_after_race(|app, run| async move {
            sqlx::query("UPDATE runs SET pane_id='pane-replaced' WHERE id=?").bind(&run.id).execute(&app.db).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn a_herdr_session_replacement_receives_no_survey_zero() {
        assert_no_survey_key_after_race(|app, run| async move {
            sqlx::query("UPDATE runs SET herdr_session='default' WHERE id=?").bind(&run.id).execute(&app.db).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn a_native_session_replacement_receives_no_survey_zero() {
        assert_no_survey_key_after_race(|app, run| async move {
            sqlx::query("UPDATE runs SET native_session_id='native-replaced' WHERE id=?").bind(&run.id).execute(&app.db).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn one_stage_survey_dismissal_does_not_press_enter() {
        let (env, run) = survey_test_env().await;
        let pane = run.pane_id.clone().unwrap();
        let replace = env.herdr.set_screen_later();
        crate::lifecycle::race_point::arm("survey_after_zero", &run.id, move || async move {
            replace(&pane, screens::IDLE_CLAUDE);
        });

        assert!(dismiss_if_survey(&env.app, &run).await, "the authorized 0 key was sent");
        let keys: Vec<serde_json::Value> = env.herdr.calls_to("pane.send_keys").iter().map(|call| call["keys"].clone()).collect();
        assert_eq!(keys, [serde_json::json!(["0"])]);
    }

    #[tokio::test]
    async fn a_changed_second_stage_receives_no_stale_enter() {
        let (env, run) = survey_test_env().await;
        let pane = run.pane_id.clone().unwrap();
        let replace = env.herdr.set_screen_later();
        crate::lifecycle::race_point::arm("survey_before_enter", &run.id, move || async move {
            replace(&pane, PERMISSION);
        });

        assert!(dismiss_if_survey(&env.app, &run).await, "the first 0 was authorized");
        let keys: Vec<serde_json::Value> = env.herdr.calls_to("pane.send_keys").iter().map(|call| call["keys"].clone()).collect();
        assert_eq!(keys, [serde_json::json!(["0"])]);
    }

    #[tokio::test]
    async fn an_unreadable_final_second_stage_read_receives_no_enter() {
        let (env, run) = survey_test_env().await;
        let fail = env.herdr.fail_later();
        crate::lifecycle::race_point::arm("survey_before_enter", &run.id, move || async move {
            fail("pane.read", crate::testing::Fault::Refuse);
        });

        assert!(dismiss_if_survey(&env.app, &run).await, "the first 0 was authorized");
        let keys: Vec<serde_json::Value> = env.herdr.calls_to("pane.send_keys").iter().map(|call| call["keys"].clone()).collect();
        assert_eq!(keys, [serde_json::json!(["0"])]);
    }

    #[test]
    fn survey_is_recognised_even_when_wrapped() {
        assert!(is_feedback_survey(SURVEY));
        assert!(is_feedback_survey(SURVEY_WRAPPED));
    }

    /// #485 的真畫面：2.1.281 的問卷底下就是空的輸入列。套其他偵測那道「輸入列空著就不是框」的守衛，
    /// 這張會被判成不是問卷（1804c9f8 就是這樣）。
    #[test]
    fn the_real_2_1_281_survey_is_recognised() {
        assert!(is_feedback_survey(screens::FEEDBACK_SURVEY));
    }

    /// #485：agent 在回覆裡逐行照抄問卷（真畫面）——引文與輸入框之間隔著 `✻ … · done`，那不是開著的問卷。
    #[test]
    fn a_reply_that_quotes_the_survey_is_not_the_survey() {
        assert!(!is_feedback_survey(screens::FEEDBACK_SURVEY_QUOTED), "底下隔著回合結束那一行＝引文");
        // 沒有空輸入列（還在跑）時靠選項列那一條：問句與四個選項**都**出現，但全寫在句子中間。
        // （這一行必須同時含 `how is claude doing`，否則問句那一關就先擋掉，測不到 `is_option_row`。）
        let prose = " ⏺ The popup asked How is Claude doing this session? with 1: Bad 2: Fine 3: Good 0: Dismiss and I chose 0.\n ⎿  done\n   ✽ Working…\n";
        assert!(!is_feedback_survey(prose), "選項夾在句子裡不是選項列");
    }

    /// 按下的 `0` 如果落進了輸入列（那就不是問卷），畫面就不能再算問卷：否則 700 毫秒後會補 Enter 把「0」送出去，
    /// 下一輪巡邏也會再按一次。
    #[test]
    fn a_zero_typed_into_the_composer_is_not_a_survey() {
        let typed = screens::FEEDBACK_SURVEY.replacen("\n❯\n", "\n❯ 0\n", 1);
        assert_ne!(typed, screens::FEEDBACK_SURVEY, "前提：fixture 裡有空的輸入列");
        assert!(!is_feedback_survey(&typed));
    }

    /// i267 審 #485：`composer_is_idle` 只能套在**跟問句同一個 tail** 上。套在整個畫面的話，
    /// 捲動區裡任何一處空的 `❯`（上一個回合結束時那一行就是）都會把真的問卷判成引文。
    #[test]
    fn an_empty_composer_further_up_the_scrollback_does_not_hide_a_live_survey() {
        let scrollback = format!("{IDLE_CLAUDE}\n{}", "⏺ 上一輪做完了。\n".repeat(20));
        let live = format!("{scrollback}{SURVEY}");
        assert!(is_feedback_survey(&live), "問卷在最底下就是問卷，上面有過空輸入列不算");
    }

    /// 前綴守衛不再靠 `●`（#485）：真的問卷就算前面不是 `● `／`> ` 也要認得出來。
    #[test]
    fn the_survey_is_recognised_without_relying_on_the_assistant_bullet() {
        let no_bullet = " How is Claude doing this session? (optional)\n   1: Bad    2: Fine   3: Good   0: Dismiss\n > │\n";
        assert!(is_feedback_survey(no_bullet));
    }

    /// daemon 不自動按鍵時，問卷必須變成「該吵父 agent」的畫面——否則就是沒人按也沒人知道。
    #[test]
    fn not_pressing_means_the_survey_must_be_alertable() {
        assert!(
            daemon_dismisses_survey() || crate::child_alerts::alertable_question(SURVEY).is_some(),
            "不按鍵就一定要吵人"
        );
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

    /// Codex 0.157.0 `rust-v0.157.0` model migration copy plus its full-screen choices.
    /// The private-prefix CLI reached the sign-in menu before this screen, so this fixture follows
    /// `models.json` migration markdown and `tui/src/model_migration.rs` rendering.
    #[test]
    fn codex_0157_model_migration_is_a_user_choice_prompt() {
        let screen = include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt");
        assert!(is_codex_model_migration_prompt(screen));

        let quoted = format!("⏺ Model migration screen:\n{screen}\n────────\n›\n");
        assert!(!is_codex_model_migration_prompt(&quoted), "an assistant quote followed by a composer is not a modal");
        let no_footer = screen.replace("enter/esc confirm · ctrl+c quit", "");
        assert!(!is_codex_model_migration_prompt(&no_footer), "the migration copy alone is not a prompt");
    }

    /// 窄 pane 把 `enter/esc confirm · ctrl+c quit` 折成兩行時，遷移選單仍開著。
    #[test]
    fn a_wrapped_migration_footer_is_still_the_prompt() {
        let screen = include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt")
            .replace("  enter/esc confirm · ctrl+c quit", "  enter/esc confirm ·\n  ctrl+c quit");
        assert!(is_codex_model_migration_prompt(&screen), "{screen}");
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

    /// #483：真畫面底部長高了仍要認得出來。原本窗口是 20，而真 onboarding 頁的標題就在距底部
    /// 第 16 行——只剩 4 行餘裕，新版多一句提示、窄 pane 折個幾行就掉出窗口，`is_onboarding_theme`
    /// 會**靜默**失效（沒有 log、沒有 health 欄位，只以「交辦被送進登入畫面」的形式出現）。
    #[test]
    fn the_onboarding_page_is_still_recognised_when_the_bottom_grows() {
        for extra in [4usize, 8, 16] {
            let hints: String = (0..extra).map(|i| format!("  hint line {i}\n")).collect();
            let grown = format!("{ONBOARDING_THEME}\n{hints}");
            assert!(is_onboarding_theme(&grown), "底部多 {extra} 行仍要認得出來");
        }
    }

    /// 放寬窗口不能把「引文在上面、底下還在跑」放進來：那種畫面輸入列不是空的，
    /// `composer_is_idle` 擋不到，靠的是「選單必須是最底下那個 UI」（`menu_is_bottom_most`）。
    #[test]
    fn a_quote_with_tool_output_below_it_is_not_the_menu_even_with_a_wider_window() {
        let below = "  ⏺ Bash(cargo test)\n  ⎿  running…\n".repeat(12);
        let quoted = format!("{}\n{below}", REPORT_QUOTING_ONBOARDING.split("0 tokens").next().unwrap());
        assert!(!is_onboarding_theme(&quoted), "底下還有對話輸出＝引文，不是開著的選單");
        assert!(!is_login_menu(&quoted));
        // 選單底下只有它自己的預覽／提示時才算——真畫面就是這樣。
        assert!(is_onboarding_theme(ONBOARDING_THEME));
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

    use super::screens::IDLE_CLAUDE;

    use super::screens::{DANGEROUS_RM, DANGEROUS_RM_AUTO_DENIED, DANGEROUS_RM_M12, DANGEROUS_RM_ONE_ROW, DANGEROUS_RM_VARIABLE};

    /// 2.1.281 的防誤刪框：真畫面（含倒數）、巡檢抄的 m12 畫面、09-20 的變數路徑，三種都認得，而且把目標帶出來。
    #[test]
    fn the_dangerous_rm_prompt_is_recognised_with_its_target() {
        let rm = dangerous_rm_prompt(DANGEROUS_RM).expect("2.1.281 真畫面");
        assert_eq!(rm.warning, "Dangerous rm operation on statically-unresolvable target: command substitution output");
        assert_eq!(rm.target, "command substitution output");
        let cmd = rm.command.expect("框上方的指令");
        assert!(cmd.starts_with("rm -rf $(cat /private/tmp/") && cmd.ends_with("zz-nonexistent)/*"), "{cmd}");
        assert!(!cmd.contains("will automatically deny"), "倒數那句不是指令：{cmd}");

        let m12 = dangerous_rm_prompt(DANGEROUS_RM_M12).expect("m12 的框");
        assert_eq!(m12.target, "/Users/…/web/docs/screenshots/pin-3rows/*");
        assert_eq!(m12.command, None);

        let var = dangerous_rm_prompt(DANGEROUS_RM_VARIABLE).expect("變數路徑");
        assert_eq!(var.target, r#""$D"/* in `rm -f "$D"/*`"#);
        assert_eq!(var.command.as_deref(), Some("D=$(cat /tmp/.origin_dir); P=robinstech-com-tw\nshred -u \"$D\"/origin.key 2>/dev/null || rm -f \"$D\"/*; rmdir \"$D\" 2>/dev/null"));

        // 單列指令沒有 `│`：以前只收 `│` 開頭的列，通知裡指令是空的。說明那一列不能算進去。
        let one = dangerous_rm_prompt(DANGEROUS_RM_ONE_ROW).expect("單列指令的真畫面");
        assert_eq!(one.target, "command substitution output");
        assert_eq!(one.command.as_deref(), Some(r#"rm -rf "$(echo tmpdir2)""#));
    }

    use super::screens::{
        DANGEROUS_RM_2286_MULTILINE, DANGEROUS_RM_2286_ONE_ROW, PERMISSION_2286_BASH, PERMISSION_2286_FETCH, PERMISSION_2286_READ_1_OF_3,
        PERMISSION_2286_READ_2_OF_3,
    };

    /// 2.1.286（#746）：指令夾在 `╌` 虛線之間、說明在虛線上方。以前單列指令取「標題下第一列」，會把說明當成指令。
    #[test]
    fn the_2_1_286_dangerous_rm_prompt_reads_the_command_between_the_dashed_rules() {
        let one = dangerous_rm_prompt(DANGEROUS_RM_2286_ONE_ROW).expect("2.1.286 單列指令");
        assert_eq!(one.warning, "Dangerous rm operation on statically-unresolvable target: command substitution output");
        assert_eq!(one.target, "command substitution output");
        assert_eq!(one.command.as_deref(), Some(r#"rm -rf "$(echo tmpdir2)""#));

        let multi = dangerous_rm_prompt(DANGEROUS_RM_2286_MULTILINE).expect("2.1.286 兩列指令");
        assert_eq!(multi.target, "command substitution output");
        assert_eq!(multi.command.as_deref(), Some("touch m1.txt\nrm -rf \"$(echo tmpdir3)\""));

        assert!(awaits_menu_choice(DANGEROUS_RM_2286_ONE_ROW) && awaits_menu_choice(DANGEROUS_RM_2286_MULTILINE));
    }

    /// 2.1.289（真畫面）：長 heredoc 指令不能因為超過回看範圍就讀不到；倒數那句不算警語也不算指令；標題捲出畫面時只拿虛線上方的後半段並標記被截。
    #[test]
    fn the_2_1_289_long_heredoc_command_and_countdown_are_read() {
        use super::screens::{DANGEROUS_RM_2289_COUNTDOWN_30ROWS, DANGEROUS_RM_2289_LONG_HEREDOC};
        let full = dangerous_rm_prompt(DANGEROUS_RM_2289_LONG_HEREDOC).expect("2.1.289 長 heredoc");
        assert!(full.warning.starts_with(r#"Dangerous rm operation on possibly-empty variable path: "$S"/{gauth.url,gauth.log,glogin.mjs}"#), "{}", full.warning);
        assert!(full.warning.ends_with("or use a literal path)"), "折行的第二列要接回：{}", full.warning);
        let cmd = full.command.as_deref().expect("指令");
        assert!(cmd.starts_with("S=$(mktemp -d)\npython3 - <<'EOF'\nimport json"), "{cmd}");
        assert!(cmd.contains("\n    return json.load("), "指令自己的縮排要留著：{cmd}");
        assert!(cmd.contains("\nEOF\nrm -f ") && cmd.lines().count() == 14, "{cmd}");
        assert!(!full.command_truncated);

        let cut = dangerous_rm_prompt(DANGEROUS_RM_2289_COUNTDOWN_30ROWS).expect("2.1.289 倒數＋矮 pane");
        assert!(cut.warning.ends_with("or use a literal path)"), "倒數那句不能被接進警語：{}", cut.warning);
        assert!(!cut.warning.contains("will automatically deny"));
        let cmd = cut.command.as_deref().expect("標題捲出畫面時仍要拿到指令後半段");
        assert!(cmd.ends_with("\nEOF\nrm -f \"$S\"/{gauth.url,gauth.log,glogin.mjs}"), "{cmd}");
        assert!(!cmd.contains("will automatically deny") && !cmd.contains("Dangerous rm"), "{cmd}");
        // 這張的標題與上緣虛線還在畫面裡（30 列剛好夠），所以沒有被截。
        assert!(!cut.command_truncated);
        // 再把畫面頂端削掉 6 列（標題、說明、虛線與頭幾列指令）：標題看不到，仍讀得到剩下的指令，並標記被截。
        let mut seen = 0;
        let top_cut: String = DANGEROUS_RM_2289_COUNTDOWN_30ROWS
            .lines()
            .filter(|l| {
                let drop = !l.trim().is_empty() && seen < 6;
                seen += usize::from(!l.trim().is_empty());
                !drop
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rm = dangerous_rm_prompt(&top_cut).expect("頂端被截");
        assert!(rm.command_truncated, "標題與上緣虛線沒了");
        let cmd = rm.command.expect("後半段指令");
        assert!(cmd.ends_with("rm -f \"$S\"/{gauth.url,gauth.log,glogin.mjs}") && !cmd.contains("mktemp"), "{cmd}");
    }

    /// 2.1.286 的一般權限框（虛線、`1 of 3` 計數、Read／Fetch 換成編輯框的外觀）：是等人選的選單，
    /// 但不是防誤刪框、不是 daemon 會替人按掉的那幾種框。
    #[test]
    fn the_2_1_286_permission_prompts_are_open_menus_and_nothing_daemon_answers() {
        for (name, screen) in [
            ("bash", PERMISSION_2286_BASH),
            ("read 1 of 3", PERMISSION_2286_READ_1_OF_3),
            ("read 2 of 3", PERMISSION_2286_READ_2_OF_3),
            ("fetch", PERMISSION_2286_FETCH),
        ] {
            assert!(awaits_menu_choice(screen), "{name}：等人選的選單");
            assert_eq!(dangerous_rm_prompt(screen), None, "{name}：不是防誤刪框");
            assert!(!is_switch_model_dialog(screen) && !is_auto_mode_offer(screen) && !is_feedback_survey(screen), "{name}");
            assert!(!is_session_paused_menu(screen) && !stuck_at_login(screen) && !is_not_logged_in_reply(screen), "{name}");
        }
    }

    /// 2.1.287 default 模式的四種權限框（Bash／Write／Fetch／MCP）：一樣是等人選的選單，daemon 一個鍵都不替人按。
    #[test]
    fn the_2_1_287_default_mode_permission_prompts_are_open_menus_and_nothing_daemon_answers() {
        use super::screens::{PERMISSION_2287_BASH, PERMISSION_2287_FETCH, PERMISSION_2287_MCP, PERMISSION_2287_WRITE};
        for (name, screen) in [("bash", PERMISSION_2287_BASH), ("write", PERMISSION_2287_WRITE), ("fetch", PERMISSION_2287_FETCH), ("mcp", PERMISSION_2287_MCP)] {
            assert!(awaits_menu_choice(screen), "{name}：等人選的選單");
            assert_eq!(dangerous_rm_prompt(screen), None, "{name}：不是防誤刪框");
            assert!(!is_switch_model_dialog(screen) && !is_auto_mode_offer(screen) && !is_feedback_survey(screen), "{name}");
            assert!(!is_session_paused_menu(screen) && !stuck_at_login(screen) && !is_not_logged_in_reply(screen), "{name}");
            assert!(!is_grok_trust_dialog(screen) && !is_onboarding_theme(screen), "{name}");
        }
    }

    /// #788：2.1.288 default 模式回合結束的真畫面——輸入列空著、底下只有 `⏸ manual mode on` 模式列。
    /// 那一行不是選單、不是任何框（換成其他三種模式列也一樣），空的輸入列照樣認得出來。
    #[test]
    fn the_2_1_288_manual_mode_screen_is_an_idle_composer_not_a_dialog() {
        use super::screens::MANUAL_MODE_FINISHED_2288 as MANUAL;
        let rows = [
            "⏸ manual mode on · ← for agents",
            "⏸ manual mode on · ? for shortcuts",
            "⏸ plan mode on (shift+tab to cycle) · ← for agents",
            "⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
        ];
        for row in rows {
            let screen = MANUAL.replace(rows[0], row);
            let tail: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
            assert!(crate::capture::claude::is_mode_row(tail[tail.len() - 1]), "{row}");
            assert!(composer_is_idle(&tail), "{row}：輸入列空著");
            assert!(!awaits_menu_choice(&screen) && permission_prompt(&screen).is_none() && dangerous_rm_prompt(&screen).is_none(), "{row}");
            assert!(!is_switch_model_dialog(&screen) && !is_auto_mode_offer(&screen) && !is_feedback_survey(&screen), "{row}");
            assert!(!is_session_paused_menu(&screen) && !is_held_message_prompt(&screen) && !stuck_at_login(&screen), "{row}");
            assert!(!is_login_menu(&screen) && !is_onboarding_theme(&screen) && !is_not_logged_in_reply(&screen), "{row}");
            assert!(!is_grok_trust_dialog(&screen) && update_notice(&screen).is_none(), "{row}");
        }
    }

    /// claude 一般的權限確認選單（Bash／Write／Edit／Fetch／Read／MCP）：認得「是權限框」與「哪個工具」，網頁的 blocked 原因才寫得出
    /// 「等待權限確認：Bash」。真畫面是 2.1.286／2.1.287 的 fixture；防誤刪框、Session paused、一般輸入列、回覆裡引用原文都不算。
    #[test]
    fn a_claude_permission_menu_is_recognised_with_its_tool() {
        use super::screens::*;
        for (name, screen, tool) in [
            ("2.1.287 bash", PERMISSION_2287_BASH, "Bash"),
            ("2.1.287 write", PERMISSION_2287_WRITE, "Write"),
            ("2.1.287 fetch", PERMISSION_2287_FETCH, "Fetch"),
            ("2.1.287 mcp", PERMISSION_2287_MCP, "MCP"),
            ("2.1.286 bash", PERMISSION_2286_BASH, "Bash"),
            ("2.1.286 read 1/3", PERMISSION_2286_READ_1_OF_3, "Read"),
            ("2.1.286 read 2/3", PERMISSION_2286_READ_2_OF_3, "Read"),
            ("2.1.286 fetch", PERMISSION_2286_FETCH, "Fetch"),
        ] {
            assert_eq!(permission_prompt(screen).as_deref(), Some(tool), "{name}");
        }
        for (name, screen) in [("dangerous rm", DANGEROUS_RM), ("session paused", SESSION_PAUSED), ("auto mode", AUTO_MODE), ("idle", IDLE_CLAUDE), ("empty", "")] {
            assert_eq!(permission_prompt(screen), None, "{name}：不是一般權限框");
        }
        // 回覆裡逐行引用權限框原文、底下是空的輸入列：不是真的框。
        let quoted = format!("⏺ 剛才停在：\n  Bash command\n  Do you want to proceed?\n  1. Yes\n  2. No\n{IDLE_CLAUDE}");
        assert_eq!(permission_prompt(&quoted), None);

        // The latest permission menu can be partially scrolled so its title is gone. Do not
        // borrow the tool name from an earlier quoted menu still visible in scrollback.
        let earlier = PERMISSION_2287_BASH.lines().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n");
        let write_lines: Vec<&str> = PERMISSION_2287_WRITE.lines().collect();
        let first_choice = write_lines.iter().position(|line| line.trim_start().starts_with("❯ 1.")).expect("Write fixture choice");
        let partial_write = write_lines[first_choice..].join("\n");
        let screen = format!("⏺ Earlier output quoted this Bash permission menu:\n{earlier}\n\n{partial_write}");
        assert_eq!(permission_prompt(&screen), None, "標題不在最新 menu viewport 時不能把舊引文分類成 Bash");
        let two_visible_menus = format!("⏺ Earlier output quoted this Bash permission menu:\n{earlier}\n\n{PERMISSION_2287_WRITE}");
        assert_eq!(permission_prompt(&two_visible_menus).as_deref(), Some("Write"));
    }

    /// #775：2.1.287 的 `Tool use` 框不只 MCP（WebSearch 也是）、held message 框沒有編號、多框改成最舊在上。
    /// 框種要分得出來，排隊時讀到的是畫面上那一框（最舊的），後面排隊的不算開著。
    #[test]
    fn the_2_1_287_tool_use_held_message_and_queued_prompts_are_told_apart() {
        use super::screens::*;
        for (name, screen, tool) in [
            ("websearch", PERMISSION_2287_WEBSEARCH, "Web Search"),
            ("mcp", PERMISSION_2287_MCP, "MCP"),
            ("bash queue first", PERMISSION_2287_BASH_QUEUE_FIRST, "Bash"),
            ("bash queue second", PERMISSION_2287_BASH_QUEUE_SECOND, "Bash"),
            ("read 1/3", PERMISSION_2287_READ_1_OF_3, "Read"),
            ("read 2/3", PERMISSION_2287_READ_2_OF_3, "Read"),
            ("bash with held queued", PERMISSION_2287_BASH_WITH_HELD_QUEUED, "Bash"),
            ("held message", HELD_MESSAGE_2287, "Held message"),
        ] {
            assert_eq!(permission_prompt(screen).as_deref(), Some(tool), "{name}");
            assert!(awaits_menu_choice(screen), "{name}：等人選，不是回合結束");
            assert_eq!(dangerous_rm_prompt(screen), None, "{name}");
            assert!(!is_session_paused_menu(screen) && !is_auto_mode_offer(screen) && !is_feedback_survey(screen), "{name}");
        }
        assert!(is_held_message_prompt(HELD_MESSAGE_2287));
        assert!(!is_held_message_prompt(PERMISSION_2287_BASH_WITH_HELD_QUEUED), "排在 Bash 框後面的 held message 還沒開");
        // 排隊時畫面上的是最舊的那一框。
        assert!(PERMISSION_2287_BASH_QUEUE_FIRST.contains("\n touch q1.txt\n") && PERMISSION_2287_BASH_QUEUE_SECOND.contains("\n touch q2.txt\n"));
        // 回覆裡逐行引用 held message 框、底下是空的輸入列：不是真的框。
        let quoted = format!(
            "● 剛才停在：\n────────────────\n Held message from another session\n ❯ Deny — drop it and tell the sender it was declined\n   Deliver this message to Claude\n{IDLE_CLAUDE}"
        );
        assert!(!is_held_message_prompt(&quoted) && permission_prompt(&quoted).is_none() && !awaits_menu_choice(&quoted));
        // 沒有游標（選完了、框在收）、或選項底下還在跑對話輸出：都不算開著。
        let no_cursor = HELD_MESSAGE_2287.replace(" ❯ Deny", "   Deny");
        assert!(!is_held_message_prompt(&no_cursor));
        let below = format!("{HELD_MESSAGE_2287}\n● 繼續做事\n");
        assert!(!is_held_message_prompt(&below));
    }

    /// `Tool use` 框的工具名：MCP 看標題下一行的 `(MCP)`，其他看虛線之間那一行括號前的名字，認不出來不再假裝是 MCP。
    #[test]
    fn a_tool_use_prompt_names_its_tool() {
        assert_eq!(tool_use_name(&[" demo — Echo Tool: (MCP)", "╌╌╌╌", " text: \"hello\"", "╌╌╌╌"]), "MCP");
        assert_eq!(tool_use_name(&[" │ Claude wants to search the web for: x", "╌╌╌╌", " Web Search(\"x\")", "╌╌╌╌"]), "Web Search");
        assert_eq!(tool_use_name(&[" 說明", "╌╌╌╌", " 沒有括號的內容", "╌╌╌╌"]), "Tool use");
        assert_eq!(tool_use_name(&[]), "Tool use");
    }

    /// Tool use 內文是工具輸入資料，不能把它自己的實線誤當成外框標題線。
    #[test]
    fn permission_prompt_ignores_tool_use_payload_rules() {
        use super::screens::PERMISSION_2287_MCP;
        let payload_rule = PERMISSION_2287_MCP.replace(
            " text: \"hello\"\n",
            " text: \"hello\"\n ────────────\n fake payload heading\n",
        );
        assert_eq!(permission_prompt(&payload_rule).as_deref(), Some("MCP"), "工具輸入內文不能改寫權限框類型");
    }

    /// 高視窗／長內容時，標題可在末 30 行外但仍在 pane 的可見範圍內。
    #[test]
    fn permission_prompt_keeps_tall_frame_title() {
        use super::screens::PERMISSION_2287_READ_1_OF_3;
        let detail_rows = (0..22).map(|i| format!(" wrapped detail row {i}")).collect::<Vec<_>>().join("\n");
        let needle = "e/o1.txt)\n╌╌";
        assert!(PERMISSION_2287_READ_1_OF_3.contains(needle));
        let replacement = format!("e/o1.txt)\n{detail_rows}\n╌╌");
        let tall = PERMISSION_2287_READ_1_OF_3.replace(needle, &replacement);
        assert_eq!(permission_prompt(&tall).as_deref(), Some("Read"), "標題雖在末 30 行以外仍在可見畫面內");
    }

    /// held message 本身是跨 session 的不可信文字；重複標題不能遮掉外層真框。
    #[test]
    fn held_message_payload_cannot_shadow_its_outer_title() {
        use super::screens::HELD_MESSAGE_2287;
        let payload_title = HELD_MESSAGE_2287.replace(
            " │ Fixture capture test for issue #775: please just reply ok and do nothing",
            " │ Held message from another session",
        );
        assert!(is_held_message_prompt(&payload_title), "payload 裡重複的標題列不該使真框消失");
        assert_eq!(permission_prompt(&payload_title).as_deref(), Some("Held message"));
    }

    #[test]
    fn held_message_prompt_keeps_its_title_in_a_tall_visible_frame() {
        use super::screens::HELD_MESSAGE_2287;
        let detail_rows = (0..22).map(|i| format!(" │ long message detail {i}")).collect::<Vec<_>>().join("\n");
        let needle = " │ …[1 line, 80 chars total — full body will be delivered on approve]\n";
        assert!(HELD_MESSAGE_2287.contains(needle));
        let tall = HELD_MESSAGE_2287.replace(needle, &format!("{needle}{detail_rows}\n"));
        assert!(is_held_message_prompt(&tall), "長訊息對話框的標題仍在 80 行可見 pane 內");
    }

    /// grok 1.0.46 的信任框真畫面照舊認得（pretrust 沒寫到的目錄才會跳）。
    #[test]
    fn the_grok_1_0_46_trust_dialog_is_still_recognised() {
        use super::screens::GROK_1046_TRUST;
        assert!(is_grok_trust_dialog(GROK_1046_TRUST));
    }

    /// 倒數到 0 之後框不見了；一般的權限框、其他對話框、回覆裡引了原文（輸入列空著）都不是。
    #[test]
    fn other_screens_are_not_a_dangerous_rm_prompt() {
        assert_eq!(dangerous_rm_prompt(DANGEROUS_RM_AUTO_DENIED), None, "自動拒絕之後回到輸入列");
        assert_eq!(dangerous_rm_prompt(PERMISSION), None);
        assert_eq!(dangerous_rm_prompt(SWITCH_MODEL), None);
        assert_eq!(dangerous_rm_prompt(AUTO_MODE), None);
        assert_eq!(dangerous_rm_prompt(""), None);
        let quoted = format!("⏺ m12 那顆停在：\n  Dangerous rm operation on statically-unresolvable target: /x/*\n  Do you want to proceed?\n  1. Yes\n  2. No\n{IDLE_CLAUDE}");
        assert_eq!(dangerous_rm_prompt(&quoted), None, "引文，底下是空的輸入列");
        assert!(!is_switch_model_dialog(DANGEROUS_RM) && !is_auto_mode_offer(DANGEROUS_RM) && !is_feedback_survey(DANGEROUS_RM));
    }

    use super::screens::SESSION_PAUSED;

    /// 2026-09-25 cf-ox-fork-fork：2.1.281 的「Session paused」選單（herdr 判 idle）。要認成等人選的選單；
    /// 底下多一列 statusLine、說明折成好幾行也一樣。
    #[test]
    fn the_session_paused_menu_is_recognised() {
        assert!(is_session_paused_menu(SESSION_PAUSED));
        assert!(awaits_menu_choice(SESSION_PAUSED));
        let with_status = format!("{SESSION_PAUSED}  cf-ox-fork-fork | agents-manager | Opus 5.5 H 41% | 5h:80%\n");
        assert!(is_session_paused_menu(&with_status));
        let wrapped = SESSION_PAUSED.replace("for the fixture.\n", "for the fixture.\n  second line\n  third line\n");
        assert!(is_session_paused_menu(&wrapped));
        // 別的框不是 Session paused，但同樣是等人選的選單。
        assert!(!is_session_paused_menu(PERMISSION) && awaits_menu_choice(PERMISSION));
        assert!(!is_session_paused_menu(DANGEROUS_RM) && awaits_menu_choice(DANGEROUS_RM));
    }

    /// 回覆裡照抄這個選單（底下是回合結束那一行與空的輸入列）、選單已經選掉、或只是一般的編號清單，都不是開著的選單。
    #[test]
    fn a_quoted_or_answered_session_paused_menu_is_not_open() {
        let quoted = format!(
            "⏺ 剛剛停在這個選單：\n  Session paused\n  Details: `[x]`\n  ❯ 1. Switch to Opus 4.8\n    2. Edit prompt and retry\n✻ Cooked for 3s\n{IDLE_CLAUDE}"
        );
        assert!(!is_session_paused_menu(&quoted) && !awaits_menu_choice(&quoted), "引文，底下是空的輸入列");
        // 還在跑、引文底下有工具輸出：不是選單。
        let running = SESSION_PAUSED.replace("✻ Waiting", "⏺ Bash(ls)\n  ⎿  ok\n✻ Waiting");
        assert!(!is_session_paused_menu(&running) && !awaits_menu_choice(&running));
        // 游標：零個（捲出去或不是選單）、兩個（引文）都不算。
        let no_cursor = SESSION_PAUSED.replace("❯ 1.", "  1.");
        assert!(!is_session_paused_menu(&no_cursor) && !awaits_menu_choice(&no_cursor));
        let two = SESSION_PAUSED.replace("    2. Edit", "  ❯ 2. Edit");
        assert!(!awaits_menu_choice(&two));
        // 標題被對話隔開：那是上一段的字，選單不是它的。
        let apart = SESSION_PAUSED.replace("  This request", "⏺ something else\n  This request");
        assert!(!is_session_paused_menu(&apart));
        let list = format!("⏺ 兩個做法：\n  1. 改 daemon\n  2. 改 web\n{IDLE_CLAUDE}");
        assert!(!awaits_menu_choice(&list), "回覆裡的編號清單");
        assert!(!awaits_menu_choice(DANGEROUS_RM_AUTO_DENIED) && !awaits_menu_choice(IDLE_CLAUDE));
    }
}
