//! codex 的更新提示（issue #388）。跟 claude 相反：claude 是新版**已經下載好**、重啟就換；codex 是新版**還沒安裝**，
//! 要先跑安裝指令再重啟，所以通知文字要寫明「需安裝後重啟」，而且**不進** claude 的批次 exit＋resume
//! （`bulk_restart::is_candidate` 只收 claude）——重啟一顆沒裝新版的 codex 換不到任何東西。安裝本身照舊要人或 AGM 核准，不自動跑。
//!
//! 畫面上兩種寫法，都是同一句 `✨ Update available! 0.154.0 -> 0.155.1`：
//! 1. 啟動時的**互動選單**：底下接 `1. Update now (…)`／`2. Skip`／`3. Skip until next version`；
//! 2. **非互動方框**：底下接 `Run sh -c '…install.sh…' to update.`。
//!
//! 只認「那一句＋緊接著的選項或安裝指令」：光有那一句（例如對話裡引用）不算。方框印在 session 開頭，之後被對話推出畫面，
//! 所以還要靠版本比對（`codex --version` 的磁碟版本對上跑著的版本）補位；跑著的版本從啟動畫面的 `OpenAI Codex (v…)`
//! 或提示句的 `from` 讀到，記在記憶體（[`remember_running`]）。

use crate::changelog::{cli_version_string, parse_version, version_string};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 存進 `runs.update_notice` 的字都以它開頭，web／測試靠它分辨是 codex 的通知。
pub const NOTICE_PREFIX: &str = "codex 有新版";

/// 畫面上讀到的一則提示。`from` 讀不到（窄 pane 折行）時是 `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub from: Option<String>,
    pub to: String,
}

/// 一行的正文：框線、行首的游標／項目符號／emoji 拿掉，其餘原樣（大小寫保留，版本號與指令要用）。
fn body(line: &str) -> String {
    let spaced: String = line.chars().map(|c| if c.is_whitespace() || "│┃┌┐└┘─├┤┬┴┼╭╮╯╰▎▔".contains(c) { ' ' } else { c }).collect();
    let joined = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.trim_start_matches(|c: char| !c.is_alphanumeric()).to_string()
}

fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// `0.154.0 -> 0.155.1`（`->`、`→`、`=>`，有沒有空白都行）。只有一個版本時 `from` 是 `None`。
fn versions_after(text: &str) -> Option<(Option<String>, String)> {
    let vs: Vec<String> = text
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter(|t| t.contains('.'))
        .filter_map(version_string)
        .collect();
    match vs.as_slice() {
        [a, b, ..] => Some((Some(a.clone()), b.clone())),
        [b] => Some((None, b.clone())),
        [] => None,
    }
}

/// 畫面上有沒有 codex 的更新提示。兩種寫法之一，而且緊接著要有選項或安裝指令。
pub fn parse_prompt(screen: &str) -> Option<Prompt> {
    let lines: Vec<String> = screen.lines().map(body).filter(|l| !l.is_empty()).collect();
    for (i, line) in lines.iter().enumerate() {
        let low = lower(line);
        let Some(at) = low.find("update available") else { continue };
        // 句子本身要像提示：`Update available!` 在行首（框線、emoji 之後）；對話裡「the Update available! banner」不算。
        if at > 2 {
            continue;
        }
        // 窄 pane 會把 `a -> b` 折到下一行。
        let joined = format!("{} {}", &line[at..], lines.get(i + 1).map(String::as_str).unwrap_or(""));
        let Some((from, to)) = versions_after(&joined) else { continue };
        let next: Vec<String> = lines.iter().skip(i + 1).take(8).map(|l| lower(l)).collect();
        let menu = next.iter().any(|l| l.starts_with("1. update now")) && next.iter().any(|l| l.starts_with("2. skip"));
        let boxed = next.iter().take(3).any(|l| l.contains("to update") && (l.contains("install") || l.contains("run ")));
        if menu || boxed {
            return Some(Prompt { from, to });
        }
    }
    None
}

/// 啟動畫面的 `>_ OpenAI Codex (v0.154.0)`：**跑著的**版本。
pub fn parse_running_version(screen: &str) -> Option<String> {
    screen.lines().find_map(|l| {
        let low = l.to_lowercase();
        let at = low.find("openai codex (v")?;
        let rest = &l[at + "openai codex (v".len()..];
        let ver: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        version_string(&ver)
    })
}

