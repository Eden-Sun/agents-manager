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
//! **活過重啟**（issue #108 重開）：`app.quotas` 只在記憶體（SPEC §12.4），而開機的 `rearm_queue_retries` 在身分偵測
//! 之前就把排著的叫醒——只憑記憶體，重啟後第一個 flush 就把它送進同一個還沒額度的身分。所以跟交辦的 `resume_at`
//! 同一個做法：**等著的那一列自己帶著憑據**。
//! - 擋下的當下把那筆撞限（身分、桶、撞限時刻、到期、原因）寫進排著的這一則 `turns.quota_hold`；閘門放行就清掉。
//! - 那台主機的開機回填（[`backfill_once`]，掛在 `tools::install_host_tools`、跟 `controller::backfill_quota_limits_once`
//!   同一個點）把它原樣種回記憶體（`quota::restore_limit_hit`）。回填之後記憶體是唯一的判準，新讀數、換身分、換模型、
//!   成功回合照舊校正或清掉它。
//! - **上一輪開機**寫下的憑據，在這台主機回填之前（身分表還沒進來，key 算不準）flush 直接看它：身分還是同一個、
//!   撞的桶管得到現在的模型、還沒到期、之後沒有成功回合清過那把 key，就擋。這一輪自己寫的憑據記憶體本來就有，照舊只看記憶體。
//! - 撞限記下的當下（`turn_error::mark_claude_limit_hit` → [`stamp_queued`]）就蓋到這顆 bot 排著的每一則上，不等 flush。
//!
//! **讀不到就擋**（#108 重開）：讀不到主機、讀不到那一列的憑據，都不能當成「沒撞限」——照擋、短時間後再看
//! （[`unverified`]）。撞限本身記不進正確那把 key 時由 `turn_error` 記成欠著，閘門照欠著的那一筆擋。憑據寫不進去不算
//! 圍籬做完：記憶體照擋，很快再寫一次。回填讀不到就不算回填過，回填時**憑據解不開**的那顆 bot 也不算
//! （issue #522）——它的撞限沒被種回記憶體，「記憶體說不擋」對它不是證據，照舊問那一列自己的憑據。

use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 擋著的時候最多隔多久再看一次撞限。
pub(crate) const RECHECK_MAX: Duration = Duration::from_secs(300);

/// `turns.quota_hold` 的內容：擋下那一刻看到的撞限，加上當時的身分與寫下的時間。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct Held {
    identity: Option<String>,
    message: String,
    until: Option<String>,
    at: String,
    bucket: Option<String>,
    /// 最後一次確認還在擋的時刻：之後同一把 key 被成功回合清過撞限（`quota::limit_cleared_since`）就不再種回去。
    held_at: String,
    /// 哪一輪開機寫的（`App::boot_id`）。
    boot: String,
}

impl Held {
    fn new(app: &App, identity: Option<&str>, hit: &crate::quota::LimitHit) -> Self {
        Held {
            identity: identity.map(String::from),
            message: hit.message.clone(),
            until: hit.until.clone(),
            at: hit.at.clone(),
            bucket: hit.bucket.clone(),
            held_at: db::now(),
            boot: app.boot_id.clone(),
        }
    }

    fn hit(&self) -> crate::quota::LimitHit {
        crate::quota::LimitHit { message: self.message.clone(), until: self.until.clone(), at: self.at.clone(), bucket: self.bucket.clone() }
    }
}

/// 判斷擋不擋要的狀態讀不到時，多久之後再看一次。
pub(crate) const UNVERIFIED_RETRY: Duration = Duration::from_secs(10);

/// 憑據寫不進排著的那一列時，多久之後再寫一次（flush 重來一輪，閘門會再寫）。
pub(crate) const PERSIST_RETRY: Duration = Duration::from_secs(5);

/// 判斷不了（讀不到主機、讀不到那一列的憑據）：照「擋」算（#108 重開）——讀不到不等於沒撞限。`until` 寫
/// [`UNVERIFIED_RETRY`] 之後：flush 照它掛重看的 timer，保險絲（[`bounded`]）也看得出它有盡頭，不會當成「看不到盡頭
/// 的額度」撤掉派工。
fn unverified(bot: &db::Bot, e: &anyhow::Error) -> crate::quota::LimitHit {
    tracing::warn!(bot = %bot.id, error = %e, "cannot tell whether a queued prompt's identity has quota; holding it");
    crate::quota::LimitHit {
        message: format!("判斷額度要的狀態讀不到（{e:#}），先不送"),
        until: Some(db::iso_at(chrono::Utc::now() + chrono::Duration::from_std(UNVERIFIED_RETRY).unwrap_or_default())),
        at: db::now(),
        bucket: None,
    }
}

