//! Run／session 的世代圍籬（issue #69）：一則遲到的事件屬於**哪一代**。
//!
//! 光比對「這顆 bot 現在的 active run」不夠。典型的 race：使用者 interrupt 之後 bot 重啟，新的 run
//! 已經開始新回合，舊 CLI session 的 `Stop` 這時候才抵達——按舊規則它會被當成「這顆 bot 的 hook」，
//! 去收新回合的尾，把一個還在跑的回合標成完成，還把上一代的回覆貼進去。
//!
//! **世代是什麼**：就是 `runs` 那一列本身。`runs.id` 是 ULID（毫秒時間序），同一顆 bot 的兩個 run
//! 不可能落在同一毫秒，所以 `id` 的字典序就是單調遞增的世代序——不必另外再養一個計數器欄位，也就
//! 不會有「79 個 INSERT 只有幾個記得填」那種半populated 的假保證。
//!
//! **怎麼證明一則事件是舊的**：事件帶的 native session id，在這顆 bot **某個 id 比現在這代小的 run**
//! 上找得到。這比「session 跟現在這代不一樣就丟」嚴格得多，而且是刻意的：claude 在同一個 CLI 裡
//! `/clear` 會換一個 session id，run 沒變——用「不一樣就丟」的話，`/clear` 之後每一則 Stop 都會被
//! 殺掉，回合再也不會完成。證不出來就**放行**（見 [`Ownership::Unproven`]），維持既有行為。
//!
//! 這條規則只住在這裡。hook 那邊只呼叫 [`classify`]，不自己判斷。

use crate::db;

/// 一則事件自己說得出來的身分。
#[derive(Debug, Clone, Copy, Default)]
pub struct EventIdentity<'a> {
    /// 事件指名的 run（目前沒有 provider 會帶；留著給日後自己帶 run id 的事件用）。
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
}

/// 事件的 session 在哪一個別的 run 上找到。
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
}

/// 純函式版本：所有查詢都由呼叫端做完再餵進來，規則本身測得到。
pub fn decide(now: &Generation, ev: &EventIdentity, prior: Option<&PriorRun>) -> Ownership {
    fn trimmed(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|v| !v.is_empty())
    }
    // 事件自己指名 run 時最直接：不是這一代就是別代，不必再看 session。
    if let Some(rid) = trimmed(ev.run_id) {
        return if rid == now.run_id {
            Ownership::Current
        } else {
            Ownership::Stale { prior_run_id: rid.to_string(), why: "事件指名的是另一個 run" }
        };
    }
    let Some(sid) = trimmed(ev.session_id) else {
        return Ownership::Unproven("事件沒帶 session id");
    };
    if trimmed(now.session_id) == Some(sid) || trimmed(now.resume_session_id) == Some(sid) {
        return Ownership::Current;
    }
    match prior {
        // ULID 是時間序：上一代的 run id 一定比較小。撈到的若不比現在這代舊（理論上不會，
        // 但資料是外面來的），就不要用它下重手——當成證不出來。
        Some(p) if p.run_id < now.run_id => {
            Ownership::Stale { prior_run_id: p.run_id.to_string(), why: "這個 session 屬於這顆 bot 更早一代的 run" }
        }
        Some(_) => Ownership::Unproven("找到的 run 不比現在這一代舊"),
        // session 對不上、又不屬於任何舊 run：同一個 CLI 換了 session（claude `/clear`）就是這樣。
        // 這時候放行才對——殺掉的話 `/clear` 之後的回合再也不會完成。
        None => Ownership::Unproven("這個 session 不屬於任何更早的 run"),
    }
}

