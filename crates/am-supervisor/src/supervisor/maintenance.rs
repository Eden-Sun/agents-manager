//! Approvals and execution leases for rebuild / restart windows.
//!
//! Before this, "AGM said yes" lived in a chat message and the safety check was a snapshot: the
//! runtime update script read `working` bots once, decided the coast was clear, and then spent
//! several minutes building and swapping a binary during which anything could start. Two bots
//! could both be told "go when it is quiet" and both believe it was quiet.
//!
//! So a window has two halves, and they are deliberately separate calls:
//!
//! 1. **Wait for a safe window** — [`safety`] is a read. Poll it as often as you like; it
//!    promises nothing about the next second.
//! 2. **Take the window** — [`acquire`] re-checks the same conditions *and* takes the lease in
//!    one locked step, so nothing can slip in between the check and the hold. While a `restart`
//!    lease is held new assignments are not dispatched (they stay queued), which is the half
//!    the snapshot approach could never do.
//!
//! **What the pause covers (issue #86):** holding a `restart` lease is a **daemon-wide prompt
//! admission fence** — [`window_held`] is the one definition, and all three entrances read it:
//! `controller::dispatch` (assignments stay queued), `lifecycle::prompt` (new turns are refused
//! with 409 `maintenance_window`) and `lifecycle::queue::flush_queued_locked` (a queued prompt
//! waits instead of going into the pane). It used to gate assignment dispatch only, so the very
//! thing the window promises — nothing new starts in here — was true on one channel and false on
//! the two that actually type into panes.
//!
//! **What it deliberately does not cover**: the user's own keyboard. The fence is on daemon-side
//! admission; somebody typing into a pane in their terminal is not going through this daemon and
//! is not ours to block. Nor are the daemon's own control-plane prompts (the AGM / responder
//! start handshakes): a window that blocked those would lock the box out of the very operation it
//! was opened for.
//!
//! The other honest limit, for the same reason: an arbitrary shell on this machine can still
//! `kill` the daemon without asking anybody. The lease binds the paths that go through this API
//! and the operational scripts in `scripts/ops/`; it is not an OS-level mutex.

use crate::lifecycle::LcError;
use sqlx::SqlitePool;
use serde_json::{json, Value};

use super::store;

/// Resources a lease can be taken on. Anything else is refused: a typo must not silently create
/// a private lock that protects nothing.
pub const RESOURCES: [&str; 2] = ["rebuild", "restart"];

/// Holding one of these is the daemon-wide admission fence ([`window_held`]): assignment
/// dispatch, new prompts and queued-prompt flushes all stop. A rebuild does not interrupt
/// anybody; a restart does, and handing a bot new work while waiting to kill its session is the
/// race this closes.
pub const EXCLUSIVE: [&str; 1] = ["restart"];

/// 等太久就縮小封鎖面（SPEC §18.10）：一筆已核准、還沒用掉的窗口從**核准時間**起等超過這麼多分鐘，
/// 安全檢查就從「全靜止才 safe」換成「只有送達臨界區／還握著的租約／讀不到畫面才擋」。
///
/// 為什麼要有這條：這台機器上隨時有人在跟 bot 講話，「任何 bot 在回合中就不換」在這個負載下
/// 等同永遠不安全——2026-09-15 那筆核准因此卡了 11 小時。放寬有界線，不是「有人講話也照換」。
///
/// 2026-10-04 使用者：「太久，改成 5 分鐘或者累積三個版本」——原本 30 分鐘，AGM 底下 l1–l8 一直在忙時每次部署都等滿；
/// 另見 [`ESCALATE_BEHIND_COMMITS`]。
pub const ESCALATE_AFTER_MINS: i64 = 5;
/// 線上 binary 落後 origin/main 這麼多個「動到程式碼」的 commit 就不再等，直接放寬（同上，使用者 2026-10-04）。
pub const ESCALATE_BEHIND_COMMITS: u64 = 3;
/// 覆寫上面那個門檻（分鐘）。0、負數或看不懂的值一律當沒設，回到預設。
pub const ESCALATE_ENV: &str = "AM_MAINTENANCE_ESCALATE_MINS";

pub fn escalate_after_secs() -> i64 {
    parse_escalate_mins(std::env::var(ESCALATE_ENV).ok().as_deref()) * 60
}

/// 純函式，測得到：看不懂、0 或負數都回預設，不要讓一個手滑的環境變數把門檻變成「馬上放寬」。
pub fn parse_escalate_mins(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok()).filter(|m| *m > 0).unwrap_or(ESCALATE_AFTER_MINS)
}

/// 兩個 RFC3339 之間差幾秒。看不懂的時間回 0——不知道等了多久就不該升級。
pub fn waited_secs(since: &str, now: &str) -> i64 {
    match (chrono::DateTime::parse_from_rfc3339(since), chrono::DateTime::parse_from_rfc3339(now)) {
        (Ok(a), Ok(b)) => (b - a).num_seconds().max(0),
        _ => 0,
    }
}