/// 排著的這一則要不要擋：回傳擋住它的撞限；`None` ＝ 可以送（順手清掉它身上的舊憑據）。
///
/// 判準跟派送前同一支 [`crate::quota::try_limit_hit_for_bot`]。記憶體說擋就把憑據記在這一列上；記憶體說不擋，
/// 而這一列帶著**上一輪開機**的憑據、這台主機的開機回填還沒跑完（重啟後記憶體是空的）時不算數，改看憑據。
/// 任何一步讀不到都照擋（[`unverified`]），不當作「沒撞限」。
pub(crate) async fn blocking_hit(app: &Arc<App>, bot: &db::Bot, turn_id: &str) -> Option<crate::quota::LimitHit> {
    match crate::quota::try_limit_hit_for_bot(app, bot).await {
        Ok(Some(hit)) => {
            // 這一輪靠記憶體擋著，憑據卻沒落地：圍籬還沒做完，很快再寫一次（`arm_queue_retry` 留較早的那個 timer）。
            // 寫成之前 daemon 重啟的話，只剩撞限當下蓋上的那一份（`stamp_queued`）。
            if let Err(e) = remember(app, turn_id, bot, &hit).await {
                tracing::warn!(turn = %turn_id, error = %e, "could not persist the quota hold of a queued prompt; retrying soon");
                super::schedule_flush_retry(app, &bot.id, PERSIST_RETRY);
            }
            return Some(hit);
        }
        Ok(None) => {}
        Err(e) => return Some(unverified(bot, &e)),
    }
    // 查完撞限、再讀一次主機之前（測試在這裡讓前一次讀不到的 DB 恢復）。
    #[cfg(test)]
    super::race_point::hit("quota_hold_after_lookup", &bot.id).await;
    let host = match db::bot_host(&app.db, &bot.id).await {
        Ok(h) => h,
        Err(e) => return Some(unverified(bot, &e)),
    };
    // 這台主機回填過了、而且這顆 bot 的憑據在回填時讀得懂（或本來就沒有）：記憶體就是唯一判準。
    // 回填時解不開的那幾顆是例外——它們的撞限沒有被種回記憶體，「記憶體說不擋」對它們不是證據（issue #522）。
    if !backfilled(app, &host) || unresolved(app, &bot.id) {
        match held_on_turn(app, bot, turn_id).await {
            Ok(Some(hit)) => return Some(hit),
            Ok(None) => {}
            Err(e) => return Some(unverified(bot, &e)),
        }
    }
    forget(app, turn_id).await;
    None
}

/// 這一列上一輪開機的憑據對這顆 bot **現在**還擋不擋：身分沒換、還沒到期、之後沒有成功回合清過那把 key、
/// 撞的桶管得到它在跑的模型。讀不到那一列、或憑據解不開，回錯（呼叫端照擋）。
async fn held_on_turn(app: &Arc<App>, bot: &db::Bot, turn_id: &str) -> anyhow::Result<Option<crate::quota::LimitHit>> {
    let raw: Option<Option<String>> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?").bind(turn_id).fetch_optional(&app.db).await?;
    let Some(raw) = raw.flatten() else { return Ok(None) };
    let held: Held = serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("the quota hold on turn {turn_id} is unreadable: {e}"))?;
    if held.boot == app.boot_id || !still_holds(app, bot, &held).await? {
        return Ok(None);
    }
    let hit = held.hit();
    let model = crate::quota::running_model(app, bot).await;
    Ok(crate::quota::limit_hit_blocks_model(&hit, model.as_deref()).then_some(hit))
}

/// 憑據還算數：身分沒換、還沒到期、寫下之後同一把 key 沒有被成功回合清過撞限。身分是這顆 bot 現在實際跑的那個
/// （[`crate::quota::billing_identity`]，issue #238）；讀不到回錯（呼叫端照擋）。
async fn still_holds(app: &Arc<App>, bot: &db::Bot, held: &Held) -> anyhow::Result<bool> {
    if held.identity != crate::quota::billing_identity(app, bot).await? || crate::quota::limit_hit_expired(Some(&held.hit())) {
        return Ok(false);
    }
    Ok(match chrono::DateTime::parse_from_rfc3339(&held.held_at) {
        Ok(t) => !crate::quota::limit_cleared_since(app, bot, t.with_timezone(&chrono::Utc)).await,
        Err(_) => true,
    })
}

