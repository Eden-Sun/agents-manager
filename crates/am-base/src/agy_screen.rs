//! agy（Antigravity CLI）會停下來等人的畫面（設計 A.8，1.2.16 實測；權限框文案取自 CHANGELOG，**未實測**）。
//!
//! 這些畫面 herdr 全判 `idle`（含 `interactive_ready:true`），而且 herdr 內建的 agy 偵測規則是舊版——所以 daemon 自己認：
//! 認到就標 blocked、**不送 prompt、一個鍵都不按**。登入、條款（預設已勾選「允許 Google 收集並使用我的 Interactions 資料」，
//! 一個 Enter 就切換同意與否）、色彩頁都只由人處理；信任框我們預寫 `trustedWorkspaces` 讓它不出現，出現了也不代按。
//!
//! 引文的防誤判：agy 在回覆裡把這些字抄一遍時，畫面上其實沒有框，輸入列（單獨一個 `>`）還在框標題的下面——用這個結構性事實分辨
//! （同 `tui_prompts::composer_is_idle` 的做法），不猜引文排得像不像框。標題上面的單獨 `>` 是回覆的引文，不算輸入列。

/// 只看畫面最底下這幾行（框一定在輸入區的位置；更上面是對話內容）。
const TAIL_LINES: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgyDialog {
    /// 未登入：`Select login method:`（1. Google OAuth／2. Use a Google Cloud project）。
    Login,
    /// 首次啟動的 `Choose your color scheme:`。
    ColorScheme,
    /// 首次啟動的 `Terms of Service & Data Use`。
    Terms,
    /// `Do you trust the contents of this project?`。
    Trust,
    /// 工具權限確認：`Run this command?`／`Allow access to this URL?`／`Allow access to this file?`／`Allow calling this tool?`。
    Permission,
}

impl AgyDialog {
    /// 409 的 `reason`：沒登入走既有的 `needs_login`，其餘都是 `dialog_open`。
    pub fn reason(self) -> &'static str {
        match self {
            AgyDialog::Login => "needs_login",
            _ => "dialog_open",
        }
    }

    /// 給人看的一句話（blocked 原因、系統訊息）。
    pub fn label(self) -> &'static str {
        match self {
            AgyDialog::Login => "agy 還沒登入：到「終端」分頁選 Google OAuth 完成登入（AG Man 不代按）",
            AgyDialog::ColorScheme => "agy 首次啟動的色彩選擇頁：到「終端」分頁選好再送（AG Man 不代按）",
            AgyDialog::Terms => "agy 首次啟動的「Terms of Service & Data Use」頁：預設已勾選允許 Google 使用 Interactions 資料，請本人到「終端」分頁決定（AG Man 不代按）",
            AgyDialog::Trust => "agy 在問「要不要信任這個專案資料夾」：到「終端」分頁回答（AG Man 沒能預先信任）",
            AgyDialog::Permission => "agy 等待工具權限確認：到「終端」分頁回答",
        }
    }
}

/// 一行去掉框線與空白、轉小寫，用來比對標題。
fn norm(line: &str) -> String {
    let cleaned = crate::composer_parse::strip_ansi(line);
    let t: String = cleaned.chars().filter(|c| !"│┃╭╮╰╯─━".contains(*c)).collect();
    t.trim().to_lowercase()
}

/// 輸入列：單獨一個 `>` 或 agy 的空框 placeholder（框線不算）。對話框裡的選項列是 `> 1. …`／`> [x] …`，後面還有字，不會誤中。
fn is_composer_row(line: &str) -> bool {
    let cleaned = crate::composer_parse::strip_ansi(line);
    let t = cleaned.trim();
    if let Some(rest) = t.strip_prefix('>') {
        let after = rest.trim();
        return after.is_empty() || crate::composer_parse::is_agy_placeholder(after);
    }
    let mut chars = cleaned.chars().filter(|c| !c.is_whitespace() && !"│┃╭╮╰╯─━▔".contains(*c));
    chars.next() == Some('>') && chars.next().is_none()
}

