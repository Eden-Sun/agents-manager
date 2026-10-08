//! 部署等換版窗口等太久就告訴使用者、讓使用者調度（使用者裁示 2026-10-04：「等待部署超過 3 分鐘，馬上通知 user 調度」，SPEC §18.10）。
//!
//! 例行自動部署與「立即部署」最後都走 daemon-swap → `POST /api/services/daemon-swap/restart-window`：要等沒有人 working、
//! 沒有人在送達臨界區才換版，拿不到就 DEFER，同一張核准等滿門檻（5 分鐘，或線上落後 ≥3 個程式碼 commit）才放寬。使用者常常不知道它卡在等誰。所以：
//!
//! - **一次部署**＝同一個 owner（`daemon-update-kick`）連續在試的那段：每次試窗口都回報到這裡（[`observe`]），換 commit 或換核准
//!   都算同一次；換版拿到窗口、換版完成、或 [`STALE_SECS`] 沒再試（kick 放棄、請求取消）才算結束。
//! - 從開始等窗口（核准的 `waiting_since`）起滿 [`NOTIFY_AFTER_SECS`] 還沒拿到，就**通知一次**：WS `deploy_wait`（網頁 header＋toast）
//!   與 supervisor inbox `deploy_waiting`（巡檢收、叫醒；它有使用者手機上的 Remote Control）。之後只有使用者操作、自動放寬生效、
//!   拿到窗口、換版完成或放棄才推 inbox（同一個 `id`，`rev` 遞增）；擋住名單變動只推 WS，不每 15 秒洗 inbox。
//!   「拿到窗口→§3a abort 交還」同一個 commit 最多每 [`SWAP_CYCLE_THROTTLE_SECS`] 推一次。
//! - 使用者在 header 按「現在換版」＝這次部署直接放寬（[`user_escalated_for`]，等同等滿門檻：working 不擋，送達臨界區、
//!   別人的租約、讀不到狀態照樣擋），只對這次部署的自動核准有效；「先等」＝收起通知。
//! - 狀態存在 `<data_dir>/deploy-wait.json`：換版本身就是重啟 daemon，新 daemon 開機讀回來才報得出「換好了」或「回滾了」。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::lifecycle::LcError;
use crate::state::App;

/// 開始等窗口起多久還沒拿到就通知使用者。
pub const NOTIFY_AFTER_SECS: i64 = 180;
/// 這麼久沒再試窗口＝這次部署不等了（kick 每 5 分鐘一輪，一輪試 3 分鐘；留三輪的餘裕）。
pub const STALE_SECS: i64 = 15 * 60;
/// 「拿到窗口→3a ABORT→交還」循環同一 sha 最多每 30 分鐘一則（issue #856）。
pub const SWAP_CYCLE_THROTTLE_SECS: i64 = 30 * 60;
/// 結束的那一則在 `/api/state` 再留這麼久，重整的網頁還對得上最後一次更新。
const KEEP_ENDED_SECS: i64 = 10 * 60;
pub const FILE: &str = "deploy-wait.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 還在等窗口。
    Waiting,
    /// 拿到窗口了，換版中。
    Swapping,
    /// 新 daemon 起來了，跑的是要上的 commit。
    Done,
    /// 不等了：太久沒再試、核准撤了，或換版後跑的不是要上的 commit（回滾）。
    Abandoned,
}

/// 擋住窗口的一方。`why`：`working`／`delivering`／`unreadable`／`lease`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    pub bot_id: Option<String>,
    pub name: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    /// 這次部署的穩定 id：通知、更新、按鈕都認它。
    pub id: String,
    pub owner: String,
    pub approval_id: String,
    pub commit: String,
    /// 開始等窗口的時間（核准的 `waiting_since`，換核准取早的）。
    pub since: String,
    pub last_attempt_at: String,
    pub blockers: Vec<Blocker>,
    /// 預計幾點自動放寬（已經放寬時是 null）。
    pub escalates_at: Option<String>,
    /// 使用者按了「現在換版」。
    pub user_escalated: bool,
    /// 等滿門檻已自動放寬（issue #856）。
    #[serde(default)]
    pub auto_escalated: bool,
    pub notified_at: Option<String>,
    /// 使用者按了「先等」：header 收起來（狀態照樣更新）。
    pub dismissed: bool,
    pub phase: Phase,
    /// 通知發出後每一次有意義的狀態轉換 +1。
    pub rev: u32,
    pub ended_at: Option<String>,
    /// 各 commit 上一次發出「拿到換版窗口」的時間，節流 3a ABORT 循環用（issue #856）。
    #[serde(default)]
    pub swap_announced_at: BTreeMap<String, String>,
}