async fn remember(app: &Arc<App>, turn_id: &str, bot: &db::Bot, hit: &crate::quota::LimitHit) -> anyhow::Result<()> {
    let identity = crate::quota::billing_identity(app, bot).await?;
    let json = serde_json::to_string(&Held::new(app, identity.as_deref(), hit))?;
    sqlx::query("UPDATE turns SET quota_hold=? WHERE id=? AND status='queued'").bind(&json).bind(turn_id).execute(&app.db).await?;
    Ok(())
}

/// 撞限剛記下（`turn_error::mark_claude_limit_hit`）：這顆 bot 現在排著的每一則當場帶上憑據（#108 重開），不等下一次
/// flush 擋下才寫——記下到擋下之間 daemon 死掉、或 flush 那一下寫不進去，重啟之後都還有憑據。寫不進去回錯：撞限那條路
/// 會把它記成欠著、`StopFailure` 由收件匣重試。
pub(crate) async fn stamp_queued(app: &Arc<App>, bot_id: &str, identity: Option<&str>, hit: &crate::quota::LimitHit) -> anyhow::Result<()> {
    let json = serde_json::to_string(&Held::new(app, identity, hit))?;
    sqlx::query(
        "UPDATE turns SET quota_hold=? WHERE status='queued'
            AND conversation_id IN (SELECT id FROM conversations WHERE bot_id=?)",
    )
    .bind(&json)
    .bind(bot_id)
    .execute(&app.db)
    .await?;
    Ok(())
}

/// 閘門放行：清掉那一列的憑據。清不掉照樣放行（判準已經說不擋），留下的舊憑據只會讓重啟之後多擋到它自己的判準
/// （[`still_holds`]）說不擋為止。
async fn forget(app: &Arc<App>, turn_id: &str) {
    if let Err(e) = sqlx::query("UPDATE turns SET quota_hold=NULL WHERE id=? AND quota_hold IS NOT NULL").bind(turn_id).execute(&app.db).await {
        tracing::warn!(turn = %turn_id, error = %e, "could not clear the quota hold of a released prompt");
    }
}

/// 每台主機在這一輪開機裡的回填：`false` 跑到一半、`true` 跑完了。鍵帶 `boot_id`：測試裡模擬重啟的新 `App`
/// 跟舊的共用資料目錄，也不會互相看到。
fn backfill_state() -> &'static Mutex<HashMap<String, bool>> {
    static M: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn backfill_key(app: &App, host: &str) -> String {
    format!("{}\u{0}{host}", app.boot_id)
}

/// 回填時憑據解不開的 bot（鍵同樣帶 `boot_id`）：對它們而言「這台回填完了」不成立，`blocking_hit` 照舊
/// 問那一列自己的憑據。只在這一輪開機內有效——下一次開機重新回填時會再判一次。
fn unresolved_holds() -> &'static Mutex<std::collections::HashSet<String>> {
    static M: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn mark_unresolved(app: &App, bot_id: &str) {
    if let Ok(mut m) = unresolved_holds().lock() {
        m.insert(backfill_key(app, bot_id));
    }
}

fn unresolved(app: &App, bot_id: &str) -> bool {
    unresolved_holds().lock().ok().is_some_and(|m| m.contains(&backfill_key(app, bot_id)))
}

fn backfilled(app: &App, host: &str) -> bool {
    backfill_state().lock().ok().and_then(|m| m.get(&backfill_key(app, host)).copied()).unwrap_or(false)
}

/// 開機回填（排著的 prompt 那一半）：`host` 的身分表剛寫好，把那台 bot 排著的 prompt 身上**上一輪開機**記下的
/// 撞限種回記憶體。**每台主機每一輪開機只跑一次**，跑完之後 flush 只看記憶體。身分已經換掉、到期、或擋下之後
/// 同一把 key 被成功回合清過的不種（[`still_holds`]）。種完叫醒那幾顆的 flush：已經不擋的馬上送，不必等重看的 timer。
///
/// 讀不到就**不算回填過**（#108 重開）：以前讀不到那幾列當作沒有、照樣標成跑完，flush 從此只看空的記憶體，重啟前記下的
/// 撞限就這樣放行。現在跑到一半讀不到就拿掉標記——flush 繼續看每一列自己的憑據（照擋），[`BACKFILL_RETRY`] 之後
/// （或那台主機下一次偵測完）再回填一次。已經種回去的留著（`restore_limit_hit` 只留較晚的，重來不會疊）。
pub(crate) async fn backfill_once(app: &Arc<App>, host: &str) {
    let key = backfill_key(app, host);
    {
        let Ok(mut m) = backfill_state().lock() else { return };
        if m.contains_key(&key) {
            return;
        }
        m.insert(key.clone(), false);
    }
    match backfill(app, host).await {
        Ok(woken) => {
            if let Ok(mut m) = backfill_state().lock() {
                m.insert(key, true);
            }
            for bot_id in woken {
                schedule_flush_queued(app, &bot_id);
            }
        }
        Err(e) => {
            tracing::warn!(host, error = %e, "the quota-hold backfill could not read what it needs; queued prompts keep their own evidence until it runs again");
            if let Ok(mut m) = backfill_state().lock() {
                m.remove(&key);
            }
            retry_backfill(app, host);
        }
    }
}

