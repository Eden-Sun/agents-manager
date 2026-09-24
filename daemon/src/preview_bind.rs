//! 預覽起的 dev server 綁在哪個介面（issue #434）。
//!
//! `allow_lan` 是 daemon 的對外開關：同一個判斷決定 daemon 自己 bind `0.0.0.0` 還是 `127.0.0.1`，
//! 也決定要不要放行同網段的 peer 與 `Origin`（SPEC §7.1）。預覽有兩條起法，以前只有一條受它管：
//!
//! * 沒有 dev script → daemon 自己組 `bunx vite --host <bind>`，bind 跟著 `allow_lan`；
//! * 有 dev script → `bun run dev`，綁哪裡**完全由專案決定**。這個 repo 自己的 `web/` 就是
//!   `dev: "vite"` ＋ `vite.config.ts` 的 `server.host: true`（＝`0.0.0.0`），`allow_lan` 關著也照樣對外。
//!
//! 所以是兩道，缺一不可：
//!
//! 1. **能指定就指定**（[`pin_dev_command`]）：認得出框架時把 loopback 旗標接在 `bun run dev --` 後面。
//!    旗標一個框架一個樣（vite 是 `--host`，next 是 `-H`），而且**猜錯會讓 dev server 直接以未知參數退出**，
//!    所以只列有把握的那兩個——其餘交給第 2 道，寧可失敗也不要亂送旗標。
//! 2. **一律驗**（[`exposed_addr`]）：起來之後看行程樹**實際** listen 的位址（`lsof`），`allow_lan` 關著卻
//!    綁到 loopback 以外就記 `failed`，不記 `running`。第 1 道管不到的框架、以及在設定檔裡又把位址
//!    蓋回去的專案，都由這一道接住。**而且要一直驗**（issue #452）：原本只在 `starting → running`
//!    那一拍量一次，起來時乖、之後才改綁對外的 server 就永遠抓不到。已經 `running` 的每
//!    [`RECHECK_EVERY_MS`] 毫秒重驗一次（[`take_recheck_slot`]）——不是每一拍，因為量位址要多跑一趟
//!    `lsof`，而 `running` 的輪詢本來只是一次 TCP connect。
//!
//! 驗不到（`lsof` 讀不到、行程樹問不到）**不算違規**：跟整個預覽模組同一條原則——讀不到是「不知道」，
//! 不是「沒有」，不下結論。

/// 強制綁這個位址。
pub const LOOPBACK: &str = "127.0.0.1";

/// 已經 `running` 的預覽多久重驗一次綁的位址（issue #452）。
///
/// 測試裡是 0＝每一拍都驗：測的是「running 期間會不會再驗」這條線，不是計時器本身；
/// 間隔的算法由 [`recheck_due`] 自己的測試釘住。
#[cfg(not(test))]
pub const RECHECK_EVERY_MS: i64 = 60_000;
#[cfg(test)]
pub const RECHECK_EVERY_MS: i64 = 0;

/// 距離上一次驗夠久了沒（間隔是 [`RECHECK_EVERY_MS`]）。
pub fn recheck_due(last: Option<&str>, now: &str) -> bool {
    due_after(last, now, RECHECK_EVERY_MS)
}

/// [`recheck_due`] 的本體，間隔可指定——正式的間隔在測試裡是 0（每一拍都驗），算法本身要另外釘。
/// `last` 是上一次驗的時間戳（`db::now()` 的格式），`None`＝沒驗過（要驗）。
/// 讀不懂的時間戳當成沒驗過：寧可多跑一次 `lsof`，也不要因為一個壞字串從此不再檢查。
fn due_after(last: Option<&str>, now: &str, every_ms: i64) -> bool {
    let (Some(last), Some(now)) = (parse_ts(last), parse_ts(Some(now))) else { return true };
    (now - last).num_milliseconds() >= every_ms
}

fn parse_ts(ts: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts?.trim()).ok().map(|t| t.with_timezone(&chrono::Utc))
}

/// 每顆 bot 上一次驗位址的時間。只在 `running` 期間用得到，預覽收掉時由 [`forget`] 清掉。
fn last_checked() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// 這一拍輪不輪得到這顆 bot 重驗；回 `true` 時順手記下「這一拍驗了」。
/// 轉成 `running` 的那一拍一律驗，不必問這支。
pub fn take_recheck_slot(bot_id: &str, now: &str) -> bool {
    take_slot_after(bot_id, now, RECHECK_EVERY_MS)
}

