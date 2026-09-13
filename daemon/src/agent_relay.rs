//! 替 `herdr agent prompt` 直送的字補 `relay_from`（SPEC §6.5d）。那句話只會以 hook 回音回來，
//! 否則跟使用者打的字一樣（2026-09-12 使用者：「這種 agm 的訊息標示為 agm 訊息」）。
//! herdr shim（§6.5b）轉發前先報備，回音回來時認領。認不出來就不標——不能把使用者的字說成別人送的。

/// daemon 自發訊息的 `relay_from` 哨符（`NULL` 只代表使用者）；launchd 例行腳本送進總管的話也用它。
pub const DAEMON_SENDER: &str = "daemon";

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Pending {
    from_bot: String,
    /// shim 補完前綴之後的名字。
    agent: String,
    text: String,
    at: Instant,
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
fn same_prompt(pending: &str, echo: &str) -> bool {
    let (a, b) = (norm(pending), norm(echo));
    let n = a.chars().count().min(b.chars().count());
    if n < MIN_MATCH {
        return a == b && !a.is_empty();
    }
    let head = |s: &str| s.chars().take(n).collect::<String>();
    head(&a) == head(&b)
}

pub fn announce(from_bot: &str, agent: &str, text: &str) {
    if from_bot.is_empty() || agent.is_empty() || text.trim().is_empty() {
        return;
    }
    let mut s = store().lock().unwrap();
    s.retain(|p| p.at.elapsed() < TTL);
    s.push(Pending { from_bot: from_bot.to_string(), agent: agent.to_string(), text: text.to_string(), at: Instant::now() });
}

/// 認出來就**用掉**那一筆，同一句不會被標兩次。
pub fn claim(agent: &str, echo: &str) -> Option<String> {
    let mut s = store().lock().unwrap();
    s.retain(|p| p.at.elapsed() < TTL);
    let i = s.iter().position(|p| p.agent == agent && same_prompt(&p.text, echo))?;
    Some(s.remove(i).from_bot)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 表是行程全域、測試平行跑：每個測試用自己的 agent 名字。

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

    /// 「繼續」這種字使用者自己也會打。
    #[test]
    fn a_short_prompt_must_match_exactly() {
        announce("bot-agm", "t-short", "繼續");
        assert_eq!(claim("t-short", "繼續做別的事"), None);
        assert_eq!(claim("t-short", "繼續"), Some("bot-agm".into()));
    }

    #[test]
    fn another_agents_echo_is_left_alone() {
        announce("bot-agm", "t-agent", "AGM 裁示：這一句是給 abc 的，不是給 xyz 的");
        assert_eq!(claim("t-agent-other", "AGM 裁示：這一句是給 abc 的，不是給 xyz 的"), None);
    }
}
