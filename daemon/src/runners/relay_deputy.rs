//! 專家模式副管理者：管理者回合中太久時，排隊給它的請求改送副管理者；管理者閒下來後收到處理摘要。
//!
//! 只改派 `relay_requests` 裡還沒送出的請求（`pending`、沒有 `to_turn_id`）。shim 直送 pane、結果通知、
//! 使用者輸入、AGM 訊息都不動。設定在專案層（`ProjectCfg.deputy`，投影到 `projects.deputy_*`），
//! 不新增角色：副管理者仍是 advisor／specialist。
use std::sync::Arc;
use serde_json::{json, Value};
use crate::{config::{DeputyCfg, DEPUTY_DEFAULT_AFTER_SEC, DEPUTY_MIN_AFTER_SEC}, db, events::ports::TurnCommands, lifecycle::LcError, state::App};

/// 一次 sweep 對一個專案最多改派幾則（一則一則來，避免把副管理者灌爆）。
const REDIRECT_PER_SWEEP: usize = 1;
/// 一則摘要最多帶幾筆請求。
const DIGEST_MAX: usize = 10;
const DIGEST_PREFIX: &str = "relay-deputy-digest:";

/// `PATCH /api/projects/{id}` 的 `deputy` 欄位：`null`＝清掉；物件 `{bot_id, after_sec?}`。
/// `after_sec` 小於下限回 400 `deputy_after_sec_too_small`。
pub fn parse_patch(raw: &Value) -> Result<Option<DeputyCfg>, LcError> {
    let bad = |reason: &str| LcError::BadValue(json!({"error": "bad_request", "reason": reason}));
    match raw {
        Value::Null => Ok(None),
        Value::Object(map) => {
            let bot_id = map.get("bot_id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad("deputy_bot_id_required"))?;
            let after_sec = match map.get("after_sec") {
                None | Some(Value::Null) => DEPUTY_DEFAULT_AFTER_SEC,
                Some(v) => {
                    let n = v.as_u64().ok_or_else(|| bad("deputy_after_sec_invalid"))?;
                    if n < u64::from(DEPUTY_MIN_AFTER_SEC) {
                        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "deputy_after_sec_too_small", "min": DEPUTY_MIN_AFTER_SEC})));
                    }
                    u32::try_from(n).map_err(|_| bad("deputy_after_sec_invalid"))?
                }
            };
            Ok(Some(DeputyCfg { bot_id: bot_id.to_string(), after_sec }))
        }
        _ => Err(bad("deputy_invalid")),
    }
}

/// 副管理者必須是專家專案裡的 advisor／specialist 成員（不能是管理者、執行者或沒角色）。
pub async fn validate_target(app: &Arc<App>, project_id: &str, expert: bool, d: &DeputyCfg) -> Result<(), LcError> {
    let bad = |reason: &str| LcError::BadValue(json!({"error": "bad_request", "reason": reason, "bot_id": d.bot_id}));
    if !expert { return Err(bad("deputy_requires_expert_project")); }
    let bot = db::bot(&app.db, &d.bot_id).await.map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|b| b.deleted_at.is_none() && b.project_id == project_id && b.parent_bot_id.is_none()).ok_or_else(|| bad("deputy_bot_not_found"))?;
    if !matches!(bot.role.as_deref(), Some("advisor" | "specialist")) { return Err(bad("deputy_role_invalid")); }
    Ok(())
}

/// `from`→`to` 之間有沒有一則改派過來、還沒做完的請求（`Deliver` 授權用：原本送管理者時已通過 NewRequest，不重驗副管理者）。
pub async fn redirected_open(app: &Arc<App>, from: &str, to: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM relay_requests WHERE from_bot_id=? AND to_bot_id=? AND redirected_from IS NOT NULL AND state IN ('pending','delivered'))")
        .bind(from).bind(to).fetch_one(&app.db).await
}

