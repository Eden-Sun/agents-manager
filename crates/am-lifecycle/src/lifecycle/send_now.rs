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

/// am-turn-send（P4send）對其他 feature 的窄介面與 `App` 端實作（crate 拆分第 3 步）；檔案在 `daemon/src/`，不碰 `lifecycle/mod.rs`。
#[path = "../send_ports.rs"]
pub mod ports;
/// 有 send-now 鍵的最低 claude 版本。低於它的 run 照舊排隊／409。
pub const MIN_VERSION: &str = "2.1.275";

/// 有 `instant_interrupt` 的最低 codex 版本（issue #748，codex 0.159.0 release）。
pub const CODEX_MIN_VERSION: &str = "0.159.0";

/// 不能插隊的原因；回進 409 的 body（`send_now_refused`），讓呼叫端知道為什麼沒插隊。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 只有 claude 有這顆鍵。codex／grok 照舊。
    UnsupportedKind,
    /// 這個 run 跑的 claude 比 2.1.275 舊：那顆鍵按下去是別的東西（或什麼都不是）。
    CliTooOld,
    /// statusLine 還沒回報版本。**不賭**：按錯鍵的代價是把使用者的字打進不知道什麼地方。
    VersionUnknown,
    /// 旗標開著，但這個 run 跑的 codex 比 0.159.0 舊：沒有 `instant_interrupt`，打進去的字只會排在輸入佇列（#748）。
    CodexTooOld,
    /// 旗標開著，但還沒看過這個 run 的 codex 版本（畫面上的版本行還沒被巡邏讀到）。不賭（#748）。
    CodexVersionUnknown,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::UnsupportedKind => "send_now_unsupported_kind",
            Refusal::CliTooOld => "send_now_cli_too_old",
            Refusal::VersionUnknown => "send_now_version_unknown",
            Refusal::CodexTooOld => "send_now_codex_too_old",
            Refusal::CodexVersionUnknown => "send_now_codex_version_unknown",
        }
    }

    pub fn message(self) -> String {
        match self {
            Refusal::UnsupportedKind => "插隊送出只有 claude 有（2.1.275 的 send-now 鍵）；這顆 bot 照舊排隊。".into(),
            Refusal::CliTooOld => format!("這個 run 跑的 claude 比 {MIN_VERSION} 舊，沒有 send-now 鍵；重啟套用新版後才能插隊。"),
            Refusal::VersionUnknown => "還不知道這個 run 跑的 claude 版本（statusLine 尚未回報），不賭那顆鍵。".into(),
            Refusal::CodexTooOld => format!("這個 run 跑的 codex 比 {CODEX_MIN_VERSION} 舊，沒有 instant_interrupt；重啟套用新版後才能插隊。"),
            Refusal::CodexVersionUnknown => "還不知道這個 run 跑的 codex 版本（畫面上的版本行尚未讀到），不賭 instant_interrupt。".into(),
        }
    }
}

/// 這顆 run 能不能插隊送出。`status_json` 是 statusLine 回報的那包（`runs.status_json`）——
/// **跑著的**版本，不是磁碟上的：更新下載完但還沒重啟時，磁碟已經 2.1.275、process 還是舊的。
pub fn supported(kind: &str, status_json: Option<&str>) -> Result<(), Refusal> {
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

/// 這一次插隊怎麼做。兩條路的 DB 語意不同，不能混用：
/// claude 是**打斷舊回合再開新回合**（`deliver`）；codex 是 **steer 進同一個進行中的回合**（`codex_steer`，不收舊回合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    ClaudeKey,
    CodexSteer,
}

/// 這顆 run 用哪一條路插隊，或為什麼不能（issue #748）。`codex_running` 是 `codex_update::running_version_of`，
/// `codex_instant_interrupt` 是 `[codex] instant_interrupt` 旗標。旗標關的時候 codex 一律回 [`Refusal::UnsupportedKind`]，
/// 跟這個功能出現以前一模一樣。
pub fn gate(
    kind: &str,
    status_json: Option<&str>,
    codex_running: Option<&str>,
    codex_instant_interrupt: bool,
) -> Result<Mode, Refusal> {
    match kind {
        "claude" => supported(kind, status_json).map(|()| Mode::ClaudeKey),
        "codex" if codex_instant_interrupt => {
            let Some(running) = codex_running.and_then(crate::changelog::version_string) else {
                return Err(Refusal::CodexVersionUnknown);
            };
            let (Some(r), Some(min)) = (crate::changelog::parse_version(&running), crate::changelog::parse_version(CODEX_MIN_VERSION)) else {
                return Err(Refusal::CodexVersionUnknown);
            };
            if r < min {
                return Err(Refusal::CodexTooOld);
            }
            Ok(Mode::CodexSteer)
        }
        _ => Err(Refusal::UnsupportedKind),
    }
}

