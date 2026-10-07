//! agy（Antigravity CLI）會停下來等人的畫面（設計 A.8，1.2.16 實測；權限框文案取自 CHANGELOG，**未實測**）。
//!
//! 這些畫面 herdr 全判 `idle`（含 `interactive_ready:true`），而且 herdr 內建的 agy 偵測規則是舊版——所以 daemon 自己認：
//! 認到就標 blocked、**不送 prompt、一個鍵都不按**。登入、條款（預設已勾選「允許 Google 收集並使用我的 Interactions 資料」，
//! 一個 Enter 就切換同意與否）、色彩頁都只由人處理；信任框我們預寫 `trustedWorkspaces` 讓它不出現，出現了也不代按。
//!
//! 引文的防誤判：agy 在回覆裡把這些字抄一遍時，畫面上其實沒有框，輸入列（單獨一個 `>`）還在——用這個結構性事實分辨
//! （同 `tui_prompts::composer_is_idle` 的做法），不猜引文排得像不像框。

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
    /// 工具權限確認：`Run this command?`／`Allow access to this URL?`／`Allow calling this tool?`。
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
    let t: String = line.chars().filter(|c| !"│┃╭╮╰╯─━".contains(*c)).collect();
    t.trim().to_lowercase()
}

/// 輸入列：單獨一個 `>`（框線不算）。對話框裡的選項列是 `> 1. …`／`> [x] …`，後面還有字，不會誤中。
fn composer_is_idle(lines: &[&str]) -> bool {
    lines.iter().any(|l| {
        let mut chars = l.chars().filter(|c| !c.is_whitespace() && !"│┃╭╮╰╯─━▔".contains(*c));
        chars.next() == Some('>') && chars.next().is_none()
    })
}

pub fn blocking_dialog(screen: &str) -> Option<AgyDialog> {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(TAIL_LINES)..];
    if composer_is_idle(tail) {
        return None;
    }
    let rows: Vec<String> = tail.iter().map(|l| norm(l)).collect();
    let has = |f: &dyn Fn(&str) -> bool| rows.iter().any(|r| f(r));
    // 整行就是標題（對話框的標題自己一行，前面最多一個圖示）；回覆裡的引文前面是項目符號、引號或別的字，不算。
    let title = |t: &str| {
        has(&|r| {
            r == t || r.strip_suffix(t).is_some_and(|pre| pre.chars().all(|c| !c.is_alphanumeric() && !"●⏺⎿•>*-\"'`".contains(c)))
        })
    };
    if title("select login method:") || has(&|r| r.starts_with("welcome to the antigravity cli") && r.contains("not signed in")) {
        return Some(AgyDialog::Login);
    }
    if title("terms of service & data use") {
        return Some(AgyDialog::Terms);
    }
    if title("choose your color scheme:") {
        return Some(AgyDialog::ColorScheme);
    }
    if title("do you trust the contents of this project?") && has(&|r| r.contains("yes, i trust this folder") || r.contains("no, exit")) {
        return Some(AgyDialog::Trust);
    }
    if ["run this command?", "allow access to this url?", "allow calling this tool?"].iter().any(|t| title(t)) {
        return Some(AgyDialog::Permission);
    }
    None
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
 /home/ubuntu/project

 Do you trust the contents of this project?

 > Yes, I trust this folder
   No, exit
";
    const PERMISSION: &str = "\n ⚡ Run this command?\n\n   git status\n   Reason: check the tree\n\n > Allow once\n   Deny\n";
    const MAIN: &str = "\
 Antigravity CLI 1.2.16
 Gemini 3.1 Pro (Low)
 /home/ubuntu/project
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
        for url in ["Allow access to this URL?", "Allow calling this tool?"] {
            assert_eq!(blocking_dialog(&format!("\n {url}\n > Allow\n   Deny\n")), Some(AgyDialog::Permission), "{url}");
        }
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
}
