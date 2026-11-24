//! 排著的 prompt 送出之前，目標身份要有額度（issue #108）。
//!
//! 撞額度的那一回合被 `StopFailure` 收成失敗之後，回合結束的事件照例叫醒 queue flush——排在後面的派工
//! 就被立刻送進**同一個還沒額度的身份**，再撞一次，派工白白燒掉。派送前（`supervisor::controller::dispatch`）
//! 本來就用 [`crate::quota::limit_hit_for_bot`] 擋，已經排進佇列的這一則卻沒人問。
//!
//! 規則只住在這裡：
//! - **判準**跟派送前同一支 `limit_hit_for_bot`：看這顆 bot **現在**的身份那把 key、撞的那一桶管不管得到
//!   它正在跑的模型。所以換身份（`bots.identity` 已經是 B）、換模型（撞的是 Fable 桶、改跑 opus）、撞限
//!   到期或被新讀數校正掉，都會讓它放行。
//! - **擋的時候**（`queue::flush_queued_locked`）：留在佇列、不 claim、不花重試額度，掛 timer 到撞限到期，
//!   最多 [`RECHECK_MAX`] 就再看一次（新讀數可能提早解除）。換身份重啟會叫醒 flush（#106）。
//! - **只送一次**：始終只有排著的那一則，放行之後走 flush 原本的 CAS claim。
//! - **不會永遠卡住**：supervisor 的排隊保險絲（`block_stale_queues`）對「正在被擋、而且撞限在它自己的
//!   等待上限內會到期」的不撤（[`bounded`]）；flush 剛放掉擋、下一次重看還沒到的那一小段也不撤
//!   （[`recently_held`]）。撞限沒寫時間（codex credits 用完）或遠超過上限，保險絲照舊撤，理由寫額度。
//!
//! **活過重啟**（issue #108 重開）：`app.quotas` 只在記憶體（SPEC §12.4），而開機的 `rearm_queue_retries` 在身份偵測
//! 之前就把排著的叫醒——只憑記憶體，重啟後第一個 flush 就把它送進同一個還沒額度的身份。所以跟交辦的 `resume_at`
//! 同一個做法：**等著的那一列自己帶著憑據**。
//! - 擋下的當下把那筆撞限（身份、桶、撞限時刻、到期、原因）寫進排著的這一則 `turns.quota_hold`；閘門放行就清掉。
//! - 那台主機的開機回填（[`backfill_once`]，掛在 `tools::install_host_tools`、跟 `controller::backfill_quota_limits_once`
//!   同一個點）把它原樣種回記憶體（`quota::restore_limit_hit`）。回填之後記憶體是唯一的判準，新讀數、換身份、換模型、
//!   成功回合照舊校正或清掉它。
//! - **上一輪開機**寫下的憑據，在這台主機回填之前（身份表還沒進來，key 算不準）flush 直接看它：身份還是同一個、
//!   撞的桶管得到現在的模型、還沒到期、之後沒有成功回合清過那把 key，就擋。這一輪自己寫的憑據記憶體本來就有，照舊只看記憶體。
//! - 撞限記下的當下（`turn_error::mark_claude_limit_hit` → [`stamp_queued`]）就蓋到這顆 bot 排著的每一則上，不等 flush。
//!
//! **讀不到就擋**（#108 重開）：讀不到主機、讀不到那一列的憑據，都不能當成「沒撞限」——照擋、短時間後再看
//! （[`unverified`]）。撞限本身記不進正確那把 key 時由 `turn_error` 記成欠著，閘門照欠著的那一筆擋。憑據寫不進去不算
//! 圍籬做完：記憶體照擋，很快再寫一次。回填讀不到就不算回填過，回填時**憑據解不開**的那顆 bot 也不算
//! （issue #522）——它的撞限沒被種回記憶體，「記憶體說不擋」對它不是證據，照舊問那一列自己的憑據。

use crate::capabilities::{BootId, Db};
use crate::lifecycle::s6_ports::QuotaHoldContext;
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 擋著的時候最多隔多久再看一次撞限。
pub const RECHECK_MAX: Duration = Duration::from_secs(300);

/// `turns.quota_hold` 的內容：擋下那一刻看到的撞限，加上當時的身份與寫下的時間。
test_visible_struct! {
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
    /// 寫下時這顆 bot 的主機名指到哪台機器（[`host_target`]，#347）。同名主機之後改指到另一台，這筆撞限說的是舊機器的帳號，
    /// 不能再擋新機器上的派工（[`still_holds`]）。舊版寫的沒有這一欄：照舊算數。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
}
}