/// 最早那筆還沒用掉的窗口核准等了多久，以及是不是已經超過門檻。
#[derive(Debug, Clone)]
pub struct Escalation {
    pub approval_id: String,
    pub waited_secs: i64,
    pub escalated: bool,
}

/// 誰等太久就放寬誰（AGM 裁示 2026-09-16）：`approval_id` 給了就只看**那一筆**核准等了多久，
/// 別人放著沒用掉的核准不算數。沒給（唯讀查詢，還不知道會用哪一筆）才退回最早那筆還活著的。
pub async fn escalation_for(app: &(impl crate::capabilities::Db + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView), approval_id: Option<&str>) -> Result<Option<Escalation>, LcError> {
    let now = crate::db::now();
    let found = match approval_id {
        Some(id) => store::approval(app.db(), id)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?
            .filter(|a| a.status == "approved" && RESOURCES.contains(&a.purpose.as_str()))
            .filter(|a| !a.effective_expiry().as_deref().is_some_and(|t| crate::db::cmp_ts(t, &now).is_le())),
        None => store::oldest_live_window_approval(app.db(), &now).await.map_err(|e| LcError::Upstream(e.to_string()))?,
    };
    let Some(a) = found else { return Ok(None) };
    let Some(since) = a.waiting_since() else { return Ok(None) };
    let waited = waited_secs(since, &now);
    let by_user = app.deploy_user_escalated_for(&a);
    let escalated = by_user || waited >= escalate_after_secs() || behind_escalates(behind_code_commits(app).await);
    Ok(Some(Escalation { approval_id: a.id, waited_secs: waited, escalated }))
}

/// 線上 binary 落後 origin/main 幾個動到程式碼的 commit（同部署狀態 `GET /api/deploy/status` 的 `code_commits`）。
/// 安全檢查每 15 秒就問一次，算一次要跑幾個 git，所以快取 60 秒；算不出來（沒有 repo、線上 sha 不明）當 0——不知道就不放寬。
pub fn behind_escalates(code_commits: u64) -> bool {
    code_commits >= ESCALATE_BEHIND_COMMITS
}

async fn behind_code_commits(app: &impl crate::supervisor::ports::HostProbes) -> u64 {
    if cfg!(any(test, feature = "test-hooks")) {
        return 0;
    }
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, u64)>> = std::sync::Mutex::new(None);
    if let Some((at, n)) = *CACHE.lock().unwrap_or_else(|p| p.into_inner()) {
        if at.elapsed() < std::time::Duration::from_secs(60) {
            return n;
        }
    }
    let n = match app.deploy_behind().await {
        Ok(v) => v.get("code_commits").and_then(serde_json::Value::as_u64).unwrap_or(0),
        Err(_) => 0,
    };
    *CACHE.lock().unwrap_or_else(|p| p.into_inner()) = Some((std::time::Instant::now(), n));
    n
}

/// 還握著的租約。縮小封鎖面時這是**唯一**新增的阻擋條件：窗口一次只給一個人。
///
/// `own` 標出「就是發問的這個 owner 自己握的」（`owner` 給了才會是 true）。**自己的租約不擋自己**
/// （AGM 2026-09-16，58d3587 的規格漏洞）：標準換版是同一人先拿 rebuild、build 完再拿 restart，
/// 把自己手上的 rebuild 也算成「別人握著窗口」，restart 就會被卡到 rebuild 自己到期為止。
async fn held_leases(db: &SqlitePool, owner: Option<&str>) -> Result<Vec<Value>, LcError> {
    let now = crate::db::now();
    let mut out = Vec::new();
    for resource in RESOURCES {
        let l = store::lease(db, resource).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        if let Some(l) = l.filter(|l| l.held_at(&now)) {
            let own = owner.is_some_and(|o| l.owner.as_deref() == Some(o));
            out.push(json!({"resource": resource, "owner": l.owner, "own": own, "fence": l.fence, "expires_at": l.expires_at}));
        }
    }
    Ok(out)
}

/// 測試接縫（#1862 S3b）：`held_leases` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn held_leases_for_test(db: &SqlitePool, owner: Option<&str>) -> Result<Vec<Value>, LcError> {
    held_leases(db, owner).await
}

