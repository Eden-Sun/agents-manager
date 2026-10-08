//! 全機 cargo/rustc 併發排程（issue #90）：`cargo` PATH shim（`cargo_shim.rs`）在真的 cargo 之前
//! 先跟這裡要一個名額，拿到才 exec。目前用一支手動腳本（`cargo-slot.sh`）＋兩顆全機名額硬擋，
//! 這裡把它變成 daemon 提供的機制，跨 worktree、跨 agent、跨 herdr session 都生效。
//!
//! **一列一個持有者**（`holder` 是主鍵）：同一個呼叫者同時只會佔一列，重試會覆寫同一列，不會愈疊愈多。
//! `holder` 由呼叫端自己保證唯一（shim 用 `<agent 名>:<pid>`），daemon 不驗證這一點——名字衝突是
//! 呼叫端的事，daemon 只管「數字對不對」。
//!
//! **名額是 TTL 租的，不是等建置跑完才還**：拿到名額後背景執行緒要定期 `renew`，跑多久都行，只要
//! 續約還在動；停止續約（持有者掛了、被砍、pane 消失）超過 TTL 就被下一次 acquire 收回——跟
//! `supervisor_leases`（daemon 換版的租約）同一個道理，這裡不需要那麼重（沒有核准流程、沒有 fence，
//! 因為搶不到名額的後果只是「慢一點」，不是「兩個人同時換 daemon 二進位」）。
//!
//! **等待中的呼叫者也是一列**（`status='waiting'`）：`GET /build-slots` 因此能同時列出「誰在建置、
//! 誰在等」（issue 要求 `waiting_for_build_slot`／`building` 可觀測）。等待中的持有者每次重試 poll
//! 都要刷新 `last_seen`（不是 `since`——`since` 是排隊起點，拿來算「等了多久」；`last_seen` 才是
//! 「還活著嗎」，兩者混在一起會把排隊排很久但還在 poll 的人跟真的死掉不再 poll 的人搞混）。
//!
//! **daemon 重啟不會留下永久卡住的名額**：這張表本身會跟著 SQLite 檔案活下來（cargo 行程不是 daemon
//! 的子行程，daemon 重啟不代表建置真的停了，硬把表清空反而會讓重啟後的名額數失真），但沒有人能永遠
//! 賴著不放：TTL 到了、沒有人 renew，下一次 acquire（或背景 sweep）就收回。真的想要「daemon 重啟＝
//! 全部歸零」可以砍這張表，但目前沒有理由這麼做。

use crate::lc_error::LcError;
use anyhow::Result;
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::time::Duration;


pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS build_slots (
           holder TEXT PRIMARY KEY,
           -- 'waiting' 列沒有真的名額，token 是空字串（renew／release 用不到，也不該被拿去用）。
           token TEXT NOT NULL,
           bot_id TEXT,
           purpose TEXT,
           host TEXT NOT NULL DEFAULT 'local',
           status TEXT NOT NULL, -- 'held' | 'waiting'
           since TEXT NOT NULL,
           last_seen TEXT NOT NULL,
           expires_at TEXT
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 等待中的列多久沒刷新 `last_seen` 就當持有者不在了（停止 poll：行程被砍、pane 消失）。
/// shim 的 poll 間隔遠短於這個值，正常排隊不會被誤收。
const STALE_WAITING: Duration = Duration::from_secs(60);

/// 每顆 bot 同時最多佔幾列（held＋waiting）。`holder` 是呼叫端自己取的，不設上限的話一顆 bot 就能用不同 holder 把佇列塞滿
/// （FIFO 擋在最前面的是幽靈）、把表撐大。正常用法是每個進行中的 cargo 一列，遠低於這個數字。沒有 bot 身分的人工呼叫不受限。
pub const MAX_ROWS_PER_BOT: usize = 32;

/// holder／purpose／host 的長度上限（字元）：呼叫端給的字串原樣進 DB 再顯示在網頁。
pub const MAX_FIELD_CHARS: usize = 200;

pub fn has_oversized_fields(fields: &[&str]) -> bool {
    fields.iter().any(|f| f.chars().count() > MAX_FIELD_CHARS)
}

pub fn field_limit_error() -> LcError {
    LcError::Bad(format!("build-slot 欄位最多 {MAX_FIELD_CHARS} 個字元"))
}

/// 背景 sweep 的間隔：跟 `lifecycle::stuck_turns` 的節奏一致（見那邊的說明）。
pub const SWEEP_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, sqlx::FromRow)]
struct SlotRow {
    token: String,
    bot_id: Option<String>,
    status: String,
    since: String,
    expires_at: Option<String>,
}

