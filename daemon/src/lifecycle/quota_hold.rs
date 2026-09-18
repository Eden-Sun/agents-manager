//! 排著的 prompt 送出之前，目標身分要有額度（issue #108）。
//!
//! 撞額度的那一回合被 `StopFailure` 收成失敗之後，回合結束的事件照例叫醒 queue flush——排在後面的派工
//! 就被立刻送進**同一個還沒額度的身分**，再撞一次，派工白白燒掉。派送前（`supervisor::controller::dispatch`）
//! 本來就用 [`crate::quota::limit_hit_for_bot`] 擋，已經排進佇列的這一則卻沒人問。
//!
//! 規則只住在這裡：
//! - **判準**跟派送前同一支 `limit_hit_for_bot`：看這顆 bot **現在**的身分那把 key、撞的那一桶管不管得到
//!   它正在跑的模型。所以換身分（`bots.identity` 已經是 B）、換模型（撞的是 Fable 桶、改跑 opus）、撞限
//!   到期或被新讀數校正掉，都會讓它放行。
//! - **擋的時候**（`queue::flush_queued_locked`）：留在佇列、不 claim、不花重試額度，掛 timer 到撞限到期，
//!   最多 [`RECHECK_MAX`] 就再看一次（新讀數可能提早解除）。換身分重啟會叫醒 flush（#106）。
//! - **只送一次**：始終只有排著的那一則，放行之後走 flush 原本的 CAS claim。
//! - **不會永遠卡住**：supervisor 的排隊保險絲（`block_stale_queues`）對「正在被擋、而且撞限在它自己的
//!   等待上限內會到期」的不撤（[`bounded`]）；flush 剛放掉擋、下一次重看還沒到的那一小段也不撤
//!   （[`recently_held`]）。撞限沒寫時間（codex credits 用完）或遠超過上限，保險絲照舊撤，理由寫額度。
//!
//! 已知限制：`app.quotas` 只在記憶體（SPEC §12.4），daemon 重啟後這裡跟派送前一樣看不到撞限。

use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 擋著的時候最多隔多久再看一次撞限。
pub(crate) const RECHECK_MAX: Duration = Duration::from_secs(300);

/// 這顆 bot 現在的身分／模型還收不下工作：回傳擋住它的撞限；`None` ＝ 可以送。
pub(crate) async fn blocking_hit(app: &Arc<App>, bot: &db::Bot) -> Option<crate::quota::LimitHit> {
    crate::quota::limit_hit_for_bot(app, bot).await
}

