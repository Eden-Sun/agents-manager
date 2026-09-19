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
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;

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
pub const ESCALATE_AFTER_MINS: i64 = 30;
/// 覆寫上面那個門檻（分鐘）。0、負數或看不懂的值一律當沒設，回到 30。
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
pub async fn escalation_for(app: &Arc<App>, approval_id: Option<&str>) -> Result<Option<Escalation>, LcError> {
    let now = crate::db::now();
    let found = match approval_id {
        Some(id) => store::approval(&app.db, id)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?
            // 只有「還能用來開窗口」的核准才有資格計時：已消耗、被撤、過期的都不算。
            .filter(|a| a.status == "approved" && RESOURCES.contains(&a.purpose.as_str()))
            .filter(|a| !a.expires_at.as_deref().is_some_and(|t| crate::db::cmp_ts(t, &now).is_le())),
        None => store::oldest_live_window_approval(&app.db, &now).await.map_err(|e| LcError::Upstream(e.to_string()))?,
    };
    let Some(a) = found else { return Ok(None) };
    // 同一個申請者換 commit 接續的等待（`wait_since`）也算：main 一動核准就換一筆，計時不能跟著歸零。
    let Some(since) = a.waiting_since() else { return Ok(None) };
    let waited = waited_secs(since, &now);
    Ok(Some(Escalation { approval_id: a.id, waited_secs: waited, escalated: waited >= escalate_after_secs() }))
}

pub async fn escalation(app: &Arc<App>) -> Result<Option<Escalation>, LcError> {
    escalation_for(app, None).await
}