fn path(app: &impl crate::capabilities::DataDir) -> PathBuf {
    app.data_dir().join(FILE)
}

fn save(app: &impl crate::capabilities::DataDir, w: Option<&Wait>) {
    let p = path(app);
    let res = match w {
        // 暫存檔 + fsync + rename：寫到一半當機，正式檔也不會是半份（issue #869）。
        Some(w) => serde_json::to_vec(w).map_err(std::io::Error::other).and_then(|b| crate::lifecycle::setup::write_private(&p, &b)).map(|()| sync_dir(&p)),
        None => match std::fs::remove_file(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    };
    if let Err(e) = res {
        tracing::warn!(error = %e, file = %p.display(), "could not persist the deploy wait");
    }
}

/// best effort：rename 之後再 fsync 目錄，讓新名字也落盤；失敗只記 debug。
fn sync_dir(file: &std::path::Path) {
    let Some(dir) = file.parent() else { return };
    if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::debug!(error = %e, dir = %dir.display(), "could not fsync the data dir after saving the deploy wait");
    }
}

/// 讀回上一顆 daemon 留下的狀態檔。沒有檔＝真的沒有等待；讀不了或壞檔都不能當成「沒有等待」靜默覆寫（issue #869）：
/// 讀不了就原樣留著，壞檔改名成 `deploy-wait.json.corrupt-<時間>` 保留證據。
fn load(app: &impl crate::capabilities::DataDir) -> Option<Wait> {
    let p = path(app);
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::error!(path = %p.display(), error = %e, "deploy wait state unreadable; keeping it for inspection");
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(w) => Some(w),
        Err(e) => {
            let stamp = crate::db::now().replace(':', "");
            let aside = p.with_file_name(format!("{FILE}.corrupt-{stamp}"));
            match std::fs::rename(&p, &aside) {
                Ok(()) => tracing::error!(path = %p.display(), aside = %aside.display(), error = %e, "deploy wait state is corrupt; moved aside"),
                Err(re) => tracing::error!(path = %p.display(), error = %e, rename_error = %re, "deploy wait state is corrupt and could not be moved aside"),
            }
            None
        }
    }
}

fn secs_between(a: &str, b: &str) -> i64 {
    crate::supervisor::maintenance::waited_secs(a, b)
}

/// 要對外說的那一次狀態轉換（issue #856：同一個部署 sha 只在狀態轉換時推）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Announce {
    First,
    UserAction,
    AutoEscalated,
    Swapping,
    Done,
    Abandoned,
}

/// 門檻到了就標成已通知（回 `First`）；已經通知過、而且這次有意義的狀態轉換（`transition`）就對應回傳。純函式，好測。
fn evaluate(w: &mut Wait, transition: Option<Announce>, now: &str) -> Option<Announce> {
    if w.notified_at.is_none() {
        if w.phase == Phase::Waiting && secs_between(&w.since, now) >= NOTIFY_AFTER_SECS {
            w.notified_at = Some(now.to_string());
            w.rev += 1;
            return Some(Announce::First);
        }
        return None;
    }
    transition.map(|a| {
        w.rev += 1;
        a
    })
}