/// `acquire` 的結果。`Granted` 對已經持有的 holder 重call 是幂等的（回同一個 token／到期時間）。
#[derive(Debug, Clone, PartialEq)]
pub enum Acquired {
    Granted { token: String, expires_at: String },
    Waiting { active: usize, since: String },
    /// A bot tried to reuse a holder row owned by another bot (or by a manual caller).
    HolderOwnedByAnotherBot,
    /// 這顆 bot 已經佔了 [`MAX_ROWS_PER_BOT`] 列，不收新的 holder。
    TooManyForBot,
}

fn now_str() -> String {
    crate::db::now()
}

/// Slot tokens are bearer secrets, so they must not use the public monotonic ULID generator.
fn new_slot_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

pub fn expires_at_after(cfg: &crate::config::BuildCfg) -> Result<String> {
    let secs = cfg.lease_ttl().map_err(anyhow::Error::msg)?;
    let delta = chrono::Duration::try_seconds(secs).ok_or_else(|| anyhow::anyhow!("lease_ttl_secs {secs} is out of range"))?;
    let at = chrono::Utc::now().checked_add_signed(delta).ok_or_else(|| anyhow::anyhow!("lease_ttl_secs {secs} overflows the clock"))?;
    Ok(crate::db::iso_at(at))
}

/// 收掉過期沒 renew 的 held 列。回傳收掉幾列——由呼叫端（acquire／sweep）決定要不要記 log。
async fn reap_expired_held(pool: &SqlitePool, now: &str) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM build_slots WHERE status = 'held' AND expires_at <= ?")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected())
}

/// 收掉太久沒刷新 `last_seen` 的 waiting 列（停止 poll：行程被砍、pane 消失）。acquire 自己也要呼叫這個
/// （不只等背景 sweep）：死掉的號碼牌卡在佇列最前面會擋住後面活著的人，FIFO 判斷不能讓它拖到下一輪 sweep。
async fn reap_stale_waiting(pool: &SqlitePool) -> Result<u64> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::from_std(STALE_WAITING).unwrap()).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(sqlx::query("DELETE FROM build_slots WHERE status = 'waiting' AND last_seen <= ?")
        .bind(&cutoff)
        .execute(pool)
        .await?
        .rows_affected())
}

/// 插一列（或覆寫既有的同名列）成 `waiting`；`since` 是排隊起點，呼叫端負責決定（保留舊的、還是這一刻）。
async fn mark_waiting(app: &impl crate::capabilities::Db, holder: &str, bot_id: Option<&str>, purpose: &str, host: &str, since: &str, now: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO build_slots (holder, token, bot_id, purpose, host, status, since, last_seen, expires_at)
         VALUES (?,'',?,?,?, 'waiting', ?, ?, NULL)
         ON CONFLICT(holder) DO UPDATE SET
           bot_id = excluded.bot_id, purpose = excluded.purpose, host = excluded.host,
           status = 'waiting', last_seen = excluded.last_seen, expires_at = NULL",
    )
    .bind(holder)
    .bind(bot_id)
    .bind(purpose)
    .bind(host)
    .bind(since)
    .bind(now)
    .execute(app.db())
    .await?;
    Ok(())
}