/// 插隊送出一句話的結果（#120）。
pub enum Outcome {
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
pub async fn deliver(
    app: &impl super::s6_ports::SendNowContext,
    client: &super::RunClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    plan: Plan,
    interrupted: &db::Turn,
    new_turn: &str,
) -> Outcome {
    use super::interruption::{key_fate, KeyFate};
    let ready = match app.prepare_delivery(client, run, bot, text, plan).await {
        Ok(r) => r,
        Err(not) => return Outcome::NotAttempted(not),
    };
    let typed = match app.type_delivery_text(client, run, bot, text, ready).await {
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
        // 前幾段已經在框裡（#647）：不是 NotAttempted。送出鍵沒按，字留在框裡，跟「打字沒回應」同一條。
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "插隊送出：打字沒有回應，不按送出鍵");
            return Outcome::NotSent("typing_unanswered");
        }
    };
    let mut fate = key_fate(&press_submit(client, &typed).await);
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("send_now_after_key", &bot.id).await;
    let mut presses = 1;
    let proven = loop {
        match fate {
            KeyFate::Applied => break None,
            KeyFate::NotApplied => return Outcome::NotSent("herdr_refused_key"),
            KeyFate::Unknown => match app.send_now_landed(client, run, bot, text, &typed).await {
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
        None => app.confirm_send_now(client, run, bot, text, &typed).await,
    };
    match committed {
        Ok(()) => Outcome::Interrupted(delivered),
        Err(e) => {
            // 送達結果先記在帳上，補的時候跟掛上 run 一起寫。DB 一時寫不進去多半已經好了：當場再補一次。
            // 確認送出本身出錯也要記（#157）：鍵已經生效，證不出來就是 `unknown`（同 prompt 那一頭對 `Err` 的處理）——
            // 沒記的話補收尾時沒東西可寫，那一則停在 pending 直到重啟。送達時間是此刻，不是補寫的時候。
            let seen = delivered.as_ref().ok().copied().unwrap_or(Delivered::Unproven("send_now_unconfirmed"));
            super::interruption::owe_delivery(&bot.id, new_turn, seen.record(), db::now());
            #[cfg(all(test, feature = "daemon-test-harness"))]
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

#[cfg(all(test, feature = "daemon-test-harness"))]
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
        for r in [Refusal::UnsupportedKind, Refusal::CliTooOld, Refusal::VersionUnknown, Refusal::CodexTooOld, Refusal::CodexVersionUnknown] {
            assert!(r.code().starts_with("send_now_"), "{r:?}");
            assert!(!r.message().is_empty(), "{r:?}");
        }
        assert!(Refusal::CliTooOld.message().contains(MIN_VERSION));
        assert!(Refusal::CodexTooOld.message().contains(CODEX_MIN_VERSION));
    }

    /// issue #748：codex 的閘門。旗標關＝跟以前一樣「只有 claude 有」；旗標開還要看跑著的版本 >= 0.159.0，
    /// 版本不明就不賭（steer 打進去的字，舊版只會排進輸入佇列，等於假裝插了隊）。
    #[test]
    fn codex_steers_only_with_the_canary_on_and_a_new_enough_running_version() {
        let gate = |on, v: Option<&str>| gate("codex", None, v, on);
        assert_eq!(gate(false, Some("0.159.0")), Err(Refusal::UnsupportedKind), "旗標關：維持舊行為與舊代碼");
        assert_eq!(gate(false, None), Err(Refusal::UnsupportedKind));
        assert_eq!(gate(true, Some("0.159.0")), Ok(Mode::CodexSteer));
        assert_eq!(gate(true, Some("0.159.1")), Ok(Mode::CodexSteer));
        assert_eq!(gate(true, Some("0.160.0")), Ok(Mode::CodexSteer));
        assert_eq!(gate(true, Some("0.158.9")), Err(Refusal::CodexTooOld));
        assert_eq!(gate(true, Some("0.155.1")), Err(Refusal::CodexTooOld));
        assert_eq!(gate(true, None), Err(Refusal::CodexVersionUnknown));
        assert_eq!(gate(true, Some("garbage")), Err(Refusal::CodexVersionUnknown));
    }

    /// 旗標只管 codex：claude 的閘門照舊走 statusLine 版本，旗標開不開都一樣；grok／shell 永遠不行。
    #[test]
    fn the_codex_flag_changes_nothing_for_other_kinds() {
        for on in [false, true] {
            assert_eq!(gate("claude", Some(&status("2.1.275")), Some("0.159.0"), on), Ok(Mode::ClaudeKey));
            assert_eq!(gate("claude", Some(&status("2.1.274")), None, on), Err(Refusal::CliTooOld));
            for kind in ["grok", "shell"] {
                assert_eq!(gate(kind, None, Some("0.159.0"), on), Err(Refusal::UnsupportedKind), "{kind}");
            }
        }
    }
}