/// 從 restart-window 的拒絕裡讀出誰在擋，以及是否已自動放寬。
fn blockers_of(err: &LcError) -> Option<(Vec<Blocker>, Option<String>, bool)> {
    let LcError::Conflict(v) = err else { return None };
    let mut out = Vec::new();
    let safety = v.get("safety").or_else(|| v.get("detail").and_then(|d| d.get("safety")))?;
    for (key, why) in [("working", "working"), ("delivering", "delivering"), ("unreadable", "unreadable")] {
        for b in safety.get(key).and_then(Value::as_array).into_iter().flatten() {
            let name = b.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
            let bot_id = b.get("bot_id").and_then(Value::as_str).map(String::from);
            if !out.iter().any(|x: &Blocker| x.bot_id == bot_id && x.why == why) {
                out.push(Blocker { bot_id, name, why: why.into() });
            }
        }
    }
    for l in safety.get("held_leases").and_then(Value::as_array).into_iter().flatten() {
        if l.get("own") != Some(&Value::Bool(true)) {
            let owner = l.get("owner").and_then(Value::as_str).unwrap_or("?");
            out.push(Blocker { bot_id: None, name: owner.to_string(), why: "lease".into() });
        }
    }
    let eta = v.get("escalates_at").or_else(|| v.get("detail").and_then(|d| d.get("escalates_at"))).and_then(Value::as_str).map(String::from);
    let escalated = safety.get("escalated").and_then(Value::as_bool).unwrap_or(false);
    Some((out, eta, escalated))
}

/// 每次試 restart 窗口之後回報（`service_daemon_swap_restart_window`）。
pub async fn observe(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::deploy_wait::DeployWaitState), owner: &str, approval_id: &str, commit: &str, outcome: Result<bool, &LcError>) {
    let since = match crate::supervisor::store::approval(app.db(), approval_id).await {
        Ok(Some(a)) => a.waiting_since().map(String::from),
        _ => None,
    };
    let now = crate::db::now();
    let since = since.unwrap_or_else(|| now.clone());
    let mut out = Vec::new();
    let mut emit_ws = false;
    let ws_wait = {
        let mut g = app.deploy_wait().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // 換版中又來試＝上一次拿到窗口卻沒換成（窗口逾時交還），還是同一次部署。
        let continuing = g.as_ref().is_some_and(|w| {
            w.owner == owner && matches!(w.phase, Phase::Waiting | Phase::Swapping) && secs_between(&w.last_attempt_at, &now) < STALE_SECS
        });
        if !continuing {
            if let Some(mut old) = g.take() {
                if old.phase == Phase::Waiting && old.notified_at.is_some() {
                    old.phase = Phase::Abandoned;
                    old.ended_at = Some(now.clone());
                    old.rev += 1;
                    out.push((Announce::Abandoned, old));
                }
            }
            *g = Some(Wait {
                id: crate::db::ulid(),
                owner: owner.to_string(),
                approval_id: approval_id.to_string(),
                commit: commit.to_string(),
                since: since.clone(),
                last_attempt_at: now.clone(),
                blockers: Vec::new(),
                escalates_at: None,
                user_escalated: false,
                auto_escalated: false,
                notified_at: None,
                dismissed: false,
                phase: Phase::Waiting,
                rev: 0,
                ended_at: None,
                swap_announced_at: BTreeMap::new(),
            });
        }
        let w = g.as_mut().expect("just set");
        if crate::db::cmp_ts(&since, &w.since).is_lt() {
            w.since = since;
        }
        w.approval_id = approval_id.to_string();
        if w.commit != commit {
            w.commit = commit.to_string();
        }
        w.last_attempt_at = now.clone();
        let mut transition = None;
        match outcome {
            Ok(auto_escalated) => {
                let entering_swap = w.phase != Phase::Swapping;
                w.phase = Phase::Swapping;
                w.blockers.clear();
                if auto_escalated && !w.user_escalated {
                    w.auto_escalated = true;
                }
                if entering_swap && w.notified_at.is_some() {
                    // 拿到窗口：同一個 sha 最多每 30 分鐘一則（issue #856）。
                    w.swap_announced_at.retain(|_, at| secs_between(at, &now) < SWAP_CYCLE_THROTTLE_SECS);
                    let throttle_ok = w.swap_announced_at.get(commit).is_none_or(|t| secs_between(t, &now) >= SWAP_CYCLE_THROTTLE_SECS);
                    if throttle_ok {
                        w.swap_announced_at.insert(commit.to_string(), now.clone());
                        transition = Some(Announce::Swapping);
                    } else {
                        emit_ws = true;
                    }
                }
            }
            Err(e) => {
                if w.phase == Phase::Swapping {
                    w.phase = Phase::Waiting;
                    // 3a ABORT 交還：回到 waiting，不推 inbox（循環同一 sha 最多 30 分鐘一則，issue #856），但發 WS
                    emit_ws = true;
                }
                if let Some((blockers, eta, escalated)) = blockers_of(e) {
                    // 名單一樣、只是順序不同不算換人。
                    let key = |b: &[Blocker]| {
                        let mut k: Vec<String> = b.iter().map(|x| format!("{}|{:?}|{}", x.why, x.bot_id, x.name)).collect();
                        k.sort();
                        k
                    };
                    let blockers_changed = key(&blockers) != key(&w.blockers);
                    let auto_escalated_now = w.phase == Phase::Waiting
                        && !w.user_escalated
                        && !w.auto_escalated
                        && escalated;
                    w.blockers = blockers;
                    w.escalates_at = if escalated { None } else { eta.or_else(|| w.escalates_at.clone()) };
                    if auto_escalated_now {
                        w.auto_escalated = true;
                        transition = Some(Announce::AutoEscalated);
                    } else if blockers_changed {
                        // 擋住名單變動不單獨推 inbox（issue #856）；只發 WS 讓 web header 更新
                        emit_ws = true;
                    }
                }
            }
        }
        // 還沒通知就拿到窗口的：不必讓使用者知道。
        if let Some(a) = evaluate(w, transition, &now) {
            out.push((a, w.clone()));
        }
        let ws_w = (out.is_empty() && emit_ws && w.notified_at.is_some()).then(|| w.clone());
        save(app, g.as_ref());
        ws_w
    };
    if let Some(w) = ws_wait {
        let v = view_of(&w, &now);
        app.emit("deploy_wait", json!({"wait": v, "first": false})).await;
    }
    for (a, w) in out {
        announce(app, a, &w).await;
    }
}