/// 還握著的租約。縮小封鎖面時這是**唯一**新增的阻擋條件：窗口一次只給一個人。
///
/// `own` 標出「就是發問的這個 owner 自己握的」（`owner` 給了才會是 true）。**自己的租約不擋自己**
/// （AGM 2026-09-16，58d3587 的規格漏洞）：標準換版是同一人先拿 rebuild、build 完再拿 restart，
/// 把自己手上的 rebuild 也算成「別人握著窗口」，restart 就會被卡到 rebuild 自己到期為止。
async fn held_leases(app: &Arc<App>, owner: Option<&str>) -> Result<Vec<Value>, LcError> {
    let now = crate::db::now();
    let mut out = Vec::new();
    for resource in RESOURCES {
        let l = store::lease(&app.db, resource).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        if let Some(l) = l.filter(|l| l.held_at(&now)) {
            let own = owner.is_some_and(|o| l.owner.as_deref() == Some(o));
            out.push(json!({"resource": resource, "owner": l.owner, "own": own, "fence": l.fence, "expires_at": l.expires_at}));
        }
    }
    Ok(out)
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

/// Default and maximum lease lifetime. Long enough for a release build and a restart, short
/// enough that a crashed holder does not block the next window for an afternoon.
pub const DEFAULT_TTL_SECS: i64 = 900;
pub const MAX_TTL_SECS: i64 = 3600;

fn iso_in(secs: i64) -> String {
    crate::db::iso_in(secs)
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
pub async fn window_held(app: &Arc<App>) -> Result<Option<WindowHeld>, WindowUnreadable> {
    let now = crate::db::now();
    for resource in EXCLUSIVE {
        let lease = match store::lease(&app.db, resource).await {
            Ok(l) => l,
            Err(e) => return Err(unreadable(resource, e)),
        };
        let Some(l) = lease else { continue };
        // 沒放掉、卻沒有一個讀得懂的到期時間：不知道它什麼時候結束，不能當它不存在。
        // （`acquire_lease` 一定會寫到期時間，所以這只會是壞資料；daemon 重啟時會被收掉。）
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
pub async fn dispatch_paused(app: &Arc<App>) -> Result<Option<String>, WindowUnreadable> {
    Ok(window_held(app).await?.map(|w| w.expires_at))
}

/// A restart window is over: lift the holds it put on queued assignments so the next controller
/// pass sends them, instead of each waiting out the deadline it was held to. No window is open
/// any more when this runs, so there is nothing left to protect (SPEC §18.10: no grace period).
pub async fn window_closed(app: &Arc<App>, why: &str) -> u64 {
    match store::clear_restart_holds(&app.db).await {
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

/// Release a lease and consume its approval — one yes, one window. When it was a restart
/// window, the holds it placed are lifted at once.
pub async fn release(
    app: &Arc<App>,
    resource: &str,
    owner: &str,
    fence: i64,
    proof: store::LeaseProof<'_>,
) -> anyhow::Result<bool> {
    // 憑證對不對在任何寫入之前決定：`release_lease` 只認 owner＋fence，而那兩個是公開欄位
    // （`lease status` 就看得到），光憑它們等於誰都能把別人正在換 binary 的窗口收掉。
    let stored = store::lease_token(&app.db, resource).await?;
    if !proof.allows(stored.as_deref()) {
        anyhow::bail!("lease_token_mismatch");
    }
    if stored.is_none() {
        tracing::warn!(resource, owner, "released a lease created before lease tokens existed; no proof was possible");
    }
    let released = if proof.is_forced() {
        store::force_release_lease(&app.db, resource).await?
    } else {
        store::release_lease(&app.db, resource, owner, fence).await?
    };
    if released {
        if let Some(l) = store::lease(&app.db, resource).await? {
            if let Some(ap) = l.approval_id.as_deref() {
                if let Ok(Some(a)) = store::approval(&app.db, ap).await {
                    if a.status == "approved" {
                        // 走有稽核的那支：`decide_approval` 是無條件 UPDATE，不寫 supervisor_notes，
                        // 而且會把 `decided_at` 覆寫成消耗時間——升級判定（§18.10）的計時就是看那一欄。
                        let _ = store::decide_approval_from(&app.db, ap, "approved", "consumed", owner, Some("lease released"), None).await;
                    }
                }
            }
        }
        if EXCLUSIVE.contains(&resource) && matches!(dispatch_paused(app).await, Ok(None)) {
            window_closed(app, "lease released").await;
        }
    }
    Ok(released)
}

/// The daemon answering again *is* the end of the restart it was restarted for: a restart lease
/// still held from before the restart is released here, and every hold it placed is lifted.
pub async fn release_restart_on_startup(app: &Arc<App>) {
    for resource in EXCLUSIVE {
        match store::lease(&app.db, resource).await {
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
                        app.emit("supervisor_changed", json!({"lease": store::lease(&app.db, resource).await.ok().flatten().map(|l| l.to_json())})).await;
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(resource, error = ?e, "daemon started: could not release the restart lease"),
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(resource, error = ?e, "daemon started: could not read the restart lease"),
        }
    }
    window_closed(app, "daemon started").await;
}

/// What is going on that a restart would interrupt.
///
/// `blocked` panes are counted but are *not* a reason to refuse: a pane waiting for a human can
/// wait for hours, and the restart path skips blocked panes anyway. What blocks a window is work
/// actually running.
pub async fn safety(app: &Arc<App>, exclude: &[String]) -> Result<Value, LcError> {
    safety_for(app, exclude, None).await
}

/// 同一份檢查，但升級的計時綁在 `approval_id` 那一筆核准上（見 [`escalation_for`]）。
pub async fn safety_for(app: &Arc<App>, exclude: &[String], approval_id: Option<&str>) -> Result<Value, LcError> {
    safety_as(app, exclude, approval_id, None).await
}

/// 再多一個 `owner`：以這個人的身分問，**他自己握的租約不算擋**（只在縮小封鎖面時有差，
/// 全靜止模式本來就不看租約）。不給 `owner` 就是舊行為：每一把租約都算擋。
pub async fn safety_as(
    app: &Arc<App>,
    exclude: &[String],
    approval_id: Option<&str>,
    owner: Option<&str>,
) -> Result<Value, LcError> {
    let bots = crate::db::live_bots(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let sup = store::get_or_init(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let mut working = Vec::new();
    let mut blocked = Vec::new();
    let mut in_flight = Vec::new();
    // 送達臨界區（SPEC §18.10）：縮小封鎖面之後就只剩這個、還握著的租約與讀不到畫面會擋。
    let mut delivering = Vec::new();
    // A read that fails is not a bot that is idle. Before this, `let Ok(Some(run)) = … else
    // continue` swallowed the error and the bot silently counted as free — a failing database
    // would have read as "the coast is clear", which is the one answer this must never invent.
    let mut unreadable = Vec::new();
    for b in &bots {
        if exclude.contains(&b.id) {
            continue;
        }
        // 在看 run 之前先問：沒有 active run 的 bot 也可能有一筆排隊中的 prompt。
        match delivery_critical(&app.db, &b.id).await {
            Ok(Some(turn_id)) => delivering.push(json!({"bot_id": b.id, "name": b.name, "turn_id": turn_id})),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(bot = %b.id, error = ?e, "safety probe could not read this bot's delivery state");
                unreadable.push(json!({"bot_id": b.id, "name": b.name}));
            }
        }
        let run = match crate::db::active_run(&app.db, &b.id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(bot = %b.id, error = ?e, "safety probe could not read this bot's run");
                unreadable.push(json!({"bot_id": b.id, "name": b.name}));
                continue;
            }
        };
        let Some(run) = run else { continue };
        // AGM's own turn is protected by the same rule as everyone's: you do not restart the
        // daemon out from under the session the user is talking to.
        if run.agent_status == "working" {
            working.push(json!({"bot_id": b.id, "name": b.name, "is_supervisor": Some(&b.id) == sup.bot_id.as_ref()}));
        } else if run.agent_status == "blocked" {
            blocked.push(json!({"bot_id": b.id, "name": b.name}));
        }
        match crate::db::in_flight_turn(&app.db, &run.id).await {
            Ok(Some(t)) => in_flight.push(json!({"bot_id": b.id, "turn_id": t.id})),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(bot = %b.id, error = ?e, "safety probe could not read this bot's in-flight turn");
                unreadable.push(json!({"bot_id": b.id, "name": b.name}));
            }
        }
    }
    let open = store::open_assignments(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let held = held_leases(app, owner).await?;
    // 擋人的只有別人的：同一個 owner 先拿 rebuild 再拿 restart 是標準換版流程，不是搶窗口。
    let held_by_others = held.iter().filter(|l| l.get("own") != Some(&Value::Bool(true))).count();
    let esc = escalation_for(app, approval_id).await?;
    // Not knowing about even one bot is enough to refuse: the window's whole promise is that
    // nothing is running, and we cannot promise that about a bot we could not look at.
    let strict = working.is_empty() && in_flight.is_empty() && unreadable.is_empty();
    // 縮小封鎖面：思考中不再擋，送達臨界區、還握著的租約與讀不到畫面照擋（SPEC §18.10）。
    let escalated = esc.as_ref().is_some_and(|e| e.escalated);
    // 放寬只會放寬：全靜止成立的窗口，等滿門檻之後也一定成立（review 2 總管 4）。以前是二選一，
    // 等超過 30 分鐘反而多擋兩樣全靜止不看的東西——放回佇列、正在閒置的 queued 與別人的租約
    // （租約的互斥由 acquire 本身把關，全靜止本來就不看）。
    let relaxed = escalated && delivering.is_empty() && unreadable.is_empty() && held_by_others == 0;
    let safe = strict || relaxed;
    Ok(json!({
        "safe": safe,
        // 這一刻是不是已經放寬了，以及最早那筆沒用掉的核准等了多久（沒有這種核准時是 null）。
        "escalated": escalated,
        "waited_secs": esc.as_ref().map(|e| e.waited_secs),
        "escalation_approval_id": esc.as_ref().map(|e| e.approval_id.clone()),
        "escalate_after_secs": escalate_after_secs(),
        // 放寬之後仍然會擋的兩項，列出來才看得懂為什麼還是 false。租約全部列出、各自帶 owner；
        // `own: true` 的是發問者自己握的，不算擋。
        "delivering": delivering,
        "held_leases": held,
        // 以誰的身分問的（沒給就是 null＝每一把租約都算擋）。回聲出來，呼叫端才分得出是不是舊 daemon 忽略了它。
        "owner": owner,
        "working": working,
        "in_flight": in_flight,
        // Bots whose state could not be read this pass. Never empty *and* `safe` at once.
        "unreadable": unreadable,
        // Reported, not blocking: a pane waiting on a person is a normal state, and a restart
        // leaves it alone.
        "blocked_waiting_for_user": blocked,
        // Queued work is not a reason to refuse either — it is exactly what the pause holds
        // back — but the caller should see how much is waiting on the other side of the window.
        "queued_assignments": open.iter().filter(|a| a.status == "queued").count(),
        "checked_at": crate::db::now(),
    }))
}

/// Take a window: approval checked, safety re-checked and the lease taken, all under the
/// 這個 owner／requester 是哪一顆 bot（沒有對應的 bot——例如 `daemon-update-kick` 這種腳本
/// 身分——就是 `None`）。先當成 bot id 查，查不到再用名字對。
async fn requester_bot_id(app: &Arc<App>, owner: &str) -> Option<String> {
    let owner = owner.trim();
    if owner.is_empty() {
        return None;
    }
    if matches!(crate::db::bot(&app.db, owner).await, Ok(Some(_))) {
        return Some(owner.to_string());
    }
    if let Ok(bots) = crate::db::live_bots(&app.db).await {
        if let Some(b) = bots.into_iter().find(|b| b.name == owner) {
            return Some(b.id);
        }
    }
    // **agent 名也要認**：申請者常用自己的 herdr agent 名（`AM_AGENT_NAME`），那跟 bot 的名字
    // 不一定一樣——2026-09-19 實測 bot 叫 `AM-m3`、agent 叫 `agents-manager-15m2dg`，於是
    // 「排除申請者自己」永遠對不上，restart 一律 409 `exclude_not_requester`。
    sqlx::query_scalar::<_, String>("SELECT bot_id FROM runs WHERE agent_name = ? AND state = 'running' ORDER BY started_at DESC LIMIT 1")
        .bind(owner)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
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
    app: &Arc<App>,
    resource: &str,
    owner: &str,
    approval_id: &str,
    commit: Option<&str>,
    ttl_secs: i64,
    require_idle: bool,
    exclude: &[String],
) -> Result<Value, LcError> {
    if !RESOURCES.contains(&resource) {
        return Err(LcError::Bad(format!("unknown lease resource: {resource}")));
    }
    // A restart interrupts every session on the box. "Take the window anyway" is not a thing
    // you get to ask for: the idle check *is* the window for this resource.
    if EXCLUSIVE.contains(&resource) && !require_idle {
        return Err(LcError::Bad(format!(
            "the `{resource}` window cannot skip the idle check; wait for a safe window instead"
        )));
    }
    let ttl = ttl_secs.clamp(30, MAX_TTL_SECS);
    let _g = super::lock().await;

    let approval = store::approval(&app.db, approval_id)
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
    // 核准是給申請的那個人的：別人拿著它 acquire，連升級計時都一起借走（review2 sup #5）。
    if approval.requester != owner {
        return Err(LcError::conflict(
            "the approval was granted to someone else",
            json!({"reason": "approval_owner_mismatch", "approval_id": approval.id, "requester": approval.requester, "owner": owner,
                   "hint": "acquire 的 --owner 要跟申請時的 --requester 一樣；要自己開窗口就自己申請一筆"}),
        ));
    }
    // 一次核准一個窗口：這張已經開過一個、那個窗口過期沒 release（執行端掛了）時，**同一張**不能再開一次。
    // 接手過期租約時只消耗「別張」核准（store::acquire_lease），同一張會被放過去（review2 sup #6）。
    if let Some(l) = store::lease(&app.db, resource).await.map_err(|e| LcError::Upstream(e.to_string()))? {
        let expired = l.released_at.is_none() && !l.held_at(&crate::db::now());
        if expired && l.approval_id.as_deref() == Some(approval.id.as_str()) {
            let _ = store::decide_approval_from(&app.db, &approval.id, "approved", "consumed", "daemon", Some("lease expired"), None).await;
            return Err(LcError::conflict(
                "this approval already opened a window that expired without being released",
                json!({"reason": "approval_already_used", "approval_id": approval.id, "fence": l.fence,
                       "hint": "那個窗口已經用掉這筆核准；要再開一個窗口請重新申請"}),
            ));
        }
    }

    // 排除對象綁**這筆核准的申請者**（上面已驗過 `requester == owner`）：申請人自己那顆 bot。
    // 別顆 bot 一律不能被排除掉——那等於拿一張自己的核准，把別人正在打字的 pane 也算成閒置。
    let self_bot = requester_bot_id(app, owner).await;
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
    // 放寬與否只看**這一筆**核准等了多久：別人放著沒用的核准不能替它開門。
    // 以 acquire 的 owner 問：自己手上的 rebuild 不擋自己的 restart。
    let safety = safety_as(app, exclude, Some(&approval.id), Some(owner)).await?;
    if require_idle && safety.get("safe") != Some(&Value::Bool(true)) {
        // 講得出「還要等多久」：只說 not_idle 的話，呼叫端會以為沒有出路（2026-09-19 實測，
        // 有人因此連試 `--allow-busy`、最後想繞過租約）。升級機制就是這種情況的出口，
        // 所以把等待秒數、門檻與預估升級時間直接放進錯誤裡。
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

    // The lease may not outlive the permission it rests on. Otherwise "approved until 14:00"
    // quietly becomes "holding the box until 14:45", which is a different promise than the one
    // anybody agreed to.
    let expires_at = lease_deadline(&iso_in(ttl), approval.expires_at.as_deref());
    // 會中斷 pane 的窗口，連**寫下去的那一刻**都要沒有人在送達臨界區：safety 是上面讀的，
    // 讀完到寫入之間還是有可能有一則 prompt 把 turn commit 進來（issue #86 的 TOCTOU）。
    let quiet_delivery = EXCLUSIVE.contains(&resource);
    // 申請者自己這一回合不算「有人在送達臨界區」：它就是來換版的那個人，而它的回合要等 acquire
    // 回來才會結束。不放過它的話，任何 bot 在自己的回合裡都拿不到 restart 窗口，只剩「把腳本丟
    // 背景再結束回合」一條路——那正是規則 6a 禁止的（2026-09-18 AM-m3 連試 30 次都 raced=true）。
    let exclude_self = quiet_delivery.then(|| exclude_from_quiet(exclude, self_bot.as_deref())).flatten();
    let taken = store::acquire_lease(
        &app.db,
        resource,
        owner,
        Some(&approval.id),
        commit,
        &expires_at,
        quiet_delivery,
        exclude_self.as_deref(),
        &json!({"require_idle": require_idle, "safety": safety, "excluded_self": exclude_self}),
    )
    .await
    .map_err(|e| LcError::Upstream(e.to_string()))?;

    let Some(lease) = taken else {
        let held = store::lease(&app.db, resource).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        // 分得出是哪一種輸法：窗口被別人搶走，還是「就在這一瞬有 prompt 進了送達臨界區」。
        // 後者說成 `lease_held` 的話，呼叫端會去找一個根本不存在的持有者。
        if held.as_ref().is_none_or(|l| !l.held_at(&crate::db::now())) {
            let now = crate::db::now();
            let racing = store::delivery_critical_anywhere(&app.db).await.unwrap_or(true);
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
        // 換版紀錄要看得出這次不是等到全靜止才換的（SPEC §18.10）。整份 safety 也寫在租約 meta 裡。
        tracing::warn!(
            resource,
            owner,
            waited,
            "這個窗口是升級後（縮小封鎖面）才拿到的：有 bot 還在回合中，只確認了沒人在送達臨界區、沒有別人的租約、畫面都讀得到"
        );
    }
    app.emit("supervisor_changed", json!({"lease": lease.to_json()})).await;
    // token 只在這裡出現一次：`to_json()`（`lease status`、`/api/supervisor`、事件）永遠不含它。
    Ok(json!({"lease": lease.to_json(), "lease_token": lease_token, "approval": approval.to_json(), "safety": safety}))
}

/// The earlier of the requested deadline and the approval's own expiry.
///
/// A lease is only ever a permission with a clock on it; it cannot be renewed past the moment
/// that permission lapses, and it cannot be granted past it either.
pub fn lease_deadline(requested: &str, approval_expires_at: Option<&str>) -> String {
    match approval_expires_at {
        // Compare instants, not strings: the approval's `expires_at` may be a second-precision value
        // written by an older build, or any RFC3339 the AGM sent through the API (`+08:00`), while
        // `requested` is `iso_in`'s millisecond form (issue #101).
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
#[cfg(test)]
pub(crate) mod fault {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::store::Approval;

    /// 升級門檻的環境變數：只有「正整數」算數，其他一律回預設 30 分鐘。
    #[test]
    fn a_broken_threshold_env_falls_back_to_thirty_minutes() {
        assert_eq!(parse_escalate_mins(None), 30);
        assert_eq!(parse_escalate_mins(Some("45")), 45);
        assert_eq!(parse_escalate_mins(Some("  45 ")), 45);
        for bad in ["0", "-5", "abc", "", "30m"] {
            assert_eq!(parse_escalate_mins(Some(bad)), 30, "{bad:?} 不該被當成門檻");
        }
    }

    /// 排著的 queued 只有在 bot 還有活著的 run 時才算送達臨界區；run 沒了的遺留那筆不擋。
    #[tokio::test]
    async fn a_queued_turn_without_a_live_run_is_not_a_delivery_in_progress() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('q',?,'q','claude','tok-q',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('cq','q',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('rq','q','running','working',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,created_at) VALUES ('tq','cq','web','queued','pending','x',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert_eq!(delivery_critical(&app.db, "q").await.unwrap().as_deref(), Some("tq"), "還有 run：真的在等送出");

        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id='rq'").bind(&now).execute(&app.db).await.unwrap();
        assert_eq!(delivery_critical(&app.db, "q").await.unwrap(), None, "run 沒了：遺留的那筆不擋");
        let s = safety(app, &[]).await.unwrap();
        assert!(s["delivering"].as_array().unwrap().is_empty(), "{s}");

        // in_flight＋pending（打字中）照舊算，不因為這條放寬。（遺留那筆先照 stop 的流程撤掉。）
        sqlx::query("UPDATE turns SET status='failed', delivery='failed' WHERE id='tq'").execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('rq2','q','running','idle',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('tp','cq','rq2','web','in_flight','pending',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert_eq!(delivery_critical(&app.db, "q").await.unwrap().as_deref(), Some("tp"));
    }

    /// 等使用者回答的 bot（run `blocked`）排著的 queued 不擋窗口；一離開 blocked 立刻回到臨界區；
    /// 已經開始送的（in_flight＋pending）不管 blocked 與否照擋。用 acquire 真的走一遍：它在鎖內重判。
    #[tokio::test]
    async fn a_queued_prompt_behind_a_bot_waiting_for_the_user_holds_the_window_only_once_it_can_flush() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('ask',?,'ask','claude','tok-ask',?)")
            .bind(&pid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('ca','ask',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('ra','ask','running','working',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        // 正在問使用者的那一回合（已送達）＋排在後面的 AGM 派工。
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('tq_ask','ca','ra','web','in_flight','ok',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,created_at) VALUES ('tq_next','ca','web','queued','pending','x',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        let waited = approved_window(app, 40).await;
        let set = |status: &'static str| {
            let db = app.db.clone();
            async move { sqlx::query("UPDATE runs SET agent_status=? WHERE id='ra'").bind(status).execute(&db).await.unwrap(); }
        };

        // 還在 working：queued 算臨界區，窗口拿不到。
        assert_eq!(delivery_critical(&app.db, "ask").await.unwrap().as_deref(), Some("tq_next"));
        let refused = acquire(app, "rebuild", "ops", &waited, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["safety"]["delivering"][0]["turn_id"], "tq_next", "{detail}");

        // 停在 blocked（AskUserQuestion 開著）：不擋，同一筆核准在鎖內重判就拿得到。
        set("blocked").await;
        assert_eq!(delivery_critical(&app.db, "ask").await.unwrap(), None);
        let s = safety_for(app, &[], Some(&waited)).await.unwrap();
        assert_eq!((s["safe"].as_bool(), s["delivering"].as_array().map(Vec::len)), (Some(true), Some(0)), "{s}");
        assert_eq!(s["blocked_waiting_for_user"][0]["bot_id"], "ask", "照實回報在等使用者");

        // 使用者回答了（離開 blocked）：立刻回到臨界區，不沿用剛才的判定。
        set("working").await;
        assert_eq!(delivery_critical(&app.db, "ask").await.unwrap().as_deref(), Some("tq_next"));
        assert_eq!(safety_for(app, &[], Some(&waited)).await.unwrap()["safe"], false);

        // flush 已經開始送（in_flight＋pending）：就算又跳回 blocked 也照擋。
        sqlx::query("UPDATE turns SET status='completed' WHERE id='tq_ask'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='in_flight', run_id='ra' WHERE id='tq_next'").execute(&app.db).await.unwrap();
        set("blocked").await;
        assert_eq!(delivery_critical(&app.db, "ask").await.unwrap().as_deref(), Some("tq_next"));

        set("blocked").await;
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id='tq_next'").execute(&app.db).await.unwrap();
        let taken = acquire(app, "rebuild", "ops", &waited, None, 300, true, &[]).await.unwrap();
        assert_eq!(taken["lease"]["owner"], "ops", "送達之後只剩思考中，縮小封鎖面就放行");
    }

    /// 一顆有 active run、正在 working、而且該筆 turn 已經送達的 bot：思考中的樣子。
    async fn thinking_bot(app: &Arc<App>, project: &str, id: &str) {
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,?,?,'claude',?,?)")
            .bind(id).bind(project).bind(id).bind(format!("tok-{id}")).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','working',?)")
            .bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES (?,?,?)")
            .bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(id).bind(id).bind(id).bind(&now).execute(&app.db).await.unwrap();
    }

    /// 已核准、還沒用掉的 rebuild 窗口，決定時間往前推 `mins` 分鐘。申請者是 `ops`（acquire 的 owner 要跟它一樣）。
    async fn approved_window(app: &Arc<App>, mins: i64) -> String {
        let a = store::create_approval(&app.db, "ops", "rebuild", "release", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &a.id, "approved", "AGM", None, None).await.unwrap();
        let at = (chrono::Utc::now() - chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_approvals SET decided_at=? WHERE id=?").bind(&at).bind(&a.id).execute(&app.db).await.unwrap();
        a.id
    }

    /// 核准後一直等不到全靜止時才放寬，而且只放寬「思考中」這一項。
    #[tokio::test]
    async fn only_a_window_that_waited_past_the_threshold_stops_blocking_on_thinking() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;

        // 1. 沒有任何已核准的窗口：照舊，working 就是不安全。
        let s = safety(app, &[]).await.unwrap();
        assert_eq!((s["safe"].as_bool(), s["escalated"].as_bool()), (Some(false), Some(false)));
        assert_eq!(s["waited_secs"], serde_json::Value::Null);
        assert_eq!(s["working"].as_array().unwrap().len(), 1);

        // 2. 剛核准 10 分鐘：還沒到門檻，一樣不安全。
        let id = approved_window(app, 10).await;
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["escalated"], false, "10 分鐘就放寬的話這條規則等於沒有界線");
        assert_eq!(s["safe"], false);
        assert!(s["waited_secs"].as_i64().unwrap() >= 600);
        assert_eq!(s["escalation_approval_id"], id);

        // 3. 等超過 30 分鐘：思考中不再擋。
        sqlx::query("UPDATE supervisor_approvals SET decided_at=? WHERE id=?")
            .bind((chrono::Utc::now() - chrono::Duration::minutes(40)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .bind(&id).execute(&app.db).await.unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!((s["escalated"].as_bool(), s["safe"].as_bool()), (Some(true), Some(true)));
        assert!(s["waited_secs"].as_i64().unwrap() >= 2400);
        assert_eq!(s["working"].as_array().unwrap().len(), 1, "還是照實回報誰在跑，只是不再擋");
        assert_eq!(s["escalate_after_secs"], 1800);
    }

    /// 放寬只會放寬：全靜止時是 safe 的窗口，等超過門檻之後不能變成 unsafe。
    /// 以前的反例：大家都閒著，但有一筆放回佇列、正在等退避的 queued（bot 還有 run），或別人握著租約——
    /// 全靜止判 safe，等滿 30 分鐘改走縮小封鎖面的規則反而判 unsafe。
    #[tokio::test]
    async fn waiting_past_the_threshold_never_makes_a_quiet_box_unsafe() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('idle',?,'idle','claude','tok-idle',?)")
            .bind(&pid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('ri','idle','running','idle',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('ci','idle',?)").bind(&now).execute(&app.db).await.unwrap();
        // 畫面擋住被放回佇列、正在等退避的那一則：bot 閒著、沒有 in_flight。
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,next_flush_at,created_at) VALUES ('tb','ci','web','queued','pending','x','2099-01-01T00:00:00Z',?)")
            .bind(&now).execute(&app.db).await.unwrap();

        let fresh = approved_window(app, 0).await;
        let quiet = safety_for(app, &[], Some(&fresh)).await.unwrap();
        assert_eq!((quiet["escalated"].as_bool(), quiet["safe"].as_bool()), (Some(false), Some(true)), "全靜止：safe");

        let waited = approved_window(app, 40).await;
        let relaxed = safety_for(app, &[], Some(&waited)).await.unwrap();
        assert_eq!(relaxed["escalated"], true);
        assert_eq!(relaxed["delivering"].as_array().unwrap().len(), 1, "照實列出那一則");
        assert_eq!(relaxed["safe"], true, "等得越久不能越難開：{relaxed}");

        // 對照：有人在思考、又有送達中的那一則 → 全靜止不成立，縮小封鎖面也不成立，照擋。
        thinking_bot(app, &pid, "busy").await;
        let blocked = safety_for(app, &[], Some(&waited)).await.unwrap();
        assert_eq!(blocked["safe"], false, "{blocked}");
    }

    /// 誰等太久就放寬誰：A 放了很久沒用掉，不能替剛核下來的 B 開門。
    /// 這是這條規則的界線——否則一張被遺忘的核准等於把所有人的窗口都打開。
    #[tokio::test]
    async fn a_window_is_only_relaxed_by_its_own_wait() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        let old = approved_window(app, 40).await;
        let fresh = approved_window(app, 0).await;

        // 綁在哪一筆，就看哪一筆等了多久。
        let by_old = safety_for(app, &[], Some(&old)).await.unwrap();
        assert_eq!((by_old["escalated"].as_bool(), by_old["safe"].as_bool()), (Some(true), Some(true)));
        assert_eq!(by_old["escalation_approval_id"], old);
        let by_fresh = safety_for(app, &[], Some(&fresh)).await.unwrap();
        assert_eq!((by_fresh["escalated"].as_bool(), by_fresh["safe"].as_bool()), (Some(false), Some(false)));
        assert_eq!(by_fresh["escalation_approval_id"], fresh);
        assert!(by_fresh["waited_secs"].as_i64().unwrap() < 60);

        // 不帶 approval id 的純查詢維持舊行為：看最早那筆還活著的。
        let plain = safety(app, &[]).await.unwrap();
        assert_eq!((plain["escalated"].as_bool(), plain["escalation_approval_id"].as_str()), (Some(true), Some(old.as_str())));

        // 認不得、已消耗、已過期的核准都不算數：不放寬，也不會炸。
        for bad in [crate::db::ulid().as_str(), ""] {
            let s = safety_for(app, &[], Some(bad)).await.unwrap();
            assert_eq!((s["escalated"].as_bool(), s["safe"].as_bool()), (Some(false), Some(false)), "{bad:?}");
        }
        store::decide_approval(&app.db, &old, "consumed", "AGM", None, None).await.unwrap();
        let used = safety_for(app, &[], Some(&old)).await.unwrap();
        assert_eq!(used["escalated"], false, "用掉的核准不能再拿來計時");
    }

    /// 真的走 acquire：拿著剛核下來的 B，即使 A 已經等了很久，窗口一樣拿不到。
    #[tokio::test]
    async fn acquire_with_a_fresh_approval_does_not_borrow_another_ones_wait() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        let old = approved_window(app, 40).await;
        let fresh = approved_window(app, 0).await;

        let refused = acquire(app, "rebuild", "ops", &fresh, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "not_idle");
        assert_eq!(detail["safety"]["escalated"], false, "B 自己才剛核下來");
        assert!(store::leases(&app.db).await.unwrap().is_empty(), "拿不到就什麼都不該留下");

        // 換成等很久的那一筆：同一個盤面就開得了窗口。
        let taken = acquire(app, "rebuild", "ops", &old, None, 300, true, &[]).await.unwrap();
        assert_eq!(taken["safety"]["escalated"], true);
        assert_eq!(taken["safety"]["escalation_approval_id"], old);
        assert_eq!(taken["lease"]["owner"], "ops");
    }

    /// 放寬之後仍然擋的三件事：正在送出、排隊中待送、別人還握著租約。
    #[tokio::test]
    async fn the_relaxed_window_still_waits_for_delivery_and_leases() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        approved_window(app, 40).await;
        assert_eq!(safety(app, &[]).await.unwrap()["safe"], true, "先確認這個盤面本來是安全的");

        // a) daemon 正在往 pane 打字／送出：turn 還在 in_flight 而 delivery 是 pending。
        sqlx::query("UPDATE turns SET delivery='pending' WHERE id='user'").execute(&app.db).await.unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["safe"], false, "送達臨界區不能被打斷");
        assert_eq!(s["delivering"][0]["bot_id"], "user");

        // b) 送完了，但還有一筆排隊中的 prompt 等著進去。
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id='user'").execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES ('q','user','web','queued','pending',?)")
            .bind(crate::db::now()).execute(&app.db).await.unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["safe"], false, "排隊中待送的 prompt 也算臨界區");
        assert_eq!(s["delivering"][0]["turn_id"], "q");

        // c) 沒有任何待送，但別人還握著窗口。
        sqlx::query("DELETE FROM turns WHERE id='q'").execute(&app.db).await.unwrap();
        assert_eq!(safety(app, &[]).await.unwrap()["safe"], true);
        store::acquire_lease(&app.db, "rebuild", "someone-else", None, None,
            &(chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true), false, None, &json!({}))
            .await.unwrap().unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["safe"], false, "窗口一次只給一個人");
        assert_eq!(s["held_leases"][0]["owner"], "someone-else");
    }

    /// issue #86 的 TOCTOU：safety 是在拿租約**之前**讀的，中間還是可能有一則 prompt 把 turn
    /// commit 進來。`acquire_lease` 那一句 UPDATE 自己帶了同一份送達臨界區條件，所以寫下去的那一刻
    /// 有人在打字就拿不到——跟 `lifecycle::prompt` 那邊的
    /// `a_held_restart_window_refuses_new_prompts_and_says_who_holds_it` 是一對：兩句都是單句寫入、
    /// 由 SQLite 排序，先 commit 的那個贏，兩個不會同時進送達臨界區。
    #[tokio::test]
    async fn a_window_is_not_taken_while_a_prompt_is_in_the_delivery_critical_section() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('tb',?,'tb','claude','tok-tb',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('ct','tb',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('rt','tb','running','idle',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // 就在拿租約之前，一則 prompt commit 了它的 turn（in_flight＋pending＝正在送）。
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('tt','ct','rt','web','in_flight','pending',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert!(store::delivery_critical_anywhere(&app.db).await.unwrap());
        let taken = store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, true, None, &json!({})).await.unwrap();
        assert!(taken.is_none(), "有人在送達臨界區：窗口拿不走");
        assert!(window_held(app).await.unwrap().is_none(), "沒拿到就不該有閘門");

        // 那一則送完了（`delivery` 不再是 pending）：窗口就拿得到。
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id='tt'").execute(&app.db).await.unwrap();
        assert!(!store::delivery_critical_anywhere(&app.db).await.unwrap());
        let taken = store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, true, None, &json!({})).await.unwrap();
        assert!(taken.is_some(), "沒有人在送達臨界區：拿得到");
        let w = window_held(app).await.unwrap().expect("拿到了就有閘門");
        assert_eq!((w.resource, w.owner.as_str()), ("restart", "k8bw2f"));
        assert!(w.retry_after_secs(&crate::db::now()) > 0);

        // `rebuild` 不中斷任何人，不帶這個條件（`acquire` 只對 EXCLUSIVE 帶）。
        sqlx::query("UPDATE turns SET delivery='pending' WHERE id='tt'").execute(&app.db).await.unwrap();
        let rebuild = store::acquire_lease(&app.db, "rebuild", "k8bw2f", None, None, &until, false, None, &json!({})).await.unwrap();
        assert!(rebuild.is_some(), "rebuild 不看送達臨界區");
    }

    /// 2026-09-18：申請者在**自己的回合裡**拿 restart 永遠拿不到——`lease safety --owner X
    /// --exclude-bot X` 說 safe=true，`acquire` 卻連 30 次都 409 `not_idle` / `raced=true`，因為
    /// 送達臨界區那句條件不吃排除，而申請者自己的回合要等 acquire 回來才結束。結果只剩「把腳本
    /// 丟背景、回合先結束」一條路，正好是任務規則 6a 禁止的。
    ///
    /// 修法：放過**申請者自己那一顆**（且必須出現在 `--exclude-bot` 裡），其他 bot 照擋。
    #[tokio::test]
    async fn the_requester_is_not_blocked_by_its_own_turn_but_everyone_else_still_blocks() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        for (bot, conv, run) in [("mine", "cm", "rm"), ("other", "co", "ro")] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,?,?,'claude',?,?)")
                .bind(bot).bind(&e.project_id).bind(bot).bind(format!("tok-{bot}")).bind(&now).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES (?,?,?)")
                .bind(conv).bind(bot).bind(&now).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','idle',?)")
                .bind(run).bind(bot).bind(&now).execute(&app.db).await.unwrap();
        }
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // 申請者自己正在送達臨界區（就是這一回合）。
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('t1','cm','rm','web','in_flight','pending',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert!(store::acquire_lease(&app.db, "restart", "mine", None, None, &until, true, None, &json!({})).await.unwrap().is_none(),
            "不排除的話，申請者自己的回合就把自己擋在門外——這就是原本的 bug");
        let taken = store::acquire_lease(&app.db, "restart", "mine", None, None, &until, true, Some("mine"), &json!({})).await.unwrap();
        assert!(taken.is_some(), "放過申請者自己那顆之後就拿得到");
        sqlx::query("UPDATE supervisor_leases SET released_at=? WHERE resource='restart'").bind(&now).execute(&app.db).await.unwrap();

        // 別顆 bot 也在臨界區：照擋，排除自己不會把別人一起放過去。
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('t2','co','ro','web','in_flight','pending',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        assert!(store::acquire_lease(&app.db, "restart", "mine", None, None, &until, true, Some("mine"), &json!({})).await.unwrap().is_none(),
            "別顆 bot 正在送達臨界區，窗口就不該拿得到");

        // 別人送完了就拿得到（自己那一筆還在 pending）。
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id='t2'").execute(&app.db).await.unwrap();
        assert!(store::acquire_lease(&app.db, "restart", "mine", None, None, &until, true, Some("mine"), &json!({})).await.unwrap().is_some());
    }

    /// 排除對象綁核准的 requester：拿自己的核准去排除**別顆** bot，等於把別人正在打字的 pane
    /// 算成閒置，一律拒絕（巡檢 2026-09-18 定的規則）。
    #[tokio::test]
    async fn a_restart_window_may_only_ignore_the_requesters_own_bot() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('mine',?,'mine','claude','tok-mine',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('other',?,'other','claude','tok-other',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        let a = store::create_approval(&app.db, "mine", "restart", "release", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &a.id, "approved", "AGM", None, None).await.unwrap();

        let err = acquire(app, "restart", "mine", &a.id, None, 300, true, &["other".to_string()]).await.unwrap_err();
        let LcError::Conflict(v) = err else { panic!("要是 409 conflict") };
        assert_eq!(v.get("reason").and_then(|r| r.as_str()), Some("exclude_not_requester"), "{v}");
        assert_eq!(v.get("excluded").and_then(|r| r.as_str()), Some("other"), "{v}");

        // 排除自己是允許的（這一步沒有任何 bot 在臨界區，所以會真的拿到窗口）。
        let ok = acquire(app, "restart", "mine", &a.id, None, 300, true, &["mine".to_string()]).await.unwrap();
        assert_eq!(ok.get("lease").and_then(|l| l.get("owner")).and_then(|o| o.as_str()), Some("mine"), "{ok}");
    }

    /// requester 用的是 **agent 名**（`AM_AGENT_NAME`）而不是 bot 名時，也要認得出是同一顆
    /// （2026-09-19：bot 叫 AM-m3、agent 叫 agents-manager-15m2dg，restart 因此一律 409
    /// `exclude_not_requester`，人只好去試 --allow-busy）。
    #[tokio::test]
    async fn the_requester_can_be_the_agent_name_not_just_the_bot_name() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query(
            "INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b-m3',?,'AM-m3','claude','tok',?)",
        )
        .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query(
            "INSERT INTO runs (id,bot_id,state,agent_status,agent_name,started_at) VALUES ('r-m3','b-m3','running','idle','agents-manager-15m2dg',?)",
        )
        .bind(&now).execute(&app.db).await.unwrap();

        assert_eq!(requester_bot_id(app, "b-m3").await.as_deref(), Some("b-m3"), "bot id");
        assert_eq!(requester_bot_id(app, "AM-m3").await.as_deref(), Some("b-m3"), "bot 名");
        assert_eq!(requester_bot_id(app, "agents-manager-15m2dg").await.as_deref(), Some("b-m3"), "agent 名");
        assert!(requester_bot_id(app, "daemon-update-kick").await.is_none(), "腳本身分對不到 bot");
        assert!(requester_bot_id(app, "  ").await.is_none());
    }

    /// 拿不到窗口時要講得出「還要等多久」：只回 not_idle 會讓人以為沒有出路（2026-09-19 實測）。
    #[tokio::test]
    async fn a_refused_window_says_when_the_approval_escalates() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('busy',?,'busy','claude','tok',?)")
            .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('r-busy','busy','running','working',?)")
            .bind(&now).execute(&app.db).await.unwrap();
        let ap = approved_restart(app, 1).await;

        let err = acquire(app, "restart", "k8bw2f", &ap, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(v) = err else { panic!("要是 409") };
        assert_eq!(v.get("reason").and_then(Value::as_str), Some("not_idle"));
        assert!(v.get("waited_secs").and_then(Value::as_i64).is_some(), "{v}");
        assert_eq!(v.get("escalate_after_secs").and_then(Value::as_i64), Some(escalate_after_secs()), "{v}");
        assert!(v.get("escalates_at").and_then(Value::as_str).is_some(), "要說得出預計什麼時候可以拿：{v}");
        assert!(v.get("hint").and_then(Value::as_str).is_some_and(|h| h.contains("不再擋")), "{v}");
    }

    /// 過期沒 release 的窗口不再是閘門：`held_at` 看 `expires_at`，不需要任何人來收尾（issue #86）。
    #[tokio::test]
    async fn an_expired_window_stops_fencing_by_itself() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let soon = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &soon, true, None, &json!({})).await.unwrap().unwrap();
        assert!(window_held(app).await.unwrap().is_some());

        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='restart'").bind(&past).execute(&app.db).await.unwrap();
        assert!(store::lease(&app.db, "restart").await.unwrap().unwrap().released_at.is_none(), "沒有人 release");
        assert!(window_held(app).await.unwrap().is_none(), "過期就自動不再擋");
        assert!(dispatch_paused(app).await.unwrap().is_none(), "派工那條讀的是同一份");
    }

    /// issue #127：入場閘門是三態——明確有窗口、明確沒有窗口、**讀不到**。讀不到不等於沒有：
    /// 觀測不到 durable 的租約狀態，不代表已經證明沒有租約。三種讀不到（DB 出錯、那一列解不開、沒放掉卻沒有
    /// 讀得懂的到期時間）都回 `Err`；DB 一恢復就重新判斷，不會永久卡住。
    #[tokio::test]
    async fn the_gate_tells_no_window_from_a_window_it_cannot_read() {
        let e = crate::testing::env().await;
        let app = &e.app;
        assert!(window_held(app).await.unwrap().is_none(), "明確沒有窗口：放行");

        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, false, None, &json!({})).await.unwrap().unwrap();
        let fence = store::lease(&app.db, "restart").await.unwrap().unwrap().fence;
        assert!(window_held(app).await.unwrap().is_some(), "明確有窗口：擋");

        // 1. DB 讀取出錯（表讀不到）：`Err`，不是 `Ok(None)`。
        fault::break_lease_reads(&app.db).await;
        let u = window_held(app).await.expect_err("讀不到要回 Err，不能當成沒有窗口");
        assert_eq!(u.resource, "restart");
        assert!(!u.error.is_empty());
        assert!(dispatch_paused(app).await.is_err(), "派工那條讀的是同一份");
        fault::restore_lease_reads(&app.db).await;
        assert!(window_held(app).await.unwrap().is_some(), "DB 恢復後重新判斷：窗口還在就擋，沒有卡在 Err");

        // 2. 那一列讀得到、但解不開。
        fault::corrupt_lease_row(&app.db).await;
        assert!(window_held(app).await.is_err(), "解不開＝讀不到");
        sqlx::query("UPDATE supervisor_leases SET fence=? WHERE resource='restart'").bind(fence).execute(&app.db).await.unwrap();
        assert!(window_held(app).await.unwrap().is_some(), "修好之後恢復");

        // 3. 沒放掉、卻沒有讀得懂的到期時間：不知道它什麼時候結束，不能當它不存在。
        for bad in [None, Some("not-a-time")] {
            sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='restart'").bind(bad).execute(&app.db).await.unwrap();
            let u = window_held(app).await.expect_err("到期時間讀不懂＝讀不到");
            assert!(u.error.contains("expires_at"), "{}", u.error);
        }
        // 已經放掉的租約，到期時間怎樣都不重要。
        sqlx::query("UPDATE supervisor_leases SET released_at=? WHERE resource='restart'").bind(crate::db::now()).execute(&app.db).await.unwrap();
        assert!(window_held(app).await.unwrap().is_none(), "放掉了就是放掉了");
    }

    /// 錯誤要**可觀察、可區分**：503 `maintenance_state_unavailable` 跟 409 `maintenance_window` 分得開
    /// （使用者才知道是「真的有人在維護」還是「狀態暫時讀不到」），而且明講一個字都沒送、可以原樣重送。
    /// 不是偽裝成一個假的長 TTL 租約。
    #[tokio::test]
    async fn an_unreadable_window_is_a_503_that_says_nothing_was_sent_and_a_real_window_stays_a_409() {
        use axum::response::IntoResponse as _;
        let e = crate::testing::env().await;
        let app = &e.app;
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, false, None, &json!({})).await.unwrap().unwrap();

        let real = window_held(app).await.unwrap().unwrap().refusal();
        assert_eq!(real.into_response().status(), axum::http::StatusCode::CONFLICT, "真的有窗口：409");

        fault::break_lease_reads(&app.db).await;
        let LcError::Unavailable(body) = window_held(app).await.unwrap_err().refusal() else { panic!("要是 Unavailable") };
        assert_eq!(body["reason"], "maintenance_state_unavailable", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(body["resource"], "restart", "{body}");
        assert!(body["retry_after_secs"].as_i64().unwrap_or(0) > 0, "{body}");
        assert!(body.get("held_by").is_none() && body.get("expires_at").is_none(), "不編一個假的持有者／到期時間：{body}");

        let resp = window_held(app).await.unwrap_err().refusal().into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE, "讀不到：5xx（可重試），不是 409");
        assert!(resp.headers().get(axum::http::header::RETRY_AFTER).is_some(), "帶 Retry-After");
    }

    /// 已核准、還沒用掉的 restart 窗口（同 [`approved_window`]，只是用途不同）。申請者是 `k8bw2f`。
    async fn approved_restart(app: &Arc<App>, mins: i64) -> String {
        let a = store::create_approval(&app.db, "k8bw2f", "restart", "release", None, None, None).await.unwrap().approval;
        store::decide_approval(&app.db, &a.id, "approved", "AGM", None, None).await.unwrap();
        let at = (chrono::Utc::now() - chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_approvals SET decided_at=? WHERE id=?").bind(&at).bind(&a.id).execute(&app.db).await.unwrap();
        a.id
    }

    async fn hold_rebuild(app: &Arc<App>, owner: &str) {
        store::acquire_lease(&app.db, "rebuild", owner, None, None,
            &(chrono::Utc::now() + chrono::Duration::minutes(50)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true), false, None, &json!({}))
            .await.unwrap().unwrap();
    }

    /// 2026-09-16 09:26Z 實況：k8bw2f 先拿 rebuild（fence 21）、build 完要拿 restart，`held_leases` 列出來的就是它自己那把。
    /// 標準換版流程（同一人先 rebuild 後 restart）不能被自己卡到 rebuild 到期。
    #[tokio::test]
    async fn my_own_rebuild_lease_does_not_block_my_escalated_restart() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        // 有人在思考中：全靜止模式會擋，所以拿得到窗口只可能是走升級那條路。
        thinking_bot(app, &pid, "user").await;
        hold_rebuild(app, "k8bw2f").await;
        let restart = approved_restart(app, 40).await;

        let taken = acquire(app, "restart", "k8bw2f", &restart, None, 300, true, &[]).await.unwrap();
        assert_eq!(taken["safety"]["escalated"], true);
        assert_eq!(taken["safety"]["held_leases"][0]["owner"], "k8bw2f");
        assert_eq!(taken["safety"]["held_leases"][0]["own"], true, "自己的那把要標出來，而且不算擋");
        assert_eq!(taken["lease"]["owner"], "k8bw2f");
    }

    /// 別人握著 rebuild 時照擋：「窗口一次只給一個人」的語意不變。
    #[tokio::test]
    async fn someone_elses_rebuild_lease_still_blocks_an_escalated_restart() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        hold_rebuild(app, "someone-else").await;
        let restart = approved_restart(app, 40).await;

        let refused = acquire(app, "restart", "k8bw2f", &restart, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "not_idle");
        assert_eq!(detail["safety"]["escalated"], true, "有升級，擋下來的是別人的租約");
        assert_eq!(detail["safety"]["held_leases"][0]["owner"], "someone-else");
        assert_eq!(detail["safety"]["held_leases"][0]["own"], false);
        assert!(store::lease(&app.db, "restart").await.unwrap().is_none(), "拿不到就什麼都不該留下");
    }

    /// 唯讀 safety：不帶 owner 維持舊行為（全部列出、全部算擋）；帶了 owner 用同一條規則排除自己的。
    #[tokio::test]
    async fn read_only_safety_only_excludes_leases_when_asked_as_their_owner() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        hold_rebuild(app, "k8bw2f").await;
        let restart = approved_restart(app, 40).await;

        let anonymous = safety_for(app, &[], Some(&restart)).await.unwrap();
        assert_eq!((anonymous["escalated"].as_bool(), anonymous["safe"].as_bool()), (Some(true), Some(false)), "不帶 owner：每一把都算擋");
        assert_eq!(anonymous["held_leases"][0]["owner"], "k8bw2f");
        assert_eq!(anonymous["held_leases"][0]["own"], false);
        assert_eq!(anonymous["owner"], Value::Null);

        let mine = safety_as(app, &[], Some(&restart), Some("k8bw2f")).await.unwrap();
        assert_eq!(mine["safe"], true, "以握著它的人問：自己的租約不擋");
        assert_eq!(mine["held_leases"][0]["own"], true);
        assert_eq!(mine["owner"], "k8bw2f");

        assert_eq!(safety_as(app, &[], Some(&restart), Some("someone-else")).await.unwrap()["safe"], false, "別人問照擋");

        // 全靜止模式本來就不看租約：沒有可升級的核准時，帶了 owner 也照樣由 working 決定。
        sqlx::query("DELETE FROM supervisor_approvals").execute(&app.db).await.unwrap();
        let strict = safety_as(app, &[], None, Some("k8bw2f")).await.unwrap();
        assert_eq!(strict["escalated"], false);
        assert_eq!(strict["safe"], false, "非升級模式行為不變：有人在思考就不安全");
    }

    /// AGM 三顆（巡檢、協調者、建置 child）由呼叫端排除，門檻高低都一樣——放寬不是「連排除都不管了」。
    #[tokio::test]
    async fn excluded_agm_bots_are_ignored_at_both_thresholds() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        for id in ["manager", "responder", "builder"] {
            thinking_bot(app, &pid, id).await;
        }
        // 連 AGM 自己正在送出的一筆也照排除：它就是那個要換 binary 的人。
        sqlx::query("UPDATE turns SET delivery='pending' WHERE id='builder'").execute(&app.db).await.unwrap();
        let exclude = ["manager".to_string(), "responder".to_string(), "builder".to_string()];

        let strict = safety(app, &exclude).await.unwrap();
        assert_eq!((strict["escalated"].as_bool(), strict["safe"].as_bool()), (Some(false), Some(true)));
        assert!(strict["working"].as_array().unwrap().is_empty());

        approved_window(app, 40).await;
        let relaxed = safety(app, &exclude).await.unwrap();
        assert_eq!((relaxed["escalated"].as_bool(), relaxed["safe"].as_bool()), (Some(true), Some(true)));
        assert!(relaxed["delivering"].as_array().unwrap().is_empty(), "被排除的 bot 連送達中都不算數");
        // 沒排除的話，那筆正在送出的就會擋下來。
        assert_eq!(safety(app, &[]).await.unwrap()["safe"], false);
    }

    /// 核准是給申請的那個人的：bot B 拿 bot A 已核准的申請 acquire，不開窗口、也不借走 A 的等待（review2 sup #5）。
    #[tokio::test]
    async fn someone_elses_approval_does_not_open_my_window() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let a = approved_window(app, 40).await; // 申請者 ops
        let refused = acquire(app, "rebuild", "bot-b", &a, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "approval_owner_mismatch");
        assert!(store::lease(&app.db, "rebuild").await.unwrap().is_none(), "什麼都沒拿到");
        assert_eq!(store::approval(&app.db, &a).await.unwrap().unwrap().status, "approved", "A 的核准原封不動");
        acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.expect("申請者自己照常拿得到");
    }

    /// 執行端拿了窗口之後掛掉、租約過期沒 release：用**同一張**核准重試不能再開一個窗口（review2 sup #6）。
    #[tokio::test]
    async fn an_approval_whose_window_expired_unreleased_cannot_open_another() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let a = approved_window(app, 0).await;
        acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.unwrap();
        let decided_at = store::approval(&app.db, &a).await.unwrap().unwrap().decided_at;
        sqlx::query("UPDATE supervisor_leases SET expires_at='2000-01-01T00:00:00Z' WHERE resource='rebuild'").execute(&app.db).await.unwrap();

        let refused = acquire(app, "rebuild", "ops", &a, None, 300, true, &[]).await.unwrap_err();
        let LcError::Conflict(detail) = &refused else { panic!("expected a conflict, got {refused:?}") };
        assert_eq!(detail["reason"], "approval_already_used");
        let after = store::approval(&app.db, &a).await.unwrap().unwrap();
        assert_eq!(after.status, "consumed", "那個窗口已經用掉它，當場記下來");
        assert_eq!(after.decided_at, decided_at, "消耗不是裁示，不覆寫核准時間");
        assert_eq!(after.decided_by.as_deref(), Some("AGM"));
        let notes = store::approval_decisions(&app.db).await.unwrap();
        assert!(notes.get(&a).is_some_and(|v| v.iter().any(|n| n["to"] == "consumed")), "誰在什麼時候消耗的記在歷程裡");

        // 新申請的核准照常能接手那個過期的窗口。
        let b = approved_window(app, 0).await;
        acquire(app, "rebuild", "ops", &b, None, 300, true, &[]).await.expect("別張核准照常接手");
    }

    /// 換 commit 重新申請（supersedes）：舊的標 superseded、它的等待接過來，升級計時不因為 main 動了就歸零（review2 sup 新發現 2）。
    #[tokio::test]
    async fn a_new_commit_carries_the_wait_of_the_approval_it_supersedes() {
        let e = crate::testing::env().await;
        let (app, pid) = (&e.app, e.project_id.clone());
        thinking_bot(app, &pid, "user").await;
        let old = approved_window(app, 40).await;
        let decided_old = store::approval(&app.db, &old).await.unwrap().unwrap().decided_at.unwrap();

        let out = store::create_approval_superseding(&app.db, "ops", "rebuild", "release", Some("h2"), None, None, Some(&old), None).await.unwrap();
        assert_eq!(out.superseded.as_deref(), Some(old.as_str()));
        let new = out.approval;
        assert_eq!(new.wait_since.as_deref(), Some(decided_old.as_str()), "接過來的是舊那筆被核准的時間");
        let o = store::approval(&app.db, &old).await.unwrap().unwrap();
        assert_eq!(o.status, "superseded");
        assert_eq!(o.refusal(&crate::db::now(), "rebuild", None), Some("approval_superseded"));
        assert_eq!(o.decided_at.as_deref(), Some(decided_old.as_str()), "取代不覆寫核准時間");

        // 新的剛被核准（decided_at = 現在），但等待從 40 分鐘前算：直接升級。
        store::decide_approval(&app.db, &new.id, "approved", "AGM", None, None).await.unwrap();
        let s = safety_for(app, &[], Some(&new.id)).await.unwrap();
        assert_eq!((s["escalated"].as_bool(), s["safe"].as_bool()), (Some(true), Some(true)), "{s}");
        assert!(s["waited_secs"].as_i64().unwrap() >= 2400);

        // 接力可以一直往下傳；別人的、或用途不同的不能取代。
        let third = store::create_approval_superseding(&app.db, "ops", "rebuild", "release", Some("h3"), None, None, Some(&new.id), None).await.unwrap().approval;
        assert_eq!(third.wait_since.as_deref(), Some(decided_old.as_str()));
        for (who, purpose) in [("bot-b", "rebuild"), ("ops", "restart")] {
            let err = store::create_approval_superseding(&app.db, who, purpose, "release", Some("h4"), None, None, Some(&third.id), None).await.unwrap_err();
            assert!(err.downcast_ref::<store::ApprovalSupersedeRefused>().is_some(), "{who}/{purpose}: {err}");
        }
        assert_eq!(store::approval(&app.db, &third.id).await.unwrap().unwrap().status, "pending", "被拒的取代什麼都不動");

        // 舊的已經不能用（被駁）：新的照開，但不接等待。
        store::decide_approval(&app.db, &third.id, "denied", "AGM", None, None).await.unwrap();
        let fresh = store::create_approval_superseding(&app.db, "ops", "rebuild", "release", Some("h5"), None, None, Some(&third.id), None).await.unwrap();
        assert_eq!((fresh.superseded, fresh.approval.wait_since), (None, None));
        assert_eq!(store::approval(&app.db, &third.id).await.unwrap().unwrap().status, "denied");
    }

    /// 被取代那筆還沒送給協調者的 `approval_requested` 一起收掉，不要叫它醒來裁示一筆作廢的申請。
    #[tokio::test]
    async fn superseding_retires_the_old_requests_unsent_inbox_event() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let old = store::create_approval(&app.db, "ops", "rebuild", "release", Some("h1"), None, None).await.unwrap().approval;
        store::push_inbox(&app.db, &format!("approval:{}:requested", old.id), "approval_requested", None, None, None, &json!({})).await.unwrap();
        store::create_approval_superseding(&app.db, "ops", "rebuild", "release", Some("h2"), None, None, Some(&old.id), None).await.unwrap();
        let (state, acked): (String, Option<String>) = sqlx::query_as("SELECT state, acked_by FROM supervisor_inbox WHERE event_key=?")
            .bind(format!("approval:{}:requested", old.id))
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((state.as_str(), acked.as_deref()), ("handled", Some("daemon")));
    }

    fn approval(status: &str, purpose: &str, commit: Option<&str>, expires: Option<&str>) -> Approval {
        Approval {
            request_reason: None,
            id: "ap1".into(),
            supervisor_id: "AGM".into(),
            requester: "bot-x".into(),
            purpose: purpose.into(),
            scope: "daemon".into(),
            target_commit: commit.map(str::to_string),
            status: status.into(),
            decided_by: Some("AGM".into()),
            decided_at: Some("2026-09-12T00:00:00Z".into()),
            reason: None,
            expires_at: expires.map(str::to_string),
            client_request_id: None,
            created_at: "2026-09-12T00:00:00Z".into(),
            updated_at: "2026-09-12T00:00:00Z".into(),
            wait_since: None,
        }
    }

    const NOW: &str = "2026-09-12T12:00:00Z";

    #[test]
    fn only_an_approved_unexpired_approval_for_this_commit_counts() {
        let ok = approval("approved", "rebuild", Some("abc123"), Some("2026-09-12T13:00:00Z"));
        assert_eq!(ok.refusal(NOW, "rebuild", Some("abc123")), None);
        // The same yes does not carry to a different tree, a different operation, or tomorrow.
        assert_eq!(ok.refusal(NOW, "rebuild", Some("def456")), Some("approval_commit_mismatch"));
        assert_eq!(ok.refusal(NOW, "restart", Some("abc123")), Some("approval_purpose_mismatch"));
        assert_eq!(ok.refusal(NOW, "rebuild", None), Some("approval_commit_required"));
        let expired = approval("approved", "rebuild", Some("abc123"), Some("2026-09-12T11:00:00Z"));
        assert_eq!(expired.refusal(NOW, "rebuild", Some("abc123")), Some("approval_expired"));
    }

    #[test]
    fn a_revoked_or_undecided_approval_is_not_a_yes() {
        for (status, why) in [
            ("pending", "approval_not_decided"),
            ("denied", "approval_denied"),
            ("revoked", "approval_revoked"),
            ("consumed", "approval_already_used"),
        ] {
            let a = approval(status, "rebuild", None, None);
            assert_eq!(a.refusal(NOW, "rebuild", None), Some(why), "{status}");
        }
        // No deadline is allowed — it is the approval's author's choice — and no commit means
        // the approval was written for a resource, not a tree.
        assert_eq!(approval("approved", "rebuild", None, None).refusal(NOW, "rebuild", Some("x")), None);
    }

    #[test]
    fn a_restart_window_pauses_dispatch_but_a_rebuild_does_not() {
        assert!(EXCLUSIVE.contains(&"restart"));
        assert!(!EXCLUSIVE.contains(&"rebuild"));
        assert!(pause_note("2026-09-12T12:15:00Z").contains("12:15"));
    }

    /// A lease is a permission with a clock on it, so it cannot outlive the permission — on
    /// acquire *or* on renew. Otherwise "approved until 14:00" quietly becomes "holding the box
    /// until 14:45".
    #[test]
    fn a_lease_never_outlives_its_approval() {
        let asked = "2026-09-12T12:45:00Z";
        assert_eq!(lease_deadline(asked, Some("2026-09-12T14:00:00Z")), asked, "approval outlasts it: keep the ask");
        assert_eq!(
            lease_deadline(asked, Some("2026-09-12T12:10:00Z")),
            "2026-09-12T12:10:00Z",
            "approval lapses first: the lease ends with it"
        );
        // An approval with no deadline is the author's choice; it does not shorten anything.
        assert_eq!(lease_deadline(asked, None), asked);
    }
}