/// 這顆 bot 現在是不是「代班中」：它是專案的副管理者，且名下有改派來、還沒做完的請求。
/// 代班中才有管理者的派工權限（可對成員派工、start／restart）；不含 hire。
pub async fn acting(app: &Arc<App>, bot_id: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM relay_requests r JOIN bots b ON b.id=r.to_bot_id JOIN projects p ON p.id=b.project_id WHERE r.to_bot_id=? AND p.deputy_bot_id=b.id AND p.mode='expert' AND p.deleted_at IS NULL AND r.redirected_from IS NOT NULL AND r.state IN ('pending','delivered'))")
        .bind(bot_id).fetch_one(&app.db).await.unwrap_or(false)
}

/// 送給副管理者的文字加一行說明（只在送出時加，`prompt_text` 原文不動，client_request_id 去重才不會失配）。
pub async fn decorate(app: &Arc<App>, text: &str, redirected_from: Option<&str>) -> String {
    let Some(manager_id) = redirected_from else { return text.to_string() };
    let name = db::bot(&app.db, manager_id).await.ok().flatten().map(|b| b.name).unwrap_or_else(|| manager_id.to_string());
    format!("[原本送給管理者 {name}，他回合中太久，改由你（副管理者）代為處理：能回就直接回；需要管理者決策的請明說「留給管理者」，不要自己拍板。你處理過的事會在他閒下來時以摘要回報給他。]\n\n{text}")
}

type RunRow = (String, String);

async fn run_status(app: &Arc<App>, bot_id: &str) -> Option<RunRow> {
    sqlx::query_as("SELECT state,agent_status FROM runs WHERE bot_id=? AND state IN ('starting','running','stopping')")
        .bind(bot_id).fetch_optional(&app.db).await.ok().flatten()
}

pub async fn sweep(app: &Arc<App>) {
    let rows: Vec<(String, String, Option<i64>)> = match sqlx::query_as("SELECT id,deputy_bot_id,deputy_after_sec FROM projects WHERE deleted_at IS NULL AND handed_off_to IS NULL AND mode='expert' AND deputy_bot_id IS NOT NULL")
        .fetch_all(&app.db).await {
        Ok(r) => r,
        Err(e) => { tracing::warn!(error = ?e, "relay deputy sweep failed"); return }
    };
    for (project_id, deputy_id, after_sec) in rows {
        let after_sec = after_sec.map_or(DEPUTY_DEFAULT_AFTER_SEC, |n| n.clamp(i64::from(DEPUTY_MIN_AFTER_SEC), i64::from(u32::MAX)) as u32);
        if let Err(e) = sweep_project(app, &project_id, &deputy_id, after_sec).await {
            tracing::warn!(project = %project_id, error = ?e, "relay deputy sweep failed");
        }
    }
    // 摘要不看專案現在有沒有副管理者：換人、清掉設定之前代處理過的也要回報給管理者。
    let managers: Vec<String> = sqlx::query_scalar("SELECT DISTINCT redirected_from FROM relay_requests WHERE redirected_from IS NOT NULL AND digest_at IS NULL AND state IN ('answered','notified','failed')")
        .fetch_all(&app.db).await.unwrap_or_default();
    for manager_id in managers {
        if !run_status(app, &manager_id).await.is_some_and(|(state, status)| state == "running" && status == "idle") { continue }
        if let Err(e) = digest(app, &manager_id).await { tracing::warn!(manager = %manager_id, error = ?e, "relay deputy digest failed"); }
    }
}

async fn sweep_project(app: &Arc<App>, project_id: &str, deputy_id: &str, after_sec: u32) -> anyhow::Result<()> {
    let manager: Option<(String, String)> = sqlx::query_as("SELECT id,name FROM bots WHERE project_id=? AND role='manager' AND managed_by='user' AND parent_bot_id IS NULL AND deleted_at IS NULL LIMIT 1")
        .bind(project_id).fetch_optional(&app.db).await?;
    let Some((manager_id, _)) = manager else { return Ok(()) };
    let deputy_ok: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots WHERE id=? AND project_id=? AND parent_bot_id IS NULL AND deleted_at IS NULL AND role IN ('advisor','specialist') AND manager_use=1)")
        .bind(deputy_id).bind(project_id).fetch_one(&app.db).await?;
    if !deputy_ok { return Ok(()) }
    let manager_busy = run_status(app, &manager_id).await.is_some_and(|(state, status)| state == "running" && status == "working");
    let deputy_idle = run_status(app, deputy_id).await.is_some_and(|(state, status)| state == "running" && status == "idle");
    if manager_busy && deputy_idle { redirect(app, &manager_id, deputy_id, after_sec).await?; }
    Ok(())
}