/// 背景每 15 秒：門檻到了就通知；太久沒再試就收掉；結束的過一陣子清掉。
pub async fn tick(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::deploy_wait::DeployWaitState)) {
    let now = crate::db::now();
    let mut out = None;
    {
        let mut g = app.deploy_wait().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(w) = g.as_mut() else { return };
        match w.phase {
            Phase::Waiting if secs_between(&w.last_attempt_at, &now) >= STALE_SECS => {
                w.phase = Phase::Abandoned;
                w.ended_at = Some(now.clone());
                if w.notified_at.is_some() {
                    w.rev += 1;
                    out = Some((Announce::Abandoned, w.clone()));
                }
                save(app, g.as_ref());
            }
            Phase::Waiting => {
                if w.notified_at.is_none() {
                    if let Some(a) = evaluate(w, None, &now) {
                        out = Some((a, w.clone()));
                        save(app, g.as_ref());
                    }
                }
            }
            Phase::Done | Phase::Abandoned => {
                if w.ended_at.as_deref().is_none_or(|t| secs_between(t, &now) >= KEEP_ENDED_SECS) {
                    *g = None;
                    save(app, None);
                }
            }
            Phase::Swapping => {
                // 換版中：daemon 馬上就會被換掉；換不成（窗口逾時交還）時 kick 下一輪會再試，回到 Waiting 那條路。
                if secs_between(&w.last_attempt_at, &now) >= STALE_SECS {
                    w.phase = Phase::Abandoned;
                    w.ended_at = Some(now.clone());
                    if w.notified_at.is_some() {
                        w.rev += 1;
                        out = Some((Announce::Abandoned, w.clone()));
                    }
                    save(app, g.as_ref());
                }
            }
        }
    }
    if let Some((a, w)) = out {
        announce(app, a, &w).await;
    }
}

/// daemon 開機：讀回上一顆 daemon 留下的。換版中的那一則，看這顆跑的是不是要上的 commit：是＝換好了，不是＝回滾了。
pub async fn startup(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::deploy_wait::DeployWaitState)) {
    startup_as(app, crate::build_info::BUILD_SHA).await
}

async fn startup_as(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::deploy_wait::DeployWaitState), running_sha: &str) {
    let Some(mut w) = load(app) else { return };
    let now = crate::db::now();
    let mut out = None;
    if w.phase == Phase::Swapping {
        let same = !running_sha.is_empty() && (w.commit.starts_with(running_sha) || running_sha.starts_with(&w.commit));
        let announce_type = if same {
            w.phase = Phase::Done;
            Announce::Done
        } else {
            w.phase = Phase::Abandoned;
            Announce::Abandoned
        };
        w.ended_at = Some(now);
        if w.notified_at.is_some() {
            w.rev += 1;
            out = Some((announce_type, w.clone()));
        }
    }
    save(app, Some(&w));
    *app.deploy_wait().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(w);
    if let Some((a, w)) = out {
        announce(app, a, &w).await;
    }
}

