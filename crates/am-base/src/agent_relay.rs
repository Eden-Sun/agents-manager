//! 替 `herdr agent prompt` 直送的字補 `relay_from`（SPEC §6.5d）。那句話只會以 hook 回音回來，
//! 否則跟使用者打的字一樣（2026-09-12 使用者：「這種 agm 的訊息標示為 agm 訊息」）。
//! herdr shim（§6.5b）轉發前先報備，回音回來時認領。認不出來就不標——不能把使用者的字說成別人送的。

/// daemon 自發訊息的 `relay_from` 哨符（`NULL` 只代表使用者）；launchd 例行腳本送進總管的話也用它。
pub const DAEMON_SENDER: &str = "daemon";

/// 分享頁面來源的 `relay_from` 哨符。
pub const SHARE_SENDER: &str = "share";

/// 常數時間字串比較。
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Pending {
    /// 寄件者所在的主機：agent 名字（`<專案>-<bot>`）只在一台主機內唯一，兩台各有同名 agent 並不稀奇，
    /// 報備只能由同一台主機上的收件方認領。
    host: String,
    from_bot: String,
    /// shim 補完前綴之後的名字。
    agent: String,
    text: String,
    /// 寄件 bot 送這句話當下正在跑的回合（#927）。送的時候找不到就是 `None`。
    from_turn: Option<String>,
    at: Instant,
}

/// 認出來的報備：寄件 bot，以及它送這句話時正在跑的回合（#927：child_done 用它判斷「這一回合已經自己回報過」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relayed {
    pub from_bot: String,
    pub from_turn: Option<String>,
}

/// 五分鐘涵蓋收件方排隊等前一回合；再久寧可不認，錯標比不標更糟。
const TTL: Duration = Duration::from_secs(300);

/// 太短的相同開頭（「繼續」）會把使用者自己打的字認錯。
const MIN_MATCH: usize = 12;

fn store() -> &'static Mutex<Vec<Pending>> {
    static S: std::sync::OnceLock<Mutex<Vec<Pending>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

/// 空白**整個拿掉**：TUI 會在任意位置折行補縮排，收成一個空格不夠。
fn norm(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 回音常被 TUI 截斷，所以誰是誰的開頭都算。
pub fn same_prompt(pending: &str, echo: &str) -> bool {
    let (a, b) = (norm(pending), norm(echo));
    let n = a.chars().count().min(b.chars().count());
    if n < MIN_MATCH {
        return a == b && !a.is_empty();
    }
    let head = |s: &str| s.chars().take(n).collect::<String>();
    head(&a) == head(&b)
}

pub fn announce(host: &str, from_bot: &str, agent: &str, text: &str, from_turn: Option<&str>) {
    if from_bot.is_empty() || agent.is_empty() || text.trim().is_empty() {
        return;
    }
    let mut s = store().lock().unwrap();
    s.retain(|p| p.at.elapsed() < TTL);
    s.push(Pending {
        host: host.to_string(),
        from_bot: from_bot.to_string(),
        agent: agent.to_string(),
        text: text.to_string(),
        from_turn: from_turn.map(str::to_string),
        at: Instant::now(),
    });
}

/// 認出來就**用掉**那一筆，同一句不會被標兩次。
pub fn claim_relayed(host: &str, agent: &str, echo: &str) -> Option<Relayed> {
    let mut s = store().lock().unwrap();
    s.retain(|p| p.at.elapsed() < TTL);
    let i = s.iter().position(|p| p.host == host && p.agent == agent && same_prompt(&p.text, echo))?;
    let p = s.remove(i);
    Some(Relayed { from_bot: p.from_bot, from_turn: p.from_turn })
}

/// 同 [`claim_relayed`]，只要寄件 bot。
pub fn claim(host: &str, agent: &str, echo: &str) -> Option<String> {
    claim_relayed(host, agent, echo).map(|r| r.from_bot)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "local";

    // 表是行程全域、測試平行跑：每個測試用自己的 agent 名字。

    #[test]
    fn an_echo_is_attributed_to_the_agent_that_typed_it() {
        announce(H, "bot-agm", "t-echo", "AGM 裁示：重建這個 daemon，條件如下……", None);
        // TUI 的回音換了行、縮了排。
        assert_eq!(claim(H, "t-echo", "AGM 裁示：重建這個 daemon，\n  條件如下……"), Some("bot-agm".into()));
        // 同一句只認一次。
        assert_eq!(claim(H, "t-echo", "AGM 裁示：重建這個 daemon，條件如下……"), None);
    }

    #[test]
    fn a_truncated_echo_still_matches_but_a_different_prompt_does_not() {
        announce(H, "bot-agm", "t-trunc", "AGM 定期交辦：正式 daemon 落後 origin/main，請重建", None);
        assert_eq!(claim(H, "t-trunc", "AGM 定期交辦：正式 daemon 落"), Some("bot-agm".into()));
        announce(H, "bot-agm", "t-trunc", "AGM 定期交辦：正式 daemon 落後 origin/main，請重建", None);
        assert_eq!(claim(H, "t-trunc", "使用者自己打的另一句話，完全不一樣"), None);
    }

    /// 「繼續」這種字使用者自己也會打。
    #[test]
    fn a_short_prompt_must_match_exactly() {
        announce(H, "bot-agm", "t-short", "繼續", None);
        assert_eq!(claim(H, "t-short", "繼續做別的事"), None);
        assert_eq!(claim(H, "t-short", "繼續"), Some("bot-agm".into()));
    }

    /// #927：認出來的報備帶著寄件者送它的那一回合；沒有回合的報備照舊只給寄件者。
    #[test]
    fn a_claim_carries_the_sender_turn_it_was_sent_in() {
        announce(H, "bot-child", "t-turn", "請核准這一則", Some("turn-42"));
        assert_eq!(
            claim_relayed(H, "t-turn", "請核准這一則"),
            Some(Relayed { from_bot: "bot-child".into(), from_turn: Some("turn-42".into()) })
        );
        announce(H, "bot-child", "t-turn", "請核准另一則", None);
        assert_eq!(claim(H, "t-turn", "請核准另一則"), Some("bot-child".into()));
    }

    #[test]
    fn another_agents_echo_is_left_alone() {
        announce(H, "bot-agm", "t-agent", "AGM 裁示：這一句是給 abc 的，不是給 xyz 的", None);
        assert_eq!(claim(H, "t-agent-other", "AGM 裁示：這一句是給 abc 的，不是給 xyz 的"), None);
    }
}