/// 要一個名額；額滿（或有人排得比自己前面）就記一列 `waiting`，`GET /build-slots` 看得到。全程在
/// `app.build_slot_lock` 底下，先數後寫、FIFO 判斷都在同一個臨界區裡完成。
///
/// **FIFO**（2026-09-18 使用者交辦；手工 `cargo-slot.sh` 的舊版每個等待者各自搶，實測有人餓死 74 分鐘）：
/// 名額空出來時，只有排隊排最早的那個 holder 可以真的拿到，其他人就算這一刻也在問、名額也空著，一樣要等——
/// 跟 `cargo-slot.sh` 的號碼牌是同一個道理，只是這裡用 `build_slots.since` 當號碼牌，不需要另開一張表。
/// 佇列順序＝`(since, holder)` 字典序（`since` 相同——理論上毫秒級撞期——用 `holder` 當穩定的第二排序鍵）。
pub async fn acquire(app: &(impl crate::build_scheduler::BuildSlotLock + crate::capabilities::Cfg + crate::capabilities::Db), holder: &str, bot_id: Option<&str>, purpose: &str, host: &str) -> Result<Acquired> {
    let _g = app.build_slot_lock().lock().await;
    let cfg = app.cfg().build_fresh().await;
    let max = cfg.max_concurrent();
    let now = now_str();
    reap_expired_held(app.db(), &now).await?;
    reap_stale_waiting(app.db()).await?;

    let existing: Option<SlotRow> = sqlx::query_as("SELECT token, bot_id, status, since, expires_at FROM build_slots WHERE holder = ?")
        .bind(holder)
        .fetch_optional(app.db())
        .await?;
    // `holder` comes from the caller, so it cannot establish identity by itself. Otherwise any
    // authenticated bot can repeat another bot's holder and recover its live lease token, or
    // overwrite its queued row. UI-token callers are the explicit manual/admin bypass.
    if let (Some(caller_bot_id), Some(row)) = (bot_id, existing.as_ref()) {
        if row.bot_id.as_deref() != Some(caller_bot_id) {
            return Ok(Acquired::HolderOwnedByAnotherBot);
        }
    }
    // 新的 holder 才算數：已經有的列（重 poll、重問）照舊，不會被自己的上限擋在門外。
    if let (Some(bot), None) = (bot_id, existing.as_ref()) {
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_slots WHERE bot_id = ?").bind(bot).fetch_one(app.db()).await?;
        if rows as usize >= MAX_ROWS_PER_BOT {
            return Ok(Acquired::TooManyForBot);
        }
    }
    // 已經握著且沒過期：把同一份憑證還回去，重call（例如逾時後重問一次）安全。FIFO 不擋自己已經有的名額。
    if let Some(row) = &existing {
        if row.status == "held" {
            if let Some(exp) = &row.expires_at {
                if exp.as_str() > now.as_str() {
                    return Ok(Acquired::Granted { token: row.token.clone(), expires_at: exp.clone() });
                }
            }
        }
    }

    let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_slots WHERE status = 'held'").fetch_one(app.db()).await?;
    // 排隊起點：已經在等的人保留原本的 `since`（重試不該讓排隊起點歸零），第一次來的人以「現在」當自己的號碼牌。
    let my_since = existing.as_ref().filter(|r| r.status == "waiting").map(|r| r.since.clone()).unwrap_or_else(|| now.clone());

    if (held as usize) < max {
        // 名額空著，但要先看看排在自己前面的人（排除自己那一列）——有更早的號碼牌就不能插隊，
        // 即使這一刻剛好是自己在問、名額也剛好空著。
        let ahead: Option<(String, String)> =
            sqlx::query_as("SELECT since, holder FROM build_slots WHERE status = 'waiting' AND holder != ? ORDER BY since, holder LIMIT 1")
                .bind(holder)
                .fetch_optional(app.db())
                .await?;
        let someone_is_ahead = ahead.is_some_and(|(ahead_since, ahead_holder)| (ahead_since.as_str(), ahead_holder.as_str()) < (my_since.as_str(), holder));
        if !someone_is_ahead {
            let token = new_slot_token();
            let expires_at = expires_at_after(&cfg)?;
            sqlx::query(
                "INSERT INTO build_slots (holder, token, bot_id, purpose, host, status, since, last_seen, expires_at)
                 VALUES (?,?,?,?,?, 'held', ?, ?, ?)
                 ON CONFLICT(holder) DO UPDATE SET
                   token = excluded.token, bot_id = excluded.bot_id, purpose = excluded.purpose, host = excluded.host,
                   status = 'held', since = excluded.since, last_seen = excluded.last_seen, expires_at = excluded.expires_at",
            )
            .bind(holder)
            .bind(&token)
            .bind(bot_id)
            .bind(purpose)
            .bind(host)
            .bind(&now)
            .bind(&now)
            .bind(&expires_at)
            .execute(app.db())
            .await?;
            return Ok(Acquired::Granted { token, expires_at });
        }
    }

    mark_waiting(app, holder, bot_id, purpose, host, &my_since, &now).await?;
    Ok(Acquired::Waiting { active: held as usize, since: my_since })
}