/// 還沒安裝的通知（提示句或選單讀到的）。
pub fn pending_text(from: Option<&str>, to: &str) -> String {
    match from {
        Some(f) => format!("{NOTICE_PREFIX} {f} → {to}，需安裝後重啟"),
        None => format!("{NOTICE_PREFIX} {to}，需安裝後重啟"),
    }
}

/// 「需安裝」通知寫的起點（`codex 有新版 <a> → <b>，需安裝後重啟` 的 `a`）：那顆 run 當時跑著的版本。沒寫起點是 `None`。
pub fn pending_from(notice: &str) -> Option<String> {
    if !(notice.starts_with(NOTICE_PREFIX) && notice.contains("需安裝")) {
        return None;
    }
    versions_after(notice).and_then(|(from, _)| from)
}

/// 「需安裝」通知寫的目標版本（`b`）：header 一鍵安裝確認框寫的那一版，`cli_update` 拿它核對使用者核准的版本（#569）。
pub fn pending_to(notice: &str) -> Option<String> {
    if !(notice.starts_with(NOTICE_PREFIX) && notice.contains("需安裝")) {
        return None;
    }
    versions_after(notice).map(|(_, to)| to)
}

/// 磁碟上已經是新版（安裝過了）、這個 run 還跑著舊的：重啟就換。`--version` 的原文（`codex-cli 0.155.1`）也收。
pub fn installed_text(disk: &str, running: &str) -> Option<String> {
    let (d, r) = (parse_version(&cli_version_string(disk).unwrap_or_else(|| disk.to_string()))?, parse_version(running)?);
    (d > r).then(|| format!("{NOTICE_PREFIX} {}（這個 run 跑的是 {running}），已安裝，重啟套用", d.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(".")))
}

/// 這一輪 codex run 的通知該是什麼。優先序：磁碟已是新版（重啟就換，最具體）→ 畫面上的提示（新版還沒裝）→
/// 分診帳本裡的上游最新版比裝著的新（新版還沒裝）→ 既有的「還沒裝」通知（畫面被推掉、選了 Skip 都不代表新版不存在，
/// run 重啟後新 run 本來就乾淨）。畫面上的提示若磁碟已經裝好，就不能再說「需安裝」——那是過期的提示，所以磁碟先。
///
/// `upstream` 是 `release_triage` 帳本裡最新的正式版（issue #561）：畫面上的提示只在啟動那一刻印、
/// 而且版本是 codex 自己上一次檢查（`~/.codex/version.json`）的結果——2026-09-25 實測帳本已有 0.157.0，
/// 6 顆 codex bot 裡 4 顆的通知還停在 0.156.1、2 顆沒被巡到提示就一直沒有通知。目標版本取提示、帳本、
/// 既有通知三者較新的那個（安裝指令裝的就是最新版）。
pub fn decide(
    screen_prompt: Option<&Prompt>,
    running: Option<&str>,
    disk: Option<&str>,
    upstream: Option<&str>,
    existing: Option<&str>,
) -> Option<String> {
    // 跑著的版本沒看過（啟動畫面早被推掉）時，「需安裝」通知寫的起點就是它：不然 header 一鍵裝好之後，
    // 這顆的通知會一直停在「需安裝」、批次永遠不收（`cli_update`，2026-09-25）。
    let from_notice = existing.and_then(pending_from);
    let running = running.or(from_notice.as_deref());
    if let (Some(d), Some(r)) = (disk, running) {
        if let Some(t) = installed_text(d, r) {
            return Some(t);
        }
    }
    let disk_v = disk.and_then(|d| cli_version_string(d).or_else(|| version_string(d)));
    // 裝著的版本：磁碟優先（跑著的不會比磁碟新），讀不到才用跑著的。兩個都不知道就不拿帳本比。
    let have = disk_v.clone().or_else(|| running.and_then(version_string));
    let upstream_target = match (have.as_deref().and_then(parse_version), upstream.and_then(version_string)) {
        (Some(h), Some(u)) if parse_version(&u).is_some_and(|uv| uv > h) => Some(u),
        _ => None,
    };
    if let Some(p) = screen_prompt {
        // 提示說的新版磁碟上已經有了（安裝過、這個 run 還舊）：也是「重啟就換」，不再說需安裝——
        // 除非帳本知道還有更新的一版。
        if let Some(d) = disk_v.as_deref() {
            if let (Some(dv), Some(tv)) = (parse_version(d), parse_version(&p.to)) {
                if dv >= tv {
                    return Some(match upstream_target {
                        Some(u) => pending_text(Some(d), &u),
                        None => format!("{NOTICE_PREFIX} {}，已安裝，重啟套用", p.to),
                    });
                }
            }
        }
        return Some(pending_text(p.from.as_deref(), &newest(&p.to, upstream_target.as_deref())));
    }
    if let Some(u) = upstream_target {
        // 既有的「還沒裝」通知若寫的新版更新（帳本落後 codex 自己的檢查），沿用它的目標。
        let prev = existing.filter(|e| e.starts_with(NOTICE_PREFIX) && e.contains("需安裝")).and_then(versions_after);
        let to = newest(&u, prev.as_ref().map(|(_, t)| t.as_str()));
        let from = running.and_then(version_string).or_else(|| prev.and_then(|(f, _)| f)).or(disk_v);
        return Some(pending_text(from.as_deref(), &to));
    }
    existing.filter(|e| e.starts_with(NOTICE_PREFIX)).map(str::to_string)
}