/// 這顆 bot 此刻在不在**送達臨界區**：有排隊中待送的 prompt（`queued`），或 daemon 正在往 pane
/// 打字／送出（`in_flight` 而 `delivery` 還是 `pending`）。回傳擋住的那一筆 turn id。
///
/// `delivery='unknown'` 不算：那是送完但驗不到、停在那裡等人處理的狀態，跟 `blocked` 一樣可以等很久。
/// queued 只在這顆 bot 還有活著的 run 時才算：沒有 run 就沒有人會送它，是遺留下來的（會被撤銷），
/// 算進來的話 restart safety 永遠判成臨界區，縮小封鎖面之後也 unsafe（AGM 2026-09-16）。
/// run 此刻停在 `blocked`（等使用者回答）也不算：flush 在 blocked 時不會開始打字，queued 跨 daemon 重啟保得住；
/// 不放行的話「有人在等使用者」就讓整台機器不能換版（AGM 裁示 2026-09-16）。一離開 blocked 就立刻回到臨界區——
/// 讀的是 runs 的即時狀態，acquire 在鎖內重判一次、而且**寫的那一句**也帶同一個條件；
/// 已經開始送的 in_flight＋pending 不管 blocked 與否照算。條件本身在 [`store::DELIVERY_CRITICAL_PREDICATE`]。
async fn delivery_critical(pool: &sqlx::SqlitePool, bot_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(&store::delivery_critical_for_bot_sql())
        .bind(bot_id)
        .fetch_optional(pool)
        .await?)
}

/// 測試接縫（#1862 S3b）：`delivery_critical` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn delivery_critical_for_test(pool: &sqlx::SqlitePool, bot_id: &str) -> anyhow::Result<Option<String>> {
    delivery_critical(pool, bot_id).await
}

/// Default and maximum lease lifetime. Long enough for a release build and a restart, short
/// enough that a crashed holder does not block the next window for an afternoon.
pub const DEFAULT_TTL_SECS: i64 = 900;
pub const MAX_TTL_SECS: i64 = 3600;

fn iso_in(secs: i64) -> String {
    crate::db::iso_in(secs)
}

/// 測試接縫（#1862 S3b）：`iso_in` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub fn iso_in_for_test(secs: i64) -> String {
    iso_in(secs)
}

/// 一個還握著的維護窗口，照入場閘門要講給呼叫端聽的樣子。
#[derive(Debug, Clone)]
pub struct WindowHeld {
    pub resource: &'static str,
    pub owner: String,
    pub fence: i64,
    pub expires_at: String,
}

impl WindowHeld {
    /// 409 的 body：**誰**握著、到**什麼時候**，以及還要等幾秒。擋下來而不說清楚這兩件事，
    /// 使用者只會看到「送不出去」然後一直重送（issue #86）。
    pub fn detail(&self) -> Value {
        json!({
            "reason": "maintenance_window",
            "resource": self.resource,
            "held_by": self.owner,
            "fence": self.fence,
            "expires_at": self.expires_at,
            "retry_after_secs": self.retry_after_secs(&crate::db::now()),
            "retryable": true,
            "sent": false,
            "message": format!("{} 正在進行維護（{} 窗口），到 {} 為止不送新的 prompt；窗口關閉或過期就自動恢復。",
                               self.owner, self.resource, self.expires_at),
        })
    }

    /// 還要等幾秒才會自動恢復。看不懂的時間回 0——不知道要等多久就不要叫人等。
    pub fn retry_after_secs(&self, now: &str) -> i64 {
        waited_secs(now, &self.expires_at)
    }

    pub fn refusal(&self) -> LcError {
        LcError::Conflict(self.detail())
    }
}

/// 讀不到窗口狀態時，呼叫端多久之後再判斷一次（秒）。
pub const UNREADABLE_RETRY_SECS: i64 = 10;

/// 入場閘門**讀不到**租約狀態（DB 出錯、那一列解不開、沒放掉卻沒有讀得懂的到期時間）。
///
/// 這**不等於**「沒有窗口」：觀測不到 durable 的租約，不代表已經證明沒有租約。所以三個入口一律
/// **fail closed**——prompt 回 503 `maintenance_state_unavailable`（一個字都不送）、排隊的 turn 留在佇列不 claim、
/// 交辦留 `queued`。錯誤不偽裝成一個假的長 TTL 租約，可觀察（log／回應）、可重試（DB 一恢復就重新判斷，不會卡住）。
#[derive(Debug, Clone)]
pub struct WindowUnreadable {
    pub resource: &'static str,
    pub error: String,
}

impl WindowUnreadable {
    fn new(resource: &'static str, error: impl std::fmt::Display) -> Self {
        Self { resource, error: error.to_string() }
    }

    /// 503 的 body。跟 [`WindowHeld::detail`] 的 `maintenance_window` 分得開：那個是「確定有窗口」，
    /// 這個是「不知道有沒有」。
    pub fn detail(&self) -> Value {
        json!({
            "reason": "maintenance_state_unavailable",
            "resource": self.resource,
            "retry_after_secs": UNREADABLE_RETRY_SECS,
            "retryable": true,
            "sent": false,
            "message": format!("讀不到 {} 維護窗口的狀態，所以不送新的 prompt（不確定有沒有窗口）；稍後原樣重送即可。", self.resource),
        })
    }