/// 帶查詢的版本：`hookrecv` 只呼叫這支。
pub async fn classify(pool: &sqlx::SqlitePool, bot_id: &str, run: &db::Run, ev: EventIdentity<'_>) -> Ownership {
    let now = Generation {
        run_id: &run.id,
        session_id: run.native_session_id.as_deref(),
        resume_session_id: run.resume_session_id.as_deref(),
    };
    // 先用純規則問一次：能當場判定的（指名 run、session 就是這一代的、根本沒帶 session）不必查 DB。
    let quick = decide(&now, &ev, None);
    if !matches!(quick, Ownership::Unproven("這個 session 不屬於任何更早的 run")) {
        return quick;
    }
    let sid = ev.session_id.map(str::trim).unwrap_or_default();
    let prior: Option<String> = match sqlx::query_scalar(
        "SELECT id FROM runs WHERE bot_id = ? AND native_session_id = ? AND id <> ? ORDER BY id DESC LIMIT 1",
    )
    .bind(bot_id)
    .bind(sid)
    .bind(&run.id)
    .fetch_optional(pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            // 讀不到不等於是舊的：查詢壞掉時放行（既有行為），但要留痕。
            tracing::warn!(bot = %bot_id, error = ?e, "世代圍籬查不到舊 run；這一則照既有規則處理");
            return Ownership::Unproven("查不到舊 run");
        }
    };
    decide(&now, &ev, prior.as_deref().map(|run_id| PriorRun { run_id }).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen<'a>(run_id: &'a str, session: Option<&'a str>) -> Generation<'a> {
        Generation { run_id, session_id: session, resume_session_id: None }
    }

    /// 這一代自己的事件照常處理。
    #[test]
    fn an_event_from_this_generation_is_current() {
        let now = gen("01RUN-B", Some("s-new"));
        assert_eq!(decide(&now, &EventIdentity { run_id: None, session_id: Some("s-new") }, None), Ownership::Current);
        assert_eq!(decide(&now, &EventIdentity { run_id: Some("01RUN-B"), session_id: None }, None), Ownership::Current);
        // `resume_native` 起的 run：第一則 hook 把 session 收進 `native_session_id` 之前的窗口。
        let resuming = Generation { run_id: "01RUN-B", session_id: None, resume_session_id: Some("s-old") };
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
        // 指名別的 run 也一樣。
        let named = decide(&now, &EventIdentity { run_id: Some("01RUN-A"), session_id: None }, None);
        assert!(!named.may_mutate(), "{named:?}");
    }

    /// 證不出來就放行——不靠時序猜。最重要的是 claude 在同一個 CLI 裡 `/clear` 換 session 的情況：
    /// 用「session 不一樣就丟」的話，`/clear` 之後每一則 Stop 都會被殺掉。
    #[test]
    fn an_unprovable_event_is_let_through_not_guessed_at() {
        let now = gen("01RUN-B", Some("s-first"));
        // `/clear` 之後的新 session：不屬於任何舊 run。
        let cleared = decide(&now, &EventIdentity { run_id: None, session_id: Some("s-after-clear") }, None);
        assert!(cleared.may_mutate(), "{cleared:?}");
        assert!(matches!(cleared, Ownership::Unproven(_)));
        // 完全沒帶 session 的事件（codex 的某些 payload、手寫的）。
        let bare = decide(&now, &EventIdentity::default(), None);
        assert!(bare.may_mutate(), "{bare:?}");
        // run 還沒回報過 session：也證不出來。
        let fresh = decide(&gen("01RUN-B", None), &EventIdentity { run_id: None, session_id: Some("s-x") }, None);
        assert!(fresh.may_mutate(), "{fresh:?}");
        // 撈到的 run 不比現在這代舊：不拿它下重手。
        let newer = decide(&now, &EventIdentity { run_id: None, session_id: Some("s-y") }, Some(&PriorRun { run_id: "01RUN-C" }));
        assert!(newer.may_mutate(), "{newer:?}");
    }

    /// 空字串不是身分：`Some("")` 要跟沒帶一樣。
    #[test]
    fn blank_identities_count_as_missing() {
        let now = gen("01RUN-B", Some("s-new"));
        assert_eq!(decide(&now, &EventIdentity { run_id: Some("  "), session_id: Some(" ") }, None), Ownership::Unproven("事件沒帶 session id"));
    }
}