/// 回填讀不到之後多久再試。
const BACKFILL_RETRY: Duration = Duration::from_secs(30);

fn retry_backfill(app: &Arc<App>, host: &str) {
    if cfg!(test) {
        return;
    }
    let (app, host) = (app.clone(), host.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(BACKFILL_RETRY).await;
        backfill_once(&app, &host).await;
    });
}

/// 回填本身：回傳要叫醒 flush 的 bot。任何一步讀不到就回錯。
async fn backfill(app: &Arc<App>, host: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.bot_id, t.quota_hold FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'queued' AND t.quota_hold IS NOT NULL",
    )
    .fetch_all(&app.db)
    .await?;
    let mut woken = Vec::new();
    for (bot_id, raw) in rows {
        let Some(bot) = db::bot(&app.db, &bot_id).await? else { continue };
        if db::bot_host(&app.db, &bot.id).await? != host {
            continue;
        }
        // 解不開的憑據（不是讀不到，是內容壞了）重來也一樣：跳過它，不讓它擋住整台主機的回填。
        // 但**這顆 bot 不算回填過**（issue #522）：撞限沒被種回記憶體，之後 `blocking_hit` 若照「回填完了、
        // 只看記憶體」走，就會讀到一個空的記憶體、把唯一的證據 `forget` 掉，然後把那一則送進還沒額度的身分——
        // 正是這個模組要擋的事。記下來，讓它繼續走 `held_on_turn`（那裡對解不開是回錯、呼叫端照擋，方向一致）。
        // 也不叫醒它的 flush：叫醒只是提早去踩那條路。
        let held = match serde_json::from_str::<Held>(&raw) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(bot = %bot.id, error = %e, "a queued prompt carries an unreadable quota hold; it keeps holding that prompt");
                mark_unresolved(app, &bot.id);
                continue;
            }
        };
        woken.push(bot.id.clone());
        if held.boot == app.boot_id || !still_holds(app, &bot, &held).await? {
            continue;
        }
        // 種回撞到的那個帳號（憑據上記的身分；`still_holds` 已經確認它就是這顆 bot 現在實際跑的，issue #238）。
        let base = crate::quota::quota_base_for_host(app, host, &bot.kind, held.identity.as_deref()).await;
        if crate::quota::restore_limit_hit(app, host, &base, held.hit()).await {
            tracing::info!(host, bot = %bot.id, until = ?held.until, "重啟回填：排著的 prompt 記下的撞限補回記憶體");
        }
    }
    Ok(woken)
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

    /// shell 的 cc0（沒有自己的 `CLAUDE_CONFIG_DIR`＝預設帳號）：撞限的 key 是裸 `claude`，身分偵測之前卻算成 `claude:cc0`。
    fn shell_cc0() -> crate::tools::HostTools {
        crate::tools::HostTools {
            tools: Default::default(),
            identities: Default::default(),
            shell_identities: vec![crate::config::IdentityCfg { name: "cc0".into(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] }],
            utc_offset_secs: None, herdr_cli: None, checked_at: db::now(),
        }
    }

    struct Queued {
        env: tt::Env,
        bot: db::Bot,
        turn: String,
    }

    /// 一顆 claude bot（身分 `identity`）、一個 run、一則排著的 prompt。
    async fn queued_on(identity: Option<&str>, name: &str) -> Queued {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(identity).bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        forget_queue_retry_timer(&bot.id);
        forget_held(&bot.id);
        Queued { env, bot, turn }
    }

    async fn flush(app: &Arc<App>, bot_id: &str) {
        forget_queue_retry_timer(bot_id);
        flush_queued_locked(app, bot_id).await.unwrap();
    }

    /// `(還排著而且沒花重試＝被額度擋, 身上的憑據)`。過了閘門的在測試裡沒有真的 pane，會被放回並花一次重試。
    async fn state(app: &Arc<App>, turn: &str) -> (bool, Option<Held>) {
        let (status, retries, raw): (String, i64, Option<String>) =
            sqlx::query_as("SELECT status, flush_retries, quota_hold FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap();
        (status == "queued" && retries == 0, raw.map(|r| serde_json::from_str(&r).unwrap()))
    }

    fn later(hours: i64) -> String {
        db::iso_at(chrono::Utc::now() + chrono::Duration::hours(hours))
    }

    /// #238：run 以 cc1 起來、設定已改成 cc2（還沒重啟）：排著的那一則會送進 cc1 的行程，照 cc1 的撞限擋，憑據也記 cc1——
    /// 記成 cc2 的話，重啟後回填拿它跟 run 的 cc1 比對不上，重啟前記下的撞限就被放掉。cc2 撞限不擋它（它還不在 cc2 上）。
    #[tokio::test]
    async fn a_queued_prompt_is_held_on_the_identity_its_run_started_with() {
        for (hit_on, held) in [("claude:cc1", true), ("claude:cc2", false)] {
            let q = queued_on(Some("cc2"), "switched").await;
            let app = q.env.app.clone();
            for name in ["cc1", "cc2"] {
                let dir = q.env.dir.join(format!("claude-{name}")).to_string_lossy().into_owned();
                app.cfg
                    .update(move |c| {
                        c.identities.push(crate::config::IdentityCfg {
                            name: name.into(),
                            kind: "claude".into(),
                            host: None,
                            env: [("CLAUDE_CONFIG_DIR".to_string(), dir)].into(),
                            args: vec![],
                        });
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            sqlx::query("UPDATE runs SET runtime_identity='cc1' WHERE bot_id=?").bind(&q.bot.id).execute(&app.db).await.unwrap();
            assert!(crate::quota::seed_limit_hit(&app, crate::config::LOCAL_HOST, hit_on, &later(2), "You've hit your session limit", Some("five_hour".into())).await);
            flush(&app, &q.bot.id).await;
            let (was_held, hold) = state(&app, &q.turn).await;
            assert_eq!(was_held, held, "{hit_on} 撞限：擋不擋");
            if held {
                assert_eq!(hold.and_then(|h| h.identity).as_deref(), Some("cc1"), "憑據記的是 run 的身分");
            }
        }
    }

    /// 重開留言那條序列，而且 key 在偵測前後不一樣：cc0 撞限（裸 `claude`）→ 排著的被擋、憑據寫在那一列 → 重啟
    /// → 偵測前開機叫醒的 flush 照樣擋 → 偵測完回填到裸 `claude`（不是沒人讀的 `claude:cc0`），帶著原本的撞限時刻與桶名
    /// → 記憶體接手：撞限被成功回合清掉就放行，憑據也清掉。
    #[tokio::test]
    async fn a_hold_from_the_previous_boot_holds_until_the_backfill_then_memory_takes_over() {
        let q = queued_on(Some("cc0"), "cc0-restart").await;
        let old = q.env.app.clone();
        crate::tools::install_host_tools(&old, LOCAL_HOST, shell_cc0()).await;
        let until = later(2);
        assert!(crate::quota::seed_limit_hit(&old, LOCAL_HOST, "claude", &until, "You've hit your session limit", Some("five_hour".into())).await);
        let hit_at = crate::quota::limit_hit_for_bot(&old, &q.bot).await.unwrap().at;
        flush(&old, &q.bot.id).await;
        let (blocked, held) = state(&old, &q.turn).await;
        let held = held.expect("擋下的當下寫進那一列");
        assert!(blocked);
        assert_eq!((held.identity.as_deref(), held.until.as_deref(), held.at.as_str(), held.bucket.as_deref()), (Some("cc0"), Some(until.as_str()), hit_at.as_str(), Some("five_hour")));

        forget_held(&q.bot.id);
        let app = tt::restart_app(&q.env).await;
        assert!(crate::quota::limit_hit_for_bot(&app, &q.bot).await.is_none(), "新行程：記憶體是空的");
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0, "身分偵測之前：看那一列的憑據，照樣擋");
        assert!(recently_held(&q.bot.id), "保險絲看得到正在擋");
        assert!(!q.env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");

        crate::tools::install_host_tools(&app, LOCAL_HOST, shell_cc0()).await;
        {
            let quotas = app.quotas.lock().await;
            let restored = quotas.get("claude").and_then(|x| x.limit_hit.clone()).expect("回填到裸 claude");
            assert_eq!((restored.until.as_deref(), restored.at.as_str(), restored.bucket.as_deref()), (Some(until.as_str()), hit_at.as_str(), Some("five_hour")), "原樣種回");
            assert!(quotas.get("claude:cc0").is_none(), "不能生出一格沒人讀的 claude:cc0");
        }
        assert!(crate::quota::limit_hit_for_bot(&app, &q.bot).await.is_some(), "派送前也看得到");
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0, "回填之後照樣擋");

        crate::quota::clear_limit_hit(&app, LOCAL_HOST, "claude").await;
        flush(&app, &q.bot.id).await;
        let (blocked, held) = state(&app, &q.turn).await;
        assert!(!blocked && held.is_none(), "記憶體說不擋：放行，憑據清掉");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    async fn put_hold(app: &Arc<App>, turn: &str, held: &Held) {
        sqlx::query("UPDATE turns SET quota_hold=? WHERE id=?").bind(serde_json::to_string(held).unwrap()).bind(turn).execute(&app.db).await.unwrap();
    }

    fn old_hold(identity: Option<&str>, until: Option<String>, bucket: Option<&str>) -> Held {
        Held {
            identity: identity.map(String::from),
            message: "You've hit your limit".into(),
            until,
            at: db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(10)),
            bucket: bucket.map(String::from),
            held_at: db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(5)),
            boot: "previous-boot".into(),
        }
    }

    /// 上一輪的憑據在回填之前也照「現在」判：換了身分、換了模型、到期、之後同一把 key 有成功回合，都放行。
    /// 這一輪自己寫的憑據記憶體本來就有，記憶體說不擋就不擋。沒寫時間的（codex credits）照樣擋。
    #[tokio::test]
    async fn a_previous_boot_hold_does_not_outlive_a_switch_an_expiry_or_a_successful_turn() {
        let q = queued_on(Some("cc-a"), "old-hold").await;
        let app = q.env.app.clone();
        let cases: Vec<(&str, Held, Option<&str>, bool)> = vec![
            ("同一個身分、還沒到期", old_hold(Some("cc-a"), Some(later(2)), Some("five_hour")), None, true),
            ("沒寫時間（codex credits）", old_hold(Some("cc-a"), None, None), None, true),
            ("身分換掉了", old_hold(Some("cc-b"), Some(later(2)), None), None, false),
            ("到期了", old_hold(Some("cc-a"), Some(db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(1))), None), None, false),
            ("撞的是 Fable 桶、現在跑 opus", old_hold(Some("cc-a"), Some(later(2)), Some("fable")), Some("opus"), false),
            ("撞的是 Fable 桶、還在跑 fable", old_hold(Some("cc-a"), Some(later(2)), Some("fable")), Some("fable"), true),
            ("這一輪自己寫的：記憶體說了算", Held { boot: app.boot_id.clone(), ..old_hold(Some("cc-a"), Some(later(2)), None) }, None, false),
        ];
        for (why, held, model, want) in cases {
            sqlx::query("UPDATE turns SET status='queued', run_id=NULL, flush_retries=0, next_flush_at=NULL WHERE id=?").bind(&q.turn).execute(&app.db).await.unwrap();
            sqlx::query("UPDATE bots SET model=? WHERE id=?").bind(model).bind(&q.bot.id).execute(&app.db).await.unwrap();
            put_hold(&app, &q.turn, &held).await;
            flush(&app, &q.bot.id).await;
            let (blocked, left) = state(&app, &q.turn).await;
            assert_eq!(blocked, want, "{why}");
            assert_eq!(left.is_some(), want, "{why}：放行就清掉憑據");
        }

        // 寫下之後同一把 key 有一回合真的答完（`clear_limit_hit` 記下的時刻）：額度回來的直接證據。
        sqlx::query("UPDATE turns SET status='queued', run_id=NULL, flush_retries=0, next_flush_at=NULL WHERE id=?").bind(&q.turn).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET model=NULL WHERE id=?").bind(&q.bot.id).execute(&app.db).await.unwrap();
        put_hold(&app, &q.turn, &old_hold(Some("cc-a"), Some(later(2)), None)).await;
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0);
        let bot = db::bot(&app.db, &q.bot.id).await.unwrap().unwrap();
        crate::quota::clear_limit_hit_for_bot(&app, &bot).await;
        flush(&app, &q.bot.id).await;
        assert!(!state(&app, &q.turn).await.0, "成功回合清過那把 key：放行");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    /// 回填每台主機每一輪開機只跑一次，跑完之後記憶體說了算；換了身分、之後有成功回合清過的不種；
    /// 沒寫時間的原樣黏著種回去。
    #[tokio::test]
    async fn the_backfill_runs_once_per_boot_and_skips_what_no_longer_holds() {
        let a = queued_on(Some("cc-a"), "bf-a").await;
        let app = a.env.app.clone();
        let add = |name: &'static str, identity: &'static str| {
            let app = app.clone();
            let pid = a.env.project_id.clone();
            async move {
                let bot = tt::claude_bot(&app, &pid, name).await;
                sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(identity).bind(&bot.id).execute(&app.db).await.unwrap();
                let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
                let turn = db::ulid();
                sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
                    .bind(&turn).bind(&conv).bind(db::now()).execute(&app.db).await.unwrap();
                (db::bot(&app.db, &bot.id).await.unwrap().unwrap(), turn)
            }
        };
        let (switched, switched_turn) = add("bf-switched", "cc-c").await;
        let (cleared, cleared_turn) = add("bf-cleared", "cc-d").await;
        put_hold(&app, &a.turn, &old_hold(Some("cc-a"), None, None)).await;
        put_hold(&app, &switched_turn, &old_hold(Some("cc-b"), Some(later(2)), None)).await;
        put_hold(&app, &cleared_turn, &old_hold(Some("cc-d"), Some(later(2)), None)).await;
        crate::quota::clear_limit_hit_for_bot(&app, &cleared).await;

        backfill_once(&app, LOCAL_HOST).await;
        let hit = crate::quota::limit_hit_for_bot(&app, &a.bot).await.expect("種回來了");
        assert_eq!(hit.until, None, "沒寫時間的照樣黏著");
        assert!(crate::quota::limit_hit_for_bot(&app, &switched).await.is_none(), "身分換掉了：不種");
        assert!(crate::quota::limit_hit_for_bot(&app, &cleared).await.is_none(), "之後有成功回合清過：不種");

        crate::quota::clear_limit_hit_for_bot(&app, &a.bot).await;
        backfill_once(&app, LOCAL_HOST).await;
        assert!(crate::quota::limit_hit_for_bot(&app, &a.bot).await.is_none(), "同一輪開機只回填一次，不把清掉的種回去");
        flush(&app, &a.bot.id).await;
        assert!(!state(&app, &a.turn).await.0, "回填跑完之後記憶體說了算");
        forget_queue_retry_timer(&a.bot.id);
        forget_held(&a.bot.id);
    }

    async fn rename(app: &Arc<App>, from: &str, to: &str) {
        sqlx::query(&format!("ALTER TABLE {from} RENAME TO {to}")).execute(&app.db).await.unwrap();
    }

    /// #108 重開：判斷擋不擋要的狀態讀不到，照擋、短時間後再看——不是「沒撞限」。
    /// (a) 讀不到主機（以前退回 `local`：查錯 key、回沒撞限、放行）；(b) 回填之前讀不到那一列自己帶的上一輪憑據
    /// （以前 `.ok()?` 當作沒有憑據、放行）。擋的時候有盡頭，保險絲不會把它當成「看不到盡頭的額度」撤掉。
    #[tokio::test]
    async fn a_queued_prompt_is_held_when_its_quota_state_cannot_be_read() {
        let q = queued_on(Some("cc-a"), "unreadable").await;
        let app = q.env.app.clone();
        put_hold(&app, &q.turn, &old_hold(Some("cc-a"), Some(later(2)), Some("five_hour"))).await;

        rename(&app, "projects", "projects_unreadable").await;
        let hit = blocking_hit(&app, &q.bot, &q.turn).await.expect("讀不到主機：照擋");
        assert!(bounded(&hit, chrono::Utc::now(), chrono::Duration::seconds(60)), "有盡頭：{hit:?}");
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0, "不 claim、不花重試");
        assert!(queue_retry_timer_left(&q.bot.id).is_some_and(|d| d <= UNVERIFIED_RETRY), "很快再看一次");
        assert!(!q.env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
        rename(&app, "projects_unreadable", "projects").await;

        // 回填之前、那一列讀不到。
        rename(&app, "turns", "turns_unreadable").await;
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "讀不到那一列的憑據：照擋");
        rename(&app, "turns_unreadable", "turns").await;
        assert!(state(&app, &q.turn).await.1.is_some(), "憑據沒被當成放行清掉");

        // 憑據壞了（解不開）：回填之前照擋，**回填之後也照擋**（issue #522）。這裡以前斷言的是
        // 「回填跳過它之後只看記憶體」——而那正是放行：解不開的憑據沒被種回記憶體，空的記憶體不是
        // 「沒撞限」的證據，照那條走會把唯一的證據 `forget` 掉、再把那一則送進還沒額度的身分。
        // 跟上面一格（那一列讀不到 → 照擋）也自相矛盾：同樣是「證明不了」，不該一個擋一個放。
        sqlx::query("UPDATE turns SET quota_hold='not json' WHERE id=?").bind(&q.turn).execute(&app.db).await.unwrap();
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "解不開：回填之前照擋");
        backfill_once(&app, LOCAL_HOST).await;
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "解不開：回填之後照樣擋（#522）");
        // 這裡不能用 `state`（它會把憑據 parse 回 `Held`，而這一格正是故意壞的）：直接看那一欄還在不在。
        let raw: Option<String> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?").bind(&q.turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(raw.as_deref(), Some("not json"), "唯一的證據不准被 forget 掉");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    /// 查撞限那一下讀不到主機、下一步又讀得到了（DB 剛恢復）：前一步的判斷根本沒做成，不能因為那一列上沒有憑據就放行——
    /// 記憶體裡明明有這個身分的撞限。
    #[tokio::test]
    async fn a_lookup_that_failed_is_not_mistaken_for_no_limit_when_the_db_recovers_mid_way() {
        let q = queued_on(Some("cc-a"), "recovered").await;
        let app = q.env.app.clone();
        assert!(crate::quota::seed_limit_hit(&app, LOCAL_HOST, "claude:cc-a", &later(2), "You've hit your session limit", Some("five_hour".into())).await);
        rename(&app, "projects", "projects_unreadable").await;
        let app2 = app.clone();
        super::super::race_point::arm("quota_hold_after_lookup", &q.bot.id, move || async move {
            rename(&app2, "projects_unreadable", "projects").await;
        });
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "照擋");
        assert!(blocking_hit(&app, &q.bot, &q.turn).await.is_some(), "恢復之後：記憶體的撞限擋");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    /// 憑據寫不進排著的那一列：這一輪記憶體照擋，但不算圍籬做完了——很快再寫一次（不是等五分鐘的重看），寫成為止。
    #[tokio::test]
    async fn a_hold_that_cannot_be_persisted_is_written_again_soon() {
        let q = queued_on(None, "unpersisted").await;
        let app = q.env.app.clone();
        assert!(crate::quota::seed_limit_hit(&app, LOCAL_HOST, "claude", &later(2), "You've hit your session limit", Some("five_hour".into())).await);
        sqlx::query("CREATE TRIGGER refuse_quota_hold BEFORE UPDATE OF quota_hold ON turns BEGIN SELECT RAISE(ABORT, 'injected'); END")
            .execute(&app.db)
            .await
            .unwrap();
        flush(&app, &q.bot.id).await;
        let (blocked, held) = state(&app, &q.turn).await;
        assert!(blocked && held.is_none(), "照擋，憑據還沒落地");
        assert!(queue_retry_timer_left(&q.bot.id).is_some_and(|d| d <= PERSIST_RETRY), "很快再寫一次：{:?}", queue_retry_timer_left(&q.bot.id));

        sqlx::query("DROP TRIGGER refuse_quota_hold").execute(&app.db).await.unwrap();
        flush(&app, &q.bot.id).await;
        let (blocked, held) = state(&app, &q.turn).await;
        assert!(blocked && held.is_some(), "寫成了");
        assert!(queue_retry_timer_left(&q.bot.id).is_some_and(|d| d > PERSIST_RETRY), "寫成之後回到一般的重看節奏");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }

    /// 回填讀不到就不算回填過：以前讀不到那幾列當作沒有、照樣標成跑完，flush 從此只看空的記憶體——上一輪記下的撞限放行。
    #[tokio::test]
    async fn a_backfill_that_cannot_read_is_not_counted_as_done() {
        let q = queued_on(Some("cc-a"), "bf-unreadable").await;
        let app = q.env.app.clone();
        put_hold(&app, &q.turn, &old_hold(Some("cc-a"), Some(later(2)), None)).await;

        rename(&app, "conversations", "conversations_unreadable").await;
        backfill_once(&app, LOCAL_HOST).await;
        rename(&app, "conversations_unreadable", "conversations").await;
        assert!(!backfilled(&app, LOCAL_HOST), "沒讀到：不算回填過");
        assert!(crate::quota::limit_hit_for_bot(&app, &q.bot).await.is_none());
        flush(&app, &q.bot.id).await;
        assert!(state(&app, &q.turn).await.0, "照那一列自己的憑據擋");

        backfill_once(&app, LOCAL_HOST).await;
        assert!(backfilled(&app, LOCAL_HOST), "讀得到了：再跑一次就跑完");
        assert!(crate::quota::limit_hit_for_bot(&app, &q.bot).await.is_some(), "種回記憶體");
        forget_queue_retry_timer(&q.bot.id);
        forget_held(&q.bot.id);
    }
}