    pub fn refusal(&self) -> LcError {
        LcError::Unavailable(self.detail())
    }
}

/// **入場閘門**：現在有沒有人握著會中斷 pane 的維護窗口。
///
/// 這是唯一的定義，三個入口都讀它——`controller::dispatch`（交辦留在佇列）、`lifecycle::prompt`
/// （新回合 409）、`lifecycle::queue::flush_queued_locked`（排隊的 prompt 等窗口關掉再送）。
/// 以前只有第一個有擋，於是窗口承諾的「裡面不會有新東西開始」在另外兩條路上是假的（issue #86）。
///
/// 三態（issue #127）：`Ok(Some)`＝明確有窗口，擋；`Ok(None)`＝明確沒有，放行；`Err`＝讀不到，**也擋**
/// （[`WindowUnreadable`]）。以前 SELECT 失敗會被當成 `None`，窗口明明握著、卻因為一次 DB 讀錯就放新工作進 pane。
///
/// **過期不會鎖死**：判斷走 `held_at`，`expires_at` 一到就自動不再擋，不需要任何人來收尾
/// （租約本身 TTL 上限 1 小時，預設 15 分鐘）；daemon 重啟時 `release_restart_on_startup` 再收一次。
pub async fn window_held(app: &impl crate::capabilities::Db) -> Result<Option<WindowHeld>, WindowUnreadable> {
    let now = crate::db::now();
    for resource in EXCLUSIVE {
        let lease = match store::lease(app.db(), resource).await {
            Ok(l) => l,
            Err(e) => return Err(unreadable(resource, e)),
        };
        let Some(l) = lease else { continue };
        if l.released_at.is_none() && l.expires_at.as_deref().is_none_or(|t| chrono::DateTime::parse_from_rfc3339(t).is_err()) {
            return Err(unreadable(resource, format!("lease is not released but its expires_at is unreadable: {:?}", l.expires_at)));
        }
        if l.held_at(&now) {
            return Ok(Some(WindowHeld {
                resource,
                owner: l.owner.clone().unwrap_or_default(),
                fence: l.fence,
                expires_at: l.expires_at.clone().unwrap_or_default(),
            }));
        }
    }
    Ok(None)
}

/// 讀不到就留一行結構化 log（`error` 等級：這是安全閘門在失效，不是一般的讀取失敗）。
fn unreadable(resource: &'static str, error: impl std::fmt::Display) -> WindowUnreadable {
    let u = WindowUnreadable::new(resource, error);
    tracing::error!(resource, error = %u.error, "restart window state unreadable: failing closed (no new work goes in)");
    u
}

/// Is a restart window currently held by someone? Used by the dispatcher to hold work back.
/// 讀不到回 `Err`：呼叫端要**當作還握著**，不能拿去解除 hold。
pub async fn dispatch_paused(app: &impl crate::capabilities::Db) -> Result<Option<String>, WindowUnreadable> {
    Ok(window_held(app).await?.map(|w| w.expires_at))
}

/// A restart window is over: lift the holds it put on queued assignments so the next controller
/// pass sends them, instead of each waiting out the deadline it was held to. No window is open
/// any more when this runs, so there is nothing left to protect (SPEC §18.10: no grace period).
pub async fn window_closed(db: &SqlitePool, why: &str) -> u64 {
    match store::clear_restart_holds(db).await {
        Ok(n) => {
            if n > 0 {
                tracing::info!(released = n, why, "restart window closed; held assignments go out on the next pass");
            }
            n
        }
        Err(e) => {
            tracing::warn!(error = ?e, why, "could not lift restart-window holds");
            0
        }
    }
}

/// 持有人帶憑證自己交還窗口時，被消耗那張核准的 `reason`。
pub const RELEASED_REASON: &str = "lease released";
/// 強制收掉（daemon 開機收上一輪殘留、AGM `--force`）時的 `reason`。
pub const FORCE_RELEASED_REASON: &str = "lease force-released";

