//! System incidents: what is wrong *outside* the manager itself.
//!
//! `health::snapshot` answers "is AGM up". That was never the same question as "is the system
//! healthy", and the 2026-09-12 review found the gap: a remote host could drop, a bot could sit
//! dead for hours and an assignment could stall, and the summary still read `healthy` because
//! none of it touched the manager's own row.
//!
//! So faults are tracked per *resource*, not per tick. A condition has to persist past a
//! configured threshold before it becomes an incident ([`Detector`] holds the first sighting);
//! once open, the incident is a durable row that a restart deduplicates against; when the
//! condition clears, exactly one resolution event goes out. That shape is what keeps this
//! honest in both directions — a fault cannot be lost in a debounce, and a flapping counter
//! cannot turn into a notification storm.
//!
//! What is deliberately *not* an incident: a bot the user stopped, a pane blocked waiting for
//! an answer, a queue that is merely busy, and the manager's own idle/busy churn. An unknown
//! reading is reported as `unknown`, never folded into `healthy`.

use crate::state::App;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use super::store;

/// Severity ordering, worst last. `unknown` sits above healthy on purpose: not knowing is not
/// the same as being fine.
pub const SEVERITIES: [&str; 4] = ["healthy", "unknown", "degraded", "critical"];

pub fn worst(a: &str, b: &str) -> String {
    let rank = |s: &str| SEVERITIES.iter().position(|x| *x == s).unwrap_or(0);
    if rank(a) >= rank(b) { a.to_string() } else { b.to_string() }
}

/// 協調者的事件送了這麼多次還在 pending 就開 incident（issue #420）。跟巡檢的 `notify_max_attempts`、
/// 補送上限 `RECOVER_MAX_DELIVERIES` 同一個數字：試了五次都送不進去，第六次也不會。
pub const RESPONDER_UNDELIVERED_ATTEMPTS: i64 = 5;

/// One thing that is currently wrong, as the probes see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub kind: String,
    pub resource: String,
    pub severity: String,
    pub detail: String,
}

impl Observation {
    fn key(&self) -> (String, String) {
        (self.kind.clone(), self.resource.clone())
    }
}

/// Thresholds, in seconds, plus the notify budget. Read from `[supervisor]` in config.toml.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub host_disconnected_secs: i64,
    pub bot_stopped_secs: i64,
    pub assignment_stalled_secs: i64,
    pub notify_max_attempts: i64,
    /// issue #421：核准開著多久沒人裁示才算卡住。不從 config 讀（使用者定的是固定 30 分鐘），
    /// 放在這裡只是為了測試能換一個小值。
    pub approval_stalled_secs: i64,
    /// issue #472：`roles::classify` 連續失敗幾拍才開 incident。同上，放這裡是為了測試能換小值。
    pub classify_failures: u32,
    pub spool_fold_stuck_rounds: u32,
}

impl Thresholds {
    pub fn from_cfg(cfg: &crate::config::SupervisorCfg) -> Self {
        Self {
            classify_failures: CLASSIFY_FAILURE_LIMIT,
            spool_fold_stuck_rounds: SPOOL_FOLD_STUCK_LIMIT,
            host_disconnected_secs: cfg.host_disconnected_secs as i64,
            bot_stopped_secs: cfg.bot_stopped_secs as i64,
            assignment_stalled_secs: cfg.assignment_stalled_secs as i64,
            // `.max(1)` 跟 `controller::notify` 同一句（issue #504 附帶）：那裡夾了、這裡沒夾，
            // `notify_max_attempts = 0` 就會讓 `notify_attempts >= 0` 對**每一則** pending 巡檢事件
            // 成立，一則事件一張 critical incident，而通知本身其實還在正常送（送出的那一路用的是 1）。
            notify_max_attempts: cfg.notify_max_attempts.max(1),
            approval_stalled_secs: super::failover::STALLED_AFTER_SECS,
        }
    }
}

/// Holds how long each condition has been true, so a threshold can be applied before anything
/// durable is written.
///
/// In memory on purpose: after a daemon restart a condition has to be observed for its
/// threshold again before it opens an incident. That errs towards quiet, and the incidents that
/// were already open are still in the database — a restart cannot lose one, only delay a new
/// one by at most the threshold.
#[derive(Debug, Default)]
pub struct Detector {
    since: HashMap<(String, String), i64>,
}

/// What one pass decided to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Conditions that have now been true for long enough to be written down.
    pub open: Vec<Observation>,
    /// Incidents whose condition is no longer observed.
    pub resolve: Vec<(String, String)>,
}

impl Detector {
    /// `now` is a unix timestamp; `open` is what is currently in the database.
    /// `blind` names probe kinds that could not run this pass. Their incidents are left alone:
    /// an empty result from a query that errored is not evidence that the fault cleared.
    pub fn plan(
        &mut self,
        observations: &[Observation],
        open: &[(String, String)],
        blind: &[&str],
        thresholds: &Thresholds,
        now: i64,
    ) -> Plan {
        let mut plan = Plan::default();
        let seen: Vec<(String, String)> = observations.iter().map(Observation::key).collect();
        for obs in observations {
            let key = obs.key();
            let first = *self.since.entry(key.clone()).or_insert(now);
            let held = now - first;
            let needed = match obs.kind.as_str() {
                "host_disconnected" => thresholds.host_disconnected_secs,
                "bot_stopped" => thresholds.bot_stopped_secs,
                ROLE_UNAVAILABLE_KIND => ROLE_UNAVAILABLE_HOLD_SECS,
                // The stalled, undelivered and exhausted probes carry their own age test; a
                // second wait here would just double the threshold.
                _ => 0,
            };
            if held >= needed || open.contains(&key) {
                plan.open.push(obs.clone());
            }
        }
        // Anything open that nobody observed this pass has cleared — unless the probe that
        // would have seen it never ran, in which case we know nothing and say nothing.
        for key in open {
            if !seen.contains(key) && !blind.contains(&key.0.as_str()) {
                plan.resolve.push(key.clone());
            }
        }
        // A blind probe's timer is left alone too, so a fault that was already accumulating
        // does not have to start its threshold over because of one failed query.
        self.since.retain(|k, _| seen.contains(k) || blind.contains(&k.0.as_str()));
        plan
    }
}

/// What one sweep could see.
///
/// `failed` names the probes whose query errored. It matters because "the query said nothing is
/// wrong" and "the query did not answer" produce the same empty list, and treating the second
/// as the first makes the sweep *resolve* open incidents — announcing a recovery that nobody
/// observed. A probe that could not run keeps its incidents exactly where they are.
#[derive(Debug, Default)]
pub struct Probed {
    pub seen: Vec<Observation>,
    pub failed: Vec<&'static str>,
}