/// 給人看的一句話（inbox 與 log 用）。
pub fn summary(w: &Wait, now: &str) -> String {
    let short = &w.commit[..w.commit.len().min(8)];
    let mins = secs_between(&w.since, now) / 60;
    let who = if w.blockers.is_empty() {
        "（沒有回報擋住的人）".to_string()
    } else {
        w.blockers.iter().map(|b| format!("{}（{}）", b.name, b.why)).collect::<Vec<_>>().join("、")
    };
    match w.phase {
        Phase::Waiting => {
            let when = if w.user_escalated {
                "使用者已按「現在換版」，working 不再擋".to_string()
            } else if w.auto_escalated {
                "已自動放寬，working 不再擋".to_string()
            } else if w.dismissed {
                "使用者已按「先等」，通知收起".to_string()
            } else {
                w.escalates_at.as_deref().map(|t| format!("預計 {t} 自動放寬")).unwrap_or_else(|| "已放寬".into())
            };
            format!("部署 {short} 等換版窗口 {mins} 分鐘了，被 {who} 擋住；{when}。請使用者調度：網頁 header「現在換版」或「先等」。")
        }
        Phase::Swapping => {
            let relaxed = if w.auto_escalated { "（已自動放寬）" } else { "" };
            format!("部署 {short}{relaxed} 拿到換版窗口，換版中（等了 {mins} 分鐘）。")
        }
        Phase::Done => format!("部署 {short} 換版完成（等了 {mins} 分鐘）。"),
        Phase::Abandoned => format!("部署 {short} 不等了（等了 {mins} 分鐘，沒拿到窗口或換版後回滾），之後的部署會重新計時。"),
    }
}

fn view_of(w: &Wait, now: &str) -> Value {
    json!({
        "id": w.id,
        "commit": w.commit,
        "since": w.since,
        "waited_secs": secs_between(&w.since, now),
        "blockers": w.blockers,
        "escalates_at": w.escalates_at,
        "user_escalated": w.user_escalated,
        "auto_escalated": w.auto_escalated,
        "dismissed": w.dismissed,
        "phase": w.phase,
        "rev": w.rev,
        "notified_at": w.notified_at,
        "ended_at": w.ended_at,
        "summary": summary(w, now),
    })
}

/// `GET /api/state` 的 `deploy_wait`：已經通知過的那一次部署（結束的再留 10 分鐘），其他時候 null。
pub fn view(app: &impl crate::deploy_wait::DeployWaitState) -> Value {
    let g = app.deploy_wait().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = crate::db::now();
    match g.as_ref().filter(|w| w.notified_at.is_some()) {
        Some(w) => view_of(w, &now),
        None => Value::Null,
    }
}

async fn announce(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), a: Announce, w: &Wait) {
    let now = crate::db::now();
    let v = view_of(w, &now);
    let first = a == Announce::First;
    let wake = first; // 只記錄類的 deploy_waiting 不設 wake（issue #856）
    tracing::info!(id = %w.id, rev = w.rev, phase = ?w.phase, first, wake, "{}", summary(w, &now));
    app.emit("deploy_wait", json!({"wait": v, "first": first})).await;
    let key = format!("deploy_waiting:{}:{}", w.id, w.rev);
    if let Err(e) = crate::supervisor::store::push_inbox(
        app.db(),
        &key,
        "deploy_waiting",
        None,
        None,
        None,
        &json!({"deploy_wait": v, "text": summary(w, &now), "wake": wake}),
    )
    .await
    {
        tracing::warn!(error = %e, "could not tell AGM about the deploy wait");
    }
}