/// Release a lease and consume its approval — one yes, one window. When it was a restart
/// window, the holds it placed are lifted at once.
pub async fn release(
    app: &impl crate::capabilities::Db,
    resource: &str,
    owner: &str,
    fence: i64,
    proof: store::LeaseProof<'_>,
) -> anyhow::Result<bool> {
    let Some(held) = store::lease(app.db(), resource).await? else { return Ok(false) };
    if !proof.allows(held.lease_token.as_deref()) {
        anyhow::bail!("lease_token_mismatch");
    }
    if held.lease_token.is_none() {
        tracing::warn!(resource, owner, "released a lease created before lease tokens existed; no proof was possible");
    }
    let released = if proof.is_forced() {
        store::force_release_lease(app.db(), resource, held.fence).await?
    } else {
        store::release_lease(app.db(), resource, owner, fence).await?
    };
    #[cfg(any(test, feature = "test-hooks"))]
    if released {
        crate::race_point::hit("lease_release_after_mark", held.lease_token.as_deref().unwrap_or(resource)).await;
    }
    if released {
        if let Some(ap) = held.approval_id.as_deref() {
            if let Ok(Some(a)) = store::approval(app.db(), ap).await {
                if a.status == "approved" {
                    let why = if proof.is_forced() { FORCE_RELEASED_REASON } else { RELEASED_REASON };
                    let _ = store::decide_approval_from(app.db(), ap, "approved", "consumed", owner, Some(why), None).await;
                }
            }
        }
        if EXCLUSIVE.contains(&resource) && matches!(dispatch_paused(app).await, Ok(None)) {
            window_closed(app.db(), "lease released").await;
        }
    }
    Ok(released)
}