pub fn blocking_dialog(screen: &str) -> Option<AgyDialog> {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(TAIL_LINES)..];
    let rows: Vec<String> = tail.iter().map(|l| norm(l)).collect();
    let has = |f: &dyn Fn(&str) -> bool| rows.iter().any(|r| f(r));
    let title = |t: &str| {
        rows.iter().position(|r| {
            r == t || r.strip_suffix(t).is_some_and(|pre| pre.chars().all(|c| !c.is_alphanumeric() && !"●⏺⎿•>*-\"'`".contains(c)))
        })
    };
    let footer = |text: &str, button: &dyn Fn(&str) -> bool| {
        let n = rows.len();
        let last = &rows[n.saturating_sub(5)..];
        last.iter().any(|r| r.starts_with("↑/↓ navigate · enter") && r.contains(text)) && last.iter().any(|r| button(r))
    };
    if footer("confirm", &|r| r == "[next]") && !tail.iter().any(|l| is_composer_row(l)) {
        return Some(AgyDialog::ColorScheme);
    }
    if footer("toggle", &|r| r.contains("[done]")) && !tail.iter().any(|l| is_composer_row(l)) {
        return Some(AgyDialog::Terms);
    }
    let (at, dialog) = if let Some(i) = title("select login method:")
        .or_else(|| rows.iter().position(|r| r.starts_with("welcome to the antigravity cli") && r.contains("not signed in")))
    {
        (i, AgyDialog::Login)
    } else if let Some(i) = title("terms of service & data use") {
        (i, AgyDialog::Terms)
    } else if let Some(i) = title("choose your color scheme:") {
        (i, AgyDialog::ColorScheme)
    } else if let Some(i) = title("do you trust the contents of this project?")
        .filter(|_| has(&|r| r.contains("yes, i trust this folder") || r.contains("no, exit")))
    {
        (i, AgyDialog::Trust)
    } else {
        let i = [
            "run this command?",
            "allow access to this url?",
            "allow access to this file?",
            "allow calling this tool?",
        ]
        .iter()
        .find_map(|t| title(t))?;
        (i, AgyDialog::Permission)
    };
    if tail[at + 1..].iter().any(|l| is_composer_row(l)) {
        return None;
    }
    Some(dialog)
}

pub fn shortcuts_help_open(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].iter().any(|l| {
        let n = norm(l);
        n.starts_with("keyboard:") && n.contains("esc close")
    })
}

const FEEDBACK_QUESTION: &str = "how's the cli experience so far?";
const FEEDBACK_OPTION_ROWS: usize = 3;
const FEEDBACK_FOOTER: &str = "? for shortcuts";

fn words(line: &str) -> String {
    norm(line).replace('’', "'").split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn feedback_survey_span(screen: &str) -> Option<(usize, usize)> {
    let rows: Vec<(usize, String)> = screen
        .lines()
        .enumerate()
        .map(|(n, l)| (n, words(l)))
        .filter(|(_, w)| !w.is_empty())
        .collect();
    for k in 0..rows.len() {
        if !rows[k].1.contains(FEEDBACK_QUESTION) {
            continue;
        }
        let mut options = String::new();
        let mut last = None;
        for j in k + 1..rows.len().min(k + 1 + FEEDBACK_OPTION_ROWS) {
            let w = &rows[j].1;
            if ["[0]", "[1]", "[2]", "[3]"].iter().any(|t| w.contains(*t)) {
                options.push_str(w);
                options.push(' ');
                last = Some(j);
            }
        }
        let Some(last) = last else { continue };
        let others = ["[1] good", "[2] fine", "[3] bad"].iter().filter(|label| options.contains(**label)).count();
        if !options.contains("[0] skip") || others < 2 {
            continue;
        }
        if rows[last + 1..].iter().all(|(_, w)| w.contains(FEEDBACK_FOOTER)) {
            return Some((rows[k].0, rows[last].0));
        }
    }
    None
}

pub fn feedback_survey_open(screen: &str) -> bool {
    feedback_survey_span(screen).is_some()
}

/// 一行是不是輸入框的上下框線（整行都是 `─`，至少 20 格）。
fn is_rule(line: &str) -> bool {
    let t = crate::composer_parse::strip_ansi(line);
    let t = t.trim();
    t.chars().count() >= 20 && t.chars().all(|c| c == '─' || c == '━')
}

/// `● [09:18:37] … running`：輸入框下方的背景工作列。
fn is_task_running_row(line: &str) -> bool {
    let cleaned = crate::composer_parse::strip_ansi(line);
    let t = cleaned.trim();
    let Some(rest) = t.strip_prefix('●') else { return false };
    let rest = rest.trim_start();
    let Some(inner) = rest.strip_prefix('[').and_then(|r| r.split_once(']')) else { return false };
    let b = inner.0.as_bytes();
    b.len() == 8 && b[2] == b':' && b[5] == b':' && b.iter().enumerate().all(|(i, c)| i == 2 || i == 5 || c.is_ascii_digit()) && inner.1.trim_end().ends_with("running")
}

/// 1.5.16 的 spinner（braille）開頭。
fn is_spinner(c: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&c)
}

/// `Thinking... 12s`、`Running command... 18s`：階段名（ASCII 字母，首字大寫）後接 `...` 與一或多個時長（`12s`、`1m`、`250ms`）。
fn is_timer_row(t: &str) -> bool {
    let t = t.trim_start_matches(is_spinner).trim_start();
    let Some((label, rest)) = t.split_once("...") else { return false };
    let label_ok = label.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && label.chars().all(|c| c.is_ascii_alphabetic() || c == ' ');
    let durations: Vec<&str> = rest.split_whitespace().collect();
    label_ok && !durations.is_empty() && durations.iter().all(|w| is_duration(w))
}

