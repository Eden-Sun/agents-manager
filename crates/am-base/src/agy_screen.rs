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
    // 整行就是標題（對話框的標題自己一行，前面最多一個圖示）；回覆裡的引文前面是項目符號、引號或別的字，不算。
    // 回傳標題所在的列，用來判斷輸入列是在框上面還是下面。
    let title = |t: &str| {
        rows.iter().position(|r| {
            r == t || r.strip_suffix(t).is_some_and(|pre| pre.chars().all(|c| !c.is_alphanumeric() && !"●⏺⎿•>*-\"'`".contains(c)))
        })
    };
    // 1.5.9 的首次精靈（色彩預覽頁、Terms 頁）比 `TAIL_LINES` 高，標題被擠出視窗；頁尾操作列是結構性的指紋：
    // 色彩頁 `[Next]`＋`↑/↓ Navigate · enter Confirm`、Terms 頁 `[Done]`＋`↑/↓ Navigate · enter Toggle`，都在畫面最底下幾列。
    // 回覆的引文不會剛好在最底下又沒有輸入列。
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
    } else if let Some(i) = [
        "run this command?",
        "allow access to this url?",
        "allow access to this file?",
        "allow calling this tool?",
    ]
    .iter()
    .find_map(|t| title(t))
    {
        (i, AgyDialog::Permission)
    } else {
        return None;
    };
    // 輸入列在標題下面＝框已經關了（回覆抄了框的字，畫面上只剩輸入列）。標題上面的單獨 `>` 是回覆裡的引文，框照樣要認到。
    if tail[at + 1..].iter().any(|l| is_composer_row(l)) {
        return None;
    }
    Some(dialog)
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

/// agy 這一刻是不是還在工作（#2146）：herdr 報 idle、Stop hook 也來過，agy 仍可能在跑前景工具或背景工作。
/// 只認結構，不全畫面搜尋 `Running`、也不看 `N task(s)`（常駐 daemon task 會永遠擋住）：
/// 1. 輸入框（最後兩條框線之間）下方的 footer 有 `esc to cancel`；
/// 2. 輸入框下方有 `● [HH:MM:SS] … running` 的背景工作列；
/// 3. 輸入框上方，最後一行 `○ Running (` 之後沒有新的 assistant 正文（只有 `●`／`○`／`└`／spinner 的進度列）。
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
    let above: Vec<String> = lines[..top].iter().map(|l| crate::composer_parse::strip_ansi(l)).filter(|l| !l.trim().is_empty()).collect();
    let Some(at) = above.iter().rposition(|l| l.trim_start().starts_with("○ Running (")) else { return false };
    above[at + 1..].iter().all(|l| {
        let t = l.trim_start();
        t.starts_with('●') || t.starts_with('○') || t.starts_with('└') || t.starts_with('…') || t.chars().next().is_some_and(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
    })
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
        // 輸入列出現（精靈已過）就不是對話框；回覆裡引文也不誤判。
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
}
