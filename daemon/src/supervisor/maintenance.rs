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
//! **What the pause actually covers, stated narrowly because the gap matters:** holding a
//! `restart` lease stops *supervisor assignment dispatch* — `controller::dispatch`, the path
//! AGM's own work goes out through. It does **not** gate `POST /api/bots/{id}/prompt`; a user
//! typing into a bot still goes through during the window. So the lease makes the window quiet on the one channel
//! the supervisor controls, not on the whole daemon. Closing that gap means a check inside
//! `lifecycle::prompt` itself, which reaches well outside this module and is not attempted here.
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

/// Holding this one pauses **supervisor assignment dispatch**. A rebuild does not interrupt
/// anybody; a restart does, and handing a bot new work while waiting to kill its session is the
/// race this closes — for assignments. Ordinary prompts are not gated (see
/// the module docs); do not read a held restart lease as "nothing can reach any bot".
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
            .filter(|a| !a.expires_at.as_deref().is_some_and(|t| t <= now.as_str())),
        None => store::oldest_live_window_approval(&app.db, &now).await.map_err(|e| LcError::Upstream(e.to_string()))?,
    };
    let Some(a) = found else { return Ok(None) };
    let Some(decided) = a.decided_at.as_deref() else { return Ok(None) };
    let waited = waited_secs(decided, &now);
    Ok(Some(Escalation { approval_id: a.id, waited_secs: waited, escalated: waited >= escalate_after_secs() }))
}

pub async fn escalation(app: &Arc<App>) -> Result<Option<Escalation>, LcError> {
    escalation_for(app, None).await
}

/// 還握在別人手上的租約。縮小封鎖面時這是**唯一**新增的阻擋條件：窗口一次只給一個人。
async fn held_leases(app: &Arc<App>) -> Result<Vec<Value>, LcError> {
    let now = crate::db::now();
    let mut out = Vec::new();
    for resource in RESOURCES {
        let l = store::lease(&app.db, resource).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        if let Some(l) = l.filter(|l| l.held_at(&now)) {
            out.push(json!({"resource": resource, "owner": l.owner, "fence": l.fence, "expires_at": l.expires_at}));
        }
    }
    Ok(out)
}

/// 這顆 bot 此刻在不在**送達臨界區**：有排隊中待送的 prompt（`queued`），或 daemon 正在往 pane
/// 打字／送出（`in_flight` 而 `delivery` 還是 `pending`）。回傳擋住的那一筆 turn id。
///
/// `delivery='unknown'` 不算：那是送完但驗不到、停在那裡等人處理的狀態，跟 `blocked` 一樣可以等很久。
async fn delivery_critical(pool: &sqlx::SqlitePool, bot_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE c.bot_id = ? AND (t.status='queued' OR (t.status='in_flight' AND t.delivery='pending'))
          LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?)
}

/// Default and maximum lease lifetime. Long enough for a release build and a restart, short
/// enough that a crashed holder does not block the next window for an afternoon.
pub const DEFAULT_TTL_SECS: i64 = 900;
pub const MAX_TTL_SECS: i64 = 3600;

