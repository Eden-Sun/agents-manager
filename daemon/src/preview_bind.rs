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
//!    蓋回去的專案，都由這一道接住。
//!
//! 驗不到（`lsof` 讀不到、行程樹問不到）**不算違規**：跟整個預覽模組同一條原則——讀不到是「不知道」，
//! 不是「沒有」，不下結論。

/// 強制綁這個位址。
pub const LOOPBACK: &str = "127.0.0.1";

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
}
