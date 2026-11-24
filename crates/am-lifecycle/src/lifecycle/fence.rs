//! Run／session 的世代圍籬（issue #69）：一則遲到的事件屬於**哪一代**。
//!
//! 光比對「這顆 bot 現在的 active run」不夠。典型的 race：使用者 interrupt 之後 bot 重啟，新的 run
//! 已經開始新回合，舊 CLI session 的 `Stop` 這時候才抵達——按舊規則它會被當成「這顆 bot 的 hook」，
//! 去收新回合的尾，把一個還在跑的回合標成完成，還把上一代的回覆貼進去。
//!
//! **世代是什麼**：就是 `runs` 那一列本身，**寫入順序**（SQLite 的隱式 `rowid`）決定先後——不必
//! 另外再養一個計數器欄位。`runs.id` 是 ULID，**不能**拿字典序當世代序：ULID 只有毫秒精度的時間戳
//! 是遞增的，同一毫秒內的隨機段不保證單調（issue #98：同一顆 bot 兩個 run 理論上可能落在同一毫秒，
//! `a4605b2` 修過同一個成因在 `mission_events` 上的版本——這裡本來也錯著，只是舊測試偶爾抓到）。
//!
//! **怎麼證明一則事件是舊的**：事件帶的 native session id，在這顆 bot **某個確實比現在這代早寫進去
//! 的 run**（`rowid` 較小）上找得到。這比「session 跟現在這代不一樣就丟」嚴格得多，而且是刻意的：
//! claude 在同一個 CLI 裡 `/clear` 會換一個 session id，run 沒變——用「不一樣就丟」的話，`/clear`
//! 之後每一則 Stop 都會被殺掉，回合再也不會完成。證不出來就**放行**（見 [`Ownership::Unproven`]），
//! 維持既有行為。
//!
//! 這條規則只住在這裡。hook 那邊只呼叫 [`classify`]，不自己判斷。

use crate::db;

/// 一則事件自己說得出來的身份。
#[derive(Debug, Clone, Copy, Default)]
pub struct EventIdentity<'a> {
    /// 送出這則事件的 CLI 行程是替哪個 run 起的（hook body 的 `run_id`＝那個行程 pane env 的
    /// `AM_RUN_ID`，issue #92）。舊行程、遠端舊版 `hook.sh` 沒有。
    pub run_id: Option<&'a str>,
    /// provider 的 native session id（claude `session_id`、grok `sessionId`、codex `thread-id`）。
    pub session_id: Option<&'a str>,
}

/// 這顆 bot 現在這一代。
#[derive(Debug, Clone, Copy)]
pub struct Generation<'a> {
    pub run_id: &'a str,
    pub session_id: Option<&'a str>,
    /// `resume_native` 起的 run 要接回的原對話：那也算這一代自己的 session（第一則 hook 之後會被
    /// 收進 `native_session_id`，這裡是收進去之前的那個窗口）。
    pub resume_session_id: Option<&'a str>,
    /// 這個 run 是收編來的（`runs.adopted`：對帳、預設 session、子 agent 原地重啟）。它的行程是在
    /// **別的** run 底下起的，pane env 的 `AM_RUN_ID` 是那個 run——事件指名別的 run 在這裡證明不了什麼。
    /// daemon 自己 `start_bot` 起的 run（`adopted=0`）才保證行程帶的就是這個 run 的 id。
    pub adopted: bool,
}

/// 事件的 session 在哪一個**確實更早寫進去**的 run 上找到——呼叫端（`classify`）必須已經用 rowid
/// （寫入順序）驗過，`decide` 不會再用 ULID 字典序複查一次（issue #98）。
#[derive(Debug, Clone, Copy)]
pub struct PriorRun<'a> {
    pub run_id: &'a str,
}

/// 判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// 證明屬於這一代：照常處理。
    Current,
    /// 證明屬於**更早**的一代：只可以記錄，一個欄位都不准改。
    Stale { prior_run_id: String, why: &'static str },
    /// 證不出來：照既有規則走（不依賴時序去猜），但留一行說明為什麼證不出來。
    Unproven(&'static str),
}

impl Ownership {
    /// 可不可以拿它去改狀態。
    pub fn may_mutate(&self) -> bool {
        !matches!(self, Ownership::Stale { .. })
    }

    /// 放行的證明：只有這裡做得出來。hook 改 Turn 的那兩支（`turn_controller` 的
    /// `*_with_native_evidence`）要它——沒問過圍籬就沒辦法呼叫（issue #125）。
    pub fn admit(&self, run_id: &str) -> Option<Admitted> {
        self.may_mutate().then(|| Admitted { run_id: run_id.to_string() })
    }
}

/// 圍籬放行了這一代（`run_id`）的事件。欄位私有：只能由 [`Ownership::admit`] 做出來。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    run_id: String,
}

