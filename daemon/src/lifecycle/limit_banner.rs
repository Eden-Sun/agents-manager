//! 畫面上那張「You've hit your usage limit」是**現在**撞的，還是重播的舊字？
//!
//! 2026-09-14 AGM 交辦（實況 15:50，bot agm-pxf2pv-solrev pane w168:p49）：`codex fork` 起的新 bot 會把
//! 上一段 session 尾端的畫面重播出來，裡面有**舊的**「… try again at Sep 19th, 2026 6:43 PM」橫幅。
//! capture 掃到它就把裸 `codex` 標成 5h 100%，派過去的交辦因此 `quota_blocked`——而同一個畫面最底下，
//! codex 自己的狀態列寫的是 `5h 82% left · weekly 97% left`。
//!
//! 兩條判定，都只看**同一次**讀到的畫面，不靠後到的輪詢去蓋：
//!
//! 1. [`sighting`]／[`is_history`]：一個 run 第一次讀畫面時看到的橫幅一律算歷史（fork／resume 都會重播，而這個
//!    process 啟動前就存在的字，第一次讀到時已經在畫面上）。之後的讀取，某張橫幅的出現次數**比上一次
//!    多**才算新撞到——同樣的字又印一次，次數會加一；舊字捲出畫面次數變少，不會被誤當成新的。
//! 2. [`status_line_says_headroom`]：同一畫面裡，codex 的結構化狀態列（`5h N% left`）排在最後一張橫幅
//!    **之後**、而且 5h 與 weekly 都還有餘裕 → 橫幅是舊的，不寫 limit_hit。這是 9a12d60 拿掉那條規則的
//!    窄版：只在「同一畫面同時出現、狀態列比較新」時適用。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 一張橫幅的身分：空白收斂後的 `try again at …` 那段（沒有就用整句）。畫面折行會把時間拆到下一行，
/// 所以比對一律在收斂過空白的文字上做。
fn banner_key(notice: &str) -> String {
    let squashed = squash(notice);
    match squashed.find("try again at") {
        Some(i) => squashed[i..].trim_end_matches('.').to_string(),
        None => squashed,
    }
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 這張橫幅在畫面上出現幾次（折行也算同一次）。
fn banner_count(screen: &str, key: &str) -> usize {
    if key.is_empty() {
        return 0;
    }
    squash(screen).matches(key).count()
}

/// run id → 上一次讀畫面時每張橫幅的出現次數。只放記憶體：daemon 重啟後第一次讀到的一律當歷史，
/// 寧可少標一次撞限（回合答不出來時 in-flight 那條路照樣會處理），也不要被重播的字鎖住交辦。
fn last_counts() -> &'static Mutex<HashMap<String, HashMap<String, usize>>> {
    static M: OnceLock<Mutex<HashMap<String, HashMap<String, usize>>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 這張橫幅在這個 run 裡是第幾種情況。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sighting {
    /// 這個 run 第一次讀畫面：畫面上的字是 process 起來時就在的（fork／resume 重播、啟動前的舊字）。
    FirstRead,
    /// 讀過了，這張的出現次數沒有增加。
    Old,
    /// 出現次數比上一次多：真的又印了一次。
    New,
}

/// 這張橫幅在這個 run 裡是哪一種（見 [`Sighting`]）。會順便更新記錄——每張橫幅每次讀畫面只該問一次。
///
/// `all_banners` 是這次畫面上所有撞限橫幅（第一次讀時要一次把整張表建好，不然同一次讀取裡第二張
/// 橫幅會被當成「比上次多」）。
pub(crate) fn sighting(run_id: &str, screen: &str, notice: &str, all_banners: &[String]) -> Sighting {
    let key = banner_key(notice);
    let now = banner_count(screen, &key);
    let mut m = last_counts().lock().unwrap();
    if !m.contains_key(run_id) {
        if m.len() > 512 {
            m.clear();
        }
        let first: HashMap<String, usize> =
            all_banners.iter().map(|b| (banner_key(b), banner_count(screen, &banner_key(b)))).collect();
        m.insert(run_id.to_string(), first);
        return Sighting::FirstRead;
    }
    let seen = m.get_mut(run_id).expect("inserted above");
    let before = seen.get(&key).copied().unwrap_or(0);
    seen.insert(key, now);
    if now > before { Sighting::New } else { Sighting::Old }
}

/// 要不要把這張橫幅當歷史。第一次讀畫面時，**有回合在飛**就不當歷史：那表示這次讀取不是 run 剛起來那一下
/// （啟動那一下沒有回合），而是 daemon 中途重啟後、剛好讀在一個真的撞限的回合上——那張字就是答案。
pub(crate) fn is_history(s: Sighting, turn_in_flight: bool) -> bool {
    match s {
        Sighting::Old => true,
        Sighting::FirstRead => !turn_in_flight,
        Sighting::New => false,
    }
}

/// 同一畫面裡，最後一張撞限橫幅之後是否有 codex 的狀態列、而且 5h／weekly 都還有剩？
pub(crate) fn status_line_says_headroom(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    let Some(banner_at) = lines.iter().rposition(|l| l.to_ascii_lowercase().contains("hit your usage limit")) else {
        return false;
    };
    let Some(status_at) = lines.iter().rposition(|l| is_status_line(l)) else { return false };
    if status_at <= banner_at {
        return false;
    }
    let Some(q) = crate::codex_live::parse_status_quota(lines[status_at]) else { return false };
    let five_ok = q.five_hour_left.is_some_and(|v| v > 0.0);
    let weekly_ok = q.weekly_left.map_or(true, |v| v > 0.0);
    five_ok && weekly_ok
}

/// codex 底部那一行：`<model> · <dir> · [Context N% used ·] 5h N% left · weekly N% left`。
fn is_status_line(line: &str) -> bool {
    let l = line.trim();
    l.contains('·') && l.contains("% left")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORK_REPLAY: &str = include_str!("fixtures/codex_fork_replay.txt");

    /// 2026-09-14 實況：fork 重播出舊橫幅，但同一畫面底下的狀態列說 5h 82%、weekly 97%。
    #[test]
    fn a_replayed_banner_under_a_newer_status_line_with_headroom_is_not_a_limit_hit() {
        assert!(status_line_says_headroom(FORK_REPLAY));
    }

    /// 狀態列見底、或排在橫幅之前（比較舊）時，橫幅照舊算數。
    #[test]
    fn the_status_line_only_wins_when_it_is_newer_and_has_headroom() {
        let exhausted = FORK_REPLAY.replace("5h 82% left", "5h 0% left");
        assert!(!status_line_says_headroom(&exhausted), "5h 見底");
        let weekly_out = FORK_REPLAY.replace("weekly 97% left", "weekly 0% left");
        assert!(!status_line_says_headroom(&weekly_out), "weekly 見底");
        let status_first = "  gpt-5.6-sol medium · ~/p · 5h 82% left · weekly 97% left\n\n■ You've hit your usage limit. … try again at 6:43 PM.\n";
        assert!(!status_line_says_headroom(status_first), "狀態列在橫幅之前＝比較舊");
        assert!(!status_line_says_headroom("■ You've hit your usage limit. … try again at 6:43 PM.\n"), "沒有狀態列");
    }

    /// 第一次讀畫面時已經在的橫幅算歷史；之後同一張字又印一次（次數加一）才算新撞到；捲出畫面不算。
    #[test]
    fn banners_on_the_first_read_are_history_and_only_a_new_occurrence_counts() {
        let run = "run-fork-replay-test";
        let banner = "You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2026 6:43 PM.";
        let banners = vec![banner.to_string()];
        let first = format!("■ {banner}\n\n› Ask Codex to do anything\n");
        assert_eq!(sighting(run, &first, banner, &banners), Sighting::FirstRead, "第一次讀到＝重播／啟動前就在");
        assert_eq!(sighting(run, &first, banner, &banners), Sighting::Old, "畫面沒變，還是舊的");
        let again = format!("{first}\n› 繼續\n\n■ {banner}\n");
        assert_eq!(sighting(run, &again, banner, &banners), Sighting::New, "同一句又印一次＝真的又撞到");
        let scrolled = "› 繼續\n".to_string();
        assert_eq!(sighting(run, &scrolled, banner, &banners), Sighting::Old, "捲出畫面次數變少，不是新撞到");
        // 折行把時間拆到下一行也認得是同一張。
        let wrapped = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2026\n6:43 PM.\n";
        assert_eq!(banner_count(wrapped, &banner_key(banner)), 1);
    }

    /// 第一次讀畫面就碰上在飛的回合（daemon 中途重啟）：那張字是這個回合的答案，不能當歷史。
    #[test]
    fn a_first_read_during_an_in_flight_turn_is_not_history() {
        assert!(is_history(Sighting::FirstRead, false), "run 剛起來那一下沒有回合＝重播");
        assert!(!is_history(Sighting::FirstRead, true), "有回合在飛＝真的撞到");
        assert!(is_history(Sighting::Old, true));
        assert!(!is_history(Sighting::New, false));
    }
}
