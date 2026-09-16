//! LAN 配對（SPEC §7.1a）：`GET /api/session` 只對 loopback 直接發 token。
//!
//! 為什麼：`allow_lan` 打開之後 daemon 綁在 `0.0.0.0`，而 `/api/session` 是**不需要 token** 的端點——
//! 同網段（含 Tailscale 這種疊加網路）任何裝置一個 `curl` 就拿到整把鑰匙，而那把鑰匙過得了 `auth()`
//! ＝整個 API（往任何 bot 的 TUI 打字、開主機 shell、kill 程序、刪專案）。2026-09-16 實測拿得到。
//!
//! 但使用者的手機就是走這條路，所以不能直接把門關上：非 loopback 改成**出示一次性配對碼**換 token。
//! 碼由本機端產生（設定畫面或 `bin/agm pair-code`），短、可唸、五分鐘到期、用過即失效、猜錯會被限流。
//! 已經拿到 token 的裝置照舊——`auth()` 只看 token，不受這條影響。

use std::collections::HashMap;
use std::net::IpAddr;

/// 碼的長度與字母表：拿掉會唸錯的 I／O／0／1，六碼夠短又夠難猜（32^6 ≈ 10 億，配上限流與五分鐘到期）。
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 6;
/// 碼的壽命。短到「貼在畫面上忘了關」不會變成永久的門，長到夠走到手機前面輸入。
pub const CODE_TTL_SECS: i64 = 300;
/// 同一個來源連續猜錯幾次就擋。
const MAX_FAILURES: u32 = 5;
/// 擋多久。
pub const LOCKOUT_SECS: i64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redeem {
    Ok,
    /// 碼不對、已過期、或已經被用掉——對外一律同一種回答，不要透露是哪一種。
    Bad,
    /// 這個來源猜太多次了。
    RateLimited { retry_after_secs: i64 },
}

#[derive(Debug, Default)]
pub struct Pairing {
    codes: HashMap<String, i64>,
    failures: HashMap<IpAddr, (u32, i64)>,
}

/// 大小寫、空白與連字號都不算：使用者會照著唸、照著打。
pub fn normalize(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(|c| c.to_uppercase()).collect()
}

/// `ABC-DEF`：唸起來有停頓，輸入時照 `normalize` 一律去掉。
pub fn format_code(code: &str) -> String {
    if code.len() == CODE_LEN {
        format!("{}-{}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

impl Pairing {
    /// 發一個新碼。舊的不會被作廢——使用者可能同時在配兩支手機。
    pub fn issue(&mut self, now_epoch: i64) -> (String, i64) {
        self.sweep(now_epoch);
        let mut rng = rand::thread_rng();
        use rand::Rng;
        let code: String = (0..CODE_LEN).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect();
        let expires = now_epoch + CODE_TTL_SECS;
        self.codes.insert(code.clone(), expires);
        (code, expires)
    }

    /// 用碼換 token。成功之後那個碼立刻失效（一碼一台）。
    pub fn redeem(&mut self, code: &str, peer: IpAddr, now_epoch: i64) -> Redeem {
        self.sweep(now_epoch);
        if let Some((n, until)) = self.failures.get(&peer).copied() {
            if n >= MAX_FAILURES && until > now_epoch {
                return Redeem::RateLimited { retry_after_secs: until - now_epoch };
            }
        }
        let key = normalize(code);
        match self.codes.remove(&key) {
            Some(exp) if exp > now_epoch => {
                self.failures.remove(&peer);
                Redeem::Ok
            }
            // 過期的碼也一併移除（上面 remove 已經拿掉了），照樣算一次失敗。
            _ => {
                let e = self.failures.entry(peer).or_insert((0, 0));
                e.0 += 1;
                e.1 = now_epoch + LOCKOUT_SECS;
                Redeem::Bad
            }
        }
    }

    /// 過期的碼與過期的鎖不留在記憶體裡。
    fn sweep(&mut self, now_epoch: i64) {
        self.codes.retain(|_, exp| *exp > now_epoch);
        self.failures.retain(|_, (n, until)| *n < MAX_FAILURES || *until > now_epoch);
    }

    #[cfg(test)]
    pub fn live_codes(&self) -> usize {
        self.codes.len()
    }
}

/// 這個對端是不是 loopback。**不看 `Host` 標頭**：那是呼叫端自己填的。
pub fn is_loopback(peer: &std::net::SocketAddr) -> bool {
    match peer.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|m| m.is_loopback()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_code_works_once_and_only_before_it_expires() {
        let mut p = Pairing::default();
        let (code, exp) = p.issue(1000);
        assert_eq!(exp, 1000 + CODE_TTL_SECS);
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.chars().all(|c| ALPHABET.contains(&(c as u8))), "只用唸得出來的字母：{code}");

        // 大小寫、連字號、空白都要能過：使用者是照著畫面唸／打的。
        let typed = format!(" {} ", format_code(&code).to_lowercase());
        assert_eq!(p.redeem(&typed, ip("192.168.1.9"), 1001), Redeem::Ok);
        // 用過就沒了。
        assert_eq!(p.redeem(&code, ip("192.168.1.9"), 1002), Redeem::Bad);

        let (code2, _) = p.issue(2000);
        assert_eq!(p.redeem(&code2, ip("192.168.1.9"), 2000 + CODE_TTL_SECS + 1), Redeem::Bad, "過期就不算");
    }

    #[test]
    fn guessing_gets_you_locked_out_and_a_real_code_clears_it() {
        let mut p = Pairing::default();
        let peer = ip("192.168.1.9");
        for _ in 0..MAX_FAILURES {
            assert_eq!(p.redeem("ZZZZZZ", peer, 100), Redeem::Bad);
        }
        let (code, _) = p.issue(100);
        match p.redeem(&code, peer, 100) {
            Redeem::RateLimited { retry_after_secs } => assert!(retry_after_secs > 0),
            other => panic!("猜太多次要被擋下來，拿到 {other:?}"),
        }
        // 別的來源不受影響。
        assert_eq!(p.redeem(&code, ip("192.168.1.10"), 100), Redeem::Ok);

        // 鎖過期之後可以再試，而且成功一次就把計數清掉。
        let (code2, _) = p.issue(100 + LOCKOUT_SECS + 1);
        assert_eq!(p.redeem(&code2, peer, 100 + LOCKOUT_SECS + 1), Redeem::Ok);
        assert_eq!(p.redeem("ZZZZZZ", peer, 100 + LOCKOUT_SECS + 2), Redeem::Bad, "清掉之後從頭算");
    }

    #[test]
    fn expired_codes_do_not_pile_up() {
        let mut p = Pairing::default();
        for i in 0..5 {
            p.issue(1000 + i);
        }
        assert_eq!(p.live_codes(), 5);
        p.issue(1000 + CODE_TTL_SECS + 10);
        assert_eq!(p.live_codes(), 1, "過期的碼不留在記憶體裡");
    }
}
