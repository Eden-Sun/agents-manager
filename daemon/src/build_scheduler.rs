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
use crate::state::App;
use anyhow::Result;
use axum::extract::{Form, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;
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
const MAX_ROWS_PER_BOT: usize = 32;

/// holder／purpose／host 的長度上限（字元）：呼叫端給的字串原樣進 DB 再顯示在網頁。
const MAX_FIELD_CHARS: usize = 200;

fn has_oversized_fields(fields: &[&str]) -> bool {
    fields.iter().any(|f| f.chars().count() > MAX_FIELD_CHARS)
}

fn field_limit_error() -> LcError {
    LcError::Bad(format!("build-slot 欄位最多 {MAX_FIELD_CHARS} 個字元"))
}

/// 背景 sweep 的間隔：跟 `lifecycle::stuck_turns` 的節奏一致（見那邊的說明）。
const SWEEP_EVERY: Duration = Duration::from_secs(30);

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

fn expires_at_after(cfg: &crate::config::BuildCfg) -> Result<String> {
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
    let _g = app.build_slot_lock().lock().await;
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

pub fn spawn_sweeper(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            let (held, waiting) = sweep(&app).await;
            if held > 0 || waiting > 0 {
                tracing::info!(held, waiting, "build scheduler: 收回沒人續約／沒人再 poll 的名額");
            }
        }
    });
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
    holder: String,
    bot_id: Option<String>,
    #[serde(default)]
    purpose: String,
    #[serde(default = "default_host")]
    host: String,
}

#[derive(Deserialize)]
pub struct RenewIn {
    holder: String,
    token: String,
}

#[derive(Deserialize)]
pub struct ReleaseIn {
    holder: String,
    token: String,
}

fn default_host() -> String {
    crate::config::LOCAL_HOST.to_string()
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// Bot 請求用該 bot 的 per-bot token；有 `X-AM-Bot-Id` 時還必須與 body `bot_id` 相同。
/// 舊 shim 在 body 帶 id、只送 `X-AM-Bot-Token`，仍可在相容期間驗證。出現任何 Bot 身分欄位後，
/// partial／錯誤／混合 UI 憑證都拒絕，不降級成 User。沒有 Bot 身分的人工 host shell 可用 UI token。
async fn authenticate(app: &(impl crate::capabilities::Db + crate::capabilities::UiToken), headers: &HeaderMap, bot_id: Option<&str>) -> Result<Option<String>, LcError> {
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
            Some(b) if b.deleted_at.is_none() && crate::herdr_maintenance::app_ports_p12::ct_eq(token, &b.hook_token) => {
                // 分享用的受限 bot 的 token 只能打自己的 hook（SPEC「分享 bot」）。
                if crate::share::refuses_bot_principal(app.db(), id).await {
                    return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "restricted_bot"})));
                }
                return Ok(Some(id.to_string()));
            }
            _ => {}
        }
    }
    let ui_token = headers.get("X-AM-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ui_token.is_empty() && crate::herdr_maintenance::app_ports_p12::ct_eq(ui_token, app.ui_token()) {
        return Ok(None);
    }
    Err(LcError::Forbidden(json!({"error": "unauthorized", "message": "need a matching X-AM-Bot-Token+bot_id, or X-AM-Token"})))
}

pub async fn get_status(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(status(&app).await.map_err(up)?))
}

pub async fn post_acquire(State(app): State<Arc<App>>, headers: HeaderMap, Form(body): Form<AcquireIn>) -> Result<Json<Value>, LcError> {
    if body.holder.trim().is_empty() {
        return Err(LcError::Bad("holder 不能是空的".into()));
    }
    if has_oversized_fields(&[&body.holder, &body.purpose, &body.host]) {
        return Err(field_limit_error());
    }
    let bot_id = authenticate(&app, &headers, body.bot_id.as_deref()).await?;
    match acquire(&app, body.holder.trim(), bot_id.as_deref(), body.purpose.trim(), body.host.trim()).await.map_err(up)? {
        Acquired::Granted { token, expires_at } => {
            let cfg = app.cfg.build_fresh().await;
            Ok(Json(json!({"granted": true, "token": token, "expires_at": expires_at, "cargo_jobs": cfg.cargo_jobs, "test_threads": cfg.test_threads(), "lease_ttl_secs": cfg.lease_ttl().map_err(LcError::Bad)?})))
        }
        Acquired::Waiting { active, since } => {
            let cfg = app.cfg.build_fresh().await;
            Ok(Json(json!({"granted": false, "active": active, "max_concurrent": cfg.max_concurrent(), "since": since, "retry_after_secs": 5})))
        }
        Acquired::TooManyForBot => Err(LcError::conflict(
            "too_many_build_slots",
            json!({"reason": "too_many_build_slots", "max_per_bot": MAX_ROWS_PER_BOT, "message": "這顆 bot 同時佔著或排著的名額太多了；等手上的 cargo 跑完（或放掉）再要"}),
        )),
        Acquired::HolderOwnedByAnotherBot => Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "holder_bot_mismatch",
            "message": "a bot may only reuse its own build slot holder",
        }))),
    }
}