#[derive(Debug, PartialEq, Eq)]
pub enum RenewErr {
    /// 沒這一列（從沒拿過、或已經被收回）：呼叫端要重新 acquire，不能就地續約。
    NotFound,
    /// 有這一列，但 token 對不上：不是原本的持有者。
    TokenMismatch,
}

/// 續約：只有還在 `held` 且沒過期的列才能續，否則要求重新 acquire——跟 lease 的「過期沒 release
/// 就等於放棄，重問一次」同一個語意，不做「你的名額其實已經被別人拿走了」這種模糊地帶。
pub async fn renew(app: &(impl crate::build_scheduler::BuildSlotLock + crate::capabilities::Cfg + crate::capabilities::Db), holder: &str, token: &str) -> Result<Result<String, RenewErr>> {
    let _g = app.build_slot_lock().lock().await;
    let cfg = app.cfg().build_fresh().await;
    let now = now_str();
    let row: Option<SlotRow> = sqlx::query_as("SELECT token, bot_id, status, since, expires_at FROM build_slots WHERE holder = ?")
        .bind(holder)
        .fetch_optional(app.db())
        .await?;
    let Some(row) = row else { return Ok(Err(RenewErr::NotFound)) };
    if row.status != "held" || row.expires_at.as_deref().is_none_or(|e| e <= now.as_str()) {
        return Ok(Err(RenewErr::NotFound));
    }
    if row.token != token {
        return Ok(Err(RenewErr::TokenMismatch));
    }
    let expires_at = expires_at_after(&cfg)?;
    sqlx::query("UPDATE build_slots SET expires_at = ?, last_seen = ? WHERE holder = ? AND token = ?")
        .bind(&expires_at)
        .bind(&now)
        .bind(holder)
        .bind(token)
        .execute(app.db())
        .await?;
    Ok(Ok(expires_at))
}

/// 放：一律幂等（找不到、已經過期、token 對不上都當作「已經不是你的事了」，回成功——釋放路徑
/// 不該因為競態或重送就報錯，讓呼叫端的 `trap ... EXIT` 永遠可以放心呼叫）。
pub async fn release(app: &(impl crate::build_scheduler::BuildSlotLock + crate::capabilities::Db), holder: &str, token: &str) -> Result<()> {
    release_as(app, holder, token, None).await
}