fn iso_in(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Is a restart window currently held by someone? Used by the dispatcher to hold work back.
pub async fn dispatch_paused(app: &Arc<App>) -> Option<String> {
    for resource in EXCLUSIVE {
        if let Ok(Some(l)) = store::lease(&app.db, resource).await {
            if l.held_at(&crate::db::now()) {
                return l.expires_at;
            }
        }
    }
    None
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
pub async fn release(app: &Arc<App>, resource: &str, owner: &str, fence: i64) -> anyhow::Result<bool> {
    let released = store::release_lease(&app.db, resource, owner, fence).await?;
    if released {
        if let Some(l) = store::lease(&app.db, resource).await? {
            if let Some(ap) = l.approval_id.as_deref() {
                if let Ok(Some(a)) = store::approval(&app.db, ap).await {
                    if a.status == "approved" {
                        let _ = store::decide_approval(&app.db, ap, "consumed", owner, Some("lease released"), None).await;
                    }
                }
            }
        }
        if EXCLUSIVE.contains(&resource) && dispatch_paused(app).await.is_none() {
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
                match release(app, resource, &owner, l.fence).await {
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
    let held = held_leases(app).await?;
    let esc = escalation_for(app, approval_id).await?;
    // Not knowing about even one bot is enough to refuse: the window's whole promise is that
    // nothing is running, and we cannot promise that about a bot we could not look at.
    let strict = working.is_empty() && in_flight.is_empty() && unreadable.is_empty();
    // 縮小封鎖面：思考中不再擋，送達臨界區、還握著的租約與讀不到畫面照擋（SPEC §18.10）。
    let escalated = esc.as_ref().is_some_and(|e| e.escalated);
    let safe = if escalated { delivering.is_empty() && unreadable.is_empty() && held.is_empty() } else { strict };
    Ok(json!({
        "safe": safe,
        // 這一刻是不是已經放寬了，以及最早那筆沒用掉的核准等了多久（沒有這種核准時是 null）。
        "escalated": escalated,
        "waited_secs": esc.as_ref().map(|e| e.waited_secs),
        "escalation_approval_id": esc.as_ref().map(|e| e.approval_id.clone()),
        "escalate_after_secs": escalate_after_secs(),
        // 放寬之後仍然會擋的兩項，列出來才看得懂為什麼還是 false。
        "delivering": delivering,
        "held_leases": held,
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

    // 放寬與否只看**這一筆**核准等了多久：別人放著沒用的核准不能替它開門。
    let safety = safety_for(app, exclude, Some(&approval.id)).await?;
    if require_idle && safety.get("safe") != Some(&Value::Bool(true)) {
        return Err(LcError::conflict(
            "something is still running; wait for a safe window",
            json!({"reason": "not_idle", "safety": safety}),
        ));
    }

    // The lease may not outlive the permission it rests on. Otherwise "approved until 14:00"
    // quietly becomes "holding the box until 14:45", which is a different promise than the one
    // anybody agreed to.
    let expires_at = lease_deadline(&iso_in(ttl), approval.expires_at.as_deref());
    let taken = store::acquire_lease(
        &app.db,
        resource,
        owner,
        Some(&approval.id),
        commit,
        &expires_at,
        &json!({"require_idle": require_idle, "safety": safety}),
    )
    .await
    .map_err(|e| LcError::Upstream(e.to_string()))?;

    let Some(lease) = taken else {
        let held = store::lease(&app.db, resource).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        return Err(LcError::conflict(
            "someone else holds this window",
            json!({"reason": "lease_held", "lease": held.map(|l| l.to_json())}),
        ));
    };
    let escalated = safety.get("escalated") == Some(&Value::Bool(true));
    let waited = safety.get("waited_secs").and_then(|v| v.as_i64()).unwrap_or(0);
    tracing::info!(resource, owner, fence = lease.fence, escalated, waited, "maintenance lease acquired");
    if escalated {
        // 換版紀錄要看得出這次不是等到全靜止才換的（SPEC §18.10）。整份 safety 也寫在租約 meta 裡。
        tracing::warn!(
            resource,
            owner,
            waited,
            "這個窗口是升級後（縮小封鎖面）才拿到的：有 bot 還在回合中，只確認了沒人在送達臨界區、沒有別的租約、畫面都讀得到"
        );
    }
    app.emit("supervisor_changed", json!({"lease": lease.to_json()})).await;
    Ok(json!({"lease": lease.to_json(), "approval": approval.to_json(), "safety": safety}))
}

/// The earlier of the requested deadline and the approval's own expiry.
///
/// A lease is only ever a permission with a clock on it; it cannot be renewed past the moment
/// that permission lapses, and it cannot be granted past it either.
pub fn lease_deadline(requested: &str, approval_expires_at: Option<&str>) -> String {
    match approval_expires_at {
        // RFC3339 in UTC with the same precision sorts lexicographically, and both sides come
        // from `iso_in` / the approvals table, so a string compare is the right comparison.
        Some(exp) if exp < requested => exp.to_string(),
        _ => requested.to_string(),
    }
}

/// Dispatch is paused; keep the assignment queued until the window closes rather than sending
/// work into a session that is about to be restarted.
pub fn pause_note(until: &str) -> String {
    format!("restart window held until {until}")
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

    /// 已核准、還沒用掉的 rebuild 窗口，決定時間往前推 `mins` 分鐘。
    async fn approved_window(app: &Arc<App>, mins: i64) -> String {
        let a = store::create_approval(&app.db, "bot", "rebuild", "release", None, None, None).await.unwrap().approval;
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
            &(chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true), &json!({}))
            .await.unwrap().unwrap();
        let s = safety(app, &[]).await.unwrap();
        assert_eq!(s["safe"], false, "窗口一次只給一個人");
        assert_eq!(s["held_leases"][0]["owner"], "someone-else");
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

    fn approval(status: &str, purpose: &str, commit: Option<&str>, expires: Option<&str>) -> Approval {
        Approval {
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