/// renew／release 不另外驗 bot／UI token：`acquire` 發出來的 `token` 本身就是憑證（跟 lease_token
/// 同一個道理——知道那個 token 就等於是那個持有者），少一層 header 檢查，shim 續約迴圈也簡單一點。
pub async fn post_renew(State(app): State<Arc<App>>, Form(body): Form<RenewIn>) -> Result<Json<Value>, LcError> {
    if has_oversized_fields(&[&body.holder, &body.token]) {
        return Err(field_limit_error());
    }
    match renew(&app, body.holder.trim(), &body.token).await.map_err(up)? {
        Ok(expires_at) => Ok(Json(json!({"renewed": true, "expires_at": expires_at}))),
        Err(RenewErr::NotFound) => Err(LcError::NotFound("build_slot".into())),
        Err(RenewErr::TokenMismatch) => Err(LcError::Forbidden(json!({"error": "token_mismatch"}))),
    }
}

pub async fn post_release(State(app): State<Arc<App>>, Form(body): Form<ReleaseIn>) -> (axum::http::StatusCode, Json<Value>) {
    if has_oversized_fields(&[&body.holder, &body.token]) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"released": false, "error": "bad_request", "message": format!("build-slot 欄位最多 {MAX_FIELD_CHARS} 個字元")})),
        );
    }
    // 寫不進去不能回 released:true（#327）：名額會佔到 TTL，呼叫端要知道，才有機會重試；至少 log 留痕。
    match release(&app, body.holder.trim(), &body.token).await {
        Ok(()) => (axum::http::StatusCode::OK, Json(json!({"released": true}))),
        Err(e) => {
            tracing::error!(holder = %body.holder, error = ?e, "build scheduler: could not release a slot; it stays held until its lease expires");
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({"released": false})))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use crate::testing as tt;

    async fn a_bot(env: &tt::Env, hook_token: &str) -> String {
        let id = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,1,?,?)",
        )
        .bind(&id)
        .bind(&env.project_id)
        .bind(format!("bot-{id}"))
        .bind(hook_token)
        .bind(crate::db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        id
    }

    async fn set_max_concurrent(app: &Arc<App>, n: usize) {
        app.cfg
            .update(|cfg| {
                cfg.build.max_concurrent = n;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// #322／#639：0 與超大的 lease_ttl_secs 都不能寫進設定。0 會讓名額立刻過期；u64::MAX 用 `as i64` 變 -1，更大的值讓時鐘加法 panic。
    #[tokio::test]
    async fn a_lease_ttl_outside_the_range_is_rejected_and_does_not_panic() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        for bad in [0_u64, u64::MAX, 10_000_000_000_000] {
            let err = app.cfg.update(|cfg| { cfg.build.lease_ttl_secs = bad; Ok(()) }).await.unwrap_err().to_string();
            assert!(err.contains("lease_ttl_secs") && err.contains("未變更"), "{bad}: {err}");
        }
        assert_eq!(app.cfg.build_fresh().await.lease_ttl().unwrap(), 180);
        assert!(matches!(acquire(&app, "A:1", None, "test", "local").await.unwrap(), Acquired::Granted { .. }));
        assert!(matches!(acquire(&app, "B:2", None, "test", "local").await.unwrap(), Acquired::Waiting { .. }));
        let huge = crate::config::BuildCfg { lease_ttl_secs: u64::MAX, ..crate::config::BuildCfg::default() };
        let err = expires_at_after(&huge).unwrap_err().to_string();
        assert!(err.contains("lease_ttl_secs"), "{err}");
    }

    /// #327：release 的 DB 寫失敗不能回 released:true（名額會佔到 TTL 而呼叫端以為已放）。
    #[tokio::test]
    async fn a_release_that_cannot_write_says_so() {
        let env = tt::env().await;
        let app = env.app.clone();
        let Acquired::Granted { token, .. } = acquire(&app, "A:1", None, "test", "local").await.unwrap() else { panic!() };
        tt::make_table_unreadable(&app, "build_slots").await;
        let (code, body) = post_release(State(app.clone()), Form(ReleaseIn { holder: "A:1".into(), token: token.clone() })).await;
        tt::make_table_readable(&app, "build_slots").await;
        assert_eq!(code, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0["released"], false);
    }

    /// 任何一顆 bot（含被 prompt injection 的）都能呼叫 acquire，`holder` 又是呼叫端自己取的：每個新 holder 一列，
    /// 不設上限的話，一顆 bot 用不同 holder 狂送就能把佇列塞滿、讓真正的建置排不到（FIFO 擋在最前面的是幽靈），
    /// 也能把整張表撐大。每顆 bot 最多同時佔 [`MAX_ROWS_PER_BOT`] 列；已經有的列重送照舊（冪等），別顆 bot 不受影響。
    #[tokio::test]
    async fn a_bot_cannot_flood_the_build_queue_with_holders() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        for i in 0..MAX_ROWS_PER_BOT {
            let r = acquire(&app, &format!("flood:{i}"), Some("BOTA"), "x", "local").await.unwrap();
            assert!(matches!(r, Acquired::Granted { .. } | Acquired::Waiting { .. }), "第 {i} 列：{r:?}");
        }
        assert_eq!(acquire(&app, "flood:extra", Some("BOTA"), "x", "local").await.unwrap(), Acquired::TooManyForBot);
        // 已經有的列重送不受影響（waiting 的重 poll、held 的重問）。
        assert!(!matches!(acquire(&app, "flood:1", Some("BOTA"), "x", "local").await.unwrap(), Acquired::TooManyForBot));
        // 別顆 bot 照樣排得進去。
        assert!(matches!(acquire(&app, "b:1", Some("BOTB"), "x", "local").await.unwrap(), Acquired::Waiting { .. }));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_slots").fetch_one(&app.db).await.unwrap();
        assert_eq!(rows as usize, MAX_ROWS_PER_BOT + 1, "表不會被撐大");
        // 手動（UI token，沒有 bot 身分）不受這個上限管：那是人在操作。
        assert!(matches!(acquire(&app, "manual:1", None, "x", "local").await.unwrap(), Acquired::Waiting { .. }));
    }

    /// holder／purpose／host 是呼叫端給的字串，原樣進 DB 再顯示在網頁：不設長度上限等於讓呼叫端往 DB 寫任意大的東西。
    #[tokio::test]
    async fn oversized_holder_purpose_and_host_are_rejected() {
        let env = tt::env().await;
        let app = env.app.clone();
        let mut h = HeaderMap::new();
        h.insert("X-AM-Token", app.ui_token.parse().unwrap());
        let ok = |holder: &str, purpose: &str, host: &str| AcquireIn { holder: holder.into(), bot_id: None, purpose: purpose.into(), host: host.into() };
        for (holder, purpose, host) in [("h".repeat(MAX_FIELD_CHARS + 1), String::new(), "local".to_string()), ("h".into(), "p".repeat(MAX_FIELD_CHARS + 1), "local".into()), ("h".into(), String::new(), "x".repeat(MAX_FIELD_CHARS + 1))] {
            let err = post_acquire(State(app.clone()), h.clone(), Form(ok(&holder, &purpose, &host))).await.unwrap_err();
            assert!(matches!(err, LcError::Bad(_)), "{err:?}");
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_slots").fetch_one(&app.db).await.unwrap();
        assert_eq!(rows, 0, "被擋下來的不能留下任何一列");
        let _ = post_acquire(State(app.clone()), h, Form(ok("h", "p", "local"))).await.unwrap();
    }

    /// The acquire form is not the only path accepting an untrusted holder: renew/release carry it too.
    /// Keep the same field bound there so a caller cannot bypass the DB/log cap with a long holder.
    #[tokio::test]
    async fn oversized_holders_are_rejected_by_renew_and_release_too() {
        let env = tt::env().await;
        let app = env.app.clone();
        let Acquired::Granted { token, .. } = acquire(&app, "held", None, "test", "local").await.unwrap() else { panic!() };
        let huge = "h".repeat(MAX_FIELD_CHARS + 1);

        let renew_error = post_renew(State(app.clone()), Form(RenewIn { holder: huge.clone(), token: token.clone() })).await.unwrap_err();
        assert!(matches!(renew_error, LcError::Bad(_)), "renew bypassed the holder field cap: {renew_error:?}");

        let (code, body) = post_release(State(app.clone()), Form(ReleaseIn { holder: huge, token: token.clone() })).await;
        assert_eq!(code, axum::http::StatusCode::BAD_REQUEST, "release bypassed the holder field cap: {body:?}");
        assert_eq!(body.0["released"], false);
        assert!(matches!(renew(&app, "held", &token).await.unwrap(), Ok(_)), "rejected oversized requests must leave the real lease intact");
    }

    /// 核心驗收條件（issue #90）：N 個同時的 acquire，只有設定的名額數真的拿到，其餘回 waiting。
    #[tokio::test]
    async fn only_the_configured_number_of_concurrent_acquires_are_granted() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 2).await;

        let mut handles = Vec::new();
        for i in 0..5 {
            let app = app.clone();
            handles.push(tokio::spawn(async move { acquire(&app, &format!("agent-{i}:{i}"), None, "test", "local").await.unwrap() }));
        }
        let results: Vec<Acquired> = futures::future::join_all(handles).await.into_iter().map(|r| r.unwrap()).collect();
        let granted = results.iter().filter(|r| matches!(r, Acquired::Granted { .. })).count();
        let waiting = results.iter().filter(|r| matches!(r, Acquired::Waiting { .. })).count();
        assert_eq!(granted, 2, "{results:?}");
        assert_eq!(waiting, 3, "{results:?}");

        let s = status(&app).await.unwrap();
        assert_eq!(s["active"], 2);
        assert_eq!(s["slots"].as_array().unwrap().len(), 5, "等待中的也看得到");
    }

    /// issue #428：本機隊伍排得再長，也要看得出來是不是因為外部編譯主機連不上。
    /// 「沒開」「開了但還沒有人試過」「上一次連不上」是三種狀態，不能混成同一個布林。
    #[tokio::test]
    async fn the_status_says_whether_the_remote_build_host_was_reachable_last_time() {
        let env = tt::env().await;
        let app = env.app.clone();

        let s = status(&app).await.unwrap();
        assert_eq!(s["remote"]["enabled"], json!(false), "預設沒開");
        assert_eq!(s["remote"]["remote_reachable"], json!(null), "沒開就不是 false——那會亮一個假的紅燈");

        app.cfg
            .update(|cfg| {
                cfg.build.remote.enabled = true;
                cfg.build.remote.user = "me".into();
                cfg.build.remote.host = "box".into();
                Ok(())
            })
            .await
            .unwrap();
        let s = status(&app).await.unwrap();
        assert_eq!(s["remote"]["target"], json!("me@box:22"));
        assert_eq!(s["remote"]["remote_reachable"], json!(null), "開了但還沒有人試過：不知道");

        std::fs::create_dir_all(&app.data_dir).unwrap();
        crate::remote_health::record(
            &app.data_dir,
            &crate::remote_health::Health {
                reachable: false,
                checked_at: "2026-09-24T10:00:00.000Z".into(),
                target: "me@box:22".into(),
                reason: Some("ssh 回 255".into()),
            },
        )
        .unwrap();
        let s = status(&app).await.unwrap();
        assert_eq!(s["remote"]["remote_reachable"], json!(false));
        assert_eq!(s["remote"]["checked_at"], json!("2026-09-24T10:00:00.000Z"), "什麼時候的結論要講");
        assert_eq!(s["remote"]["reason"], json!("ssh 回 255"), "原因照 helper 記的帶出來");
    }

    /// 放掉一個名額之後，等待中的下一次 acquire 就能拿到——不是永遠卡住。
    #[tokio::test]
    async fn releasing_a_slot_frees_capacity_for_a_waiter() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;

        let Acquired::Granted { token, .. } = acquire(&app, "first", None, "test", "local").await.unwrap() else { panic!() };
        let Acquired::Waiting { .. } = acquire(&app, "second", None, "test", "local").await.unwrap() else { panic!("滿了應該要等") };

        release(&app, "first", &token).await.unwrap();
        let Acquired::Granted { .. } = acquire(&app, "second", None, "test", "local").await.unwrap() else { panic!("放掉了，下一個該拿到") };
    }

    /// 持有者沒有 renew、TTL 過期：下一次 acquire 收回這個名額，不是永遠卡死（daemon 重啟也一樣，
    /// 因為 acquire 每次都先 reap 過期列——不需要額外處理「重啟後」這個特例）。
    #[tokio::test]
    async fn a_holder_that_stops_renewing_loses_its_slot_after_ttl() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;

        acquire(&app, "dead", None, "test", "local").await.unwrap();
        // 模擬 TTL 已過（不用真的等），直接把 expires_at 撥回過去。
        sqlx::query("UPDATE build_slots SET expires_at = '2020-01-01T00:00:00.000Z' WHERE holder = 'dead'").execute(&app.db).await.unwrap();

        let Acquired::Granted { .. } = acquire(&app, "new-holder", None, "test", "local").await.unwrap() else {
            panic!("過期的名額應該被收回，換人拿到")
        };
    }

    /// 重call 已經握著的名額是幂等的：同一個 holder 再 acquire 一次拿回同一個 token，不會被降級成 waiting。
    #[tokio::test]
    async fn re_acquiring_an_already_held_slot_is_idempotent() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        let Acquired::Granted { token: t1, .. } = acquire(&app, "me", None, "test", "local").await.unwrap() else { panic!() };
        let Acquired::Granted { token: t2, .. } = acquire(&app, "me", None, "test", "local").await.unwrap() else { panic!("已經握著的不該變成 waiting") };
        assert_eq!(t1, t2);
    }

    /// A build-slot token is a bearer secret. It must not reuse the daemon's public, monotonic ULID
    /// generator, whose next value can be predicted from any ID exposed in the same millisecond.
    #[tokio::test]
    async fn build_slot_tokens_are_independent_128_bit_bearer_secrets() {
        let env = tt::env().await;
        let Acquired::Granted { token, .. } = acquire(&env.app, "secret-check", None, "test", "local").await.unwrap() else { panic!() };
        assert_eq!(token.len(), 32, "slot bearer tokens need 128 random bits, not a timestamped ULID: {token}");
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "slot token must be lower-case hex: {token}");
    }

    /// renew 要對得上 token 才續得動；沒有這一列（沒拿過／已被收回）一律要求重新 acquire。
    #[tokio::test]
    async fn renew_checks_the_token_and_refuses_a_slot_nobody_holds() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        let Acquired::Granted { token, expires_at: first_exp } = acquire(&app, "me", None, "test", "local").await.unwrap() else { panic!() };

        assert_eq!(renew(&app, "me", "wrong-token").await.unwrap(), Err(RenewErr::TokenMismatch));
        assert_eq!(renew(&app, "nobody", "anything").await.unwrap(), Err(RenewErr::NotFound));

        // 毫秒級的時間戳：兩次呼叫緊接在一起，機器夠快就可能落在同一毫秒——睡一下確保牆上時間真的往前走，
        // 不然這條斷言測的是「機器夠不夠慢」而不是「續約有沒有真的延長」。
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let Ok(new_exp) = renew(&app, "me", &token).await.unwrap() else { panic!() };
        assert!(new_exp > first_exp, "續約要往後延");
    }

    #[tokio::test]
    async fn a_forged_token_cannot_release_another_holders_slot() {
        let env = tt::env().await;
        let app = env.app.clone();
        let Acquired::Granted { token, .. } = acquire(&app, "victim", Some("BOTA"), "test", "local").await.unwrap() else { panic!() };

        // Release is intentionally idempotent and reports success for an unknown token. Check the
        // stored lease itself so an unauthenticated caller cannot free somebody else's capacity.
        let (code, body) = post_release(
            State(app.clone()),
            Form(ReleaseIn { holder: "victim".into(), token: "guessed-token".into() }),
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::OK);
        assert_eq!(body.0["released"], true);
        assert!(matches!(renew(&app, "victim", &token).await.unwrap(), Ok(_)), "the valid lease must remain held");
    }

    /// sweep 收掉過期的 held 與太久沒 poll 的 waiting；還在正常範圍內的 waiting 不動（正在排隊，只是隊伍長）。
    /// 過期、太久沒 poll 都用直接改 DB 模擬「時間過去了」，不透過 acquire——acquire 自己也會 lazy reap
    /// 過期的 held 列，在這裡呼叫只會混淆「到底是誰收的」。
    #[tokio::test]
    async fn sweep_reaps_dead_held_and_stale_waiting_but_leaves_live_waiters() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        acquire(&app, "holder", None, "test", "local").await.unwrap();
        let Acquired::Waiting { .. } = acquire(&app, "dead-waiter", None, "test", "local").await.unwrap() else { panic!("滿了應該要等") };
        let Acquired::Waiting { .. } = acquire(&app, "live-waiter", None, "test", "local").await.unwrap() else { panic!("滿了應該要等") };

        sqlx::query("UPDATE build_slots SET expires_at = '2020-01-01T00:00:00.000Z' WHERE holder = 'holder'").execute(&app.db).await.unwrap();
        // dead-waiter 早就不再 poll 了；live-waiter 這一刻還在 poll（last_seen 不動，維持剛剛 acquire 留下的現在時刻）。
        sqlx::query("UPDATE build_slots SET last_seen = '2020-01-01T00:00:00.000Z' WHERE holder = 'dead-waiter'").execute(&app.db).await.unwrap();

        let (held, waiting) = sweep(&app).await;
        assert_eq!((held, waiting), (1, 1));
        let s = status(&app).await.unwrap();
        let holders: Vec<String> = s["slots"].as_array().unwrap().iter().map(|v| v["holder"].as_str().unwrap().to_string()).collect();
        assert_eq!(holders, vec!["live-waiter".to_string()], "holder 過期收掉、dead-waiter 太久沒 poll 收掉，live-waiter 還在排隊沒被誤收");
    }

    /// FIFO（使用者 2026-09-18 交辦）：名額空出來時只有排最前面的拿得到，就算別人這一刻剛好也在問、
    /// 名額也剛好空著。用「後進場的先發問」故意打亂 poll 順序，證明放行順序看的是**進場順序**不是**發問順序**。
    #[tokio::test]
    async fn waiters_are_granted_in_the_order_they_first_queued_not_the_order_they_poll_in() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;

        let Acquired::Granted { token: first_token, .. } = acquire(&app, "first", None, "t", "local").await.unwrap() else { panic!() };
        // 進場順序：a, b, c（每個之間睡一下，確保 since 的毫秒級排序穩定）。
        let Acquired::Waiting { .. } = acquire(&app, "a", None, "t", "local").await.unwrap() else { panic!() };
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let Acquired::Waiting { .. } = acquire(&app, "b", None, "t", "local").await.unwrap() else { panic!() };
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let Acquired::Waiting { .. } = acquire(&app, "c", None, "t", "local").await.unwrap() else { panic!() };

        release(&app, "first", &first_token).await.unwrap();

        // 發問順序刻意倒過來：c 先問、b 再問、a 最後問——沒有一個排在 a 前面拿得到。
        let Acquired::Waiting { .. } = acquire(&app, "c", None, "t", "local").await.unwrap() else { panic!("c 排最後，不該搶到") };
        let Acquired::Waiting { .. } = acquire(&app, "b", None, "t", "local").await.unwrap() else { panic!("b 前面還有 a，不該搶到") };
        let Acquired::Granted { token: a_token, .. } = acquire(&app, "a", None, "t", "local").await.unwrap() else { panic!("a 排最早，該輪到它") };

        release(&app, "a", &a_token).await.unwrap();
        let Acquired::Waiting { .. } = acquire(&app, "c", None, "t", "local").await.unwrap() else { panic!("c 還是排最後") };
        let Acquired::Granted { .. } = acquire(&app, "b", None, "t", "local").await.unwrap() else { panic!("該輪到 b 了") };
    }

    /// 排最前面的號碼牌死了（不再 poll）：不能永遠擋住後面活著的人（使用者實測手工腳本的舊版本會餓死 74 分鐘）。
    #[tokio::test]
    async fn a_dead_waiter_at_the_front_of_the_queue_does_not_block_the_ones_behind_it() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;

        let Acquired::Granted { token, .. } = acquire(&app, "first", None, "t", "local").await.unwrap() else { panic!() };
        let Acquired::Waiting { .. } = acquire(&app, "dead-front", None, "t", "local").await.unwrap() else { panic!() };
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let Acquired::Waiting { .. } = acquire(&app, "alive-second", None, "t", "local").await.unwrap() else { panic!() };

        // dead-front 排最前面，但早就不再 poll 了。
        sqlx::query("UPDATE build_slots SET last_seen = '2020-01-01T00:00:00.000Z' WHERE holder = 'dead-front'").execute(&app.db).await.unwrap();
        release(&app, "first", &token).await.unwrap();

        let Acquired::Granted { .. } = acquire(&app, "alive-second", None, "t", "local").await.unwrap() else {
            panic!("死掉的號碼牌不該永遠擋住後面活著的人")
        };
        // dead-front 的列也該一併被清掉，不是留著佔 GET /build-slots 的版面。
        let s = status(&app).await.unwrap();
        let holders: Vec<String> = s["slots"].as_array().unwrap().iter().map(|v| v["holder"].as_str().unwrap().to_string()).collect();
        assert!(!holders.contains(&"dead-front".to_string()), "{holders:?}");
    }

    /// issue #813：拿到名額的回應與狀態都帶 `test_threads`（shim 拿它注入 `RUST_TEST_THREADS`），預設 8；設定超過上限的夾到 256。
    #[tokio::test]
    async fn the_grant_tells_the_shim_how_many_test_threads_a_slot_gets() {
        let env = tt::env().await;
        let app = env.app.clone();
        let h = auth_headers(None, Some("test-token"), None);
        let out = post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "manual:host:813".into(), bot_id: None, purpose: "test".into(), host: "local".into() }))
            .await
            .unwrap();
        assert_eq!(out.0["granted"], true, "{}", out.0);
        assert_eq!(out.0["test_threads"], 8, "{}", out.0);
        assert_eq!(status(&app).await.unwrap()["test_threads"], 8);

        let huge = crate::config::BuildCfg { test_threads: 100_000, ..crate::config::BuildCfg::default() };
        assert_eq!(huge.test_threads(), crate::config::MAX_BUILD_TEST_THREADS);
        let off = crate::config::BuildCfg { test_threads: 0, ..crate::config::BuildCfg::default() };
        assert_eq!(off.test_threads(), 0, "0＝不設，交給 libtest 的預設");
    }

    fn auth_headers(bot_token: Option<&str>, ui_token: Option<&str>, bot_id: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(t) = bot_token {
            h.insert("X-AM-Bot-Token", t.parse().unwrap());
        }
        if let Some(t) = ui_token {
            h.insert("X-AM-Token", t.parse().unwrap());
        }
        if let Some(id) = bot_id {
            h.insert("X-AM-Bot-Id", id.parse().unwrap());
        }
        h
    }

    /// bot 用自己的 hook token 就能參與排程，不需要一般 UI token（bot 的 pane 裡本來就拿不到那個）；
    /// 人工 host shell 用 UI token 一樣放行；兩個都沒有／都不對 → 401。
    #[tokio::test]
    async fn build_slot_bot_or_user_credentials_are_exclusive() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = a_bot(&env, "tok-123").await;

        let h = auth_headers(Some("tok-123"), None, Some(&bot_id));
        let out = post_acquire(
            State(app.clone()),
            h,
            Form(AcquireIn { holder: "b1".into(), bot_id: Some(bot_id.clone()), purpose: "test".into(), host: "local".into() }),
        )
        .await
        .unwrap();
        assert_eq!(out.0["granted"], true);

        let h = auth_headers(None, Some("test-token"), None);
        let out = post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "manual:host:1".into(), bot_id: None, purpose: "".into(), host: "local".into() }))
            .await
            .unwrap();
        assert_eq!(out.0["granted"], true);

        // Old shims that send body bot_id plus the hook token (no X-AM-Bot-Id) still authenticate during
        // pane migration. Both slots are taken by now, so this one queues — the point is it is not refused.
        let h = auth_headers(Some("tok-123"), None, None);
        let out = post_acquire(
            State(app.clone()),
            h,
            Form(AcquireIn { holder: "b1-legacy".into(), bot_id: Some(bot_id.clone()), purpose: "test".into(), host: "local".into() }),
        )
        .await
        .unwrap();
        assert!(out.0["granted"].is_boolean(), "{}", out.0);

        let h = auth_headers(Some("wrong"), None, Some(&bot_id));
        assert!(matches!(
            post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "b2".into(), bot_id: Some(bot_id.clone()), purpose: "".into(), host: "local".into() })).await,
            Err(LcError::Forbidden(_))
        ));

        let h = auth_headers(Some("wrong"), Some("test-token"), Some(&bot_id));
        assert!(matches!(
            post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "b4".into(), bot_id: Some(bot_id.clone()), purpose: "".into(), host: "local".into() })).await,
            Err(LcError::Forbidden(_))
        ), "a bad Bot proof plus valid UI token must not downgrade to User");

        let h = auth_headers(Some("tok-123"), None, Some("another-bot"));
        assert!(matches!(
            post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "b5".into(), bot_id: Some(bot_id.clone()), purpose: "".into(), host: "local".into() })).await,
            Err(LcError::Forbidden(_))
        ), "header identity must match the bot id used by the scheduler");

        let h = HeaderMap::new();
        assert!(matches!(
            post_acquire(State(app.clone()), h, Form(AcquireIn { holder: "b3".into(), bot_id: None, purpose: "".into(), host: "local".into() })).await,
            Err(LcError::Forbidden(_))
        ));
    }

    /// A holder string is caller-chosen and must not let one bot recover another bot's lease token
    /// or take over its FIFO position.
    #[tokio::test]
    async fn a_bot_cannot_reuse_another_bots_build_slot_holder() {
        let env = tt::env().await;
        let app = env.app.clone();
        set_max_concurrent(&app, 1).await;
        let owner_id = a_bot(&env, "owner-token").await;
        let attacker_id = a_bot(&env, "attacker-token").await;

        let owner = post_acquire(
            State(app.clone()),
            auth_headers(Some("owner-token"), None, Some(&owner_id)),
            Form(AcquireIn { holder: "shared-holder".into(), bot_id: Some(owner_id.clone()), purpose: "owner".into(), host: "local".into() }),
        )
        .await
        .unwrap();
        let owner_token = owner.0["token"].as_str().unwrap().to_string();

        let denied = post_acquire(
            State(app.clone()),
            auth_headers(Some("attacker-token"), None, Some(&attacker_id)),
            Form(AcquireIn {
                holder: "shared-holder".into(),
                bot_id: Some(attacker_id.clone()),
                purpose: "take owner lease".into(),
                host: "local".into(),
            }),
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(denied.status(), axum::http::StatusCode::FORBIDDEN);

        // The owner still gets the same secret; the attacker did not replace the row.
        let owner_again = post_acquire(
            State(app.clone()),
            auth_headers(Some("owner-token"), None, Some(&owner_id)),
            Form(AcquireIn { holder: "shared-holder".into(), bot_id: Some(owner_id.clone()), purpose: "owner".into(), host: "local".into() }),
        )
        .await
        .unwrap();
        assert_eq!(owner_again.0["token"].as_str(), Some(owner_token.as_str()));

        // The same check protects queued rows: a bot cannot replace another bot's saved queue entry.
        release(&app, "shared-holder", &owner_token).await.unwrap();
        let blocker = acquire(&app, "blocker", None, "test", "local").await.unwrap();
        let Acquired::Waiting { .. } = acquire(&app, "queued-holder", Some(&owner_id), "owner", "local").await.unwrap() else {
            panic!("the owner's second holder should wait")
        };
        let denied = post_acquire(
            State(app.clone()),
            auth_headers(Some("attacker-token"), None, Some(&attacker_id)),
            Form(AcquireIn {
                holder: "queued-holder".into(),
                bot_id: Some(attacker_id),
                purpose: "take queue position".into(),
                host: "local".into(),
            }),
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(denied.status(), axum::http::StatusCode::FORBIDDEN);

        let queued_owner: Option<String> = sqlx::query_scalar("SELECT bot_id FROM build_slots WHERE holder = 'queued-holder'")
            .fetch_optional(&app.db)
            .await
            .unwrap();
        assert_eq!(queued_owner.as_deref(), Some(owner_id.as_str()));
        if let Acquired::Granted { token, .. } = blocker {
            release(&app, "blocker", &token).await.unwrap();
        } else {
            panic!("the blocker should hold the only slot")
        }
    }
}

/// build 名額的序列化鎖。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait BuildSlotLock: Send + Sync {
    fn build_slot_lock(&self) -> &tokio::sync::Mutex<()>;
}