/// 把給管理者、排了超過門檻的 pending 請求改送副管理者（一次一則；副管理者手上已有改派來的未結請求就先不加）。
async fn redirect(app: &Arc<App>, manager_id: &str, deputy_id: &str, after_sec: u32) -> anyhow::Result<()> {
    let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM relay_requests WHERE to_bot_id=? AND redirected_from IS NOT NULL AND state IN ('pending','delivered'))")
        .bind(deputy_id).fetch_one(&app.db).await?;
    if busy { return Ok(()) }
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM relay_requests WHERE to_bot_id=? AND state='pending' AND to_turn_id IS NULL AND redirected_from IS NULL AND from_bot_id<>? AND julianday(created_at)<=julianday('now')-?/86400.0 ORDER BY created_at LIMIT ?")
        .bind(manager_id).bind(deputy_id).bind(i64::from(after_sec)).bind(REDIRECT_PER_SWEEP as i64).fetch_all(&app.db).await?;
    for id in ids {
        let lock = crate::runners::child_done::parent_lock(&format!("relay-request:{id}"));
        let Ok(_guard) = lock.try_lock() else { continue };
        // CAS：這段時間若已經送出（有 turn）或被別人改掉，就不動；改派後 UNIQUE(from,to,client_request_id) 撞到也略過。
        let moved = sqlx::query("UPDATE OR IGNORE relay_requests SET to_bot_id=?,redirected_from=?,redirected_at=?,attempts=0,error=NULL WHERE id=? AND state='pending' AND to_turn_id IS NULL AND to_bot_id=? AND redirected_from IS NULL AND NOT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.client_request_id='relay-request:'||relay_requests.id)")
            .bind(deputy_id).bind(manager_id).bind(db::now()).bind(&id).bind(manager_id).bind(manager_id)
            .execute(&app.db).await?.rows_affected();
        if moved == 1 { tracing::info!(request = %id, manager = %manager_id, deputy = %deputy_id, "relay request redirected to the deputy manager"); }
    }
    Ok(())
}

/// 管理者閒下來：把副管理者處理完（answered／notified／failed）還沒回報的請求整理成一則通知排給它。
async fn digest(app: &Arc<App>, manager_id: &str) -> anyhow::Result<()> {
    let rows: Vec<DigestRow> = sqlx::query_as(
        "SELECT r.id,(SELECT name FROM bots WHERE id=r.from_bot_id),r.prompt_text,r.state,r.error,(SELECT m.content FROM messages m WHERE m.turn_id=r.to_turn_id AND m.role='assistant' ORDER BY m.created_at DESC,m.rowid DESC LIMIT 1) FROM relay_requests r WHERE r.redirected_from=? AND r.digest_at IS NULL AND r.state IN ('answered','notified','failed') ORDER BY r.created_at LIMIT ?")
        .bind(manager_id).bind(DIGEST_MAX as i64).fetch_all(&app.db).await?;
    if rows.is_empty() { return Ok(()) }
    let message = compose_digest(&rows);
    let crid = format!("{DIGEST_PREFIX}{}", rows[0].0);
    // 通知的寄件者記成處理的那位副管理者（UI 把泡泡畫在左邊）。
    let sender: String = sqlx::query_scalar("SELECT to_bot_id FROM relay_requests WHERE id=?").bind(&rows[0].0).fetch_one(&app.db).await?;
    match app.prompt_relayed_queueable(manager_id, &message, &crid, Some(&sender), crate::lifecycle::QueueClass::DaemonNotice).await {
        Ok(_) => {
            let now = db::now();
            for (id, ..) in &rows {
                sqlx::query("UPDATE relay_requests SET digest_at=? WHERE id=? AND digest_at IS NULL").bind(&now).bind(id).execute(&app.db).await?;
            }
        }
        // 管理者的佇列格被別的通知占著、或輸入框有草稿：下一輪再試。
        Err(LcError::Conflict(_)) => {}
        Err(e) => tracing::warn!(manager = %manager_id, error = ?e, "deputy digest could not be queued"),
    }
    Ok(())
}