/// 這張核准要不要因為使用者按了「現在換版」而放寬：只認這次部署（同一個 owner、還在等或已拿到窗口在換）的**自動**核准
/// （daemon-swap 自己核的）。AGM 親手核的、別的 owner 的、下一次部署的一律不算。
///
/// 拿到窗口（`Swapping`）之後還要算數（issue #840）：daemon-swap 換 binary 前會用同一張核准再問一次 safety（§3a），
/// 以前只認 `Waiting`，複查時放寬已經不見，working 又把 acquire 剛給的窗口推翻，只要隨時有人 working 就永遠換不上。
/// 這次部署結束（`Done`／`Abandoned`）放寬才跟著結束。
pub fn user_escalated_for(app: &impl crate::deploy_wait::DeployWaitState, a: &crate::supervisor::store::Approval) -> bool {
    let g = app.deploy_wait().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let auto = format!("service({})", crate::service_auth::DAEMON_SWAP);
    g.as_ref().is_some_and(|w| {
        w.user_escalated
            && matches!(w.phase, Phase::Waiting | Phase::Swapping)
            && w.owner == a.requester
            && a.decided_by.as_deref() == Some(auto.as_str())
    })
}

#[derive(Deserialize)]
pub struct ActIn {
    id: String,
}

fn user_only(p: &crate::api::RequestPrincipal) -> Result<(), LcError> {
    if *p != crate::api::RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    Ok(())
}

/// 「現在換版」／「先等」共用：找到還在等的這一次部署，改完存檔、廣播。
async fn act(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::deploy_wait::DeployWaitState), id: &str, f: impl FnOnce(&mut Wait)) -> Result<Value, LcError> {
    let (w, announced) = {
        let mut g = app.deploy_wait().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(w) = g.as_mut().filter(|w| w.id == id) else {
            return Err(LcError::NotFound("deploy_wait".into()));
        };
        if w.phase != Phase::Waiting {
            return Err(LcError::conflict("deploy_not_waiting", json!({"phase": w.phase})));
        }
        f(w);
        let announced = if w.notified_at.is_some() {
            w.rev += 1;
            true
        } else {
            false
        };
        let w = w.clone();
        save(app, Some(&w));
        (w, announced)
    };
    if announced {
        announce(app, Announce::UserAction, &w).await;
    } else {
        let v = view_of(&w, &crate::db::now());
        app.emit("deploy_wait", json!({"wait": v, "first": false})).await;
    }
    Ok(view_of(&w, &crate::db::now()))
}

/// 「現在換版」：這次部署直接放寬。核心（測試直接打這支，不碰排程器）。
pub async fn escalate(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::deploy_wait::DeployWaitState), id: &str) -> Result<Value, LcError> {
    act(app, id, |w| {
        w.user_escalated = true;
        w.escalates_at = None;
    })
    .await
}

/// `POST /api/deploy/wait/escalate {id}`：放寬之後順手叫排程器跑一輪——kick 在兩輪之間時不必等到下一個 5 分鐘；
/// 正在跑的那一輪 15 秒內就會帶同一張核准再試到。
pub async fn post_escalate(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    axum::Extension(p): axum::Extension<crate::api::RequestPrincipal>,
    axum::Json(b): axum::Json<ActIn>,
) -> Result<axum::Json<Value>, LcError> {
    user_only(&p)?;
    let v = escalate(&app, &b.id).await?;
    use crate::deploy_now::KickLauncher as _;
    if let Err(e) = crate::deploy_now::SchedulerKick::for_this_host().kick() {
        tracing::warn!(error = %e, "deploy wait escalated, but kicking the update job failed; the next scheduled round picks it up");
    }
    Ok(axum::Json(v))
}

/// `POST /api/deploy/wait/dismiss {id}`：「先等」，header 收起來。部署照樣在等，到門檻照樣自動放寬。
pub async fn post_dismiss(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    axum::Extension(p): axum::Extension<crate::api::RequestPrincipal>,
    axum::Json(b): axum::Json<ActIn>,
) -> Result<axum::Json<Value>, LcError> {
    user_only(&p)?;
    Ok(axum::Json(act(&app, &b.id, |w| w.dismissed = true).await?))
}

/// 開機後每 15 秒跑一次 [`tick`]。
pub fn spawn_ticker(app: &Arc<App>) {
    let app = app.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            every.tick().await;
            tick(&app).await;
        }
    });
}

#[cfg(test)]
mod tests;

/// 換版等待窗口的記憶體狀態。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait DeployWaitState: Send + Sync {
    fn deploy_wait(&self) -> &std::sync::Mutex<Option<crate::deploy_wait::Wait>>;
}