/// [`release`]，多一個驗過身分的呼叫端 `bot_id`。
///
/// **token 是空字串＝取消等待**（issue #913）：`waiting` 列沒有 token，等名額途中被中斷（Ctrl-C、被砍、呼叫端走了）的 shim
/// 沒東西可以拿來放，以前號碼牌就一直留到 `STALE_WAITING`（60 秒）才被收，排在後面的人有空名額也要空等。
/// 這時只刪該 holder 的 `waiting` 列；`bot_id` 有值時列的 `bot_id` 要相符（別顆 bot 的號碼牌不能被取消，跟 `acquire` 的
/// `HolderOwnedByAnotherBot` 同一道守衛，靜默當成沒事），沒有值（UI token 的人工呼叫）則不限。`held` 列永遠不會被空 token 刪到。
pub async fn release_as(app: &(impl crate::build_scheduler::BuildSlotLock + crate::capabilities::Db), holder: &str, token: &str, bot_id: Option<&str>) -> Result<()> {
    let _g = app.build_slot_lock().lock().await;
    if token.is_empty() {
        match bot_id {
            Some(bot) => sqlx::query("DELETE FROM build_slots WHERE holder = ? AND status = 'waiting' AND bot_id = ?").bind(holder).bind(bot).execute(app.db()).await?,
            None => sqlx::query("DELETE FROM build_slots WHERE holder = ? AND status = 'waiting'").bind(holder).execute(app.db()).await?,
        };
        return Ok(());
    }
    sqlx::query("DELETE FROM build_slots WHERE holder = ? AND status = 'held' AND token = ?")
        .bind(holder)
        .bind(token)
        .execute(app.db())
        .await?;
    Ok(())
}

/// 收掉過期沒續約的 held 列，跟停止 poll 太久的 waiting 列。回傳 (held 收掉幾列, waiting 收掉幾列)。
pub async fn sweep(app: &(impl crate::build_scheduler::BuildSlotLock + crate::capabilities::Db)) -> (u64, u64) {
    let _g = app.build_slot_lock().lock().await;
    let now = now_str();
    let held = reap_expired_held(app.db(), &now).await.unwrap_or(0);
    let waiting = reap_stale_waiting(app.db()).await.unwrap_or(0);
    (held, waiting)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct StatusRow {
    holder: String,
    status: String,
    bot_id: Option<String>,
    purpose: Option<String>,
    host: String,
    since: String,
    last_seen: String,
    expires_at: Option<String>,
}

pub async fn status(app: &(impl crate::capabilities::Cfg + crate::capabilities::DataDir + crate::capabilities::Db)) -> Result<Value> {
    let cfg = app.cfg().build_fresh().await;
    reap_expired_held(app.db(), &now_str()).await?;
    let rows: Vec<StatusRow> = sqlx::query_as(
        "SELECT holder, status, bot_id, purpose, host, since, last_seen, expires_at FROM build_slots ORDER BY since",
    )
    .fetch_all(app.db())
    .await?;
    let slots: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({"holder": r.holder, "status": r.status, "bot_id": r.bot_id, "purpose": r.purpose, "host": r.host,
                   "since": r.since, "last_seen": r.last_seen, "expires_at": r.expires_at})
        })
        .collect();
    let active = slots.iter().filter(|s| s["status"] == "held").count();
    Ok(json!({
        "max_concurrent": cfg.max_concurrent(),
        "cargo_jobs": cfg.cargo_jobs,
        "test_threads": cfg.test_threads(),
        "lease_ttl_secs": cfg.lease_ttl().map_err(anyhow::Error::msg)?,
        "active": active,
        "slots": slots,
        "remote": remote_json(&cfg.remote, app.data_dir()),
    }))
}

/// 外部編譯主機這一格（issue #428）：本機的隊伍排得再長，也要看得出來是不是因為遠端連不上。
/// `remote_reachable` 是**上一次真的嘗試**的結論（`remote_health`），`null`＝沒開或還沒有人試過——
/// 「不知道」跟「連不上」不能混成同一個值，否則剛開機就會亮一個假的紅燈。
fn remote_json(remote: &crate::config::BuildRemoteCfg, data_dir: &std::path::Path) -> Value {
    let last = remote.enabled.then(|| crate::remote_health::read(data_dir)).flatten();
    json!({
        "enabled": remote.enabled,
        "target": remote.enabled.then(|| format!("{}@{}:{}", remote.user, remote.host, remote.ssh_port)),
        "remote_reachable": last.as_ref().map(|h| h.reachable),
        "checked_at": last.as_ref().map(|h| h.checked_at.clone()),
        "reason": last.as_ref().and_then(|h| h.reason.clone()),
    })
}