/// 代班中的副管理者可以 start／restart 同專案的成員（管理者本人與自己除外）；停止、刪除、設角色、hire 都不行。
pub async fn may_restart(app: &Arc<App>, caller: &str, method: &str, parts: &[impl AsRef<str>], target: &db::Bot) -> bool {
    let sub = parts.get(3).map(|p| p.as_ref());
    if !(method == "POST" && parts.len() == 4 && matches!(sub, Some("start" | "restart"))) { return false }
    if target.id == caller || target.deleted_at.is_some() || target.parent_bot_id.is_some() || target.manager_use == 0 || target.role.as_deref() == Some("manager") { return false }
    let Ok(Some(me)) = db::bot(&app.db, caller).await else { return false };
    me.project_id == target.project_id && me.deleted_at.is_none() && acting(app, caller).await
}

type DigestRow = (String, Option<String>, String, String, Option<String>, Option<String>);

/// 摘要本文：每件一行（誰問、原文前 80 字、副管理者的回覆前 240 字或失敗原因）。
fn compose_digest(rows: &[DigestRow]) -> String {
    let clip = |s: &str, n: usize| { let s = s.trim().replace('\n', " "); if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s } };
    let mut lines = vec![format!("你回合中時，副管理者代你處理了 {} 件請求：", rows.len())];
    for (i, (_, from, text, state, error, reply)) in rows.iter().enumerate() {
        let outcome = match (state.as_str(), reply.as_deref().filter(|r| !r.trim().is_empty())) {
            ("failed", _) => format!("失敗（{}）", error.as_deref().unwrap_or("未送達")),
            (_, Some(r)) => format!("回覆：{}", clip(r, 240)),
            _ => "已處理，沒有擷取到回覆文字".to_string(),
        };
        lines.push(format!("{}. {} 問：{} → {}", i + 1, from.as_deref().unwrap_or("?"), clip(text, 80), outcome));
    }
    lines.push("需要你決策的請自行接手；沒問題就不必回。".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_policy::{relay_prompt_allowed, Admission, RelayDenied};

    struct Fx { env: crate::testing::Env, manager: String, deputy: String, worker: String, asker: String }

    /// 專家專案：管理者、顧問（副管理者）、執行者、顧問（提問者）；管理者／副管理者各有一條 running 的 run。
    async fn fx() -> Fx {
        let env = crate::testing::env().await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let mut ids = Vec::new();
        for (name, role) in [("dep-mgr", "manager"), ("dep-deputy", "advisor"), ("dep-worker", "executor"), ("dep-asker", "specialist")] {
            let b = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
            sqlx::query("UPDATE bots SET role=?, track=? WHERE id=?").bind(role).bind(if role == "specialist" { Some("t1") } else { None }).bind(&b.id).execute(&env.app.db).await.unwrap();
            let run = crate::testing::fake_run(&env.app, &b.id).await;
            sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:{role}", env.project_id)).bind(run).execute(&env.app.db).await.unwrap();
            ids.push(b.id);
        }
        sqlx::query("UPDATE projects SET deputy_bot_id=?, deputy_after_sec=60 WHERE id=?").bind(&ids[1]).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        Fx { manager: ids[0].clone(), deputy: ids[1].clone(), worker: ids[2].clone(), asker: ids[3].clone(), env }
    }

    async fn status(f: &Fx, bot: &str, s: &str) {
        sqlx::query("UPDATE runs SET agent_status=? WHERE bot_id=? AND state='running'").bind(s).bind(bot).execute(&f.env.app.db).await.unwrap();
    }

    async fn request(f: &Fx, age_sec: i64, key: &str) -> String {
        let id = db::ulid();
        let created = (chrono::Utc::now() - chrono::Duration::seconds(age_sec)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,?,'請幫忙看一下','[]','pending',?)")
            .bind(&id).bind(&f.asker).bind(&f.manager).bind(key).bind(created).execute(&f.env.app.db).await.unwrap();
        id
    }

    async fn target(f: &Fx, id: &str) -> (String, Option<String>) {
        sqlx::query_as("SELECT to_bot_id,redirected_from FROM relay_requests WHERE id=?").bind(id).fetch_one(&f.env.app.db).await.unwrap()
    }

    #[test]
    fn patch_enforces_the_minimum_threshold() {
        assert!(parse_patch(&json!(null)).unwrap().is_none());
        let d = parse_patch(&json!({"bot_id": "b1"})).unwrap().unwrap();
        assert_eq!((d.bot_id.as_str(), d.after_sec), ("b1", DEPUTY_DEFAULT_AFTER_SEC));
        assert_eq!(parse_patch(&json!({"bot_id": "b1", "after_sec": 60})).unwrap().unwrap().after_sec, 60);
        for bad in [json!({"bot_id": "b1", "after_sec": 59}), json!({"bot_id": "b1", "after_sec": 0})] {
            assert!(matches!(parse_patch(&bad), Err(LcError::BadValue(v)) if v["reason"] == "deputy_after_sec_too_small" && v["min"] == 60), "{bad}");
        }
        assert!(matches!(parse_patch(&json!({"bot_id": "b1", "after_sec": "x"})), Err(LcError::BadValue(_))));
        assert!(matches!(parse_patch(&json!({"after_sec": 100})), Err(LcError::BadValue(_))));
        assert!(matches!(parse_patch(&json!("b1")), Err(LcError::BadValue(_))));
    }

    #[tokio::test]
    async fn only_advisors_and_specialists_of_an_expert_project_can_be_deputy() {
        let f = fx().await;
        let d = |id: &str| DeputyCfg { bot_id: id.into(), after_sec: 180 };
        let app = &f.env.app;
        let pid = f.env.project_id.as_str();
        assert!(validate_target(app, pid, true, &d(&f.deputy)).await.is_ok());
        assert!(validate_target(app, pid, true, &d(&f.asker)).await.is_ok(), "specialist");
        for bad in [&f.manager, &f.worker] {
            assert!(matches!(validate_target(app, pid, true, &d(bad)).await, Err(LcError::BadValue(v)) if v["reason"] == "deputy_role_invalid"));
        }
        assert!(validate_target(app, pid, false, &d(&f.deputy)).await.is_err(), "not an expert project");
        assert!(validate_target(app, pid, true, &d("missing")).await.is_err());
        let other = db::ulid();
        sqlx::query("INSERT INTO projects(id,path,label,created_at,mode) VALUES(?,?,'o',?,'expert')").bind(&other).bind(format!("/tmp/{other}")).bind(db::now()).execute(&app.db).await.unwrap();
        assert!(validate_target(app, &other, true, &d(&f.deputy)).await.is_err(), "bot of another project");
    }

    #[tokio::test]
    async fn waiting_requests_move_to_the_deputy_only_when_the_manager_is_busy_and_the_deputy_is_free() {
        let f = fx().await;
        let app = &f.env.app;
        let old = request(&f, 120, "k-old").await;
        let young = request(&f, 5, "k-young").await;
        // 管理者閒著：不動。
        status(&f, &f.manager, "idle").await;
        sweep(app).await;
        assert_eq!(target(&f, &old).await, (f.manager.clone(), None));
        // 管理者忙、副管理者也忙：不動。
        status(&f, &f.manager, "working").await;
        status(&f, &f.deputy, "working").await;
        sweep(app).await;
        assert_eq!(target(&f, &old).await.0, f.manager);
        // 副管理者閒著：只動排超過門檻的那則。
        status(&f, &f.deputy, "idle").await;
        sweep(app).await;
        assert_eq!(target(&f, &old).await, (f.deputy.clone(), Some(f.manager.clone())));
        assert_eq!(target(&f, &young).await, (f.manager.clone(), None), "還沒到門檻");
        // 已改派的請求還沒做完，不再加第二則。
        sqlx::query("UPDATE relay_requests SET created_at=? WHERE id=?").bind((chrono::Utc::now() - chrono::Duration::seconds(300)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).bind(&young).execute(&app.db).await.unwrap();
        sweep(app).await;
        assert_eq!(target(&f, &young).await.0, f.manager);
        // 已經有 turn 的（送出中）不改派。
        sqlx::query("UPDATE relay_requests SET state='answered' WHERE id=?").bind(&old).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE relay_requests SET to_turn_id=NULL WHERE id=?").bind(&young).execute(&app.db).await.unwrap();
        let conv = db::conversation_id(&app.db, &f.manager).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,client_request_id,status,delivery,delivery_verified,created_at) VALUES(?,?,'web',?,'queued','pending',0,?)").bind(db::ulid()).bind(&conv).bind(format!("relay-request:{young}")).bind(db::now()).execute(&app.db).await.unwrap();
        sweep(app).await;
        assert_eq!(target(&f, &young).await.0, f.manager, "管理者那邊已經排進去了");
    }

    #[tokio::test]
    async fn a_missing_or_invalid_deputy_redirects_nothing() {
        let f = fx().await;
        let id = request(&f, 600, "k").await;
        status(&f, &f.manager, "working").await;
        sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&f.deputy).execute(&f.env.app.db).await.unwrap();
        sweep(&f.env.app).await;
        assert_eq!(target(&f, &id).await.0, f.manager);
        sqlx::query("UPDATE bots SET role='advisor', manager_use=0 WHERE id=?").bind(&f.deputy).execute(&f.env.app.db).await.unwrap();
        sweep(&f.env.app).await;
        assert_eq!(target(&f, &id).await.0, f.manager, "manager_use 關掉的成員不接");
    }

    #[tokio::test]
    async fn redirected_requests_are_admitted_and_the_acting_deputy_gets_dispatch_rights_but_not_hire() {
        let f = fx().await;
        let app = &f.env.app;
        let id = request(&f, 600, "k").await;
        // 改派前：asker 不能直接找 deputy 以外的……且 deputy 不能對執行者派工。
        assert!(matches!(relay_prompt_allowed(app, &f.deputy, &f.worker, Admission::NewRequest).await, Err(RelayDenied::Denied(_))));
        status(&f, &f.manager, "working").await;
        sweep(app).await;
        assert_eq!(target(&f, &id).await.0, f.deputy);
        // 送達授權：asker（specialist）本來不能找 specialist／顧問以外，但這是改派來的。
        sqlx::query("UPDATE bots SET role='specialist', track='x' WHERE id=?").bind(&f.deputy).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE bot_id=?").bind(format!("{}:local:expert:specialist", f.env.project_id)).bind(&f.deputy).execute(&app.db).await.unwrap();
        assert!(relay_prompt_allowed(app, &f.asker, &f.deputy, Admission::Deliver).await.is_ok());
        // 代班中：可對執行者派工。
        assert!(acting(app, &f.deputy).await);
        assert!(relay_prompt_allowed(app, &f.deputy, &f.worker, Admission::NewRequest).await.is_ok());
        // 可 start／restart 成員，不能 stop／刪除，也不能動管理者。
        let worker = db::bot(&app.db, &f.worker).await.unwrap().unwrap();
        let manager = db::bot(&app.db, &f.manager).await.unwrap().unwrap();
        let parts = |bot: &str, sub: &str| vec!["api".to_string(), "bots".to_string(), bot.to_string(), sub.to_string()];
        assert!(may_restart(app, &f.deputy, "POST", &parts(&f.worker, "restart"), &worker).await);
        assert!(may_restart(app, &f.deputy, "POST", &parts(&f.worker, "start"), &worker).await);
        assert!(!may_restart(app, &f.deputy, "POST", &parts(&f.worker, "stop"), &worker).await);
        assert!(!may_restart(app, &f.deputy, "POST", &parts(&f.manager, "restart"), &manager).await);
        // 請求做完：不再代班，派工權限收回。
        sqlx::query("UPDATE relay_requests SET state='answered' WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
        assert!(!acting(app, &f.deputy).await);
        assert!(matches!(relay_prompt_allowed(app, &f.deputy, &f.worker, Admission::NewRequest).await, Err(RelayDenied::Denied(_))));
        assert!(!may_restart(app, &f.deputy, "POST", &parts(&f.worker, "restart"), &worker).await);
    }

    #[test]
    fn the_digest_lists_who_asked_and_what_the_deputy_answered() {
        let rows: Vec<DigestRow> = vec![
            ("r1".into(), Some("asker".into()), "請幫忙看一下".into(), "notified".into(), None, Some("已看過，沒問題\n可以合併".into())),
            ("r2".into(), None, "x".repeat(200), "failed".into(), Some("target turn unavailable".into()), None),
            ("r3".into(), Some("b".into()), "q".into(), "answered".into(), None, None),
        ];
        let text = compose_digest(&rows);
        assert!(text.starts_with("你回合中時，副管理者代你處理了 3 件請求："), "{text}");
        assert!(text.contains("1. asker 問：請幫忙看一下 → 回覆：已看過，沒問題 可以合併"), "{text}");
        assert!(text.contains("2. ? 問：") && text.contains("…") && text.contains("失敗（target turn unavailable）"), "{text}");
        assert!(text.contains("3. b 問：q → 已處理，沒有擷取到回覆文字"), "{text}");
        assert!(!text.contains(&"x".repeat(100)), "原文只留前 80 字");
    }

    #[tokio::test]
    async fn digest_is_sent_once_the_manager_is_idle_and_never_twice() {
        let f = fx().await;
        let app = &f.env.app;
        let id = request(&f, 600, "k").await;
        status(&f, &f.manager, "working").await;
        sweep(app).await;
        sqlx::query("UPDATE relay_requests SET state='answered' WHERE id=?").bind(&id).execute(&app.db).await.unwrap();
        // 管理者還在回合中：不送摘要。
        sweep(app).await;
        let pending: bool = sqlx::query_scalar("SELECT digest_at IS NULL FROM relay_requests WHERE id=?").bind(&id).fetch_one(&app.db).await.unwrap();
        assert!(pending);
        // 閒下來（hook 的 idle 比回合結尾早一拍，所以這裡讓管理者還有一個 in_flight 的回合）：摘要排進它的佇列。
        let conv = db::conversation_id(&app.db, &f.manager).await.unwrap();
        let run: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'").bind(&f.manager).fetch_one(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('t-live',?,?,'web','in_flight','ok',?)").bind(&conv).bind(&run).bind(db::now()).execute(&app.db).await.unwrap();
        status(&f, &f.manager, "idle").await;
        sweep(app).await;
        let queued = || sqlx::query_as::<_, (String, Option<String>)>("SELECT m.content,m.relay_from FROM turns t JOIN messages m ON m.turn_id=t.id WHERE t.conversation_id=? AND t.status='queued'").bind(conv.clone()).fetch_all(&app.db);
        let rows = queued().await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].0.contains("副管理者代你處理了 1 件請求") && rows[0].0.contains("請幫忙看一下"), "{}", rows[0].0);
        assert_eq!(rows[0].1.as_deref(), Some(f.deputy.as_str()), "寄件者記成副管理者");
        let first: Option<String> = sqlx::query_scalar("SELECT digest_at FROM relay_requests WHERE id=?").bind(&id).fetch_one(&app.db).await.unwrap();
        assert!(first.is_some(), "排進去才記 digest_at");
        sweep(app).await;
        assert_eq!(queued().await.unwrap().len(), 1, "同一則不會再摘要一次");
        let second: Option<String> = sqlx::query_scalar("SELECT digest_at FROM relay_requests WHERE id=?").bind(&id).fetch_one(&app.db).await.unwrap();
        assert_eq!(first, second);
    }

    /// 副管理者被刪、換成別人、被設成管理者或執行者、專案離開專家模式：`deputy` 一律清掉並寫進 DB。
    #[tokio::test]
    async fn deputy_setting_is_cleared_whenever_the_deputy_stops_qualifying() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-deputy-clear-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut text = String::from("[server]\nlisten = '127.0.0.1:7788'\n\n[[projects]]\nid = 'p1'\npath = '/tmp'\nlabel = 'demo'\nhost = 'local'\nmode = 'expert'\n\n[projects.deputy]\nbot_id = 'adv'\nafter_sec = 90\n");
        for (id, role) in [("mgr", "manager"), ("adv", "advisor"), ("adv2", "advisor"), ("ex", "executor"), ("e2", "executor"), ("e3", "executor"), ("e4", "executor")] {
            text.push_str(&format!("\n[[projects.bots]]\nid = '{id}'\nname = '{id}'\nkind = 'claude'\nrole = '{role}'\n"));
        }
        std::fs::write(&path, &text).unwrap();
        let pool = crate::app_ports_p1::open(&dir.join("db.sqlite3")).await.unwrap();
        let store = crate::runners::app_ports_p2::load_config(path.clone()).await.unwrap();
        crate::projection::project_config(&store, &pool).await.unwrap();
        let col = |pool: &sqlx::SqlitePool| { let pool = pool.clone(); async move { sqlx::query_as::<_, (Option<String>, Option<i64>)>("SELECT deputy_bot_id,deputy_after_sec FROM projects WHERE id='p1'").fetch_one(&pool).await.unwrap() } };
        assert_eq!(col(&pool).await, (Some("adv".into()), Some(90)), "設定投影進 DB");

        // 換一個仍然合格的人：保留（不被誤清）。
        crate::projection::update_and_project(&store, &pool, |cfg| { cfg.projects[0].deputy = Some(DeputyCfg { bot_id: "adv2".into(), after_sec: 120 }); Ok(()) }).await.unwrap();
        assert_eq!(col(&pool).await, (Some("adv2".into()), Some(120)));

        // 被設成執行者／管理者 → 清掉。
        for role in ["executor", "manager"] {
            crate::projection::update_and_project(&store, &pool, |cfg| { cfg.projects[0].deputy = Some(DeputyCfg { bot_id: "adv".into(), after_sec: 90 }); Ok(()) }).await.unwrap();
            crate::projection::update_and_project(&store, &pool, |cfg| {
                for b in &mut cfg.projects[0].bots {
                    if role == "manager" && b.role.as_deref() == Some("manager") { b.role = Some("advisor".into()); }
                    if b.id.as_deref() == Some("adv") { b.role = Some(role.into()); }
                }
                Ok(())
            }).await.unwrap();
            assert_eq!(col(&pool).await, (None, None), "role={role}");
            assert!(store.get().await.projects[0].deputy.is_none());
            crate::projection::update_and_project(&store, &pool, |cfg| {
                for b in &mut cfg.projects[0].bots {
                    if b.id.as_deref() == Some("adv") { b.role = Some("advisor".into()); }
                    if b.id.as_deref() == Some("mgr") { b.role = Some("manager".into()); }
                    if role == "manager" && b.id.as_deref() == Some("adv2") { b.role = Some("advisor".into()); }
                }
                Ok(())
            }).await.unwrap();
        }

        // 被刪 → 清掉。
        crate::projection::update_and_project(&store, &pool, |cfg| { cfg.projects[0].deputy = Some(DeputyCfg { bot_id: "adv".into(), after_sec: 90 }); Ok(()) }).await.unwrap();
        crate::projection::update_and_project(&store, &pool, |cfg| { cfg.projects[0].bots.retain(|b| b.id.as_deref() != Some("adv")); Ok(()) }).await.unwrap();
        assert_eq!(col(&pool).await, (None, None), "bot 被刪");

        // 離開專家模式 → 清掉。
        crate::projection::update_and_project(&store, &pool, |cfg| { cfg.projects[0].deputy = Some(DeputyCfg { bot_id: "adv2".into(), after_sec: 90 }); Ok(()) }).await.unwrap();
        crate::projection::update_and_project(&store, &pool, |cfg| { cfg.projects[0].mode = "parent_child".into(); Ok(()) }).await.unwrap();
        assert_eq!(col(&pool).await, (None, None), "專案不再是專家模式");
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