impl Held {
    fn new(app: &impl crate::capabilities::BootId, identity: Option<&str>, target: Option<String>, hit: &crate::quota::LimitHit) -> Self {
        Held {
            target,
            identity: identity.map(String::from),
            message: hit.message.clone(),
            until: hit.until.clone(),
            at: hit.at.clone(),
            bucket: hit.bucket.clone(),
            held_at: db::now(),
            boot: app.boot_id().to_string(),
        }
    }

    fn hit(&self) -> crate::quota::LimitHit {
        crate::quota::LimitHit { message: self.message.clone(), until: self.until.clone(), at: self.at.clone(), bucket: self.bucket.clone() }
    }
}

/// 判斷擋不擋要的狀態讀不到時，多久之後再看一次。
pub const UNVERIFIED_RETRY: Duration = Duration::from_secs(10);

/// 憑據寫不進排著的那一列時，多久之後再寫一次（flush 重來一輪，閘門會再寫）。
pub const PERSIST_RETRY: Duration = Duration::from_secs(5);

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
pub async fn blocking_hit(app: &impl QuotaHoldContext, bot: &db::Bot, turn_id: &str) -> Option<crate::quota::LimitHit> {
    if crate::config::is_api_kind(&bot.kind) {
        return None;
    }
    match app.try_limit_hit_for_bot(bot).await {
        Ok(Some(hit)) => {
            if let Err(e) = remember(app, turn_id, bot, &hit).await {
                tracing::warn!(turn = %turn_id, error = %e, "could not persist the quota hold of a queued prompt; retrying soon");
                app.schedule_flush_retry(&bot.id, PERSIST_RETRY);
            }
            return Some(hit);
        }
        Ok(None) => {}
        Err(e) => return Some(unverified(bot, &e)),
    }
    #[cfg(feature = "test-hooks")]
    super::race_point::hit("quota_hold_after_lookup", &bot.id).await;
    let host = match db::bot_host(app.db(), &bot.id).await {
        Ok(h) => h,
        Err(e) => return Some(unverified(bot, &e)),
    };
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

/// 這一列上一輪開機的憑據對這顆 bot **現在**還擋不擋：身份沒換、還沒到期、之後沒有成功回合清過那把 key、
/// 撞的桶管得到它在跑的模型。讀不到那一列、或憑據解不開，回錯（呼叫端照擋）。
async fn held_on_turn(app: &impl QuotaHoldContext, bot: &db::Bot, turn_id: &str) -> anyhow::Result<Option<crate::quota::LimitHit>> {
    let raw: Option<Option<String>> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?").bind(turn_id).fetch_optional(app.db()).await?;
    let Some(raw) = raw.flatten() else { return Ok(None) };
    let held: Held = serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("the quota hold on turn {turn_id} is unreadable: {e}"))?;
    if held.boot == app.boot_id() || !still_holds(app, bot, &held).await? {
        return Ok(None);
    }
    let hit = held.hit();
    let model = app.running_model(bot).await;
    Ok(crate::quota::limit_hit_blocks_model(&hit, model.as_deref()).then_some(hit))
}

/// 憑據還算數：身份沒換、還沒到期、寫下之後同一把 key 沒有被成功回合清過撞限。身份是這顆 bot 現在實際跑的那個
/// （[`crate::quota::billing_identity`]，issue #238）；讀不到回錯（呼叫端照擋）。
async fn still_holds(app: &impl QuotaHoldContext, bot: &db::Bot, held: &Held) -> anyhow::Result<bool> {
    if held.identity != app.billing_identity(bot).await? || crate::quota::limit_hit_expired(Some(&held.hit())) {
        return Ok(false);
    }
    if let Some(then) = held.target.as_deref() {
        let host = db::bot_host(app.db(), &bot.id).await?;
        if host_target(app, &host).await.is_some_and(|now| now != then) {
            return Ok(false);
        }
    }
    Ok(match chrono::DateTime::parse_from_rfc3339(&held.held_at) {
        Ok(t) => !app.limit_cleared_since(bot, t.with_timezone(&chrono::Utc)).await,
        Err(_) => true,
    })
}