fn is_duration(w: &str) -> bool {
    ["ms", "h", "m", "s"].iter().find_map(|suf| w.strip_suffix(suf)).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// `3/9 steps`（可附 ` · 1 skipped`）：步驟進度頭。回（完成, 總數）。
fn steps_progress(t: &str) -> Option<(u32, u32)> {
    let t = t.trim_start_matches(|c: char| c.is_whitespace() || is_spinner(c) || "●○└…⋮✓✗⊘•▸▶".contains(c));
    let (done, rest) = t.split_once('/')?;
    let total: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let after = rest[total.len()..].strip_prefix(" steps")?;
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    Some((done.parse().ok()?, total.parse().ok()?))
}

/// 進度區的單行，不論有沒有進行中的標記：計時列、步驟頭、`⋮ N earlier`／`⋮ N more`。
/// 只看這些結構，不靠任何會出現在正文裡的字（引文要是剛好排成一行，也只會是計時列形狀）。
pub fn is_progress_line(line: &str) -> bool {
    let cleaned = crate::composer_parse::strip_ansi(line);
    let t = cleaned.trim();
    is_timer_row(t) || steps_progress(t).is_some() || (t.starts_with('⋮') && (t.ends_with(" earlier") || t.ends_with(" more")))
}

/// `1. 讀規格`、`> 2. 跑測試`（游標可有可無）：編號步驟項目。數字不單獨構成進度，只在區塊裡才算。
fn is_numbered_step(t: &str) -> bool {
    let t = t.trim_start_matches(|c: char| c.is_whitespace() || ">❯›".contains(c));
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    (1..=2).contains(&digits) && t[digits..].strip_prefix(['.', ')']).is_some_and(|rest| rest.starts_with(' '))
}

/// 進度區的單行，連步驟項目也算（`✓`／`✗`／`⊘`／`[x]`／`[ ]`／`•`／`▸`／`▶`、編號步驟）與 `●`／`○`／`└`／`…`／spinner 的進度列。
fn is_status_row(line: &str) -> bool {
    let cleaned = crate::composer_parse::strip_ansi(line);
    let t = cleaned.trim();
    is_progress_line(t)
        || is_numbered_step(t)
        || t.starts_with(['●', '○', '└', '…', '✓', '✗', '⊘', '•', '▸', '▶'])
        || t.starts_with("[x]")
        || t.starts_with("[X]")
        || t.starts_with("[ ]")
        || t.chars().next().is_some_and(is_spinner)
}

/// 進行中的標記：計時列、`○ Running (`、還沒做完的步驟頭（`N/M steps`，N < M）。
fn is_busy_marker(line: &str) -> bool {
    let cleaned = crate::composer_parse::strip_ansi(line);
    let t = cleaned.trim();
    is_timer_row(t) || t.starts_with("○ Running (") || steps_progress(t).is_some_and(|(done, total)| done < total)
}

/// 輸入框（`top` 那條框線）上方、緊鄰它的尾端狀態區塊的起點（#2196，agy 1.5.16 的步驟清單與分階段計時）。
/// 從 `top` 往上連續收進可見的進度列（[`is_status_row`]，空行略過），碰到第一行正文就停。
/// 區塊裡沒有進行中的標記（[`is_busy_marker`]）就回 `None`：只有步驟項目、沒有計時的畫面不是忙碌。
///
/// 只看框線緊上方的區塊，所以正文裡引述的 `Thinking... 12s` 之後只要還有一行正文，就不會被算進來。
pub fn status_block_start(lines: &[&str], top: usize) -> Option<usize> {
    let mut start = None;
    let mut marker = false;
    for (i, line) in lines[..top].iter().enumerate().rev() {
        if crate::composer_parse::strip_ansi(line).trim().is_empty() {
            continue;
        }
        if !is_status_row(line) {
            break;
        }
        marker |= is_busy_marker(line);
        start = Some(i);
    }
    if marker { start } else { None }
}

/// 輸入框上方尾端進度區的行範圍（[`status_block_start`] 到輸入框頂線），給 am-lifecycle 的選單與提示判斷先略過（#2196）。
pub fn status_block_range(screen: &str) -> Option<std::ops::Range<usize>> {
    let lines: Vec<&str> = screen.lines().collect();
    let rules: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| is_rule(l)).map(|(i, _)| i).collect();
    let [.., top, _] = rules[..] else { return None };
    status_block_start(&lines, top).map(|start| start..top)
}