/// The daemon answering again *is* the end of the restart it was restarted for: a restart lease
/// still held from before the restart is released here, and every hold it placed is lifted.
pub async fn release_restart_on_startup(app: &(impl crate::build_scheduler::BuildSlotLock + crate::capabilities::Db + crate::capabilities::Emit + crate::credential_spawn::CredentialSpawnGate)) {
    for resource in EXCLUSIVE {
        match store::lease(app.db(), resource).await {
            Ok(Some(l)) if l.released_at.is_none() => {
                let owner = l.owner.clone().unwrap_or_default();
                let escalated = serde_json::from_str::<Value>(&l.detail_json)
                    .ok()
                    .and_then(|d| d.get("safety").and_then(|s| s.get("escalated")).and_then(Value::as_bool))
                    .unwrap_or(false);
                match release(app, resource, &owner, l.fence, store::LeaseProof::Forced).await {
                    Ok(true) => {
                        tracing::info!(resource, owner, fence = l.fence, escalated, "daemon started: released the restart lease left from before the restart");
                        if escalated {
                            tracing::warn!(resource, owner, "上一次換版是升級後（縮小封鎖面）才拿到窗口的，不是等到全靜止");
                        }
                        app.emit("supervisor_changed", json!({"lease": store::lease(app.db(), resource).await.ok().flatten().map(|l| l.to_json())})).await;
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(resource, error = ?e, "daemon started: could not release the restart lease"),
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(resource, error = ?e, "daemon started: could not read the restart lease"),
        }
    }
    window_closed(app.db(), "daemon started").await;
}

/// What is going on that a restart would interrupt.
///
/// `blocked` panes are counted but are *not* a reason to refuse: a pane waiting for a human can
/// wait for hours, and the restart path skips blocked panes anyway. What blocks a window is work
/// actually running.
pub async fn safety(app: &(impl crate::capabilities::Db + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView), exclude: &[String]) -> Result<Value, LcError> {
    safety_for(app, exclude, None).await
}

/// 同一份檢查，但升級的計時綁在 `approval_id` 那一筆核准上（見 [`escalation_for`]）。
pub async fn safety_for(app: &(impl crate::capabilities::Db + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView), exclude: &[String], approval_id: Option<&str>) -> Result<Value, LcError> {
    safety_as(app, exclude, approval_id, None).await
}

/// 再多一個 `owner`：以這個人的身份問，**他自己握的租約不算擋**（只在縮小封鎖面時有差，
/// 全靜止模式本來就不看租約）。不給 `owner` 就是舊行為：每一把租約都算擋。
pub async fn safety_as(
    app: &(impl crate::capabilities::Db + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView),
    exclude: &[String],
    approval_id: Option<&str>,
    owner: Option<&str>,
) -> Result<Value, LcError> {
    let bots = crate::db::live_bots(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let sup = store::get_or_init(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let mut working = Vec::new();
    let mut blocked = Vec::new();
    let mut in_flight = Vec::new();
    let mut delivering = Vec::new();
    let mut unreadable = Vec::new();
    for b in &bots {
        if exclude.contains(&b.id) {
            continue;
        }
        match delivery_critical(app.db(), &b.id).await {
            Ok(Some(turn_id)) => delivering.push(json!({"bot_id": b.id, "name": b.name, "turn_id": turn_id})),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(bot = %b.id, error = ?e, "safety probe could not read this bot's delivery state");
                unreadable.push(json!({"bot_id": b.id, "name": b.name}));
            }
        }
        let run = match crate::db::active_run(app.db(), &b.id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(bot = %b.id, error = ?e, "safety probe could not read this bot's run");
                unreadable.push(json!({"bot_id": b.id, "name": b.name}));
                continue;
            }
        };
        let Some(run) = run else { continue };
        if run.agent_status == "working" {
            working.push(json!({"bot_id": b.id, "name": b.name, "is_supervisor": Some(&b.id) == sup.bot_id.as_ref()}));
        } else if run.agent_status == "blocked" {
            blocked.push(json!({"bot_id": b.id, "name": b.name}));
        }
        match crate::db::in_flight_turn(app.db(), &run.id).await {
            Ok(Some(t)) => in_flight.push(json!({"bot_id": b.id, "turn_id": t.id})),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(bot = %b.id, error = ?e, "safety probe could not read this bot's in-flight turn");
                unreadable.push(json!({"bot_id": b.id, "name": b.name}));
            }
        }
    }
    let open = store::open_assignments(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let held = held_leases(app.db(), owner).await?;
    let held_by_others = held.iter().filter(|l| l.get("own") != Some(&Value::Bool(true))).count();
    let esc = escalation_for(app, approval_id).await?;
    let strict = working.is_empty() && in_flight.is_empty() && unreadable.is_empty();
    let escalated = esc.as_ref().is_some_and(|e| e.escalated);
    let relaxed = escalated && delivering.is_empty() && unreadable.is_empty() && held_by_others == 0;
    let safe = strict || relaxed;
    Ok(json!({
        "safe": safe,
        "escalated": escalated,
        "waited_secs": esc.as_ref().map(|e| e.waited_secs),
        "escalation_approval_id": esc.as_ref().map(|e| e.approval_id.clone()),
        "escalate_after_secs": escalate_after_secs(),
        "delivering": delivering,
        "held_leases": held,
        "owner": owner,
        "working": working,
        "in_flight": in_flight,
        "unreadable": unreadable,
        "blocked_waiting_for_user": blocked,
        "queued_assignments": open.iter().filter(|a| a.status == "queued").count(),
        "checked_at": crate::db::now(),
    }))
}

/// Take a window: approval checked, safety re-checked and the lease taken, all under the
/// requester 只接受穩定 bot id；人類／外部腳本沒有 bot 身份，不由顯示名稱推斷。
pub async fn requester_bot_id(app: &impl crate::capabilities::Db, owner: &str) -> Option<String> {
    try_requester_bot_id(app.db(), owner).await.unwrap_or(None)
}

/// DB 讀取錯誤必須傳回守衛，不能當成「不是申請者」。
pub async fn try_requester_bot_id(db: &SqlitePool, owner: &str) -> anyhow::Result<Option<String>> {
    Ok(crate::db::bot(db, owner).await?.filter(|b| b.deleted_at.is_none()).map(|b| b.id))
}

/// 送達臨界區真正要放過的那一顆：申請者自己，而且它確實出現在 `--exclude-bot` 裡。
///
/// 沒有指定就不放過任何人（維持原本的行為）；指定了別人上面已經擋掉，所以這裡看到的只會是自己。
fn exclude_from_quiet(exclude: &[String], self_bot: Option<&str>) -> Option<String> {
    let me = self_bot?;
    exclude.iter().any(|id| id == me).then(|| me.to_string())
}

/// supervisor lock so nothing changes between the check and the hold.
pub async fn acquire(
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView),
    resource: &str,
    owner: &str,
    approval_id: &str,
    commit: Option<&str>,
    ttl_secs: i64,
    require_idle: bool,
    exclude: &[String],
) -> Result<Value, LcError> {
    acquire_with_request(app, resource, owner, approval_id, commit, ttl_secs, require_idle, exclude, None).await
}

/// 租約列的 `detail_json` 裡記的「這張租約是哪個 request_id 開的」。只存雜湊：`detail_json` 不出現在任何 API，
/// 但 request_id 本身是申請端的祕密（拿得回 `lease_token` 靠的就是它），不落地明文。
fn request_hash(request_id: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(request_id.as_bytes()))
}

/// 同 [`acquire`]，多一個選填的 `request_id`（冪等鍵）。
///
/// `lease_token` 只在 acquire 的回應裡出現一次；那個回應逾時或連線斷掉時，租約已經開了、token 卻丟了，
/// 窗口只能握到 TTL（預設 900 秒、期間 restart 窗口擋住 assignment 派送）。帶同一個 `request_id` 重送，
/// 租約還握在**同一個 owner、同一張核准、同一個 commit** 手上、而且當初就是這個 `request_id` 開的，
/// 就原樣回同一張（同 fence、同 token、不延長、不重驗 idle），回應多一個 `replayed: true`。
/// 沒帶、換一個、租約已經還掉或過期，都照舊走完整的 acquire——request_id 只是「拿回」，不是另一條開窗口的路。
pub async fn acquire_with_request(
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView),
    resource: &str,
    owner: &str,
    approval_id: &str,
    commit: Option<&str>,
    ttl_secs: i64,
    require_idle: bool,
    exclude: &[String],
    request_id: Option<&str>,
) -> Result<Value, LcError> {
    let request_id = match request_id.map(str::trim) {
        Some("") => return Err(LcError::Bad("request_id must not be empty".into())),
        Some(r) if r.len() > 200 => return Err(LcError::Bad("request_id is too long (max 200)".into())),
        other => other,
    };
    if !RESOURCES.contains(&resource) {
        return Err(LcError::Bad(format!("unknown lease resource: {resource}")));
    }
    if commit.is_some_and(|c| !valid_commit(c)) {
        return Err(LcError::Bad("commit must be 7-64 hex characters (a git sha)".into()));
    }
    if EXCLUSIVE.contains(&resource) && !require_idle {
        return Err(LcError::Bad(format!(
            "the `{resource}` window cannot skip the idle check; wait for a safe window instead"
        )));
    }
    let ttl = ttl_secs.clamp(30, MAX_TTL_SECS);
    let _g = super::lock().await;

    let approval = store::approval(app.db(), approval_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .ok_or_else(|| LcError::NotFound("approval".into()))?;
    if let Some(reason) = approval.refusal(&crate::db::now(), resource, commit) {
        return Err(LcError::conflict(
            "the approval does not cover this operation",
            json!({"reason": reason, "approval_id": approval.id, "status": approval.status,
                   "target_commit": approval.target_commit, "requested_commit": commit}),
        ));
    }
    if approval.requester != owner {
        return Err(LcError::conflict(
            "the approval was granted to someone else",
            json!({"reason": "approval_owner_mismatch", "approval_id": approval.id, "requester": approval.requester, "owner": owner,
                   "hint": "acquire 的 --owner 要跟申請時的 --requester 一樣；要自己開窗口就自己申請一筆"}),
        ));
    }
    if let Some(l) = store::lease(app.db(), resource).await.map_err(|e| LcError::Upstream(e.to_string()))? {
        let used = l.released_at.is_some() || !l.held_at(&crate::db::now());
        if used && l.approval_id.as_deref() == Some(approval.id.as_str()) {
            let _ = store::decide_approval_from(app.db(), &approval.id, "approved", "consumed", "daemon", Some("lease expired"), None).await;
            return Err(LcError::conflict(
                "this approval already opened a window that expired without being released",
                json!({"reason": "approval_already_used", "approval_id": approval.id, "fence": l.fence,
                       "hint": "那個窗口已經用掉這筆核准；要再開一個窗口請重新申請"}),
            ));
        }
    }
    if let Some(rid) = request_id {
        if let Some(l) = store::lease(app.db(), resource).await.map_err(|e| LcError::Upstream(e.to_string()))? {
            let detail: Value = serde_json::from_str(&l.detail_json).unwrap_or(Value::Null);
            let same = l.held_at(&crate::db::now())
                && l.owner.as_deref() == Some(owner)
                && l.approval_id.as_deref() == Some(approval.id.as_str())
                && l.target_commit.as_deref() == commit
                && detail.get("request_hash").and_then(Value::as_str) == Some(request_hash(rid).as_str());
            if same {
                tracing::info!(resource, owner, fence = l.fence, "maintenance lease acquire replayed by request_id");
                let safety = detail.get("safety").cloned().unwrap_or(Value::Null);
                return Ok(json!({"lease": l.to_json(), "lease_token": l.lease_token, "approval": approval.to_json(), "safety": safety, "replayed": true}));
            }
        }
    }

    let self_bot = match store::approval_principal(app.db(), &approval.id).await.map_err(super::up)? {
        Some(store::ApprovalPrincipal::Bot { bot_id }) => Some(bot_id),
        _ => None,
    };
    if EXCLUSIVE.contains(&resource) {
        if let Some(bad) = exclude.iter().find(|id| Some(id.as_str()) != self_bot.as_deref()) {
            return Err(LcError::conflict(
                "a restart window may only ignore the requester's own bot",
                json!({"reason": "exclude_not_requester", "excluded": bad, "requester": approval.requester,
                       "requester_bot_id": self_bot,
                       "hint": "--exclude-bot 只能指到申請這筆核准的那顆 bot；別顆 bot 正在跑就等它，不要把它排掉"}),
            ));
        }
    }
    let safety = safety_as(app, exclude, Some(&approval.id), Some(owner)).await?;
    if require_idle && safety.get("safe") != Some(&Value::Bool(true)) {
        let waited = safety.get("waited_secs").and_then(Value::as_i64);
        let threshold = safety.get("escalate_after_secs").and_then(Value::as_i64);
        let eta = match (waited, threshold) {
            (Some(w), Some(t)) if w < t => Some(crate::db::iso_in(t - w)),
            _ => None,
        };
        return Err(LcError::conflict(
            "something is still running; wait for a safe window",
            json!({
                "reason": "not_idle",
                "waited_secs": waited,
                "escalate_after_secs": threshold,
                "escalates_at": eta,
                "hint": eta.as_deref().map(|at| format!(
                    "同一張核准等滿 {}s 之後 working 就不再擋（只剩送達臨界區、別人的租約、讀不到狀態）；預計 {at} 可以拿。帶同一張 --approval 再試，換一張會重算。",
                    threshold.unwrap_or(0)
                )),
                "safety": safety,
            }),
        ));
    }

    let expires_at = lease_deadline(&iso_in(ttl), approval.effective_expiry().as_deref());
    let quiet_delivery = EXCLUSIVE.contains(&resource);
    let exclude_self = quiet_delivery.then(|| exclude_from_quiet(exclude, self_bot.as_deref())).flatten();
    let taken = store::acquire_lease(
        app.db(),
        resource,
        owner,
        Some(&approval.id),
        commit,
        &expires_at,
        quiet_delivery,
        exclude_self.as_deref(),
        &json!({"require_idle": require_idle, "safety": safety, "excluded_self": exclude_self,
                "request_hash": request_id.map(request_hash)}),
    )
    .await
    .map_err(|e| LcError::Upstream(e.to_string()))?;

    let Some(lease) = taken else {
        let held = store::lease(app.db(), resource).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        if held.as_ref().is_none_or(|l| !l.held_at(&crate::db::now())) {
            let now = crate::db::now();
            let racing = store::delivery_critical_anywhere(app.db()).await.unwrap_or(true);
            if racing {
                return Err(LcError::conflict(
                    "a prompt entered the delivery critical section while this window was being taken",
                    json!({"reason": "not_idle", "raced": true, "checked_at": now,
                           "hint": "safety 讀完到拿租約之間有一則 prompt 進來了；再問一次 safety 然後重試"}),
                ));
            }
        }
        return Err(LcError::conflict(
            "someone else holds this window",
            json!({"reason": "lease_held", "lease": held.map(|l| l.to_json())}),
        ));
    };
    let escalated = safety.get("escalated") == Some(&Value::Bool(true));
    let waited = safety.get("waited_secs").and_then(|v| v.as_i64()).unwrap_or(0);
    let lease_token = lease.lease_token.clone();
    tracing::info!(resource, owner, fence = lease.fence, escalated, waited, "maintenance lease acquired");
    if escalated {
        tracing::warn!(
            resource,
            owner,
            waited,
            "這個窗口是升級後（縮小封鎖面）才拿到的：有 bot 還在回合中，只確認了沒人在送達臨界區、沒有別人的租約、畫面都讀得到"
        );
    }
    app.emit("supervisor_changed", json!({"lease": lease.to_json()})).await;
    Ok(json!({"lease": lease.to_json(), "lease_token": lease_token, "approval": approval.to_json(), "safety": safety}))
}

/// 核准與租約裡的 commit 是 git sha：7～64 碼十六進位。自由字串會被寫進核准、租約、稽核紀錄，腳本也會拿去用。
pub fn valid_commit(c: &str) -> bool {
    (7..=64).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The earlier of the requested deadline and the approval's own expiry.
///
/// A lease is only ever a permission with a clock on it; it cannot be renewed past the moment
/// that permission lapses, and it cannot be granted past it either.
pub fn lease_deadline(requested: &str, approval_expires_at: Option<&str>) -> String {
    match approval_expires_at {
        Some(exp) if crate::db::cmp_ts(exp, requested).is_lt() => exp.to_string(),
        _ => requested.to_string(),
    }
}

/// Dispatch is paused; keep the assignment queued until the window closes rather than sending
/// work into a session that is about to be restarted.
pub fn pause_note(until: &str) -> String {
    format!("restart window held until {until}")
}

/// 故障注入（只在測試裡）：讓「讀租約」這件事真的失敗，而租約本身（握著的窗口）原封不動，
/// 才分得出「沒有窗口」與「窗口在、但讀不到」（issue #127）。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod fault {
    use sqlx::SqlitePool;

    /// 之後每一句讀 `supervisor_leases` 的 SELECT 都失敗（`no such table`）——DB 出錯的樣子。
    pub async fn break_lease_reads(pool: &SqlitePool) {
        sqlx::query("ALTER TABLE supervisor_leases RENAME TO supervisor_leases_unreadable").execute(pool).await.unwrap();
    }

    /// 讀取恢復；租約列原封不動。
    pub async fn restore_lease_reads(pool: &SqlitePool) {
        sqlx::query("ALTER TABLE supervisor_leases_unreadable RENAME TO supervisor_leases").execute(pool).await.unwrap();
    }

    /// 讀得到那一列、但解不開（`fence` 不是整數）——parse 出錯的樣子。
    pub async fn corrupt_lease_row(pool: &SqlitePool) {
        sqlx::query("UPDATE supervisor_leases SET fence='corrupt' WHERE resource='restart'").execute(pool).await.unwrap();
    }
}