impl Probed {
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// 上一輪 sweep 跑不起來的探針，依資料目錄分開（同一個行程裡的測試各有自己的 App）。
///
/// 探針的 SQL 一直失敗時（例如 schema 漂移），那一類 incident 從此不開也不解；以前只記 warn，
/// `system_health` 照樣回 healthy，UI 是綠燈——跟「量不到回 unknown」的規則相反（review 2026-09-16 c1 L5）。
/// 只放記憶體：它描述的是「這個行程最近一次看得到什麼」，重啟後第一輪 sweep 就會重寫。
fn blind_probes() -> &'static std::sync::Mutex<HashMap<std::path::PathBuf, Vec<&'static str>>> {
    static BLIND: std::sync::OnceLock<std::sync::Mutex<HashMap<std::path::PathBuf, Vec<&'static str>>>> = std::sync::OnceLock::new();
    BLIND.get_or_init(Default::default)
}

pub fn note_blind(app: &Arc<App>, failed: &[&'static str]) {
    let mut failed = failed.to_vec();
    failed.sort_unstable();
    failed.dedup();
    if let Ok(mut m) = blind_probes().lock() {
        m.insert(app.data_dir.clone(), failed);
    }
}

fn blind_now(app: &Arc<App>) -> Vec<&'static str> {
    blind_probes().lock().ok().and_then(|m| m.get(&app.data_dir).cloned()).unwrap_or_default()
}

/// Read every cheap probe. No LLM, no process is killed to find out how it is doing: this runs
/// on the 30-second health tick and may not cost more than a few queries.
pub async fn observe(app: &Arc<App>, thresholds: &Thresholds) -> Probed {
    let mut probed = Probed::default();
    let out = &mut probed.seen;

    // A host the daemon cannot reach takes every bot on it with it, and nothing else notices.
    for host in app.hosts.list().await {
        if !host.is_connected() {
            out.push(Observation {
                kind: "host_disconnected".into(),
                resource: host.name.clone(),
                severity: "degraded".into(),
                detail: json!({"host": host.name}).to_string(),
            });
        }
    }

    // "Expected running" is the user's own `autostart`, not a guess: a bot somebody stopped on
    // purpose is not a fault, and treating it as one is how a health page becomes noise.
    match crate::db::live_bots(&app.db).await {
        Ok(bots) => {
            for bot in bots {
                if bot.autostart == 0 {
                    continue;
                }
                match crate::db::active_run(&app.db, &bot.id).await {
                    Ok(None) => {
                        // autostart is a launch preference, not a perpetual desired-state flag.
                        // stop_bot records `stopped` and leaves autostart unchanged, so an
                        // intentional stop must not become an outage after the debounce.
                        //
                        // 第二鍵用 `rowid`（寫入順序），不是 `id`（issue #100，同 fence.rs／a4605b2 的根因）：
                        // 一顆 bot 快速重啟時兩個 run 可能擠進同一毫秒，ULID 的亂數段不保證遞增，字典序
                        // 挑到的若不是真的最後一個 run，使用者主動停的（`stopped`）會被誤判成 outage。
                        let last = sqlx::query_scalar::<_, String>(
                            "SELECT state FROM runs WHERE bot_id=? ORDER BY started_at DESC, rowid DESC LIMIT 1",
                        ).bind(&bot.id).fetch_optional(&app.db).await;
                        match last {
                            Ok(Some(state)) if state == "stopped" => {}
                            Ok(_) => out.push(Observation {
                                kind: "bot_stopped".into(),
                                resource: bot.id.clone(),
                                severity: "degraded".into(),
                                detail: json!({"bot_id": bot.id, "name": bot.name, "expected": "autostart"}).to_string(),
                            }),
                            Err(_) => probed.failed.push("bot_stopped"),
                        }
                    },
                    Ok(Some(_)) => {}
                    // Could not tell whether this bot is running. Not knowing is not "it is fine".
                    Err(e) => {
                        tracing::warn!(bot = %bot.id, error = ?e, "bot_stopped probe failed");
                        probed.failed.push("bot_stopped");
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = ?e, "bot_stopped probe failed");
            probed.failed.push("bot_stopped");
        }
    }

    // Work that has not moved in hours. `updated_at` moves on every retry and every delivery,
    // so this only fires on something genuinely stuck — including an `awaiting_review` row
    // nobody has accepted, which is the case the review found in production.
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(thresholds.assignment_stalled_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    match store::assignments_idle_since(&app.db, &cutoff).await {
        Err(e) => {
            tracing::warn!(error = ?e, "assignment_stalled probe failed");
            probed.failed.push("assignment_stalled");
        }
        Ok(stalled) => {
        for a in stalled {
            out.push(Observation {
                kind: "assignment_stalled".into(),
                resource: a.id.clone(),
                severity: "degraded".into(),
                detail: json!({
                    "assignment_id": a.id,
                    "bot_id": a.target_bot_id,
                    "status": a.status,
                    "updated_at": a.updated_at,
                })
                .to_string(),
            });
        }
        }
    }

    // Work that has never been handed over at all — the stopped-bot case. The idle probe above
    // is blind to it: every retry moves `updated_at`, so an assignment bouncing off a stopped
    // bot every five minutes looks busy forever. The detail carries the retry count and the
    // last refusal, which is what makes it actionable instead of just red.
    match store::assignments_undelivered_since(&app.db, &cutoff).await {
        Err(e) => {
            tracing::warn!(error = ?e, "assignment_undelivered probe failed");
            probed.failed.push("assignment_undelivered");
        }
        Ok(undelivered) => {
        for a in undelivered {
            out.push(Observation {
                kind: "assignment_undelivered".into(),
                resource: a.id.clone(),
                severity: "degraded".into(),
                detail: json!({
                    "assignment_id": a.id,
                    "bot_id": a.target_bot_id,
                    "attempts": a.attempts,
                    "last_error": a.error,
                    "next_attempt_at": a.next_attempt_at,
                    "created_at": a.created_at,
                })
                .to_string(),
            });
        }
        }
    }

    // A notification nobody could deliver after every retry. Critical: this is the path the
    // manager learns anything through, and a silent one is worse than a loud failure.
    //
    // 一個巡檢一筆，不是一則事件一筆（issue #504 審核）：巡檢停在登入失效時整個佇列會一起用盡額度，
    // 逐則開等於把一則送不出去的通知變成 N 張 critical incident，而每一張又各推一則 inbox 事件給協調者。
    // 隔壁 `responder_undeliverable` 為了同一個理由早就是聚合的；兩邊現在形狀一致（`events` 是真正的總數）。
    match store::exhausted_inbox(&app.db, thresholds.notify_max_attempts).await {
        Err(e) => {
            tracing::warn!(error = ?e, "notify_exhausted probe failed");
            probed.failed.push("notify_exhausted");
        }
        Ok(stuck) if stuck.total > 0 => {
            // 樣本一定非空（`total > 0` 而上限是 20）；`max_by_key` 仍照 Option 處理，不 expect。
            let worst = stuck.events.iter().max_by_key(|e| e.notify_attempts);
            let oldest = stuck.events.first();
            out.push(Observation {
                kind: "notify_exhausted".into(),
                resource: super::roles::Role::Patrol.as_str().to_string(),
                severity: "critical".into(),
                detail: json!({
                    "events": stuck.total,
                    "sampled": stuck.events.len(),
                    "oldest_event_id": oldest.map(|e| e.id.clone()),
                    "oldest_event_key": oldest.map(|e| e.event_key.clone()),
                    "oldest_kind": oldest.map(|e| e.kind.clone()),
                    "oldest_created_at": oldest.map(|e| e.created_at.clone()),
                    "max_attempts": worst.map(|e| e.notify_attempts),
                    "error": worst.and_then(|e| e.notify_error.clone()),
                    "action": "`bin/agm inbox` 看這些事件在等什麼：巡檢收不到通知（常見是它的 CLI 停在 /login），處理完 ack",
                })
                .to_string(),
            });
        }
        Ok(_) => {}
    }

    // 協調者（issue #420）：它是 bot 申請、核准、群組任務唯一的收件者，倒了就是整條協調線靜默停擺。
    // 兩件事分開開：角色 bot 自己不能用（下面那個迴圈），以及——不管原因——
    // 它的事件已經送了 RESPONDER_UNDELIVERED_ATTEMPTS 次還在 pending。incident_opened 走巡檢（`roles::route`）。
    // #427 第 2 項：**巡檢也要看**，而且每一拍都看。以前只有協調者、而且只在 `notify` 送不出去時才看，
    // 所以巡檢停在登入失效沒有人知道（它是 incident 與 ops_alert 的收件人），協調者佇列空著時也一樣。
    // 判定在 `role_faults::refresh`（同一拍、`sweep` 之前跑），這裡只把結論變成 incident。
    // `resource` 從 bot_id 換成角色名：兩顆各自一筆，而且換 bot 不會留下關不掉的孤兒。
    for role in super::role_faults::WATCHED {
        let row = match super::roles::get(&app.db, role).await {
            Err(e) => {
                tracing::warn!(role = role.as_str(), error = ?e, "role unavailable probe failed");
                probed.failed.push(ROLE_UNAVAILABLE_KIND);
                continue;
            }
            Ok(row) => row,
        };
        // 沒建立的角色整個不留紀錄（連 blind 都不算）：算了會讓這個 kind 每一拍都是「探針沒跑」，
        // 另一顆角色已經開著的 incident 就再也關不掉。
        if row.bot_id.is_none() {
            continue;
        }
        let fault = super::role_faults::snapshot(app, role).await.unwrap_or_default();
        // 兩個獨立訊號：DB 那一欄是 `responder::notify` 送不出去時看畫面寫的（#420），
        // 記憶體那份是每拍看畫面＋notify 連續沒完成（#427）。哪一個先看到都算。
        let reason = if row.status == "needs_login" {
            Some(super::role_faults::REASON_NEEDS_LOGIN)
        } else {
            fault.reason
        };
        let Some(reason) = reason else {
            // 這一拍連畫面都讀不到＝不知道。不開也不解（跟上面每個探針同一個原則）。
            if fault.probe_failed {
                probed.failed.push(ROLE_UNAVAILABLE_KIND);
            }
            continue;
        };
        let stuck_at_login = reason == super::role_faults::REASON_NEEDS_LOGIN;
        // 停在登入要人動手，而且期間所有核准都沒有人裁示——比 degraded 嚴重。
        let severity = if stuck_at_login { "critical" } else { "degraded" };
        let action = if stuck_at_login {
            "在那顆 bot 的 pane 跑 /login（Keychain 鎖著時先在另一個終端 security unlock-keychain）；恢復後排著的核准會在下一輪 notify 被裁示"
        } else {
            "看那顆 bot 的 pane：notify 回合一直沒完成，核准與 bot 申請都沒有人在裁示"
        };
        out.push(Observation {
            kind: ROLE_UNAVAILABLE_KIND.into(),
            resource: role.as_str().to_string(),
            severity: severity.into(),
            detail: json!({
                "role": role.as_str(),
                "reason": reason,
                "bot_id": row.bot_id,
                "since": fault.since.clone().or_else(|| row.waiting_since.clone()),
                "notify_failures": fault.failed_turns.len(),
                "detail": row.status_detail,
                "action": action,
            })
            .to_string(),
        });
    }
    match store::responder_undelivered(&app.db, RESPONDER_UNDELIVERED_ATTEMPTS).await {
        Err(e) => {
            tracing::warn!(error = ?e, "responder_undeliverable probe failed");
            probed.failed.push("responder_undeliverable");
        }
        Ok(stuck) if !stuck.is_empty() => {
            // 一個協調者一筆 incident，不是一則事件一筆：它倒下時佇列裡常有十幾則，逐則開就是通知風暴。
            let worst = stuck.iter().max_by_key(|e| e.notify_attempts).expect("non-empty");
            out.push(Observation {
                kind: "responder_undeliverable".into(),
                resource: "responder".into(),
                severity: "critical".into(),
                detail: json!({
                    "events": stuck.len(),
                    "oldest_event_id": stuck[0].id,
                    "oldest_created_at": stuck[0].created_at,
                    "max_attempts": worst.notify_attempts,
                    "last_error": worst.notify_error,
                })
                .to_string(),
            });
        }
        Ok(_) => {}
    }

    // The phone entry point, but only when there is evidence it is *broken*. `unknown` with an
    // unsupported capability is a documented limit, not a fault: opening an incident for it
    // would mean a permanent red light nobody can clear.
    let remote = super::remote::status(app).await;
    let remote_status = remote.get("status").and_then(Value::as_str).unwrap_or("unknown");
    if super::remote::severity(remote_status) == "degraded" {
        out.push(Observation {
            kind: "remote_entry".into(),
            resource: super::setup::REMOTE_NAME.to_string(),
            severity: "degraded".into(),
            detail: remote.to_string(),
        });
    }

    // issue #421：核准開著超過 30 分鐘還沒有**任何人**裁示。改派（`failover`）只換得動角色；
    // 兩個角色都沒登入時換到誰手上都一樣，這時要喊的是人，而不是只留一行 log——2026-09-23 那天
    // 部署停了 9 小時，全程沒有任何東西告訴使用者「有一筆核准在等你」。
    // critical：這是一道閘門，卡住的是別人的部署，不是 AGM 自己的工作。
    match super::failover::stalled_approvals(app, thresholds.approval_stalled_secs).await {
        Ok(stalled) => {
            for (id, waiting_secs, requester) in stalled {
                out.push(Observation {
                    kind: super::failover::STALLED_KIND.into(),
                    resource: id.clone(),
                    severity: "critical".into(),
                    detail: json!({"approval_id": id, "waiting_secs": waiting_secs, "requester": requester,
                                   "action": "agm approval list 看它等什麼，再 agm approval decide"})
                    .to_string(),
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not probe for stalled approvals");
            probed.failed.push(super::failover::STALLED_KIND);
        }
    }

    // #472：分類器自己壞掉。這不是某個資源故障，是 daemon 的一段路不通——
    // 而它不通的後果是「新進 inbox 事件分不到角色，對兩個通知者同時隱形」，
    // 沒有任何其他探針會發現（那些事件根本沒被選中，所以連 `notify_exhausted` 都不會觸發）。
    // 計數在 `App`（記憶體、重啟重算，同 SPEC §18.9 的原則），tick 每拍更新。
    {
        let failures = app.classify_failures.load(std::sync::atomic::Ordering::Relaxed);
        if failures >= thresholds.classify_failures {
            out.push(Observation {
                kind: CLASSIFY_FAILING_KIND.into(),
                resource: "inbox".into(),
                // critical：期間所有新事件都送不出去，而使用者入口看起來是好的。
                severity: "critical".into(),
                detail: json!({
                    "consecutive_failures": failures,
                    "action": "看 daemon.log 裡「roles::classify 失敗」那幾行的 error；分類不通時 bot 申請、核准與 mission 事件都不會被送出",
                })
                .to_string(),
            });
        }
    }

    // #500 複看：遠端 spool 停收。這一條跟分類器那條的理由一樣——**沒有別的探針會發現**：
    // 事件還在遠端的 `.claim` 裡，本機的 `hook_events` 從頭到尾沒看過它們，
    // 而「hook 不再進來」跟「這顆 bot 很閒」在 daemon 這邊長得一模一樣。
    {
        let stuck = app.spool_fold_stuck.lock().await;
        for (resource, (rounds, bytes)) in stuck.iter() {
            if *rounds < thresholds.spool_fold_stuck_rounds {
                continue;
            }
            out.push(Observation {
                kind: SPOOL_FOLD_STUCK_KIND.into(),
                resource: resource.clone(),
                // 資料沒丟（`.claim` 還在，空間一回來就整份補上），但期間這顆 bot 的回合全部看不到。
                severity: "critical".into(),
                detail: json!({
                    "rounds": rounds,
                    "claim_bytes": bytes,
                    "action": "那台機器上 `df -h` 與 `ls -l <bot 目錄>`：.replaying 寫不進去（磁碟滿、quota、權限或被改成目錄）；修好之後下一輪 drain 會自己整份補上，不必手動搬檔",
                })
                .to_string(),
            });
        }
    }

    // #534：遠端 shim 補版放棄了。這一條跟上面兩條的理由一樣——**沒有別的探針會發現**：
    // 那台上的 bot 都還在跑、hook 照進來，只是它們手上的 cargo／herdr 是舊版，
    // 表現出來的是「工作沒被轉到外部編譯主機」「cargo 卡住」這種看起來像慢、不像壞的症狀。
    {
        let stale = app.remote_shim_stale.lock().await;
        for (host, why) in stale.iter() {
            out.push(Observation {
                kind: REMOTE_SHIM_STALE_KIND.into(),
                resource: host.clone(),
                // 不是資料問題、也沒有人被擋住，但這台的每一次建置都可能踩到舊 shim 的 bug。
                severity: "degraded".into(),
                detail: json!({
                    "why": why,
                    "action": "那台機器：`ssh <host> df -h` 與 `ls -l <bot 目錄>/bin`（磁碟滿、權限、被改成目錄都會讓 mv 失敗）；修好之後等它重連、或重啟 daemon 會再補一次，急的話重啟那顆 bot 的 pane 也會重寫",
                })
                .to_string(),
            });
        }
    }

    probed
}

/// 角色 bot 自己不能用（issue #420／#427）。`resource` 是角色名（`responder`／`patrol`），兩顆各自一筆。
///
/// 名字從 `responder_needs_login` 改成中性的（issue #459）：那個舊名現在會騙人——`resource="patrol"`
/// 時故障的是巡檢不是協調者，`reason="notify_stalled"` 時也根本不是登入問題，跟 `detail.reason` 自相矛盾。
/// 故障對象看 `resource`、原因看 `detail.reason`，kind 只說「有個角色 bot 不能用」。
/// 舊名的既有列在 `store::migrate` 改寫過來，免得留下觀測不到、因此永遠關不掉的孤兒。
/// #472：`roles::classify` 連續失敗這麼多拍就開 incident。tick 是 10 秒一拍，
/// 3 拍＝約 30 秒：撐得過一次暫時性的 DB 錯誤，又不會讓「新事件全部送不出去」躺很久。
pub const CLASSIFY_FAILURE_LIMIT: u32 = 3;

/// #472 的 incident 種類。resource 是固定字串：這是 daemon 自己那一支分類器，全機只有一個。
pub const CLASSIFY_FAILING_KIND: &str = "inbox_classify_failing";

/// #500 複看：遠端 spool 的 `.claim` 併不進 `.replaying` 就是「這顆 bot 的 hook 事件從現在起全部收不到」。
/// drain 是事件驅動的，一顆閒著的 bot 本來就沒有輪數，所以門檻壓在 3 輪——看到三次代表它真的有事件
/// 要送、而且三次都沒收成，不是一次暫時的磁碟忙。
pub const SPOOL_FOLD_STUCK_LIMIT: u32 = 3;

/// 這件事的 incident 種類。`resource` 是 `<host>/<bot_id>`：一台機器上可以只有某顆 bot 卡住。
pub const SPOOL_FOLD_STUCK_KIND: &str = "remote_spool_stuck";

/// #534：遠端 shim 補版放棄了。`resource` 是 host 名——一台機器一筆，補版是整台一起做的。
pub const REMOTE_SHIM_STALE_KIND: &str = "remote_shim_stale";

pub const ROLE_UNAVAILABLE_KIND: &str = "role_unavailable";
/// 改名前的 kind（issue #459 的遷移用）。
pub const ROLE_UNAVAILABLE_KIND_LEGACY: &str = "responder_needs_login";

/// 這個條件要連續看到這麼久才開 incident（health tick 是 30 秒，等於要連兩拍都看到）。
/// #427 把偵測從「送不出去才看」改成「每一拍都看」，一拍的閃動不該驚動使用者；真的卡住時它會一直都在。
pub const ROLE_UNAVAILABLE_HOLD_SECS: i64 = 60;

/// Incidents whose whole point is that the inbox is not working. Queueing an inbox event for
/// them *to the same role* is how a stuck notification becomes two stuck notifications: the event
/// cannot be delivered either, so it exhausts its own retries, which opens another incident, and so on.
///
/// `notify_exhausted` only ever describes the patrol's events (the responder's have no attempt cap,
/// `store::exhausted_inbox`). So when a responder exists, the event goes to *it* — the routing table
/// sends `incident_*` for this kind to the responder, whose queue never exhausts — and AGM finds out.
/// Without a responder there is no other channel: it stays on the UI and in `system_health` only
/// (review 2026-09-16 c1 L4).
fn notifiable(kind: &str, resource: &str, responder_configured: bool) -> bool {
    match kind {
        "notify_exhausted" => responder_configured,
        // #459：壞掉的是巡檢自己時，唯一收得到的是協調者（`roles::route` 會把它送過去）。
        // 沒有協調者就沒有第二條路：留在 UI 與 `system_health` 上，不要推進一個已知收不到的佇列。
        ROLE_UNAVAILABLE_KIND => resource != super::roles::Role::Patrol.as_str() || responder_configured,
        // #472：壞掉的**就是分類本身**。推一則 inbox 事件進去，那一則同樣會停在
        // `role IS NULL`、同樣沒有人收得到——等於用壞掉的那條路去通報那條路壞了
        // （跟 `notify_exhausted` 不推給巡檢是同一個理由）。留在 UI 與 `system_health` 上。
        CLASSIFY_FAILING_KIND => false,
        _ => true,
    }
}

/// Apply one pass: write what changed, and queue one inbox event per transition.
pub async fn sweep(app: &Arc<App>, detector: &mut Detector) {
    let cfg = app.cfg.get().await;
    let thresholds = Thresholds::from_cfg(&cfg.supervisor);
    let probed = observe(app, &thresholds).await;
    // Cannot read what is already open: do nothing at all rather than guess in either direction.
    let Ok(open) = store::open_incidents(&app.db).await else {
        tracing::warn!("incident sweep skipped: could not read the open incidents");
        return;
    };
    let open_keys: Vec<(String, String)> = open.iter().map(|i| (i.kind.clone(), i.resource.clone())).collect();
    if !probed.ok() {
        tracing::warn!(blind = ?probed.failed, "some incident probes could not run; their incidents are left as they are");
    }
    note_blind(app, &probed.failed);
    // 讀不到＝不知道：當成「沒有協調者」會讓 notify_exhausted 的事件被吞掉（incident 已開、通知不再補），
    // 所以整輪不做，跟上面讀不到 open incidents 同一個原則（#250）。
    let responder_configured = match super::roles::responder_configured(&app.db).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, "incident sweep skipped: could not read whether the responder is configured");
            return;
        }
    };
    let plan = detector.plan(
        &probed.seen,
        &open_keys,
        &probed.failed,
        &thresholds,
        chrono::Utc::now().timestamp(),
    );

    for obs in plan.open {
        let detail: Value = serde_json::from_str(&obs.detail).unwrap_or_else(|_| json!({}));
        // incident 與通知同一個交易（#319）：寫不進去就不開，下一輪 detector 重來。
        let Ok((incident, opened)) =
            store::open_incident_notifying(&app.db, &obs.kind, &obs.resource, &obs.severity, &detail, notifiable(&obs.kind, &obs.resource, responder_configured)).await
        else {
            continue;
        };
        if !opened {
            continue;
        }
        tracing::warn!(kind = %obs.kind, resource = %obs.resource, severity = %obs.severity, "system incident opened");
        app.emit("supervisor_changed", json!({"incident": incident.to_json()})).await;
    }

    for (kind, resource) in plan.resolve {
        let Ok(Some(incident)) = store::resolve_incident_notifying(&app.db, &kind, &resource, notifiable(&kind, &resource, responder_configured)).await else { continue };
        tracing::info!(kind = %kind, resource = %resource, "system incident resolved");
        app.emit("supervisor_changed", json!({"incident": incident.to_json()})).await;
    }
}

/// The system half of the health summary: severity, and the incidents behind it.
pub async fn system_health(app: &Arc<App>) -> Value {
    // `unwrap_or_default()` here used to turn a failed query into an empty list, and an empty
    // list into `healthy` — the summary claimed the system was fine on the strength of a read
    // that never happened.
    let open = match store::open_incidents(&app.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = ?e, "could not read open incidents");
            return json!({
                "status": "unknown",
                "error": "could not read the incident table",
                "open_incidents": null,
                "incidents": [],
            });
        }
    };
    let severity = open.iter().fold("healthy".to_string(), |acc, i| worst(&acc, &i.severity));
    // 有探針上一輪沒跑起來：那一類故障看不到，不能說 healthy（真的故障照舊比 unknown 嚴重）。
    let blind = blind_now(app);
    let severity = if blind.is_empty() { severity } else { worst(&severity, "unknown") };
    json!({
        "status": severity,
        "open_incidents": open.len(),
        "incidents": open.iter().map(store::Incident::to_json).collect::<Vec<_>>(),
        "blind_probes": blind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(kind: &str, resource: &str) -> Observation {
        Observation {
            kind: kind.into(),
            resource: resource.into(),
            severity: "degraded".into(),
            detail: "{}".into(),
        }
    }

    /// #427：角色 bot 不可用要連續看到一分鐘（兩拍）才開 incident。偵測從「送不出去才看畫面」
    /// 改成「每一拍都看」之後，一拍的閃動不該驚動使用者；真的卡住時它會一直都在。恢復了自動關。
    #[test]
    fn a_role_that_is_unavailable_for_two_ticks_opens_and_clears_by_itself() {
        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs(ROLE_UNAVAILABLE_KIND, "responder")];
        assert!(d.plan(&seen, &[], &[], &t, 1000).open.is_empty(), "第一拍只是看到，還不開");
        assert!(d.plan(&seen, &[], &[], &t, 1000 + ROLE_UNAVAILABLE_HOLD_SECS - 1).open.is_empty(), "差一秒也還不開");
        assert_eq!(d.plan(&seen, &[], &[], &t, 1000 + ROLE_UNAVAILABLE_HOLD_SECS).open.len(), 1, "過了門檻才開");
        // 恢復：這一拍沒看到就關掉，不用等任何人來按。
        let open = vec![(ROLE_UNAVAILABLE_KIND.to_string(), "responder".to_string())];
        assert_eq!(d.plan(&[], &open, &[], &t, 9000).resolve, open);
    }

    /// 讀不到畫面時**不能**把已經開著的那筆當成恢復——「探針沒跑」跟「它好了」長得一樣，
    /// 把後者當前者等於對使用者宣告一個沒人觀察到的恢復（同 host／bot 探針的既有原則）。
    /// 兩顆角色共用一個 kind，所以其中一顆瞎掉時另一顆的也一起留著：寧可晚關，不可誤關。
    #[test]
    fn a_role_probe_that_could_not_run_neither_opens_nor_resolves() {
        let mut d = Detector::default();
        let t = thresholds();
        let open = vec![
            (ROLE_UNAVAILABLE_KIND.to_string(), "responder".to_string()),
            (ROLE_UNAVAILABLE_KIND.to_string(), "patrol".to_string()),
        ];
        let plan = d.plan(&[], &open, &[ROLE_UNAVAILABLE_KIND], &t, 9000);
        assert!(plan.resolve.is_empty(), "探針沒跑：兩筆都留著");
        assert!(plan.open.is_empty());
    }

    /// #459：壞掉的是巡檢自己時，`incident_opened` 不能推回巡檢——那正是這個檔案自己對
    /// `notify_exhausted` 防過的反模式（「把一則送不出去的通知變成兩則」）。協調者在就送它；
    /// 協調者沒建立時**一則都不推**（沒有第二條路），incident 本身照開、留在 UI 與 `system_health`。
    #[test]
    fn a_broken_patrol_is_never_told_about_itself() {
        assert!(notifiable(ROLE_UNAVAILABLE_KIND, "responder", false), "協調者壞了：巡檢收得到，跟有沒有協調者無關");
        assert!(notifiable(ROLE_UNAVAILABLE_KIND, "patrol", true), "巡檢壞了、協調者在：送協調者");
        assert!(!notifiable(ROLE_UNAVAILABLE_KIND, "patrol", false), "巡檢壞了、又沒有協調者：不推進已知收不到的佇列");
        // 既有規則不受影響。
        assert!(!notifiable("notify_exhausted", "responder", false));
        assert!(notifiable("host_disconnected", "some-host", false));
    }

    /// 兩顆角色各自一筆：`resource` 是角色名，所以協調者卡住不會蓋掉巡檢那一筆（反之亦然）。
    #[test]
    fn each_role_gets_its_own_incident_row() {
        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs(ROLE_UNAVAILABLE_KIND, "responder"), obs(ROLE_UNAVAILABLE_KIND, "patrol")];
        let at = 1000 + ROLE_UNAVAILABLE_HOLD_SECS;
        d.plan(&seen, &[], &[], &t, 1000);
        assert_eq!(d.plan(&seen, &[], &[], &t, at).open.len(), 2);
        // 只剩協調者還在壞：巡檢那一筆關掉，協調者那一筆留著。
        let open: Vec<(String, String)> = seen.iter().map(Observation::key).collect();
        let plan = d.plan(&seen[..1], &open, &[], &t, at + 30);
        assert_eq!(plan.resolve, vec![(ROLE_UNAVAILABLE_KIND.to_string(), "patrol".to_string())]);
        assert_eq!(plan.open.len(), 1);
    }

    fn thresholds() -> Thresholds {
        Thresholds {
            host_disconnected_secs: 120,
            bot_stopped_secs: 300,
            assignment_stalled_secs: 7200,
            notify_max_attempts: 5,
            approval_stalled_secs: 1800,
            classify_failures: CLASSIFY_FAILURE_LIMIT,
            spool_fold_stuck_rounds: SPOOL_FOLD_STUCK_LIMIT,
        }
    }

    /// #472：`observe` 讀的是 `App` 上那個計數器，所以「連續幾拍」這件事要真的走一遍。
    /// 門檻以下不開票（一次抖動不驚動人），到門檻才開，成功歸零之後下一拍就該消失。
    #[tokio::test]
    async fn the_classifier_incident_follows_the_consecutive_counter() {
        use std::sync::atomic::Ordering;
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let t = thresholds();

        let has = |p: &Probed| p.seen.iter().any(|o| o.kind == CLASSIFY_FAILING_KIND);

        // 沒失敗過：不該有。
        assert!(!has(&observe(&app, &t).await));

        // 差一次：還不到門檻。
        app.classify_failures.store(CLASSIFY_FAILURE_LIMIT - 1, Ordering::Relaxed);
        assert!(!has(&observe(&app, &t).await), "門檻以下不開票");

        // 到門檻：開，而且 detail 要帶得出次數（查事故的人要看得到連續幾拍）。
        app.classify_failures.store(CLASSIFY_FAILURE_LIMIT, Ordering::Relaxed);
        let probed = observe(&app, &t).await;
        let o = probed.seen.iter().find(|o| o.kind == CLASSIFY_FAILING_KIND).expect("到門檻要開");
        assert_eq!(o.severity, "critical", "期間所有新事件都送不出去");
        assert!(o.detail.contains("consecutive_failures"), "{}", o.detail);

        // 成功一次歸零：下一拍就不該再看到。
        app.classify_failures.store(0, Ordering::Relaxed);
        assert!(!has(&observe(&app, &t).await), "恢復之後不該還在");
    }

    /// #500 複看：遠端 spool 停收。門檻以下不開票（drain 是事件驅動的，一次磁碟忙不該驚動人），
    /// 到門檻才開；`resource` 要分得出是哪一台的哪一顆；併回去之後（計數被移除）就該消失。
    #[tokio::test]
    async fn a_stuck_remote_spool_opens_an_incident_per_bot() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let t = thresholds();
        let of = |p: &Probed| -> Vec<String> {
            p.seen.iter().filter(|o| o.kind == SPOOL_FOLD_STUCK_KIND).map(|o| o.resource.clone()).collect()
        };

        assert!(of(&observe(&app, &t).await).is_empty(), "沒卡住就不該有");

        app.spool_fold_stuck.lock().await.insert("mac2/b1".into(), (SPOOL_FOLD_STUCK_LIMIT - 1, 42));
        assert!(of(&observe(&app, &t).await).is_empty(), "門檻以下不開票");

        app.spool_fold_stuck.lock().await.insert("mac2/b1".into(), (SPOOL_FOLD_STUCK_LIMIT, 4096));
        let probed = observe(&app, &t).await;
        let o = probed.seen.iter().find(|o| o.kind == SPOOL_FOLD_STUCK_KIND).expect("到門檻要開");
        assert_eq!(o.resource, "mac2/b1", "一台機器上可以只有某顆 bot 卡住");
        assert_eq!(o.severity, "critical");
        assert!(o.detail.contains("claim_bytes"), "查事故的人要知道卡著多少：{}", o.detail);

        // 同一台的另一顆也卡住：各開各的。
        app.spool_fold_stuck.lock().await.insert("mac2/b2".into(), (SPOOL_FOLD_STUCK_LIMIT, 7));
        let mut both = of(&observe(&app, &t).await);
        both.sort();
        assert_eq!(both, vec!["mac2/b1".to_string(), "mac2/b2".to_string()]);

        // 併回去了：計數被移除，下一拍就不該再看到。
        app.spool_fold_stuck.lock().await.clear();
        assert!(of(&observe(&app, &t).await).is_empty(), "恢復之後不該還在");

        // 這一類要推進 inbox：壞的是那台機器的磁碟，不是通知那條路自己（對照 #472）。
        assert!(notifiable(SPOOL_FOLD_STUCK_KIND, "mac2/b1", false));
    }

    /// #534：遠端補版放棄之後要開票，補成之後要消失。`resource` 是 host 名。
    #[tokio::test]
    async fn a_host_whose_shims_could_not_be_refreshed_opens_an_incident() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let t = thresholds();
        let of = |p: &Probed| -> Vec<String> {
            p.seen.iter().filter(|o| o.kind == REMOTE_SHIM_STALE_KIND).map(|o| o.resource.clone()).collect()
        };

        assert!(of(&observe(&app, &t).await).is_empty(), "沒放棄過就不該有");

        app.remote_shim_stale.lock().await.insert("mac2".into(), "重試 4 次都失敗，最後一個錯誤：connection refused".into());
        let probed = observe(&app, &t).await;
        let o = probed.seen.iter().find(|o| o.kind == REMOTE_SHIM_STALE_KIND).expect("放棄了就要開票");
        assert_eq!(o.resource, "mac2");
        assert_eq!(o.severity, "degraded");
        assert!(o.detail.contains("connection refused"), "查的人要看得到最後那個錯誤：{}", o.detail);

        // 下一次補成了：計數被移除，這一拍就不該再看到。
        app.remote_shim_stale.lock().await.clear();
        assert!(of(&observe(&app, &t).await).is_empty(), "補成之後不該還在");

        // 壞的是那台機器，不是通知那條路：這一類要推進 inbox（對照 #472）。
        assert!(notifiable(REMOTE_SHIM_STALE_KIND, "mac2", false));
    }