/// agy 這一刻是不是還在工作（#2146、#2196）：herdr 報 idle、Stop hook 也來過，agy 仍可能在跑前景工具或背景工作。
/// 只認結構，不全畫面搜尋 `Running`、也不看 `N task(s)`（常駐 daemon task 會永遠擋住）：
/// 1. 輸入框（最後兩條框線之間）下方的 footer 有 `esc to cancel`；
/// 2. 輸入框下方有 `● [HH:MM:SS] … running` 的背景工作列；
/// 3. 輸入框緊上方的尾端狀態區塊（[`status_block_start`]）有進行中的標記（`○ Running (`、`Thinking... 12s`、`N/M steps` 未完成），
///    且區塊與輸入框下方都沒有 `interrupted`。
///
/// 找不到輸入框就判不忙（維持原本行為）。
pub fn is_busy(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    let rules: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| is_rule(l)).map(|(i, _)| i).collect();
    let [.., top, bottom] = rules[..] else { return false };
    let below = &lines[bottom + 1..];
    if below.iter().any(|l| crate::composer_parse::strip_ansi(l).to_lowercase().contains("esc to cancel")) {
        return true;
    }
    if below.iter().any(|l| is_task_running_row(l)) {
        return true;
    }
    let Some(start) = status_block_start(&lines, top) else { return false };
    !lines[start..top].iter().chain(below).any(|l| crate::composer_parse::strip_ansi(l).to_lowercase().contains("interrupted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN: &str = "\
 Welcome to the Antigravity CLI. You are currently not signed in.

 Select login method:

 > 1. Google OAuth
   2. Use a Google Cloud project
";
    const COLOR: &str = "\n Choose your color scheme:\n\n > 1. Dark\n   2. Light\n";
    const TERMS: &str = "\
 Terms of Service & Data Use

 > [x] Yes, I agree to help improve Antigravity CLI by allowing Google to collect and use my Interactions data.
   Done
";
    const TRUST: &str = "\
 Accessing workspace:
 /home/user/project

 Do you trust the contents of this project?

 > Yes, I trust this folder
   No, exit
";
    const PERMISSION: &str = "\n ⚡ Run this command?\n\n   git status\n   Reason: check the tree\n\n > Allow once\n   Deny\n";
    #[test]
    fn feedback_survey_is_told_from_quoted_text() {
        let survey = " How's the CLI experience so far? Help us improve:\n [1] Good  [2] Fine  [3] Bad  [0] Skip\n? for shortcuts      Gemini 4 Neon · high\n";
        assert!(feedback_survey_open(survey));
        assert_eq!(feedback_survey_span(survey), Some((0, 1)));
        let under_banner = "❯ 幫我查\n⚠ Individual quota reached. Resets in\n13m11s.\n How’s the CLI experience so far? Help us improve:\n [1] Good  [2] Fine  [3] Bad  [0] Skip\n? for shortcuts\n";
        assert_eq!(feedback_survey_span(under_banner), Some((3, 4)));
        let quoted_then_composer = "❯ 幫我查\n你看到的是：\n How's the CLI experience so far? Help us improve:\n [1] Good  [2] Fine  [3] Bad  [0] Skip\n─────────────────────\n>\n─────────────────────\n? for shortcuts\n";
        assert!(!feedback_survey_open(quoted_then_composer));
        let quoted_then_prose = "How's the CLI experience so far? Help us improve:\n [1] Good  [2] Fine  [3] Bad  [0] Skip\n我剛剛把問卷原樣貼給你。\n";
        assert!(!feedback_survey_open(quoted_then_prose));
        let prose = "The survey asks How's the CLI experience so far? and [0] Skip is one choice.\nmore\n";
        assert!(!feedback_survey_open(prose));
        assert!(!feedback_survey_open("[1] Good  [2] Fine  [3] Bad  [0] Skip\n? for shortcuts\n"));
    }

    #[test]
    fn shortcuts_help_is_told_from_the_composer() {
        let help = " Keyboard Shortcuts\n > /   Open slash commands\n  [1-14 of 27 items]\n Keyboard: ↑/↓ Navigate  ←/→ Switch View  esc Close\n";
        assert!(shortcuts_help_open(help));
        assert!(!shortcuts_help_open("──────\n>\n──────\n ? for shortcuts      Gemini 4 Neon · high\n"));
        assert!(!shortcuts_help_open("Keyboard: ↑/↓ Navigate  esc Close\nmore\nmore\nmore\nmore\n"));
    }

    const MAIN: &str = "\
 Antigravity CLI 1.2.16
 Gemini 3.1 Pro (Low)
 /home/user/project
────────────────────────────────────────────
 >
────────────────────────────────────────────
";

    #[test]
    fn every_first_run_and_permission_screen_is_recognised() {
        assert_eq!(blocking_dialog(LOGIN), Some(AgyDialog::Login));
        assert_eq!(blocking_dialog(COLOR), Some(AgyDialog::ColorScheme));
        assert_eq!(blocking_dialog(TERMS), Some(AgyDialog::Terms));
        assert_eq!(blocking_dialog(TRUST), Some(AgyDialog::Trust));
        assert_eq!(blocking_dialog(PERMISSION), Some(AgyDialog::Permission));
        for url in ["Allow access to this URL?", "Allow access to this file?", "Allow calling this tool?"] {
            assert_eq!(blocking_dialog(&format!("\n {url}\n > Allow\n   Deny\n")), Some(AgyDialog::Permission), "{url}");
        }
    }

    /// 依 agy 1.5.2 changelog 的提示文字合成（非實機擷取），issue #2083。
    const SYNTHETIC_FILE_PERMISSION_152: &str = "\
      ▄▀▀▄        Antigravity CLI 1.5.2
     ▀▀▀▀▀▀       me@example.com (Google AI Plus)
────────────────────────────────────────────────────────
> Read the file /etc/hosts outside the workspace.

● Read(/etc/hosts) (ctrl+o to expand)

File Access
────────────────────────────────────────────────────────

Requesting permission for:
   /etc/hosts

Allow access to this file?
> 1. Allow once
  2. Always allow for this conversation
  3. Always allow in settings.json
  4. Deny
";

    #[test]
    fn file_access_permission_is_recognised() {
        assert_eq!(blocking_dialog(SYNTHETIC_FILE_PERMISSION_152), Some(AgyDialog::Permission));
    }

    /// agy 1.5.8 自訂狀態列被暫時覆蓋（如 Copied...、設定錯誤、權限重載通知）與 spinner 動畫中不誤判為對話框（issue #2114）。
    #[test]
    fn status_messages_and_spinner_are_not_dialogs() {
        let copied = format!("{MAIN}\n? for shortcuts    Copied 3 lines (42 characters)\n");
        assert_eq!(blocking_dialog(&copied), None);
        let save_err = format!("{MAIN}\nFailed to save settings: permission denied\n");
        assert_eq!(blocking_dialog(&save_err), None);
        let reload = format!("{MAIN}\nPermission rules reloaded from settings.json\n");
        assert_eq!(blocking_dialog(&reload), None);
        let spinner = "      ▄▀▀▄        Antigravity CLI 1.5.8\n\n⠋ Thinking...\n\n";
        assert_eq!(blocking_dialog(spinner), None);
    }

    /// 2026-10-04 真機（agy 1.2.16，登入後，沒開 auto_approve）：要跑 `ls` 時的權限框。
    const REAL_PERMISSION: &str = "\
      ▄▀▀▄        Antigravity CLI 1.2.16
     ▀▀▀▀▀▀       me@example.com (Google AI Plus)
────────────────────────────────────────────────────────
> Run the shell command `ls` in the current directory and tell me the output.

● Bash(ls) (ctrl+o to expand)

Command
────────────────────────────────────────────────────────

Requesting permission for:
   ls

Run this command?
> 1. Yes, run command
  2. Yes, and always allow in this conversation for commands that start with 'ls'
  3. Yes, and always allow for commands that start with 'ls' (Persist to settings.json)
  4. No, cancel

  ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command
esc to cancel
";
    /// 同一個 pane 閒著的真畫面：輸入列單獨一個 `>`，底下一條分隔線與 `? for shortcuts`。
    const REAL_IDLE: &str = "\
      ▄▀▀▄        Antigravity CLI 1.2.16
     ▀▀▀▀▀▀       me@example.com (Google AI Plus)
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
? for shortcuts                                              Gemini 3.8 Flash (Medium)
";

    #[test]
    fn the_real_1_2_16_permission_screen_is_a_dialog_and_the_idle_one_is_not() {
        assert_eq!(blocking_dialog(REAL_PERMISSION), Some(AgyDialog::Permission));
        assert_eq!(blocking_dialog(REAL_IDLE), None);
    }

    #[test]
    fn only_login_is_needs_login_and_every_dialog_has_words_for_a_person() {
        assert_eq!(AgyDialog::Login.reason(), "needs_login");
        for d in [AgyDialog::ColorScheme, AgyDialog::Terms, AgyDialog::Trust, AgyDialog::Permission] {
            assert_eq!(d.reason(), "dialog_open", "{d:?}");
        }
        for d in [AgyDialog::Login, AgyDialog::ColorScheme, AgyDialog::Terms, AgyDialog::Trust, AgyDialog::Permission] {
            assert!(d.label().contains("終端"), "{d:?}");
        }
    }

    #[test]
    fn the_idle_main_screen_is_not_a_dialog() {
        assert_eq!(blocking_dialog(MAIN), None);
        assert_eq!(blocking_dialog(""), None);
    }

    /// 回覆把框的字抄一遍：畫面上沒有真的框，輸入列還在。
    #[test]
    fn a_reply_quoting_the_dialog_text_is_not_a_dialog() {
        let quoted = format!("{LOGIN}\n{MAIN}");
        assert_eq!(blocking_dialog(&quoted), None, "輸入列 `>` 在＝沒有框擋著");
        let prose = "● agy 提到 Do you trust the contents of this project? 這句話\n  還有 Select login method: 這句\n";
        assert_eq!(blocking_dialog(prose), None, "不是整行標題");
    }

    #[test]
    fn a_trust_title_without_its_choices_is_not_the_dialog() {
        assert_eq!(blocking_dialog("\n Do you trust the contents of this project?\n"), None);
    }

    /// 回覆引文裡有單獨一個 `>`（markdown 引用的空行）排在權限框上面：那一行不是輸入列，框照樣要認到。
    /// 以前尾段只要有單獨的 `>` 就當閒置，權限框被略過，送來的 prompt 會打進框裡。
    #[test]
    fn a_lone_quote_marker_above_a_permission_dialog_is_not_the_composer() {
        let screen = format!("● 引文：\n>\n> 這是引用\n{PERMISSION}");
        assert_eq!(blocking_dialog(&screen), Some(AgyDialog::Permission));
    }

    #[test]
    fn trust_dialog_with_ansi_styling_is_recognised() {
        let ansi = "Accessing workspace:\n/home/user/project\n\n\x1b[1mDo you trust the contents of this project?\x1b[0m\n\n\x1b[36m> Yes, I trust this folder\x1b[0m\n\x1b[2m  No, exit\x1b[0m\n";
        assert_eq!(blocking_dialog(ansi), Some(AgyDialog::Trust));
    }

    /// 2026-11-15 真機（agy 1.5.9，影子家目錄第一次啟動，120×40）：色彩預覽頁很高，標題 `Choose your color scheme:` 被擠出最後 24 列，
    /// 以前判成 idle、派工送進去卡一小時。現在靠頁尾 `[Next]`＋`↑/↓ Navigate · enter Confirm` 認。
    const REAL_COLOR_WIZARD_159: &str = r#"
    ▄▀▀▄
   ▀▀▀▀▀▀
  ▀▀▀▀▀▀▀▀
 ▄▀▀    ▀▀▄
▄▀▀      ▀▀▄

Welcome to Antigravity CLI!

Choose your color scheme:        ╭─────────────────────────────────────────────────────────────╮
                                 │ > you: add a greeting function                              │
  > terminal                     │                                                             │
    light                        │   Here's the change:                                        │
    solarized light              │                                                             │
    colorblind-friendly light    │   3     import "fmt"                                        │
    dark                         │   4                                                         │
    solarized dark               │   5  -  func main() {                                       │
    colorblind-friendly dark     │   5  +  func greet(name string) {                           │
    tokyo night                  │   6  +      fmt.Printf("Hello, %s!\n", name)                │
                                 │   7     }                                                   │
                                 │                                                             │
                                 │   ▾ Thought Process                                         │
                                 │     I need to add a greeting function. I'll use fmt.Printf. │
                                 │   ⚙ tool: write_file main.go                                │
                                 │   ◉ task: Implementing greeting                             │
                                 │   ✗ error: compilation failed                               │
                                 │   ⚠ warning: deprecation warning                            │
                                 │   → link: file:///path/to/main.go                           │
                                 │   ★ accent: highlighted text                                │
                                 │   · dim: press Enter to continue                            │
                                 ╰─────────────────────────────────────────────────────────────╯

    [Next]

  ↑/↓ Navigate · enter Confirm
                                                                                                 Gemini 3.8 Flash · high"#;
    /// 同一次啟動按一個 Enter 後的 Terms 頁（預設勾選同意；人處理，我們不代按）。
    const REAL_TERMS_159: &str = r#"Terms of Service & Data Use

Antigravity CLI can make mistakes and is vulnerable to prompt injection and supply chain risks. Ensure that you monitor and verify all actions taken by the agent.
--------------------------------------------------------------------------------------------------------------
  > [x] Yes, I agree to help improve Antigravity CLI by allowing
      Google to collect and use my Interactions data,
      subject to the Google Antigravity CLI Terms of Service
      and Google Privacy Policy. I understand I can
      choose to opt out later whenever I want via my
      settings.
      Links:
      - Terms of Service: https://antigravity.google/terms
      - Privacy Policy: https://policies.google.com/privacy
    [Previous]      [Done]
  ↑/↓ Navigate · enter Toggle
                                                                                                 Gemini 3.8 Flash · high"#;

    #[test]
    fn the_159_first_run_wizard_pages_are_blocked_not_idle() {
        assert_eq!(blocking_dialog(REAL_COLOR_WIZARD_159), Some(AgyDialog::ColorScheme));
        assert_eq!(blocking_dialog(REAL_TERMS_159), Some(AgyDialog::Terms));
        assert_eq!(blocking_dialog(&format!("{REAL_COLOR_WIZARD_159}\n>\n")), None);
        assert_eq!(blocking_dialog(REAL_IDLE), None);
        assert_eq!(blocking_dialog("● 步驟說明：按 [Next] 之後看到 ↑/↓ Navigate · enter Confirm\n\n>\n? for shortcuts\n"), None);
    }

    const BUSY_FG: &str = "● Edited 1 file (a.rs)\n○ Running (wait-log.py --timeout 540 · 12s)\n────────────────────────────────────────────────────────\n>\n────────────────────────────────────────────────────────\n  Gemini 3.8 Flash · high\n";
    const BUSY_BG: &str = "  正在等待。\n────────────────────────────────────────────────────────\n>\n────────────────────────────────────────────────────────\n● [09:18:37] check.sh changed running\nesc to cancel      Gemini 3.8 Flash · high · 1 task(s) · /tasks\n";
    const IDLE_QUOTE: &str = "  我看到輸出裡有 ○ Running (x) 與 Running 字樣，已完成。\n────────────────────────────────────────────────────────\n>\n────────────────────────────────────────────────────────\n  Gemini 3.8 Flash · high · 1 task(s) · /tasks\n";
    const IDLE_AFTER_RUNNING: &str = "○ Running (wait-log.py · 12s)\n  做完了，結果如下。\n────────────────────────────────────────────────────────\n>\n────────────────────────────────────────────────────────\n  Gemini 3.8 Flash · high\n";

    #[test]
    fn busy_screen_is_recognised_by_structure() {
        assert!(is_busy(BUSY_FG), "前景 Running 在輸入框上方、其後沒有正文");
        assert!(is_busy(BUSY_BG), "背景 task running + esc to cancel");
        assert!(!is_busy(IDLE_QUOTE), "正文引用 Running、只剩常駐 daemon task 且無 esc to cancel");
        assert!(!is_busy(IDLE_AFTER_RUNNING), "Running 之後已有新的正文");
        assert!(!is_busy("no composer here\n○ Running (x)\n"), "找不到輸入框不判忙");
    }

    /// 1.5.16 合成樣本（依 issue #2196 的描述合成，非實機擷取）：步驟清單＋分階段計時，輸入框在最底下，footer 沒有 `esc to cancel`。
    const STEPS_RUNNING_156: &str = "\
● Edited 1 file (a.rs)
  我先讀規格，再改解析器。
3/9 steps · 1 skipped
  ✓ Read the spec
  ✓ Write the parser
> 3. Run the tests
  4. [ ] Update docs
  ⋮ 1 earlier
  [ ] Write the changelog
  ⋮ 2 more
Running command... 18s
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
  Gemini 3.8 Flash · high
";
    const THINKING_156: &str = "\
  我來看看檔案。
Thinking... 12s
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
  Gemini 3.8 Flash · high
";
    const STEPS_INTERRUPTED_156: &str = "\
3/9 steps
  ✓ Read the spec
● Interrupted
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
? for shortcuts                                              Gemini 3.8 Flash · high
";
    const STEPS_INTERRUPTED_FOOTER_156: &str = "\
3/9 steps
  ✓ Read the spec
  ▸ Run the tests
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
Interrupted                                                  Gemini 3.8 Flash · high
";
    const STEPS_DONE_156: &str = "\
● Edited 1 file (a.rs)
  做完了，結果如下。
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
? for shortcuts                                              Gemini 3.8 Flash · high
";
    const QUOTED_156: &str = "\
  我看到畫面上寫著「Thinking... 12s」與「2/5 steps」，之後就結束了。
  這段是引文，不是進度。
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
? for shortcuts                                              Gemini 3.8 Flash · high
";
    const QUOTED_TIMER_LINE_156: &str = "\
  Thinking... 12s
  上面那行只是我在抄寫。
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
? for shortcuts                                              Gemini 3.8 Flash · high
";

    #[test]
    fn step_list_with_phase_timer_is_busy_without_esc_to_cancel() {
        assert!(!STEPS_RUNNING_156.to_lowercase().contains("esc to cancel"));
        assert!(is_busy(STEPS_RUNNING_156), "3/9 進行中＋Running command 計時");
        assert!(is_busy(THINKING_156), "單獨一列 Thinking... 12s");
    }

    #[test]
    fn step_list_with_interrupted_is_not_busy() {
        assert!(!is_busy(STEPS_INTERRUPTED_156), "步驟清單裡的 ● Interrupted");
        assert!(!is_busy(STEPS_INTERRUPTED_FOOTER_156), "輸入框下方的 Interrupted");
        assert_eq!(blocking_dialog(STEPS_INTERRUPTED_156), None);
        assert_eq!(blocking_dialog(STEPS_INTERRUPTED_FOOTER_156), None);
    }

    #[test]
    fn finished_steps_and_empty_composer_are_not_busy() {
        assert!(!is_busy(STEPS_DONE_156));
        assert!(!is_busy("● Edited 1 file (a.rs)\n3/3 steps\n  ✓ a\n  ✓ b\n  ✓ c\n────────────────────────────────────────────────────────\n>\n────────────────────────────────────────────────────────\n"), "全部完成的步驟頭不是進行中");
        assert_eq!(blocking_dialog(STEPS_DONE_156), None);
    }

    #[test]
    fn quoted_timer_and_steps_in_a_reply_are_not_busy() {
        assert!(!is_busy(QUOTED_156), "引文夾在正文裡，框線上方不是進度區");
        assert!(!is_busy(QUOTED_TIMER_LINE_156), "引文最底下還有一行正文，計時列不在框線上方");
        assert_eq!(blocking_dialog(QUOTED_156), None);
        assert_eq!(blocking_dialog(QUOTED_TIMER_LINE_156), None);
    }

    #[test]
    fn step_list_screens_are_not_blocking_dialogs() {
        for screen in [STEPS_RUNNING_156, THINKING_156, STEPS_DONE_156] {
            assert_eq!(blocking_dialog(screen), None, "{screen}");
        }
    }

    #[test]
    fn status_block_covers_only_the_run_above_the_composer() {
        let lines: Vec<&str> = STEPS_RUNNING_156.lines().collect();
        let top = lines.iter().position(|l| is_rule(l)).unwrap();
        assert_eq!(status_block_start(&lines, top), Some(2), "從 3/9 steps 起算到框線");
        let quoted: Vec<&str> = QUOTED_156.lines().collect();
        let quoted_top = quoted.iter().position(|l| is_rule(l)).unwrap();
        assert_eq!(status_block_start(&quoted, quoted_top), None);
        assert!(is_progress_line("  Running command... 18s"));
        assert!(is_progress_line("⋮ 2 more"));
        assert!(!is_progress_line("  我看到 Thinking... 12s"));
    }

    #[test]
    fn interrupted_screens_do_not_misjudge_busy_or_blocking_dialog() {
        let teardown_footer = "\
● Edited 1 file (src/lib.rs)
○ Running (cargo test · 5s)
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
Interrupted · esc to cancel      Gemini 3.8 Flash · high
";
        let teardown_above = "\
● Edited 1 file (src/lib.rs)
○ Running (cargo test · 5s)
Interrupted
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
esc to cancel      Gemini 3.8 Flash · high
";
        assert!(is_busy(teardown_footer));
        assert_eq!(blocking_dialog(teardown_footer), None);
        assert!(is_busy(teardown_above));
        assert_eq!(blocking_dialog(teardown_above), None);

        let idle_footer = "\
● Edited 1 file (src/lib.rs)
○ Running (cargo test · 5s)
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
Interrupted                                                  Gemini 3.8 Flash · high
";
        let idle_above_bullet = "\
● Edited 1 file (src/lib.rs)
○ Running (cargo test · 5s)
● Interrupted
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
? for shortcuts                                              Gemini 3.8 Flash · high
";
        let idle_above_plain = "\
Interrupted by user.
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
? for shortcuts                                              Gemini 3.8 Flash · high
";
        let idle_placeholder = "\
● Edited 1 file (src/lib.rs)
○ Running (cargo test · 5s)
────────────────────────────────────────────────────────
> Mode: default (shift+tab to cycle)
────────────────────────────────────────────────────────
Interrupted                                                  Gemini 3.8 Flash · high
";
        for screen in [idle_footer, idle_above_bullet, idle_above_plain, idle_placeholder] {
            assert!(!is_busy(screen));
            assert_eq!(blocking_dialog(screen), None);
            assert_eq!(crate::composer_parse::box_state("agy", screen), crate::composer_parse::BoxState::Empty);
        }

        let queued_convo = "\
● Edited 1 file (src/lib.rs)
Interrupted
> please check git status
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
Interrupted · 1 queued message                               Gemini 3.8 Flash · high
";
        let queued_footer = "\
● Edited 1 file (src/lib.rs)
○ Running (cargo test · 5s)
● Interrupted
────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────
Queued: please check git status                              Gemini 3.8 Flash · high
";
        assert_eq!(blocking_dialog(queued_convo), None);
        assert_eq!(blocking_dialog(queued_footer), None);
    }
}