/// [`take_recheck_slot`] 的本體，間隔可指定——正式間隔在測試裡是 0（每一拍都給），
/// 「記下來了沒」要拿真的間隔才看得出來。
fn take_slot_after(bot_id: &str, now: &str, every_ms: i64) -> bool {
    let mut g = last_checked().lock().unwrap_or_else(|e| e.into_inner());
    if !due_after(g.get(bot_id).map(String::as_str), now, every_ms) {
        return false;
    }
    g.insert(bot_id.to_string(), now.to_string());
    true
}

/// 預覽不在 `running` 了就忘掉它：不然每顆開過預覽的 bot 都會在表裡留一筆。
pub fn forget(bot_id: &str) {
    last_checked().lock().unwrap_or_else(|e| e.into_inner()).remove(bot_id);
}

/// 這個框架要用哪個旗標指定綁的位址；`None`＝不知道，不要猜。
///
/// 只列查得到、而且語意是「綁這個位址」的：vite 的 `--host <addr>`、next 的 `-H <addr>`。
/// 其他框架不是沒有，是**沒把握**——送錯旗標 dev server 會直接退出，比綁錯介面更難查。
pub fn host_flag(kind: &str) -> Option<&'static str> {
    match kind {
        "vite" => Some("--host"),
        "next" => Some("-H"),
        _ => None,
    }
}

/// 把 loopback 旗標接到 `bun run dev` 後面。`--` 是 bun 的分隔符，後面的字原樣傳給 script。
/// 不知道怎麼指定就原樣回：由 [`exposed_addr`] 那一道接住。
pub fn pin_dev_command(cmd: &str, kind: &str) -> String {
    match host_flag(kind) {
        Some(flag) => format!("{cmd} -- {flag} {LOOPBACK}"),
        None => cmd.to_string(),
    }
}

/// `lsof -Fpn` 的 `n` 欄位：`127.0.0.1:5180`、`*:3000`、`[::1]:5180`、`[::]:3000`。
/// 回 `(位址, port)`，順序照 `lsof` 給的；解不出 port 的跳過。
pub fn parse_listeners(out: &str) -> Vec<(String, u16)> {
    let mut found = Vec::new();
    for line in out.lines() {
        let Some(rest) = line.strip_prefix('n') else { continue };
        let rest = rest.trim();
        // 位址與 port 之間是**最後**一個冒號：IPv6 的位址自己就有一堆冒號。
        let Some((addr, port)) = rest.rsplit_once(':') else { continue };
        let Ok(port) = port.trim().parse::<u16>() else { continue };
        let addr = addr.trim();
        if addr.is_empty() {
            continue;
        }
        found.push((addr.to_string(), port));
    }
    found
}