    /// #534：重試節奏是分鐘級的。20／60 秒只夠撐過「ssh 抖一下」，那台在重開機時三次全都趕不上，
    /// 而下一次機會要等它掉線再連上。
    #[test]
    fn the_remote_retry_schedule_is_minutes_not_seconds() {
        assert_eq!(crate::shim_refresh::REMOTE_RETRY_WAITS, [0, 300, 900, 3600]);
    }

    /// #472：分類器連續失敗到門檻才開 incident，而且**不推 inbox**——
    /// 壞掉的就是分類那條路，推進去那一則同樣會停在 `role IS NULL` 沒人收得到。
    #[test]
    fn a_failing_classifier_opens_an_incident_that_never_goes_through_the_inbox() {
        // 門檻以下不開：撐得過一次暫時性的 DB 錯誤。
        assert!(CLASSIFY_FAILURE_LIMIT >= 2, "至少要撐過一拍，否則一次抖動就開票");

        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs(CLASSIFY_FAILING_KIND, "inbox")];
        // 這一類沒有時間門檻（次數已經是門檻），所以看到就開。
        assert_eq!(d.plan(&seen, &[], &[], &t, 1000).open.len(), 1);

        // 不論有沒有協調者都不入 inbox。
        assert!(!notifiable(CLASSIFY_FAILING_KIND, "inbox", true));
        assert!(!notifiable(CLASSIFY_FAILING_KIND, "inbox", false));
        // 對照：一般的種類照舊會推。
        assert!(notifiable("host_disconnected", "mac2", true));