impl Admitted {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    pub fn for_test(run_id: &str) -> Self {
        Admitted { run_id: run_id.to_string() }
    }
}

/// 純函式版本：所有查詢都由呼叫端做完再餵進來，規則本身測得到。
pub fn decide(now: &Generation, ev: &EventIdentity, prior: Option<&PriorRun>) -> Ownership {
    fn trimmed(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|v| !v.is_empty())
    }
    if let Some(rid) = trimmed(ev.run_id) {
        if rid == now.run_id {
            return Ownership::Current;
        }
        if !now.adopted {
            return Ownership::Stale { prior_run_id: rid.to_string(), why: "事件是另一個 run 的行程送的" };
        }
    }
    let Some(sid) = trimmed(ev.session_id) else {
        return Ownership::Unproven("事件沒帶 session id");
    };
    if trimmed(now.session_id) == Some(sid) || trimmed(now.resume_session_id) == Some(sid) {
        return Ownership::Current;
    }
    match prior {
        Some(p) => Ownership::Stale { prior_run_id: p.run_id.to_string(), why: "這個 session 屬於這顆 bot 更早一代的 run" },
        None => Ownership::Unproven("這個 session 不屬於任何更早的 run"),
    }
}

/// 帶查詢的版本：`hookrecv` 只呼叫這支。
pub async fn classify(pool: &sqlx::SqlitePool, bot_id: &str, run: &db::Run, ev: EventIdentity<'_>) -> Ownership {
    let now = Generation {
        run_id: &run.id,
        session_id: run.native_session_id.as_deref(),
        resume_session_id: run.resume_session_id.as_deref(),
        adopted: run.adopted != 0,
    };
    let quick = decide(&now, &ev, None);
    if !matches!(quick, Ownership::Unproven("這個 session 不屬於任何更早的 run")) {
        return quick;
    }
    let sid = ev.session_id.map(str::trim).unwrap_or_default();
    let prior: Option<String> = match sqlx::query_scalar(
        "SELECT id FROM runs WHERE bot_id = ? AND native_session_id = ?
           AND rowid < (SELECT rowid FROM runs WHERE id = ?)
         ORDER BY rowid DESC LIMIT 1",
    )
    .bind(bot_id)
    .bind(sid)
    .bind(&run.id)
    .fetch_optional(pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = ?e, "世代圍籬查不到舊 run；這一則照既有規則處理");
            return Ownership::Unproven("查不到舊 run");
        }
    };
    decide(&now, &ev, prior.as_deref().map(|run_id| PriorRun { run_id }).as_ref())
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod tests {
    use super::*;
    fn gen<'a>(run_id: &'a str, session: Option<&'a str>) -> Generation<'a> {
        Generation { run_id, session_id: session, resume_session_id: None, adopted: false }
    }
    /// 這一代自己的事件照常處理。
    #[test]
    fn an_event_from_this_generation_is_current() {
        let now = gen("01RUN-B", Some("s-new"));
        assert_eq!(decide(&now, &EventIdentity { run_id: None, session_id: Some("s-new") }, None), Ownership::Current);
        assert_eq!(decide(&now, &EventIdentity { run_id: Some("01RUN-B"), session_id: None }, None), Ownership::Current);
        let resuming = Generation { run_id: "01RUN-B", session_id: None, resume_session_id: Some("s-old"), adopted: false };
        assert_eq!(decide(&resuming, &EventIdentity { run_id: None, session_id: Some("s-old") }, None), Ownership::Current);
    }
    /// 上一代的事件擋下來：那是 issue #69 的 race（interrupt → 重啟 → 舊 Stop 才到）。
    #[test]
    fn an_event_from_an_older_run_is_stale_and_may_not_mutate() {
        let now = gen("01RUN-B", Some("s-new"));
        let ev = EventIdentity { run_id: None, session_id: Some("s-old") };
        let out = decide(&now, &ev, Some(&PriorRun { run_id: "01RUN-A" }));
        assert!(matches!(&out, Ownership::Stale { prior_run_id, .. } if prior_run_id == "01RUN-A"), "{out:?}");
        assert!(!out.may_mutate());
        let named = decide(&now, &EventIdentity { run_id: Some("01RUN-A"), session_id: None }, None);
        assert!(!named.may_mutate(), "{named:?}");
    }
    /// issue #92：`--resume` 接回同一段對話，新舊兩個行程回報**同一個** session。換身份前那個行程
    /// 遲到的事件只看 session 是「這一代的」——它自己帶的 run id 才分得開。
    #[test]
    fn a_resumed_session_is_told_apart_by_the_run_its_process_was_started_for() {
        let resumed = Generation { run_id: "01RUN-B", session_id: Some("s-same"), resume_session_id: Some("s-same"), adopted: false };
        let old_process = decide(&resumed, &EventIdentity { run_id: Some("01RUN-A"), session_id: Some("s-same") }, None);
        assert!(matches!(&old_process, Ownership::Stale { prior_run_id, .. } if prior_run_id == "01RUN-A"), "{old_process:?}");
        assert_eq!(decide(&resumed, &EventIdentity { run_id: Some("01RUN-B"), session_id: Some("s-same") }, None), Ownership::Current);
        assert_eq!(decide(&resumed, &EventIdentity { run_id: None, session_id: Some("s-same") }, None), Ownership::Current);
    }
    /// 收編來的 run：它的行程是在別的 run 底下起的，帶的 run id 本來就對不上——不能因此把它自己的
    /// 事件全部丟掉，交給 session 規則。
    #[test]
    fn an_adopted_run_does_not_trust_the_run_id_its_process_carries() {
        let adopted = Generation { run_id: "01RUN-ADOPTED", session_id: Some("s-live"), resume_session_id: None, adopted: true };
        let own = decide(&adopted, &EventIdentity { run_id: Some("01RUN-STARTED"), session_id: Some("s-live") }, None);
        assert_eq!(own, Ownership::Current, "session 是這一代的：照常處理");
        let old = decide(&adopted, &EventIdentity { run_id: Some("01RUN-STARTED"), session_id: Some("s-old") }, Some(&PriorRun { run_id: "01RUN-OLD" }));
        assert!(!old.may_mutate(), "{old:?}");
    }
    /// 證不出來就放行——不靠時序猜。最重要的是 claude 在同一個 CLI 裡 `/clear` 換 session 的情況：
    /// 用「session 不一樣就丟」的話，`/clear` 之後每一則 Stop 都會被殺掉。
    #[test]
    fn an_unprovable_event_is_let_through_not_guessed_at() {
        let now = gen("01RUN-B", Some("s-first"));
        let cleared = decide(&now, &EventIdentity { run_id: None, session_id: Some("s-after-clear") }, None);
        assert!(cleared.may_mutate(), "{cleared:?}");
        assert!(matches!(cleared, Ownership::Unproven(_)));
        let bare = decide(&now, &EventIdentity::default(), None);
        assert!(bare.may_mutate(), "{bare:?}");
        let fresh = decide(&gen("01RUN-B", None), &EventIdentity { run_id: None, session_id: Some("s-x") }, None);
        assert!(fresh.may_mutate(), "{fresh:?}");
    }
    /// `decide` 完全信任 `prior`：「這是不是真的更早」是呼叫端（`classify`，用 rowid 驗過）的責任，
    /// 不是這支純函式的事（issue #98：以前這裡還會用 ULID 字典序複查一次，同一毫秒內會複查出錯的答案）。
    #[test]
    fn decide_trusts_prior_completely_it_is_the_callers_job_to_verify_it() {
        let now = gen("01RUN-B", Some("s-first"));
        let out = decide(&now, &EventIdentity { run_id: None, session_id: Some("s-y") }, Some(&PriorRun { run_id: "01RUN-Z" }));
        assert!(matches!(&out, Ownership::Stale { prior_run_id, .. } if prior_run_id == "01RUN-Z"), "{out:?}");
    }
    /// 空字串不是身份：`Some("")` 要跟沒帶一樣。
    #[test]
    fn blank_identities_count_as_missing() {
        let now = gen("01RUN-B", Some("s-new"));
        assert_eq!(decide(&now, &EventIdentity { run_id: Some("  "), session_id: Some(" ") }, None), Ownership::Unproven("事件沒帶 session id"));
    }
}