/// 這個主機名**現在**指到哪台機器：本機是 `local`；遠端是 ssh 目標、port、herdr session（同 `api::repoints_host` 認的三項）。
/// 不在設定裡回 `None`。
async fn host_target(app: &impl super::s6_ports::QuotaHoldServices, host: &str) -> Option<String> {
    app.host_target(host).await
}

async fn remember(app: &impl QuotaHoldContext, turn_id: &str, bot: &db::Bot, hit: &crate::quota::LimitHit) -> anyhow::Result<()> {
    let identity = app.billing_identity(bot).await?;
    let target = host_target(app, &db::bot_host(app.db(), &bot.id).await?).await;
    let json = serde_json::to_string(&Held::new(app, identity.as_deref(), target, hit))?;
    sqlx::query("UPDATE turns SET quota_hold=? WHERE id=? AND status='queued'").bind(&json).bind(turn_id).execute(app.db()).await?;
    Ok(())
}

/// 撞限剛記下（`turn_error::mark_claude_limit_hit`）：這顆 bot 現在排著的每一則當場帶上憑據（#108 重開），不等下一次
/// flush 擋下才寫——記下到擋下之間 daemon 死掉、或 flush 那一下寫不進去，重啟之後都還有憑據。寫不進去回錯：撞限那條路
/// 會把它記成欠著、`StopFailure` 由收件匣重試。
pub async fn stamp_queued(app: &(impl Db + BootId + super::s6_ports::QuotaHoldServices), bot_id: &str, identity: Option<&str>, hit: &crate::quota::LimitHit) -> anyhow::Result<()> {
    let target = host_target(app, &db::bot_host(app.db(), bot_id).await?).await;
    let json = serde_json::to_string(&Held::new(app, identity, target, hit))?;
    sqlx::query(
        "UPDATE turns SET quota_hold=? WHERE status='queued'
            AND conversation_id IN (SELECT id FROM conversations WHERE bot_id=?)",
    )
    .bind(&json)
    .bind(bot_id)
    .execute(app.db())
    .await?;
    Ok(())
}

/// 閘門放行：清掉那一列的憑據。清不掉照樣放行（判準已經說不擋），留下的舊憑據只會讓重啟之後多擋到它自己的判準
/// （[`still_holds`]）說不擋為止。
async fn forget(app: &impl crate::capabilities::Db, turn_id: &str) {
    if let Err(e) = sqlx::query("UPDATE turns SET quota_hold=NULL WHERE id=? AND quota_hold IS NOT NULL").bind(turn_id).execute(app.db()).await {
        tracing::warn!(turn = %turn_id, error = %e, "could not clear the quota hold of a released prompt");
    }
}

/// 每台主機在這一輪開機裡的回填：`false` 跑到一半、`true` 跑完了。鍵帶 `boot_id`：測試裡模擬重啟的新 `App`
/// 跟舊的共用資料目錄，也不會互相看到。
fn backfill_state() -> &'static Mutex<HashMap<String, bool>> {
    static M: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn backfill_key(app: &impl crate::capabilities::BootId, host: &str) -> String {
    format!("{}\u{0}{host}", app.boot_id())
}

/// 回填時憑據解不開的 bot（鍵同樣帶 `boot_id`）：對它們而言「這台回填完了」不成立，`blocking_hit` 照舊
/// 問那一列自己的憑據。只在這一輪開機內有效——下一次開機重新回填時會再判一次。
fn unresolved_holds() -> &'static Mutex<std::collections::HashSet<String>> {
    static M: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn mark_unresolved(app: &impl crate::capabilities::BootId, bot_id: &str) {
    if let Ok(mut m) = unresolved_holds().lock() {
        m.insert(backfill_key(app, bot_id));
    }
}

fn unresolved(app: &impl crate::capabilities::BootId, bot_id: &str) -> bool {
    unresolved_holds().lock().ok().is_some_and(|m| m.contains(&backfill_key(app, bot_id)))
}

fn backfilled(app: &impl crate::capabilities::BootId, host: &str) -> bool {
    backfill_state().lock().ok().and_then(|m| m.get(&backfill_key(app, host)).copied()).unwrap_or(false)
}

