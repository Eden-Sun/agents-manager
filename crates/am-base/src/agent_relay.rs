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
    /// 寄件者所在的主機；只能由已驗證的收件 run 認領。
    host: String,
    from_bot: String,
    /// 穩定的收件 run id，從不以 agent 名稱認領。
    run_id: String,
    delivery_id: String,
    claimed: bool,
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

/// 存下來比對用的字數上限（#1352）：回音只會比兩邊較短的那一段開頭（`same_prompt`），留開頭就夠認。
const MAX_TEXT_CHARS: usize = 4_000;
/// 同一顆寄件 bot 最多留幾筆報備（含已認領的 delivery）；超過先丟它自己最舊的（#1352）。
const MAX_PER_SENDER: usize = 64;
/// 整張表的上限（防很多顆同時失控）；超過丟最舊的（#1352）。
const MAX_PENDING: usize = 1_024;

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

/// 呼叫端必須先驗證 run id 與 bot id／host 的關係；名字只作傳輸及顯示。
pub fn announce(host: &str, from_bot: &str, run_id: &str, text: &str, from_turn: Option<&str>) {
    announce_delivery(host, from_bot, run_id, &crate::db::ulid(), text, from_turn);
}

/// 同一 delivery 的 spool 重播不能再次標記另一句回音；已認領的記錄保留到 TTL／容量界限。
pub fn announce_delivery(host: &str, from_bot: &str, run_id: &str, delivery_id: &str, text: &str, from_turn: Option<&str>) -> bool {
    if from_bot.is_empty() || run_id.is_empty() || delivery_id.is_empty() || text.trim().is_empty() {
        return false;
    }
    let mut s = store().lock().unwrap();
    s.retain(|p| p.at.elapsed() < TTL);
    if s.iter().any(|p| p.host == host && p.from_bot == from_bot && p.delivery_id == delivery_id) {
        return false;
    }
    // 上限（#1352）：這顆寄件 bot 滿了就先丟它自己最舊的（`Vec` 是插入順序，第一筆就是最舊的）；整張表滿了丟最舊的。
    if s.iter().filter(|p| p.from_bot == from_bot).count() >= MAX_PER_SENDER {
        if let Some(i) = s.iter().position(|p| p.from_bot == from_bot) {
            s.remove(i);
        }
    }
    if s.len() >= MAX_PENDING {
        s.remove(0);
    }
    s.push(Pending {
        host: host.to_string(),
        from_bot: from_bot.to_string(),
        run_id: run_id.to_string(),
        delivery_id: delivery_id.to_string(),
        claimed: false,
        text: text.chars().take(MAX_TEXT_CHARS).collect(),
        from_turn: from_turn.map(str::to_string),
        at: Instant::now(),
    });
    true
}

/// 認出來就**用掉**那一筆，同一句不會被標兩次。
pub fn claim_relayed(host: &str, run_id: &str, echo: &str) -> Option<Relayed> {
    let mut s = store().lock().unwrap();
    s.retain(|p| p.at.elapsed() < TTL);
    let i = s.iter().position(|p| !p.claimed && p.host == host && p.run_id == run_id && same_prompt(&p.text, echo))?;
    let p = &mut s[i];
    p.claimed = true;
    Some(Relayed { from_bot: p.from_bot.clone(), from_turn: p.from_turn.clone() })
}

/// 同 [`claim_relayed`]，只要寄件 bot。
pub fn claim(host: &str, run_id: &str, echo: &str) -> Option<String> {
    claim_relayed(host, run_id, echo).map(|r| r.from_bot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nr_relay_a_consumed_delivery_cannot_be_replayed_to_another_echo() {
        let text = "同一 delivery 的延遲報備不能標到第二句回音";
        assert!(announce_delivery("nr-host", "nr-from", "nr-run", "nr-delivery", text, None));
        assert_eq!(claim("nr-host", "nr-other-run", text), None);
        assert_eq!(claim("nr-host", "nr-run", text).as_deref(), Some("nr-from"));
        assert!(!announce_delivery("nr-host", "nr-from", "nr-run", "nr-delivery", text, None));
        assert_eq!(claim("nr-host", "nr-run", text), None);
    }

    const H: &str = "local";

    // 表是行程全域、測試平行跑：每個測試用自己的 run id。

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

    // 下面兩條用的上限與 `MAX_PER_SENDER`（64）、`MAX_TEXT_CHARS`（4000）同值；寫成字面數字，修正前才會是行為紅，而不是編不過。

    /// #1352：同一顆寄件 bot 最多留 64 筆未認領的；超過就丟它自己最舊的，別的寄件者不受影響。
    #[test]
    fn a_sender_keeps_only_its_newest_pending_announces() {
        announce(H, "bot-cap-other", "t-cap-other", "別的寄件者的報備，不該被擠掉", None);
        for n in 0..=64 {
            announce(H, "bot-cap-sender", &format!("t-cap-{n}"), &format!("這是第 {n} 則而且夠長的報備內容"), None);
        }
        assert_eq!(claim(H, "t-cap-0", "這是第 0 則而且夠長的報備內容"), None, "最舊的那筆被擠掉");
        assert_eq!(claim(H, "t-cap-64", "這是第 64 則而且夠長的報備內容"), Some("bot-cap-sender".into()), "最新的留著");
        assert_eq!(claim(H, "t-cap-other", "別的寄件者的報備，不該被擠掉"), Some("bot-cap-other".into()), "別的寄件者不受影響");
    }

    /// #1352：報備只存前 4000 個字（照字元切）。完整的回音照樣認得，截斷的回音也認得。
    #[test]
    fn a_very_long_announce_is_stored_truncated_but_still_claimed() {
        let long: String = "長".repeat(4000 + 500);
        announce(H, "bot-long", "t-long", &long, None);
        assert_eq!(claim(H, "t-long", &long), Some("bot-long".into()), "完整的原文要認得");

        announce(H, "bot-long", "t-long-2", &long, None);
        let stored = store().lock().unwrap().iter().filter(|p| p.run_id == "t-long-2").map(|p| p.text.chars().count()).collect::<Vec<_>>();
        assert_eq!(stored, vec![4000], "存進去的只有前 4000 字");
        let head: String = long.chars().take(100).collect();
        assert_eq!(claim(H, "t-long-2", &head), Some("bot-long".into()), "截斷的回音照舊認得");
    }
}