/// 兩個版本取新的；`b` 看不懂就是 `a`。
fn newest(a: &str, b: Option<&str>) -> String {
    match b {
        Some(b) if parse_version(b) > parse_version(a) => b.to_string(),
        _ => a.to_string(),
    }
}

fn running_versions() -> &'static Mutex<HashMap<String, String>> {
    static M: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 看到過的跑著的版本，掛 run（重啟後是新 run，自然是新的）。畫面上的版本優先，其次是提示的 `from`。
pub fn remember_running(run_id: &str, screen: &str, prompt: Option<&Prompt>) -> Option<String> {
    let seen = parse_running_version(screen).or_else(|| prompt.and_then(|p| p.from.clone()));
    let mut m = running_versions().lock().unwrap();
    match seen {
        Some(v) => {
            m.insert(run_id.to_string(), v.clone());
            Some(v)
        }
        None => m.get(run_id).cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 啟動時的互動選單（codex 0.15x）。
    const MENU: &str = "\
╭─────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.154.0)                  │
│ model: gpt-5.6-luna   /model to change      │
╰─────────────────────────────────────────────╯

  ✨ Update available! 0.154.0 -> 0.155.1

  Release notes: https://github.com/openai/codex/releases/latest

› 1. Update now (runs `npm install -g @openai/codex`)
  2. Skip
  3. Skip until next version

  Press enter to continue
";

    /// 非互動方框（2026-09-21 使用者截圖）。
    const BOXED: &str = "\
╭─────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.154.0)                  │
╰─────────────────────────────────────────────╯

  ✨ Update available! 0.154.0 -> 0.155.1
  Run sh -c 'curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh' to update.

› Explain this codebase
";

    fn p(from: &str, to: &str) -> Prompt {
        Prompt { from: Some(from.into()), to: to.into() }
    }

    #[test]
    fn the_interactive_menu_is_read_with_both_versions() {
        assert_eq!(parse_prompt(MENU), Some(p("0.154.0", "0.155.1")));
    }

    #[test]
    fn the_non_interactive_box_is_read_with_both_versions() {
        assert_eq!(parse_prompt(BOXED), Some(p("0.154.0", "0.155.1")));
        // 方框畫在框線裡也一樣。
        let framed = "│ ✨ Update available! 0.154.0 -> 0.155.1 │\n│ Run sh -c 'curl x/install.sh | sh' to update. │\n";
        assert_eq!(parse_prompt(framed), Some(p("0.154.0", "0.155.1")));
    }

    #[test]
    fn a_wrapped_arrow_and_a_unicode_arrow_still_read() {
        let wrapped = "✨ Update available! 0.154.0\n-> 0.155.1\nRun sh -c 'curl x/install.sh | sh' to update.\n";
        assert_eq!(parse_prompt(wrapped), Some(p("0.154.0", "0.155.1")));
        let uni = "✨ Update available! 0.154.0 → 0.155.1\nRun sh -c 'curl x/install.sh | sh' to update.\n";
        assert_eq!(parse_prompt(uni), Some(p("0.154.0", "0.155.1")));
    }

    #[test]
    fn the_sentence_alone_or_quoted_in_conversation_is_not_a_prompt() {
        assert_eq!(parse_prompt("✨ Update available! 0.154.0 -> 0.155.1\n› hello\n"), None, "後面沒有選項或安裝指令");
        assert_eq!(parse_prompt("the banner says Update available! 0.154.0 -> 0.155.1 and then Run x to update.\n"), None, "不在行首");
        assert_eq!(parse_prompt("Update available soon\nRun the installer to update.\n"), None, "沒有版本號");
        assert_eq!(parse_prompt(""), None);
        assert_eq!(parse_prompt("Update installed · Restart to update\n"), None, "那是 claude 的寫法");
    }

    #[test]
    fn the_running_version_comes_off_the_startup_banner() {
        assert_eq!(parse_running_version(MENU).as_deref(), Some("0.154.0"));
        assert_eq!(parse_running_version("just chatting\n"), None);
    }

    #[test]
    fn the_notice_says_it_must_be_installed_first() {
        let t = pending_text(Some("0.154.0"), "0.155.1");
        assert!(t.starts_with(NOTICE_PREFIX) && t.contains("0.154.0 → 0.155.1") && t.contains("需安裝後重啟"), "{t}");
        assert!(pending_text(None, "0.155.1").contains("0.155.1"));
    }

    #[test]
    fn the_prompt_on_screen_alone_makes_a_pending_notice() {
        let n = decide(Some(&p("0.154.0", "0.155.1")), Some("0.154.0"), Some("codex-cli 0.154.0"), None, None).unwrap();
        assert!(n.contains("需安裝後重啟"), "{n}");
    }

    /// 畫面被推掉、只剩版本比對：磁碟已經是新版（有人裝過了），這個 run 還跑著舊的。
    #[test]
    fn once_the_prompt_is_pushed_off_screen_the_disk_version_is_what_remains() {
        let n = decide(None, Some("0.154.0"), Some("codex-cli 0.155.1"), None, None).unwrap();
        assert!(n.contains("0.155.1") && n.contains("0.154.0") && n.contains("已安裝") && n.contains("重啟套用"), "{n}");
        assert!(!n.contains("需安裝"), "已經裝好了，不能再叫人去裝：{n}");
        // 磁碟相同或更舊、也沒有提示：沒有通知。
        assert_eq!(decide(None, Some("0.155.1"), Some("codex-cli 0.155.1"), None, None), None);
        assert_eq!(decide(None, Some("0.155.1"), Some("codex-cli 0.154.0"), None, None), None);
        assert_eq!(decide(None, None, Some("codex-cli 0.155.1"), None, None), None, "不知道跑著的版本就不比");
    }

    #[test]
    fn a_stale_prompt_does_not_say_install_when_it_is_already_installed() {
        let n = decide(Some(&p("0.154.0", "0.155.1")), None, Some("codex-cli 0.155.1"), None, None).unwrap();
        assert!(n.contains("已安裝") && !n.contains("需安裝"), "{n}");
    }

    /// 選了 Skip、或提示被推掉、磁碟也還沒裝：新版還在那裡，通知不消失（新 run 才乾淨）。
    #[test]
    fn a_pending_notice_survives_the_prompt_leaving_the_screen() {
        let pending = pending_text(Some("0.154.0"), "0.155.1");
        assert_eq!(decide(None, Some("0.154.0"), Some("codex-cli 0.154.0"), None, Some(&pending)), Some(pending.clone()));
        // 不是 codex 的通知（例如舊資料）不沿用。
        assert_eq!(decide(None, None, None, None, Some("Update installed · Restart to update")), None);
    }

    /// issue #561（2026-09-25 真 daemon）：本機 0.155.1、帳本已有 0.157.0，提示早被推出畫面（或當下沒巡到）、
    /// 跑著的版本也不知道——以前這顆 bot 一直沒有通知。帳本比磁碟新就是「新版還沒裝」。
    #[test]
    fn the_triage_ledger_alone_is_enough_to_know_there_is_a_newer_version() {
        let n = decide(None, None, Some("codex-cli 0.155.1"), Some("0.157.0"), None).expect("帳本比磁碟新＝有新版");
        assert_eq!(n, pending_text(Some("0.155.1"), "0.157.0"), "{n}");
        // 帳本沒有比裝著的新：不造通知。
        assert_eq!(decide(None, None, Some("codex-cli 0.157.0"), Some("0.157.0"), None), None);
        assert_eq!(decide(None, None, Some("codex-cli 0.157.0"), Some("0.156.1"), None), None);
        // 裝著的版本完全不知道：不拿帳本比（不能憑空說有新版）。
        assert_eq!(decide(None, None, None, Some("0.157.0"), None), None);
        // 只知道跑著的版本也行。
        assert_eq!(decide(None, Some("0.155.1"), None, Some("0.157.0"), None), Some(pending_text(Some("0.155.1"), "0.157.0")));
    }

    /// 畫面上的提示是 codex 自己上一次檢查的結果（會落後）：帳本有更新的一版就寫帳本那一版。
    #[test]
    fn the_newer_of_the_prompt_and_the_ledger_is_the_target() {
        let n = decide(Some(&p("0.155.1", "0.156.1")), Some("0.155.1"), Some("codex-cli 0.155.1"), Some("0.157.0"), None).unwrap();
        assert_eq!(n, pending_text(Some("0.155.1"), "0.157.0"));
        // 帳本反而落後（kick 停了）：提示照舊。
        let n = decide(Some(&p("0.155.1", "0.156.1")), Some("0.155.1"), Some("codex-cli 0.155.1"), Some("0.155.1"), None).unwrap();
        assert_eq!(n, pending_text(Some("0.155.1"), "0.156.1"));
        // 過期的提示（磁碟已經裝了它說的那版）但帳本還有更新的：仍是「需安裝」，不是「重啟套用」。
        let n = decide(Some(&p("0.155.1", "0.156.1")), None, Some("codex-cli 0.156.1"), Some("0.157.0"), None).unwrap();
        assert_eq!(n, pending_text(Some("0.156.1"), "0.157.0"));
    }

    /// 既有的「還沒裝」通知（真 daemon 上那 4 顆寫的 0.155.1 → 0.156.1）遇到帳本 0.157.0：換成新的目標，
    /// 起點沿用；既有的反而比帳本新（帳本落後）就留著它的目標。
    #[test]
    fn an_existing_pending_notice_moves_up_to_the_ledger_version_but_never_down() {
        let old = pending_text(Some("0.155.1"), "0.156.1");
        assert_eq!(decide(None, None, Some("codex-cli 0.155.1"), Some("0.157.0"), Some(&old)), Some(pending_text(Some("0.155.1"), "0.157.0")));
        let ahead = pending_text(Some("0.155.1"), "0.158.0");
        assert_eq!(decide(None, None, Some("codex-cli 0.155.1"), Some("0.157.0"), Some(&ahead)), Some(ahead.clone()));
    }

    /// header 一鍵裝好（`cli_update`）之後，跑著的版本沒看過的那顆：通知寫的起點就是它跑著的版本，
    /// 磁碟已經是新版就要變成「已安裝，重啟套用」，不能卡在「需安裝」讓批次永遠不收。
    #[test]
    fn a_pending_notice_turns_into_installed_once_the_disk_catches_up() {
        let pending = pending_text(Some("0.155.1"), "0.157.0");
        assert_eq!(pending_from(&pending).as_deref(), Some("0.155.1"));
        assert_eq!(pending_from(&pending_text(None, "0.157.0")), None);
        assert_eq!(pending_from("Update installed · Restart to update"), None);
        let n = decide(None, None, Some("codex-cli 0.157.0"), Some("0.157.0"), Some(&pending)).unwrap();
        assert!(n.contains("已安裝") && !n.contains("需安裝") && n.contains("0.155.1"), "{n}");
        // 磁碟還沒到：照舊是「需安裝」。
        assert_eq!(decide(None, None, Some("codex-cli 0.155.1"), Some("0.157.0"), Some(&pending)), Some(pending));
    }

    #[test]
    fn the_running_version_is_remembered_after_the_banner_scrolls_away() {
        let id = "run-remember-test";
        assert_eq!(remember_running(id, MENU, None).as_deref(), Some("0.154.0"));
        assert_eq!(remember_running(id, "long conversation\n", None).as_deref(), Some("0.154.0"));
        assert_eq!(remember_running("run-other", "nothing\n", None), None);
        // 提示句的 from 也算。
        assert_eq!(remember_running("run-from", "x\n", Some(&p("0.150.0", "0.151.0"))).as_deref(), Some("0.150.0"));
    }
}
