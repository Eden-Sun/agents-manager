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
        // `prior` 只有在呼叫端（`classify`）已經用 rowid（寫入順序）驗過「這確實是更早寫進去的
        // run」才會是 `Some`——這裡不再用 ULID 字典序複查一次（issue #98：同一毫秒內的隨機段不保證
        // 單調，字典序複查反而會把 rowid 已經證出來的答案推翻）。
        Some(p) => Ownership::Stale { prior_run_id: p.run_id.to_string(), why: "這個 session 屬於這顆 bot 更早一代的 run" },
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
    // `rowid`（寫入順序）決定「更早」，不是 `id`（ULID）：兩個 run 若落在同一毫秒，ULID 的隨機段
    // 不保證單調，字典序會偶爾把先寫進去的那個排到後面（issue #98）。子查詢先問現在這代自己的
    // rowid，再只挑 rowid 比它小的——「比自己早」由 SQL 保證，不必回 Rust 再複查一次。
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
    }

    /// `decide` 完全信任 `prior`：「這是不是真的更早」是呼叫端（`classify`，用 rowid 驗過）的責任，
    /// 不是這支純函式的事（issue #98：以前這裡還會用 ULID 字典序複查一次，同一毫秒內會複查出錯的答案）。
    #[test]
    fn decide_trusts_prior_completely_it_is_the_callers_job_to_verify_it() {
        let now = gen("01RUN-B", Some("s-first"));
        // 即使這個 run_id 字典序排在「現在這代」後面，`decide` 也不會自作主張推翻呼叫端的判斷。
        let out = decide(&now, &EventIdentity { run_id: None, session_id: Some("s-y") }, Some(&PriorRun { run_id: "01RUN-Z" }));
        assert!(matches!(&out, Ownership::Stale { prior_run_id, .. } if prior_run_id == "01RUN-Z"), "{out:?}");
    }

    /// 空字串不是身分：`Some("")` 要跟沒帶一樣。
    #[test]
    fn blank_identities_count_as_missing() {
        let now = gen("01RUN-B", Some("s-new"));
        assert_eq!(decide(&now, &EventIdentity { run_id: Some("  "), session_id: Some(" ") }, None), Ownership::Unproven("事件沒帶 session id"));
    }

    /// issue #98：`classify` 找上一代 run 要看 rowid（寫入順序），不能看 ULID 字典序——這裡故意讓
    /// **先寫進去**的那個 run 的 ULID 字典序反而比較大（模擬同一毫秒內隨機段沒有照時間排的情況），
    /// 證明圍籬還是正確認得出它是舊世代，沒有被字典序騙過去。
    #[tokio::test]
    async fn classify_finds_the_prior_run_by_insertion_order_even_when_its_ulid_sorts_after_the_current_one() {
        use crate::testing as tt;
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'fence-rowid','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        // 故意反過來：先寫進去的（真正的上一代）用一個字典序**比較大**的假 ULID。
        let old_run_id = "01ZZZZZZZZZZZZZZZZZZZZZZZZ";
        let new_run_id = "01AAAAAAAAAAAAAAAAAAAAAAAA";
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at, ended_at)
             VALUES (?,?,'exited','unknown','pane-old','s-old',?,?)",
        )
        .bind(old_run_id)
        .bind(&bot_id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, started_at)
             VALUES (?,?,'running','working','pane-new',?)",
        )
        .bind(new_run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let new_run = sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id = ?").bind(new_run_id).fetch_one(&app.db).await.unwrap();

        let ev = EventIdentity { run_id: None, session_id: Some("s-old") };
        let out = classify(&app.db, &bot_id, &new_run, ev).await;
        assert!(
            matches!(&out, Ownership::Stale { prior_run_id, .. } if prior_run_id == old_run_id),
            "寫入順序證得出 old_run 是上一代，不該被 ULID 字典序騙成 Unproven：{out:?}"
        );
    }
}
