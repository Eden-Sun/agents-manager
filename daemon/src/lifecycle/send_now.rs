//! 插隊送出（issue #103）：用 claude 2.1.275 的 send-now 鍵把一句話送進**正在忙**的 bot。
//!
//! 一般的 prompt 遇到 in-flight 回合一律回 409（AGM 派工才排隊）。使用者想「現在就讓它看到這句」時，
//! 以前只有兩條路：先中斷、等它停下來再送（中間那一回合的收尾要自己顧），或直接在終端分頁打字
//! （daemon 完全不知道那句話存在，對話裡也沒有）。
//!
//! 2.1.275 起 CLI 自己有一顆鍵把這兩步併成一步——它自己決定怎麼收掉當下那一回合，比我們從外面送
//! `ctrl+c` 再貼字準。daemon 這一側要做的只有兩件事：**打字前**確認這顆 run 真的認得那顆鍵，
//! **打字後**把被打斷的那一回合收成 `failed`，不能留一個永遠 `in_flight` 的回合。

/// 有 send-now 鍵的最低 claude 版本。低於它的 run 照舊排隊／409。
pub(crate) const MIN_VERSION: &str = "2.1.275";

/// 不能插隊的原因；回進 409 的 body（`send_now_refused`），讓呼叫端知道為什麼沒插隊。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 只有 claude 有這顆鍵。codex／grok 照舊。
    UnsupportedKind,
    /// 這個 run 跑的 claude 比 2.1.275 舊：那顆鍵按下去是別的東西（或什麼都不是）。
    CliTooOld,
    /// statusLine 還沒回報版本。**不賭**：按錯鍵的代價是把使用者的字打進不知道什麼地方。
    VersionUnknown,
}

impl Refusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Refusal::UnsupportedKind => "send_now_unsupported_kind",
            Refusal::CliTooOld => "send_now_cli_too_old",
            Refusal::VersionUnknown => "send_now_version_unknown",
        }
    }

    pub(crate) fn message(self) -> String {
        match self {
            Refusal::UnsupportedKind => "插隊送出只有 claude 有（2.1.275 的 send-now 鍵）；這顆 bot 照舊排隊。".into(),
            Refusal::CliTooOld => format!("這個 run 跑的 claude 比 {MIN_VERSION} 舊，沒有 send-now 鍵；重啟套用新版後才能插隊。"),
            Refusal::VersionUnknown => "還不知道這個 run 跑的 claude 版本（statusLine 尚未回報），不賭那顆鍵。".into(),
        }
    }
}

/// 這顆 run 能不能插隊送出。`status_json` 是 statusLine 回報的那包（`runs.status_json`）——
/// **跑著的**版本，不是磁碟上的：更新下載完但還沒重啟時，磁碟已經 2.1.275、process 還是舊的。
pub(crate) fn supported(kind: &str, status_json: Option<&str>) -> Result<(), Refusal> {
    if kind != "claude" {
        return Err(Refusal::UnsupportedKind);
    }
    let Some(running) = crate::update_watch::running_version(status_json) else {
        return Err(Refusal::VersionUnknown);
    };
    let (Some(r), Some(min)) = (crate::changelog::parse_version(&running), crate::changelog::parse_version(MIN_VERSION)) else {
        return Err(Refusal::VersionUnknown);
    };
    if r < min {
        return Err(Refusal::CliTooOld);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(version: &str) -> String {
        format!(r#"{{"version":"{version} (Claude Code)","model_name":"Opus"}}"#)
    }

    /// 2.1.275 起才有那顆鍵；剛好那一版要算過。
    #[test]
    fn the_send_now_key_starts_at_the_version_that_shipped_it() {
        assert_eq!(supported("claude", Some(&status("2.1.275"))), Ok(()));
        assert_eq!(supported("claude", Some(&status("2.1.280"))), Ok(()));
        assert_eq!(supported("claude", Some(&status("2.2.0"))), Ok(()));
        assert_eq!(supported("claude", Some(&status("2.1.274"))), Err(Refusal::CliTooOld));
        assert_eq!(supported("claude", Some(&status("2.1.9"))), Err(Refusal::CliTooOld));
    }

    /// 只有 claude 有這顆鍵：codex／grok 連版本都不用看，照舊排隊。
    #[test]
    fn only_claude_has_the_key() {
        for kind in ["codex", "grok", "shell"] {
            assert_eq!(supported(kind, Some(&status("2.1.275"))), Err(Refusal::UnsupportedKind), "{kind}");
        }
    }

    /// 版本不明就不插隊：按錯鍵的代價是把使用者的字打進不知道什麼地方，寧可退回排隊那條路。
    #[test]
    fn an_unknown_version_is_never_assumed_new_enough() {
        assert_eq!(supported("claude", None), Err(Refusal::VersionUnknown));
        assert_eq!(supported("claude", Some("{}")), Err(Refusal::VersionUnknown));
        assert_eq!(supported("claude", Some("not json")), Err(Refusal::VersionUnknown));
        assert_eq!(supported("claude", Some(r#"{"version":"unknown"}"#)), Err(Refusal::VersionUnknown));
    }

    /// 每個拒絕原因都有自己的機器可讀代碼與一句中文說明（回進 409 的 body）。
    #[test]
    fn every_refusal_says_why_in_both_forms() {
        for r in [Refusal::UnsupportedKind, Refusal::CliTooOld, Refusal::VersionUnknown] {
            assert!(r.code().starts_with("send_now_"), "{r:?}");
            assert!(!r.message().is_empty(), "{r:?}");
        }
        assert!(Refusal::CliTooOld.message().contains(MIN_VERSION));
    }
}