/// 這個位址是不是只有本機連得到。
///
/// `*` 是 `lsof` 對 `INADDR_ANY` 的寫法，跟 `0.0.0.0`／`[::]` 一樣是「全部介面」。
/// 認不得的字串一律當成**對外**：這是安全判斷，不確定就從嚴。
pub fn is_loopback(addr: &str) -> bool {
    let a = addr.trim();
    if a == "localhost" {
        return true;
    }
    let a = a.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(a);
    // IPv6 的 scope id（`fe80::1%en0`）要先去掉才 parse 得動。
    let a = a.split('%').next().unwrap_or(a);
    a.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 這組 listener 有沒有綁到 loopback 以外；回第一個違規的位址（給錯誤訊息用）。
/// 空的（沒在 listen）不算違規。
pub fn exposed_addr(listeners: &[(String, u16)]) -> Option<&str> {
    listeners.iter().find(|(addr, _)| !is_loopback(addr)).map(|(addr, _)| addr.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只有查得到怎麼指定的框架才接旗標；其餘原樣送出去，交給實際位址那一道。
    #[test]
    fn only_frameworks_with_a_known_flag_get_one() {
        assert_eq!(pin_dev_command("bun run dev", "vite"), "bun run dev -- --host 127.0.0.1");
        assert_eq!(pin_dev_command("bun run dev", "next"), "bun run dev -- -H 127.0.0.1");
        for kind in ["astro", "webpack", "unknown", "bun", ""] {
            assert_eq!(pin_dev_command("bun run dev", kind), "bun run dev", "{kind} 沒把握就不要猜旗標");
        }
    }

    #[test]
    fn loopback_addresses_are_recognised_and_everything_else_is_not() {
        for addr in ["127.0.0.1", "127.1.2.3", "[::1]", "::1", "localhost"] {
            assert!(is_loopback(addr), "{addr} 是 loopback");
        }
        // `*` 是 lsof 寫的 INADDR_ANY；認不得的字串也一律從嚴。
        for addr in ["*", "0.0.0.0", "[::]", "::", "192.168.1.9", "fe80::1%en0", "", "garbage"] {
            assert!(!is_loopback(addr), "{addr} 不能當成只有本機連得到");
        }
    }

    #[test]
    fn listeners_are_parsed_with_their_addresses() {
        let out = "p123\nn127.0.0.1:5180\nn[::1]:5180\np456\nn*:3000\nf7\nnnot-a-socket\n";
        assert_eq!(
            parse_listeners(out),
            vec![("127.0.0.1".into(), 5180u16), ("[::1]".into(), 5180), ("*".into(), 3000)],
            "位址要留著，IPv6 的冒號不能把位址切壞"
        );
    }

    #[test]
    fn a_single_non_loopback_listener_is_enough_to_be_exposed() {
        let loopback = vec![("127.0.0.1".to_string(), 5180u16), ("[::1]".to_string(), 5180)];
        assert_eq!(exposed_addr(&loopback), None);
        assert_eq!(exposed_addr(&[]), None, "沒在 listen 不算違規");

        // 同時綁 loopback 與對外（vite `--host` 會這樣）：只要有一個對外就是對外。
        let mixed = vec![("127.0.0.1".to_string(), 5180u16), ("*".to_string(), 5180)];
        assert_eq!(exposed_addr(&mixed), Some("*"));
        assert_eq!(exposed_addr(&[("192.168.1.9".to_string(), 3000u16)]), Some("192.168.1.9"));
    }

    /// 間隔的算法（正式是 60 秒；測試裡 `RECHECK_EVERY_MS` 是 0，所以這裡直接指定間隔驗）。
    #[test]
    fn a_recheck_is_due_once_the_interval_has_passed() {
        let t = |ms: i64| chrono::DateTime::from_timestamp_millis(1_700_000_000_000 + ms).unwrap().to_rfc3339();
        assert!(due_after(None, &t(0), 60_000), "沒驗過一定要驗");
        assert!(!due_after(Some(&t(0)), &t(59_999), 60_000), "還沒到就不多跑 lsof");
        assert!(due_after(Some(&t(0)), &t(60_000), 60_000), "到了就驗");
        assert!(due_after(Some(&t(0)), &t(120_000), 60_000));
        // 時鐘往回跳（NTP 校時）不該讓它從此不再檢查——但也不會比「沒驗過」更糟：下一次到期照樣驗。
        assert!(!due_after(Some(&t(60_000)), &t(0), 60_000));
        for bad in ["", "not-a-time", "2026-09-24"] {
            assert!(due_after(Some(bad), &t(0), 60_000), "讀不懂的 {bad:?} 當成沒驗過");
            assert!(due_after(Some(&t(0)), bad, 60_000));
        }
    }

    /// 名額一個間隔只給一次（給了就記下來），`forget` 之後重新開始。
    /// 拿真的間隔驗：正式間隔在測試裡是 0，每一拍都給，看不出有沒有記。
    #[test]
    fn a_slot_is_taken_once_per_interval_and_forgotten_with_the_preview() {
        let t = |ms: i64| chrono::DateTime::from_timestamp_millis(1_700_000_000_000 + ms).unwrap().to_rfc3339();
        // bot id 各測試不同：這張表是行程共用的，撞名才會互相影響。
        let bot = format!("b-{}", crate::db::ulid());

        assert!(take_slot_after(&bot, &t(0), 60_000), "第一次一定給");
        assert!(!take_slot_after(&bot, &t(0), 60_000), "給過就記下來了，同一拍不再給");
        assert!(!take_slot_after(&bot, &t(59_999), 60_000), "還沒到間隔");
        assert!(take_slot_after(&bot, &t(60_000), 60_000), "到了再給一次");
        assert!(!take_slot_after(&bot, &t(60_001), 60_000), "剛給過，重新計時");

        forget(&bot);
        assert!(take_slot_after(&bot, &t(0), 60_000), "忘掉之後重新開始");
        forget(&bot);
    }
}