// ---------------------------------------------------------------- HTTP API

#[derive(Deserialize)]
pub struct AcquireIn {
    /// 呼叫端自己保證唯一（shim 用 `<agent 名>:<pid>`）；daemon 不檢查唯一性。
    pub holder: String,
    pub bot_id: Option<String>,
    #[serde(default)]
    pub purpose: String,
    #[serde(default = "default_host")]
    pub host: String,
}

#[derive(Deserialize)]
pub struct RenewIn {
    pub holder: String,
    pub token: String,
}

#[derive(Deserialize)]
pub struct ReleaseIn {
    pub holder: String,
    /// 空字串＝取消這個 holder 的 `waiting` 列（見 [`release_as`]）；此時要帶 bot 身分（同 acquire）或 UI token。
    pub token: String,
    /// 只有空 token（取消等待）才看；跟 acquire 的 `bot_id` 同一套驗證。
    #[serde(default)]
    pub bot_id: Option<String>,
}

fn default_host() -> String {
    crate::config::LOCAL_HOST.to_string()
}

pub fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// Bot 請求用該 bot 的 per-bot token；有 `X-AM-Bot-Id` 時還必須與 body `bot_id` 相同。
/// 舊 shim 在 body 帶 id、只送 `X-AM-Bot-Token`，仍可在相容期間驗證。出現任何 Bot 身分欄位後，
/// partial／錯誤／混合 UI 憑證都拒絕，不降級成 User。沒有 Bot 身分的人工 host shell 可用 UI token。
pub async fn authenticate(app: &(impl crate::capabilities::Db + crate::capabilities::UiToken), headers: &HeaderMap, bot_id: Option<&str>) -> Result<Option<String>, LcError> {
    let id_header_present = headers.contains_key("X-AM-Bot-Id");
    let token_header_present = headers.contains_key("X-AM-Bot-Token");
    let ui_header_present = headers.contains_key("X-AM-Token");
    let header_id = headers.get("X-AM-Bot-Id").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
    let body_id = bot_id.map(str::trim).filter(|s| !s.is_empty());
    let bot_token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
    if id_header_present || token_header_present || body_id.is_some() {
        if ui_header_present {
            return Err(LcError::Forbidden(json!({"error":"unauthorized","message":"Bot identity cannot fall back to or mix with X-AM-Token"})));
        }
        let (Some(id), Some(token)) = (body_id, bot_token) else {
            return Err(LcError::Forbidden(json!({"error":"unauthorized","message":"need a matching X-AM-Bot-Id, bot_id, and X-AM-Bot-Token"})));
        };
        if id_header_present && header_id != Some(id) {
            return Err(LcError::Forbidden(json!({"error":"unauthorized","message":"X-AM-Bot-Id must match body bot_id"})));
        }
        match crate::db::bot(app.db(), id).await.map_err(up)? {
            Some(b) if b.deleted_at.is_none() && crate::agent_relay::ct_eq(token, &b.hook_token) => {
                // 分享用的受限 bot 的 token 只能打自己的 hook（SPEC「分享 bot」）。
                if crate::db::refuses_bot_principal(app.db(), id).await {
                    return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "restricted_bot"})));
                }
                return Ok(Some(id.to_string()));
            }
            _ => {}
        }
    }
    let ui_token = headers.get("X-AM-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ui_token.is_empty() && crate::agent_relay::ct_eq(ui_token, app.ui_token()) {
        return Ok(None);
    }
    Err(LcError::Forbidden(json!({"error": "unauthorized", "message": "need a matching X-AM-Bot-Token+bot_id, or X-AM-Token"})))
}



/// build 名額的序列化鎖。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait BuildSlotLock: Send + Sync {
    fn build_slot_lock(&self) -> &tokio::sync::Mutex<()>;
}
