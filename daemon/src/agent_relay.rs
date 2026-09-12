//! 誰把這句話打進另一個 agent 的 pane（SPEC §6.5d）。
//!
//! Bot 之間互相派工有兩條路。走 daemon 的那條（`POST /api/bots/{id}/prompt`、總管的 assignment）
//! 會把 `messages.relay_from` 記起來，UI 就畫成「AGM → 這顆 bot」而不是使用者自己打的。
//!
//! 另一條是 agent 直接 `herdr agent prompt <名字> …`：daemon 完全沒有參與，那句話只會以
//! **prompt 回音**的形式從 hook 回來（`source = 'hook'` 的 user 訊息），於是總管的裁示在對話裡
//! 跟使用者自己打的字長得一模一樣（2026-09-12 使用者：「這種 agm 的訊息標示為 agm 訊息」）。
//!
//! 這裡把它補成機制而不是請求：PATH 上的 herdr shim（§6.5b）在轉發 `agent prompt` 之前先向
//! daemon 報一聲「我（`AM_BOT_ID`）要送這段字給 `<agent>`」，daemon 記在這張短命的表上；那句話的
//! 回音從 hook 回來時就認得出來源，補上 `relay_from`。
//!
//! 認不出來就維持原樣——寧可少標一次，也不要把使用者自己打的字說成是別人送的。

/// 「這句話是 daemon 自己發的」的哨符（`messages.relay_from`）。不是任何 bot 的 id，所以 UI 找不到
/// 對應的 bot，就照 `daemon` 畫（SPEC-team §2.1 原本把 `NULL` 同時當成使用者與 daemon，分不出來）。
/// 例行腳本（launchd 的 daemon-update / dev-server / browser-gc）送進總管的話也用它。
pub const DAEMON_SENDER: &str = "daemon";

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 一筆「某個 bot 剛把這段字送進某個 agent 的 pane」。
struct Pending {
    /// 送出去的那顆 bot（`AM_BOT_ID`）。
    from_bot: String,
    /// 收的那個 herdr agent 名字（shim 補完前綴之後的名字）。
    agent: String,
    text: String,
    at: Instant,
}

/// 回音多半在幾秒內回來。留五分鐘是給「送進去的那顆正在忙、排隊等前一回合」的情況；
/// 再久就寧可不認——一則錯的來源標示比沒有標示更糟。
const TTL: Duration = Duration::from_secs(300);

/// 一則回音至少要對上這麼多字才算同一句：太短的相同開頭（「繼續」）會把使用者自己打的字認錯。
const MIN_MATCH: usize = 12;

fn store() -> &'static Mutex<Vec<Pending>> {
    static S: std::sync::OnceLock<Mutex<Vec<Pending>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

/// 比對時把空白**整個拿掉**：TUI 會在任意位置折行、補縮排，所以「收成一個空格」還不夠——
/// 原文沒有空格的地方，回音可能就是換行加兩個空格。
fn norm(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 這則回音是不是那一筆待認領的字？回音常被 TUI 截斷（畫面只有幾行），所以誰是誰的開頭都算。
fn same_prompt(pending: &str, echo: &str) -> bool {
    let (a, b) = (norm(pending), norm(echo));
    let n = a.chars().count().min(b.chars().count());
    if n < MIN_MATCH {
        return a == b && !a.is_empty();
    }
    let head = |s: &str| s.chars().take(n).collect::<String>();
    head(&a) == head(&b)
}

/// shim 報來的一筆。同一個 agent 連續收到兩句時兩筆都留著，[`claim`] 認掉哪一筆就少哪一筆。
pub fn announce(from_bot: &str, agent: &str, text: &str) {
    if from_bot.is_empty() || agent.is_empty() || text.trim().is_empty() {
        return;
    }
    let mut s = store().lock().unwrap();
    s.retain(|p| p.at.elapsed() < TTL);
    s.push(Pending { from_bot: from_bot.to_string(), agent: agent.to_string(), text: text.to_string(), at: Instant::now() });
}

/// 這個 agent 的這句回音是誰送的？認出來就**用掉**那一筆（同一句不會被標兩次）。
pub fn claim(agent: &str, echo: &str) -> Option<String> {
    let mut s = store().lock().unwrap();
    s.retain(|p| p.at.elapsed() < TTL);
    let i = s.iter().position(|p| p.agent == agent && same_prompt(&p.text, echo))?;
    Some(s.remove(i).from_bot)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 這張表是行程全域的，測試又是平行跑的：每個測試用自己的 agent 名字，才不會互相認領。

    #[test]
    fn an_echo_is_attributed_to_the_agent_that_typed_it() {
        announce("bot-agm", "t-echo", "AGM 裁示：重建這個 daemon，條件如下……");
        // TUI 的回音換了行、縮了排。
        assert_eq!(claim("t-echo", "AGM 裁示：重建這個 daemon，\n  條件如下……"), Some("bot-agm".into()));
        // 同一句只認一次。
        assert_eq!(claim("t-echo", "AGM 裁示：重建這個 daemon，條件如下……"), None);
    }

    #[test]
    fn a_truncated_echo_still_matches_but_a_different_prompt_does_not() {
        announce("bot-agm", "t-trunc", "AGM 定期交辦：正式 daemon 落後 origin/main，請重建");
        assert_eq!(claim("t-trunc", "AGM 定期交辦：正式 daemon 落"), Some("bot-agm".into()));
        announce("bot-agm", "t-trunc", "AGM 定期交辦：正式 daemon 落後 origin/main，請重建");
        assert_eq!(claim("t-trunc", "使用者自己打的另一句話，完全不一樣"), None);
    }

    /// 短到看不出是不是同一句時要求完全一樣（比對過的字元數，空白不算）：「繼續」這種字
    /// 使用者自己也會打。
    #[test]
    fn a_short_prompt_must_match_exactly() {
        announce("bot-agm", "t-short", "繼續");
        assert_eq!(claim("t-short", "繼續做別的事"), None);
        assert_eq!(claim("t-short", "繼續"), Some("bot-agm".into()));
    }

    /// 別的 agent 的回音不會認領到這一筆。
    #[test]
    fn another_agents_echo_is_left_alone() {
        announce("bot-agm", "t-agent", "AGM 裁示：這一句是給 abc 的，不是給 xyz 的");
        assert_eq!(claim("t-agent-other", "AGM 裁示：這一句是給 abc 的，不是給 xyz 的"), None);
    }
}