        // 恢復就自動關。
        let open = vec![(CLASSIFY_FAILING_KIND.to_string(), "inbox".to_string())];
        assert_eq!(d.plan(&[], &open, &[], &t, 9000).resolve, open);
    }

    /// A host that drops for ten seconds during a reconnect is not an outage. One that stays
    /// down past the threshold is, and it is written down exactly once.
    #[test]
    fn a_blip_is_not_an_incident_but_a_real_outage_is() {
        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs("host_disconnected", "mac2")];
        assert!(d.plan(&seen, &[], &[], &t, 1000).open.is_empty(), "first sighting is not yet an incident");
        assert!(d.plan(&seen, &[], &[], &t, 1119).open.is_empty(), "one second short of the threshold");
        assert_eq!(d.plan(&seen, &[], &[], &t, 1120).open.len(), 1, "past the threshold it opens");
        // Already open: every later pass just refreshes it, and `sweep` only notifies on the
        // transition, so a five-hour outage stays one notification.
        assert_eq!(d.plan(&seen, &[("host_disconnected".into(), "mac2".into())], &[], &t, 9999).open.len(), 1);
    }

    #[test]
    fn a_condition_that_clears_resolves_exactly_the_open_one() {
        let mut d = Detector::default();
        let t = thresholds();
        let open = vec![("host_disconnected".to_string(), "mac2".to_string())];
        let plan = d.plan(&[], &open, &[], &t, 5000);
        assert_eq!(plan.resolve, open);
        assert!(plan.open.is_empty());
        // And the clock starts over, so a fault that comes back has to hold the threshold again
        // rather than re-opening on its first tick.
        let seen = [obs("host_disconnected", "mac2")];
        assert!(d.plan(&seen, &[], &[], &t, 6000).open.is_empty());
    }

    /// The counter-churn case from 2026-09-10: bots starting and finishing all night is not a
    /// system fault and must not produce a single incident.
    #[test]
    fn bot_churn_produces_nothing() {
        let mut d = Detector::default();
        let t = thresholds();
        for i in 0..660 {
            assert!(d.plan(&[], &[], &[], &t, 1000 + i * 30).open.is_empty());
        }
    }

    /// Stalled work and exhausted notifications carry their own age test, so they open on the
    /// first pass that sees them rather than waiting a second threshold.
    #[test]
    fn probes_that_already_tested_age_open_immediately() {
        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs("assignment_stalled", "a1"), obs("notify_exhausted", "e1")];
        assert_eq!(d.plan(&seen, &[], &[], &t, 1000).open.len(), 2);
    }

    /// A probe whose query errored returns no observations — exactly like a probe that looked
    /// and found nothing. Treating them the same makes the sweep announce a recovery nobody
    /// saw, which is worse than silence: it closes an incident that is still happening.
    #[test]
    fn a_probe_that_could_not_run_never_resolves_its_incidents() {
        let mut d = Detector::default();
        let t = thresholds();
        let open = vec![
            ("host_disconnected".to_string(), "mac2".to_string()),
            ("assignment_stalled".to_string(), "a1".to_string()),
        ];
        // The stalled probe failed this pass; the host probe ran and saw nothing.
        let plan = d.plan(&[], &open, &["assignment_stalled"], &t, 5000);
        assert_eq!(
            plan.resolve,
            vec![("host_disconnected".to_string(), "mac2".to_string())],
            "only the probe that actually looked may close its incident"
        );
        // And once it can run again and still sees nothing, it resolves normally.
        let plan = d.plan(&[], &open, &[], &t, 5030);
        assert_eq!(plan.resolve.len(), 2);
    }

    /// incident 開起來與叫醒 AGM 的 `incident_opened` 同一個交易：通知寫不進去，incident 就不開（下一輪再來）。
    /// 以前先 `open_incident`、再 `let _` 推事件；寫不進去時 incident 已是 open，之後每輪 `opened=false` 不再通知，
    /// 一個真的系統故障就永遠沒人被告知。
    #[tokio::test]
    async fn an_incident_and_its_notification_land_together() {
        use super::super::bot_requests::flow_tests;
        use super::super::roles;
        let app = flow_tests::app().await;
        flow_tests::configure_responder(&app).await;
        let max = app.cfg.get().await.supervisor.notify_max_attempts.max(1);
        let stuck = store::push_inbox(&app.db, "health:y", "health_changed", None, None, None, &json!({})).await.unwrap().unwrap();
        roles::classify(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=? WHERE id=?").bind(max).bind(&stuck).execute(&app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER no_incident_event BEFORE INSERT ON supervisor_inbox WHEN NEW.kind='incident_opened' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let mut d = Detector::default();
        sweep(&app, &mut d).await;
        assert!(store::open_incidents(&app.db).await.unwrap().is_empty(), "通知寫不進去：incident 不開，下一輪再來");

        sqlx::query("DROP TRIGGER no_incident_event").execute(&app.db).await.unwrap();
        sweep(&app, &mut d).await;
        assert!(!store::open_incidents(&app.db).await.unwrap().is_empty(), "DB 好了：incident 開起來");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='incident_opened'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 1, "而且通知一則");
    }

    /// A blind pass must not restart a threshold that was already accumulating, or a fault
    /// could dodge every incident by coinciding with an intermittent query failure.
    #[test]
    fn a_blind_pass_does_not_reset_a_threshold_in_progress() {
        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs("host_disconnected", "mac2")];
        assert!(d.plan(&seen, &[], &[], &t, 1000).open.is_empty(), "clock starts");
        // The probe fails for a while: no observation, but the fault is not known to be gone.
        for at in [1030, 1060, 1090] {
            assert!(d.plan(&[], &[], &["host_disconnected"], &t, at).open.is_empty());
        }
        // Back up, still down: the original sighting still counts, so it opens on time.
        assert_eq!(d.plan(&seen, &[], &[], &t, 1120).open.len(), 1, "threshold measured from the first sighting");
    }

    /// The incidents that say "the inbox is broken" must not be announced through the inbox.
    #[test]
    fn the_broken_notification_channel_is_not_used_to_report_itself() {
        assert!(!notifiable("notify_exhausted", "responder", false), "this one would retry, exhaust, and open another incident");
        // With a responder there is a second channel whose queue never exhausts: tell it.
        assert!(notifiable("notify_exhausted", "responder", true));
        for kind in ["host_disconnected", "bot_stopped", "assignment_stalled", "assignment_undelivered", "remote_entry", super::super::failover::STALLED_KIND] {
            assert!(notifiable(kind, "whatever", false), "{kind} is safe to wake the manager about");
        }
    }

    /// issue #421：核准開著超過 30 分鐘沒人裁示 → incident，而且是走 inbox 喊人那條路（不是只留 log）。
    /// 裁示之後自己關掉。
    #[tokio::test]
    async fn a_stalled_approval_opens_an_incident_and_closes_when_someone_decides() {
        use super::super::failover;
        // `crate::testing::env()` 的 Env 一 drop 就把暫存目錄收掉，只留 app 會讓 DB 檔消失。
        let app = super::super::bot_requests::flow_tests::app().await;
        let a = store::create_approval(&app.db, "agm-kick", "rebuild", "release rebuild", Some("c1"), None, None).await.unwrap().approval.id;
        let t = thresholds();

        // 剛申請：沒有觀測。
        assert!(observe(&app, &t).await.seen.iter().all(|o| o.kind != failover::STALLED_KIND), "剛申請的不算卡住");

        sqlx::query("UPDATE supervisor_approvals SET created_at=? WHERE id=?")
            .bind(crate::db::iso_in(-t.approval_stalled_secs - 60))
            .bind(&a)
            .execute(&app.db)
            .await
            .unwrap();
        let seen = observe(&app, &t).await.seen;
        let obs = seen.iter().find(|o| o.kind == failover::STALLED_KIND).expect("30 分鐘沒裁示要被看到");
        assert_eq!(obs.resource, a, "resource 是核准 id，所以一筆核准只開一個 incident");
        assert_eq!(obs.severity, "critical", "卡住的是別人的部署");
        assert!(obs.detail.contains("waiting_secs"), "{}", obs.detail);

        // 真的寫進去，而且會叫醒人（`incident_opened` → 巡檢）。
        let mut detector = Detector::default();
        sweep(&app, &mut detector).await;
        // 寫入時不分類（`role`／`wake` 是 NULL），controller 每一拍先補上——跟其他 inbox 事件一樣。
        super::super::roles::classify(&app.db).await.unwrap();
        let woken: Vec<(String, Option<String>, Option<i64>)> =
            sqlx::query_as("SELECT kind, role, wake FROM supervisor_inbox WHERE kind='incident_opened'").fetch_all(&app.db).await.unwrap();
        assert_eq!(woken.len(), 1, "{woken:?}");
        assert_eq!((woken[0].1.as_deref(), woken[0].2), (Some("patrol"), Some(1)), "使用者入口要被叫醒：{woken:?}");

        // 有人裁示了 → 觀測消失，sweep 把 incident 關掉。
        store::decide_approval(&app.db, &a, "approved", "AGM:patrol", None, None).await.unwrap();
        assert!(observe(&app, &t).await.seen.iter().all(|o| o.kind != failover::STALLED_KIND));
        sweep(&app, &mut detector).await;
        let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_incidents WHERE kind=? AND status='open'")
            .bind(failover::STALLED_KIND)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(open, 0, "裁示之後 incident 要自己關");
    }

    /// 巡檢自己的通知一直送不出去（它倒了）時，`notify_exhausted` 跟巡檢的 `watchdog_gave_up` 都要進
    /// **協調者**的佇列並叫醒它——送回巡檢等於送進已知壞掉的那條路（review 2026-09-16 c1 M2、L4）。
    #[tokio::test]
    async fn when_patrol_cannot_be_reached_the_responder_is_told() {
        use super::super::bot_requests::flow_tests;
        use super::super::roles::{self, Role};
        let app = flow_tests::app().await;
        flow_tests::configure_responder(&app).await;
        let max = app.cfg.get().await.supervisor.notify_max_attempts.max(1);
        let stuck = store::push_inbox(&app.db, "health:x", "health_changed", None, None, None, &json!({})).await.unwrap().unwrap();
        roles::classify(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=? WHERE id=?").bind(max).bind(&stuck).execute(&app.db).await.unwrap();
        store::push_inbox(&app.db, "watchdog:gave_up:t", "watchdog_gave_up", None, None, None, &json!({"why": "CLI 起來就死"})).await.unwrap();

        let mut d = Detector::default();
        sweep(&app, &mut d).await;
        roles::classify(&app.db).await.unwrap();

        let exhausted = |e: &store::InboxEvent| e.kind == "incident_opened" && e.payload_json.contains("notify_exhausted");
        let responder = roles::due_for(&app.db, Role::Responder, true, "2999-01-01T00:00:00Z", 0).await.unwrap();
        assert!(responder.iter().any(|e| exhausted(e) && e.wake == Some(1)), "notify_exhausted 要推給協調者並叫醒它");
        assert!(responder.iter().any(|e| e.kind == "watchdog_gave_up" && e.wake == Some(1)));
        let patrol = roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 1_000).await.unwrap();
        assert!(patrol.iter().all(|e| !exhausted(e) && e.kind != "watchdog_gave_up"), "不送回倒下的巡檢");
    }

    /// issue #504 審核：整個佇列一起用盡額度時是**一張** critical incident 帶筆數，不是一則一張。
    ///
    /// 一則一張的話，巡檢停在登入失效那一晚（現場上百則）會開出上百張 critical，而每一張又各推一則
    /// `incident_opened` 給協調者——「把一則送不出去的通知變成 N 則」正是 `notifiable` 那段註解在防的事。
    /// 隔壁 `responder_undeliverable` 早就是聚合的，兩邊形狀現在一致。
    #[tokio::test]
    async fn a_whole_queue_that_ran_out_of_retries_is_one_incident_carrying_the_count() {
        use super::super::bot_requests::flow_tests;
        use super::super::roles;
        let app = flow_tests::app().await;
        flow_tests::configure_responder(&app).await;
        let max = app.cfg.get().await.supervisor.notify_max_attempts.max(1);
        for i in 0..5 {
            store::push_inbox(&app.db, &format!("health:{i}"), "health_changed", None, None, None, &json!({})).await.unwrap();
        }
        roles::classify(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=?").bind(max).execute(&app.db).await.unwrap();

        let mut d = Detector::default();
        sweep(&app, &mut d).await;

        let open = store::open_incidents(&app.db).await.unwrap();
        let mine: Vec<_> = open.iter().filter(|i| i.kind == "notify_exhausted").collect();
        assert_eq!(mine.len(), 1, "五則事件一張 incident，不是五張");
        assert_eq!(mine[0].resource, "patrol", "resource 是角色，不是事件 id（換一則事件不會留下關不掉的孤兒）");
        let detail: Value = serde_json::from_str(&mine[0].detail_json).unwrap();
        assert_eq!(detail["events"], 5, "筆數要寫在 detail 上：{detail}");
        assert_eq!(detail["max_attempts"], max);
        // 叫醒協調者的也只有一則。
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='incident_opened'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1, "一張 incident 一則通知");
    }

    /// issue #504 附帶：`notify_max_attempts` 的下限要跟送出那一路（`controller::notify` 的 `.max(1)`）一致。
    ///
    /// 沒夾的話 `0` 會讓 `notify_attempts >= 0` 對**每一則** pending 巡檢事件成立：一則都還沒送過就
    /// 開 critical incident，而通知本身其實正常在送（送出那一路用的是 1）。
    #[test]
    fn a_zero_or_negative_notify_budget_is_clamped_like_the_sending_path_does() {
        let mut cfg = crate::config::SupervisorCfg::default();
        for bad in [0, -3] {
            cfg.notify_max_attempts = bad;
            assert_eq!(Thresholds::from_cfg(&cfg).notify_max_attempts, 1, "{bad} 要夾成 1");
        }
        cfg.notify_max_attempts = 5;
        assert_eq!(Thresholds::from_cfg(&cfg).notify_max_attempts, 5, "正常值不動");
    }

    /// 探針查詢失敗：`system_health` 回 unknown 並列出是哪一類看不到；下一輪跑得起來就回 healthy。
    /// 真的有 degraded 的 incident 時照舊是 degraded（unknown 不蓋過真的故障）。
    #[tokio::test]
    async fn a_probe_that_cannot_run_makes_the_system_half_unknown_not_healthy() {
        let app = super::super::bot_requests::flow_tests::app().await;
        assert_eq!(system_health(&app).await["status"], "healthy");
        note_blind(&app, &["bot_stopped", "bot_stopped"]);
        let h = system_health(&app).await;
        assert_eq!(h["status"], "unknown", "{h}");
        assert_eq!(h["blind_probes"], json!(["bot_stopped"]));
        store::open_incident(&app.db, "host_disconnected", "mac2", "degraded", &json!({})).await.unwrap();
        assert_eq!(system_health(&app).await["status"], "degraded");
        note_blind(&app, &[]);
        let h = system_health(&app).await;
        assert_eq!((h["status"].as_str(), h["blind_probes"].as_array().map(Vec::len)), (Some("degraded"), Some(0)));
    }

    #[test]
    fn not_knowing_outranks_healthy_but_not_a_real_fault() {
        assert_eq!(worst("healthy", "unknown"), "unknown");
        assert_eq!(worst("unknown", "degraded"), "degraded");
        assert_eq!(worst("critical", "degraded"), "critical");
        assert_eq!(worst("healthy", "healthy"), "healthy");
    }

    /// issue #100（同 fence.rs／a4605b2 的根因）：一顆 bot 快速重啟時兩個 run 可能擠進同一毫秒。
    /// 「最後一個 run 是不是使用者自己停的」要看**寫入順序**（`rowid`），不是 ULID 字典序——故意讓
    /// 真正較舊的那個 run 用字典序比較大的假 ULID，證明不會把使用者主動停的（`stopped`）誤判成 outage。
    #[tokio::test]
    async fn a_bots_last_run_is_found_by_insertion_order_not_ulid_when_deciding_bot_stopped() {
        let app = super::super::bot_requests::flow_tests::app().await;
        let bot_id = "watched";
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,'p','watched','claude','[]',1,1,'tok-watched',?)",
        )
        .bind(bot_id)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let at = crate::db::now();
        // 真正較舊（先寫入）的用字典序比較大的假 ULID；真正最新（後寫入、使用者主動停的）用比較小的。
        for (id, state) in [("01ZZZZZZZZZZZZZZZZZZZZZZZZ", "exited"), ("01AAAAAAAAAAAAAAAAAAAAAAAA", "stopped")] {
            sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, started_at) VALUES (?,?,?,'unknown',?)")
                .bind(id)
                .bind(bot_id)
                .bind(state)
                .bind(&at)
                .execute(&app.db)
                .await
                .unwrap();
        }

        let probed = observe(&app, &thresholds()).await;
        assert!(
            probed.seen.iter().all(|o| !(o.kind == "bot_stopped" && o.resource == bot_id)),
            "最後一個 run（寫入順序）其實是使用者主動停的，不該被誤判成 outage：{:?}",
            probed.seen
        );
    }
}
