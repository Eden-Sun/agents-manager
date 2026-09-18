//! 插隊送出（issue #103）：用 claude 2.1.275 的 send-now 鍵把一句話送進**正在忙**的 bot。
//!
//! 一般的 prompt 遇到 in-flight 回合一律回 409（AGM 派工才排隊）。使用者想「現在就讓它看到這句」時，
//! 以前只有兩條路：先中斷、等它停下來再送（中間那一回合的收尾要自己顧），或直接在終端分頁打字
//! （daemon 完全不知道那句話存在，對話裡也沒有）。
//!
//! 2.1.275 起 CLI 自己有一顆鍵把這兩步併成一步——它自己決定怎麼收掉當下那一回合，比我們從外面送
//! `ctrl+c` 再貼字準。daemon 這一側要做的只有兩件事：**打字前**確認這顆 run 真的認得那顆鍵，
//! **那顆鍵確定生效之後**把被打斷的那一回合收成 `failed`（見 [`deliver`]，#120）——不能留一個永遠 `in_flight`
//! 的回合，也不能在鍵沒生效時假裝打斷了。

use super::*;

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

/// 插隊送出一句話的結果（#120）。
pub(crate) enum Outcome {
    /// 送出鍵確定生效（herdr 收下了，或證據證明送出去了）：被插隊的那一回合已經收成「被插隊打斷」，新的那一則
    /// 掛上 run。裡面是新那一則的送達結果。
    Interrupted(anyhow::Result<Delivered>),
    /// 一個字都沒進 pane（準備被擋、herdr 拒收打字）：撤回新的那一則、回可重試的 409。
    NotAttempted(Delivered),
    /// 送出鍵沒有生效：正在跑的那一回合照常，新的那一則沒送出（字可能還留在輸入框）。
    NotSent(&'static str),
    /// 不知道送出鍵有沒有生效：不假定打斷——正在跑的那一回合留在 in_flight，記成待證。
    Unknown(&'static str),
    /// 送出鍵生效了，DB 那一半（收舊的、掛新的）寫不進去：記成欠著，之後補。
    Uncommitted(anyhow::Error),
}

/// 插隊送出（#120）。**會打斷正在跑的那一回合的是送出鍵**，不是打字：claude 忙的時候框裡照樣可以打字，回合照跑。
/// 所以被插隊的那一筆只在送出鍵**確定生效**之後才收，而且跟新的那一則掛上 run 是同一個交易（`interruption`）：
/// - 送出鍵之前的任何放棄——準備被擋、herdr 拒收打字、打字沒回、打完框是空的——舊回合原封不動。
/// - 送出鍵：herdr 回 ok＝生效；回錯誤＝沒生效；沒回＝看證據（transcript 出現這一則＝生效；字還整個在框裡＝沒生效，
///   再按一次；都看不出來＝不知道）。
///
/// 準備（`prepare_delivery`）緊接在打字前面，中間沒有任何 DB 寫入：它重看一次框就是對「人在終端裡打字」的圍籬——
/// 框裡有字就不打，不會把兩段字接在一起送出去。herdr 沒有「框是空的才打字」的原子操作，最後那一次讀到打字之間的
/// 空檔跟一般送出一樣短。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn deliver(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    plan: Plan,
    interrupted: &db::Turn,
    new_turn: &str,
) -> Outcome {
    use super::interruption::{key_fate, KeyFate};
    let ready = match prepare_delivery(app, client, run, bot, text, plan).await {
        Ok(r) => r,
        Err(not) => return Outcome::NotAttempted(not),
    };
    let typed = match type_text(client, run, bot, text, ready).await {
        Ok(Typing::Ready(t)) => t,
        Ok(Typing::Done(not @ Delivered::NotAttempted { .. })) => return Outcome::NotAttempted(not),
        // 證據在按送出鍵之前就長出來了：不是我們按的鍵送的，舊回合怎樣了不知道。
        Ok(Typing::Done(Delivered::Submitted)) => return unknown(bot, run, interrupted, None, "submitted_before_key"),
        // 打完框是空的或讀不到：送出鍵沒按。
        Ok(Typing::Done(_)) => return Outcome::NotSent("typed_but_not_in_box"),
        Err(e) if crate::herdr::never_applied(&e) => {
            tracing::warn!(bot = %bot.name, error = %e, "插隊送出：herdr 拒收打字，一個字都沒進去");
            return Outcome::NotAttempted(Delivered::NotAttempted { reason: "pane_send_refused", retry: true });
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "插隊送出：打字沒有回應，不按送出鍵");
            return Outcome::NotSent("typing_unanswered");
        }
    };
    let mut fate = key_fate(&press_submit(client, &typed).await);
    #[cfg(test)]
    super::race_point::hit("send_now_after_key", &bot.id).await;
    let mut presses = 1;
    let proven = loop {
        match fate {
            KeyFate::Applied => break None,
            KeyFate::NotApplied => return Outcome::NotSent("herdr_refused_key"),
            KeyFate::Unknown => match submit_landed(app, client, run, bot, text, &typed).await {
                Landed::Yes(d) => break Some(d),
                Landed::No if presses < 2 => {
                    tracing::warn!(bot = %bot.name, "插隊送出：送出鍵沒有回、字還在框裡——鍵沒生效，再按一次");
                    fate = key_fate(&press_submit(client, &typed).await);
                    presses += 1;
                }
                Landed::No => return Outcome::NotSent("key_did_not_land"),
                Landed::Unknown => return unknown(bot, run, interrupted, SentProof::of(&typed, text), "key_unanswered"),
            },
        }
    };
    // 送出鍵生效了：從這一刻起舊回合才算被插隊打斷。
    let committed = super::interruption::send_now_interrupted(app, &bot.id, &run.id, &interrupted.id, new_turn).await;
    let delivered = match proven {
        Some(d) => Ok(d),
        None => confirm_submitted(app, client, run, bot, text, &typed).await,
    };
    match committed {
        Ok(()) => Outcome::Interrupted(delivered),
        Err(e) => {
            // 送達結果先記在帳上，補的時候跟掛上 run 一起寫。DB 一時寫不進去多半已經好了：當場再補一次。
            super::interruption::owe_delivery(&bot.id, new_turn, delivered.as_ref().ok().and_then(Delivered::record));
            #[cfg(test)]
            super::race_point::hit("send_now_owed", &bot.id).await;
            let settled = super::interruption::settle_locked(app, &bot.id, super::interruption::Evidence::Nothing).await;
            if settled.is_ok() && !super::interruption::owes(&bot.id, &interrupted.id) {
                Outcome::Interrupted(delivered)
            } else {
                Outcome::Uncommitted(e)
            }
        }
    }
}

fn unknown(bot: &db::Bot, run: &db::Run, interrupted: &db::Turn, proof: Option<SentProof>, why: &'static str) -> Outcome {
    super::interruption::unconfirmed_send_now(&bot.id, &run.id, &interrupted.id, proof);
    Outcome::Unknown(why)
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