/// 開機回填（排著的 prompt 那一半）：`host` 的身份表剛寫好，把那台 bot 排著的 prompt 身上**上一輪開機**記下的
/// 撞限種回記憶體。**每台主機每一輪開機只跑一次**，跑完之後 flush 只看記憶體。身份已經換掉、到期、或擋下之後
/// 同一把 key 被成功回合清過的不種（[`still_holds`]）。種完叫醒那幾顆的 flush：已經不擋的馬上送，不必等重看的 timer。
///
/// 讀不到就**不算回填過**（#108 重開）：以前讀不到那幾列當作沒有、照樣標成跑完，flush 從此只看空的記憶體，重啟前記下的
/// 撞限就這樣放行。現在跑到一半讀不到就拿掉標記——flush 繼續看每一列自己的憑據（照擋），[`BACKFILL_RETRY`] 之後
/// （或那台主機下一次偵測完）再回填一次。已經種回去的留著（`restore_limit_hit` 只留較晚的，重來不會疊）。
pub async fn backfill_once(app: &impl QuotaHoldContext, host: &str) {
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
                app.schedule_queue_flush(&bot_id);
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

fn retry_backfill(app: &impl QuotaHoldContext, host: &str) {
    if cfg!(any(test, feature = "test-hooks")) {
        return;
    }
    let (app, host) = (app.clone(), host.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(BACKFILL_RETRY).await;
        backfill_once(&app, &host).await;
    });
}

/// 回填本身：回傳要叫醒 flush 的 bot。任何一步讀不到就回錯。
async fn backfill(app: &impl QuotaHoldContext, host: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.bot_id, t.quota_hold FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'queued' AND t.quota_hold IS NOT NULL",
    )
    .fetch_all(app.db())
    .await?;
    let mut woken = Vec::new();
    for (bot_id, raw) in rows {
        let Some(bot) = db::bot(app.db(), &bot_id).await? else { continue };
        if db::bot_host(app.db(), &bot.id).await? != host {
            continue;
        }
        let held = match serde_json::from_str::<Held>(&raw) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(bot = %bot.id, error = %e, "a queued prompt carries an unreadable quota hold; it keeps holding that prompt");
                mark_unresolved(app, &bot.id);
                continue;
            }
        };
        woken.push(bot.id.clone());
        if held.boot == app.boot_id() || !still_holds(app, &bot, &held).await? {
            continue;
        }
        let base = app.quota_base_for_host(host, &bot.kind, held.identity.as_deref()).await;
        if app.restore_limit_hit(host, &base, held.hit()).await {
            tracing::info!(host, bot = %bot.id, until = ?held.until, "重啟回填：排著的 prompt 記下的撞限補回記憶體");
        }
    }
    Ok(woken)
}

/// 擋著的時候多久之後再看：撞限到期那一刻，但最多 [`RECHECK_MAX`]；沒寫時間的就照 `RECHECK_MAX`。
/// 讀不懂的時間照「沒寫」算：`limit_hit_expired` 也不讓它過期，每秒重看一次只是空轉。
pub fn recheck_in(until: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> Duration {
    let Some(until) = until.and_then(|u| chrono::DateTime::parse_from_rfc3339(u).ok()) else { return RECHECK_MAX };
    match (until.with_timezone(&chrono::Utc) - now).to_std() {
        Ok(left) => left.clamp(Duration::from_secs(1), RECHECK_MAX),
        Err(_) => Duration::from_secs(1),
    }
}

/// 保險絲用：這個擋有沒有看得到的盡頭——撞限寫了到期時間，而且在 `max_wait` 內。
pub fn bounded(hit: &crate::quota::LimitHit, now: chrono::DateTime<chrono::Utc>, max_wait: chrono::Duration) -> bool {
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
pub fn note_held(bot_id: &str) {
    if let Ok(mut m) = held_at().lock() {
        m.insert(bot_id.to_string(), std::time::Instant::now());
    }
}

/// 最近一次重看的節奏內 flush 還在擋這顆：撞限剛被校正掉或剛到期，下一次重看（最多 [`RECHECK_MAX`] 後）
/// 就會送。保險絲在這段時間撤掉它，等於把馬上要送的派工丟掉。只放記憶體：重啟後當作沒擋過。
pub fn recently_held(bot_id: &str) -> bool {
    held_at()
        .lock()
        .ok()
        .and_then(|m| m.get(bot_id).copied())
        .is_some_and(|t| t.elapsed() <= RECHECK_MAX + Duration::from_secs(60))
}

#[cfg(feature = "test-hooks")]
pub fn forget_held(bot_id: &str) {
    if let Ok(mut m) = held_at().lock() {
        m.remove(bot_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    use super::*;
    pub fn backfilled(app: &impl crate::capabilities::BootId, host: &str) -> bool { super::backfilled(app, host) }
}