/// 擋著的時候多久之後再看：撞限到期那一刻，但最多 [`RECHECK_MAX`]；沒寫時間的就照 `RECHECK_MAX`。
/// 讀不懂的時間照「沒寫」算：`limit_hit_expired` 也不讓它過期，每秒重看一次只是空轉。
pub(crate) fn recheck_in(until: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> Duration {
    let Some(until) = until.and_then(|u| chrono::DateTime::parse_from_rfc3339(u).ok()) else { return RECHECK_MAX };
    match (until.with_timezone(&chrono::Utc) - now).to_std() {
        Ok(left) => left.clamp(Duration::from_secs(1), RECHECK_MAX),
        Err(_) => Duration::from_secs(1), // 已經過了：馬上再看
    }
}

/// 保險絲用：這個擋有沒有看得到的盡頭——撞限寫了到期時間，而且在 `max_wait` 內。
pub(crate) fn bounded(hit: &crate::quota::LimitHit, now: chrono::DateTime<chrono::Utc>, max_wait: chrono::Duration) -> bool {
    hit.until
        .as_deref()
        .and_then(|u| chrono::DateTime::parse_from_rfc3339(u).ok())
        .is_some_and(|u| u.with_timezone(&chrono::Utc) <= now + max_wait)
}

fn held_at() -> &'static Mutex<HashMap<String, std::time::Instant>> {
    static M: OnceLock<Mutex<HashMap<String, std::time::Instant>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// flush 這一次因為額度把這顆 bot 的佇列擋下來了。
pub(crate) fn note_held(bot_id: &str) {
    if let Ok(mut m) = held_at().lock() {
        m.insert(bot_id.to_string(), std::time::Instant::now());
    }
}

/// 最近一次重看的節奏內 flush 還在擋這顆：撞限剛被校正掉或剛到期，下一次重看（最多 [`RECHECK_MAX`] 後）
/// 就會送。保險絲在這段時間撤掉它，等於把馬上要送的派工丟掉。只放記憶體：重啟後當作沒擋過。
pub(crate) fn recently_held(bot_id: &str) -> bool {
    held_at()
        .lock()
        .ok()
        .and_then(|m| m.get(bot_id).copied())
        .is_some_and(|t| t.elapsed() <= RECHECK_MAX + Duration::from_secs(60))
}

#[cfg(test)]
pub(crate) fn forget_held(bot_id: &str) {
    if let Ok(mut m) = held_at().lock() {
        m.remove(bot_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    fn at(iso: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&chrono::Utc)
    }

    fn hit(until: Option<&str>) -> crate::quota::LimitHit {
        crate::quota::LimitHit { message: "You've hit your session limit".into(), until: until.map(String::from), at: db::now(), bucket: Some("five_hour".into()) }
    }

    #[test]
    fn the_recheck_follows_the_limit_but_never_sleeps_past_five_minutes() {
        let now = at("2026-09-18T10:00:00Z");
        let s = Duration::from_secs;
        assert_eq!(recheck_in(Some("2026-09-18T10:00:40Z"), now), s(40), "快到期：到期那一刻");
        assert_eq!(recheck_in(Some("2026-09-18T15:00:00Z"), now), RECHECK_MAX, "還很久：最多五分鐘再看（新讀數可能提早解除）");
        assert_eq!(recheck_in(None, now), RECHECK_MAX, "沒寫時間");
        assert_eq!(recheck_in(Some("2026-09-18T09:00:00Z"), now), s(1), "已經過了：馬上再看");
        assert_eq!(recheck_in(Some("not a time"), now), RECHECK_MAX, "讀不懂：不空轉");
    }

    #[test]
    fn only_a_limit_that_ends_inside_the_wait_budget_is_bounded() {
        let now = at("2026-09-18T10:00:00Z");
        let six = chrono::Duration::hours(6);
        assert!(bounded(&hit(Some("2026-09-18T14:59:00Z")), now, six), "5 小時窗");
        assert!(!bounded(&hit(Some("2026-09-25T10:00:00Z")), now, six), "週窗：遠超過上限");
        assert!(!bounded(&hit(None), now, six), "沒寫時間（codex credits）");
    }

    /// 真的走 flush：目標身分撞限還在 → 不 claim、不花重試、一個字都不打、掛 timer；撞限解除 → 照常送。
    #[tokio::test]
    async fn a_queued_prompt_waits_while_its_identity_has_no_quota() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "no-quota").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let until = db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2));
        assert!(crate::quota::seed_limit_hit(&app, LOCAL_HOST, "claude", &until, "You've hit your session limit", Some("five_hour".into())).await);
        forget_queue_retry_timer(&bot.id);
        forget_held(&bot.id);

        flush_queued_locked(&app, &bot.id).await.unwrap();
        let t: (String, i64, Option<String>) = sqlx::query_as("SELECT status, flush_retries, run_id FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(t, ("queued".to_string(), 0, None), "沒額度：不 claim、不花重試");
        assert!(!env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
        assert_eq!(queue_retry_timer_left(&bot.id).map(|d| d <= RECHECK_MAX), Some(true), "掛了 timer，最多五分鐘再看");
        assert!(recently_held(&bot.id));

        crate::quota::clear_limit_hit(&app, LOCAL_HOST, "claude").await;
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        let t: (String, i64) = sqlx::query_as("SELECT status, flush_retries FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert!(t.0 != "queued" || t.1 > 0, "額度回來：往下送（這裡沒有真的 pane，會被放回並花一次重試）：{t:?}");
        let _ = run;
        forget_queue_retry_timer(&bot.id);
        forget_held(&bot.id);
    }
}
