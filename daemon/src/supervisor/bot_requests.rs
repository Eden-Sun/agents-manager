//! Bot 對 AGM 說話的入口：在 LLM 看到之前就依來源分派（SPEC §18.15）。
//!
//! 兩條路會進來：`POST /api/bots/{AGM}/prompt` 帶 `relay_from=<bot>`，以及 pane 裡的
//! `herdr agent prompt AGM …`（shim 先報 `/relay/announce`）。協調者建立之後，這兩條都**不再**
//! 直接打進巡檢的 pane——那等於每個 bot 申請先燒一輪 fable——而是寫成一筆 `bot_request`
//! inbox 事件，由協調者成批處理。
//!
//! 不攔的：使用者（沒有 `relay_from`）、daemon 自己（`relay_from=daemon`）、協調者還沒建立的
//! 舊部署、以及目標不是兩個角色 bot 的一般 prompt。

use crate::lifecycle::LcError;
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;

use super::roles::{self, Role};
use super::store;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// 同一個 bot 在這段時間內送出一字不差的內容、又沒帶 request id，就當成重送。
const DEDUPE_BUCKET_SECS: i64 = 600;

/// 收件角色。`None` = 不攔。
///
/// * 一般 bot → 任一個角色 bot：協調者。
/// * 巡檢 → 協調者、協調者 → 巡檢：對方（角色之間的交接）。
/// * 角色對自己：不攔（沒有意義，也不該變成自己叫醒自己）。
pub fn recipient(target: Role, sender: Option<Role>) -> Option<Role> {
    match sender {
        None => Some(Role::Responder),
        Some(s) if s == target => None,
        Some(_) => Some(target),
    }
}

/// 一個請求的內容指紋：收件角色、目標、正文、附件。`client_request_id` 只是**重試用的名字**，
/// 名字相同而內容不同就不是同一個請求。
///
/// 正文逐字納入（不做空白正規化）：程式碼片段、diff、縮排差一格都是不同的內容，把它們折成同一
/// 個指紋會讓第二次申請被當成重播而靜靜消失。
pub fn fingerprint(to: Role, target_bot_id: &str, text: &str, attachments: &[String]) -> String {
    let canon = json!({"to": to.as_str(), "target": target_bot_id, "text": text, "attachments": attachments});
    super::persona::hash(&canon.to_string())
}

/// 去重鍵。有 `client_request_id` 就用它（重試沿用同一個 id 是呼叫端說了算）；沒有的話用
/// 寄件者＋內容指紋＋十分鐘一格，讓逾時重跑的 shim 不會變成兩件事。
pub fn event_key(from: &str, crid: Option<&str>, fingerprint: &str, now_unix: i64) -> String {
    match crid.map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => format!("bot_request:{from}:crid:{c}"),
        None => format!("bot_request:{from}:fp:{fingerprint}:{}", now_unix.div_euclid(DEDUPE_BUCKET_SECS)),
    }
}

/// 為什麼這一筆不叫醒人（`None` = 要叫醒）。只看 daemon 自己的紀錄，不看字面。
///
/// * 寄件者現在跑的回合是一件**通知型交辦**（`--notice`）：這是它對通知的回覆（「收到」）。
/// * 寄件者現在跑的回合是一次喚醒（digest），而那批事件裡有收件角色送來的東西：這是角色之間的
///   來回，不再往回叫醒對方。
async fn quiet_reason(app: &Arc<App>, from: &str, to_bot: Option<&str>) -> Result<Option<&'static str>, LcError> {
    let Some(run) = crate::db::active_run(&app.db, from).await.map_err(up)? else { return Ok(None) };
    let Some(turn) = crate::db::in_flight_turn(&app.db, &run.id).await.map_err(up)? else { return Ok(None) };
    if let Some(a) = store::assignment_by_turn(&app.db, &turn.id).await.map_err(up)? {
        if a.is_notice() {
            return Ok(Some("reply_to_notice"));
        }
    }
    if let Some(to) = to_bot {
        let echoed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM supervisor_inbox WHERE notify_turn_id=? AND (bot_id=? OR json_extract(payload_json,'$.from_bot_id')=?)",
        )
        .bind(&turn.id)
        .bind(to)
        .bind(to)
        .fetch_one(&app.db)
        .await
        .map_err(up)?;
        if echoed > 0 {
            return Ok(Some("reply_between_roles"));
        }
    }
    Ok(None)
}

/// `X-AM-Bot-Token` 是寄件 bot 自己的 hook token 才算驗證過。沒有也照收，只是記下來。
pub async fn sender_verified(app: &Arc<App>, token: Option<&str>, from: &str) -> bool {
    let Some(t) = token.filter(|t| !t.is_empty()) else { return false };
    matches!(crate::db::bot(&app.db, from).await, Ok(Some(b)) if b.deleted_at.is_none() && b.hook_token == t)
}

/// 呼叫端是哪個 AGM 角色：`X-AM-Bot-Id` + `X-AM-Bot-Token`（那顆 bot 自己的 hook token）都對得上
/// 才算。CLI 在角色的 pane 裡跑，環境裡本來就有這兩個值；模型打出來的字串冒充不了。
/// 沒帶、對不上、或那顆 bot 不是角色 bot → `None`，照 UI／使用者的權限處理（跟以前一樣）。
pub async fn actor_role(app: &Arc<App>, headers: &axum::http::HeaderMap) -> Option<Role> {
    let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let (id, token) = (get("X-AM-Bot-Id")?, get("X-AM-Bot-Token"));
    if !sender_verified(app, token, id).await {
        return None;
    }
    roles::role_of_bot(&app.db, id).await.ok().flatten()
}

/// 這一句要不要攔下來排進 inbox。`Ok(None)` = 照原本的路送。
#[allow(clippy::too_many_arguments)]
pub async fn intercept(
    app: &Arc<App>,
    target_bot_id: &str,
    from_bot_id: &str,
    text: &str,
    client_request_id: Option<&str>,
    attachments: &[String],
    verified: bool,
    via: &str,
) -> Result<Option<Value>, LcError> {
    // 協調者「建立過」就一直攔：它停了、沒額度、bot 被刪掉，申請都排進它的佇列等，
    // 不會因為它不在就倒回巡檢（SPEC §18.15）。
    if from_bot_id == crate::agent_relay::DAEMON_SENDER || !roles::responder_configured(&app.db).await.map_err(up)? {
        return Ok(None);
    }
    let Some(target) = roles::role_of_bot(&app.db, target_bot_id).await.map_err(up)? else { return Ok(None) };
    let sender = roles::role_of_bot(&app.db, from_bot_id).await.map_err(up)?;
    let Some(to) = recipient(target, sender) else { return Ok(None) };
    Ok(Some(queue(app, to, from_bot_id, target_bot_id, text, client_request_id, attachments, verified, via).await?))
}

/// 寫一筆 `bot_request` 進收件角色的佇列。`intercept`（prompt／shim）與角色之間的交接
/// （`supervisor::assign` 的角色目標）共用這一條路，所以批次、節流、quiet 規則只有一份。
#[allow(clippy::too_many_arguments)]
pub async fn queue(
    app: &Arc<App>,
    to: Role,
    from_bot_id: &str,
    target_bot_id: &str,
    text: &str,
    client_request_id: Option<&str>,
    attachments: &[String],
    verified: bool,
    via: &str,
) -> Result<Value, LcError> {
    let to_bot = roles::bot_for(&app.db, to).await.map_err(up)?;
    let quiet = quiet_reason(app, from_bot_id, to_bot.as_deref()).await?;
    let from_bot = crate::db::bot(&app.db, from_bot_id).await.map_err(up)?;
    let sender = roles::role_of_bot(&app.db, from_bot_id).await.map_err(up)?;
    let fp = fingerprint(to, target_bot_id, text, attachments);
    let key = event_key(from_bot_id, client_request_id, &fp, chrono::Utc::now().timestamp());
    let payload = json!({
        "to_role": to.as_str(),
        "wake": quiet.is_none(),
        "quiet_reason": quiet,
        "from_bot_id": from_bot_id,
        "from_name": from_bot.as_ref().map(|b| b.name.clone()),
        "from_role": sender.map(Role::as_str),
        "target_bot_id": target_bot_id,
        "text": text,
        "client_request_id": client_request_id,
        "attachments": attachments,
        "fingerprint": fp,
        "sender_verified": verified,
        "via": via,
    });
    let new_id = store::push_inbox(&app.db, &key, "bot_request", None, Some(from_bot_id), None, &payload).await.map_err(up)?;
    let (id, duplicate) = match new_id {
        Some(id) => {
            // 寫入就分類，不等下一個 tick：去重與角色從這一刻起就是確定的。
            let rt = roles::route("bot_request", &payload, None);
            sqlx::query("UPDATE supervisor_inbox SET role=?, wake=? WHERE id=?")
                .bind(rt.role.as_str())
                .bind(i64::from(rt.wake))
                .bind(&id)
                .execute(&app.db)
                .await
                .map_err(up)?;
            (id, false)
        }
        None => {
            // 這個 key 已經有一筆了。只有**內容也一樣**才算重播；同一個 request id 換一段話會被
            // 讀成「我的新申請送到了」，但它其實沒有進任何人的佇列。
            let (id, existing): (String, String) =
                sqlx::query_as("SELECT id, payload_json FROM supervisor_inbox WHERE supervisor_id=? AND event_key=?")
                    .bind(store::SUPERVISOR_ID)
                    .bind(&key)
                    .fetch_one(&app.db)
                    .await
                    .map_err(up)?;
            let old: Value = serde_json::from_str(&existing).unwrap_or_else(|_| json!({}));
            let same = old.get("fingerprint").and_then(Value::as_str) == Some(fp.as_str());
            if !same {
                return Err(LcError::conflict(
                    "client_request_id already used for a different request",
                    json!({
                        "reason": "request_mismatch",
                        "inbox_event_id": id,
                        "client_request_id": client_request_id,
                        "existing_fingerprint": old.get("fingerprint"),
                        "fingerprint": fp,
                    }),
                ));
            }
            let _ = roles::count_duplicate(&app.db, to).await;
            (id, true)
        }
    };
    let state: String = sqlx::query_scalar("SELECT state FROM supervisor_inbox WHERE id=?").bind(&id).fetch_one(&app.db).await.map_err(up)?;
    if !duplicate {
        app.emit("supervisor_changed", json!({"bot_request": id, "to_role": to.as_str()})).await;
    }
    tracing::info!(from = from_bot_id, to = to.as_str(), duplicate, quiet = ?quiet, via, "bot request queued for AGM");
    Ok(json!({
        "routed": to.as_str(),
        "queued": true,
        "duplicate": duplicate,
        "wake": quiet.is_none(),
        "inbox_event_id": id,
        "state": state,
        "fingerprint": fp,
        // 與 prompt 的回應同形：沒有 turn，因為這一刻沒有任何模型被叫醒。
        "delivery": "queued",
        "turn_id": null,
        "message_id": null,
        "note": "已排入 AGM 協調佇列；有 pending 的申請就等裁示或用 inbox 查狀態，不要重複催問",
    }))
}

/// shim 的 `to_agent` 是 herdr 的 agent 名字（或原名、pane id）：對回兩個角色 bot 之一。
pub async fn role_bot_by_agent(app: &Arc<App>, agent: &str) -> Result<Option<String>, LcError> {
    let agent = agent.trim();
    if agent.is_empty() {
        return Ok(None);
    }
    for role in [Role::Patrol, Role::Responder] {
        let Some(id) = roles::bot_for(&app.db, role).await.map_err(up)? else { continue };
        let Some(bot) = crate::db::bot(&app.db, &id).await.map_err(up)? else { continue };
        let run = crate::db::active_run(&app.db, &id).await.map_err(up)?;
        let mut names = vec![bot.name.clone(), crate::db::agent_name_for_bot(&app.db, &bot).await.map_err(up)?];
        if let Some(r) = run {
            names.push(crate::db::run_target(&r, &bot));
            if let Some(p) = r.pane_id {
                names.push(p);
            }
        }
        if names.iter().any(|n| n == agent) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bots_go_to_the_responder_and_roles_hand_over_to_each_other() {
        assert_eq!(recipient(Role::Patrol, None), Some(Role::Responder), "一般 bot 找 AGM：不先燒巡檢");
        assert_eq!(recipient(Role::Responder, None), Some(Role::Responder));
        assert_eq!(recipient(Role::Responder, Some(Role::Patrol)), Some(Role::Responder));
        assert_eq!(recipient(Role::Patrol, Some(Role::Responder)), Some(Role::Patrol));
        assert_eq!(recipient(Role::Patrol, Some(Role::Patrol)), None);
    }

    #[test]
    fn a_request_id_names_a_retry_and_the_content_still_has_to_match() {
        let fp = |text: &str| fingerprint(Role::Responder, "agm", text, &[]);
        // 同一個 request id 就是同一個鍵（重試沿用它）——但內容指紋不同，`queue` 會擋下來。
        assert_eq!(event_key("b1", Some("r-1"), &fp("x"), 0), event_key("b1", Some(" r-1 "), &fp("y"), 999_999));
        assert_ne!(fp("x"), fp("y"));
        assert_ne!(event_key("b1", Some("r-1"), &fp("x"), 0), event_key("b2", Some("r-1"), &fp("x"), 0), "不同寄件者不互相吃掉");
        // 沒帶 id：同寄件者、同內容、同一個十分鐘格子算一件。
        let t = 1_757_750_400;
        assert_eq!(event_key("b1", None, &fp("請核准重建"), t), event_key("b1", None, &fp("請核准重建"), t + 599));
        assert_ne!(event_key("b1", None, &fp("請核准重建"), t), event_key("b1", None, &fp("請核准重建"), t + 600));
    }

    /// 縮排、換行、附件、收件角色、目標都算內容。把它們折掉會讓第二次申請被當成重播而消失。
    #[test]
    fn the_fingerprint_keeps_whitespace_attachments_and_the_recipient() {
        let base = fingerprint(Role::Responder, "agm", "fn main() {\n    let x = 1;\n}", &[]);
        assert_ne!(base, fingerprint(Role::Responder, "agm", "fn main() {\n  let x = 1;\n}", &[]), "縮排差一格是不同的內容");
        assert_ne!(base, fingerprint(Role::Responder, "agm", "fn main() { let x = 1; }", &[]));
        assert_ne!(base, fingerprint(Role::Patrol, "agm", "fn main() {\n    let x = 1;\n}", &[]));
        assert_ne!(base, fingerprint(Role::Responder, "other", "fn main() {\n    let x = 1;\n}", &[]));
        assert_ne!(base, fingerprint(Role::Responder, "agm", "fn main() {\n    let x = 1;\n}", &["a1".into()]));
        assert_eq!(base, fingerprint(Role::Responder, "agm", "fn main() {\n    let x = 1;\n}", &[]));
    }
}

/// 整條路：真的 App、真的 DB，驗「fable 沒被叫醒」與去重、迴圈、額度。
#[cfg(test)]
pub(crate) mod flow_tests {
    use super::*;

    pub(crate) async fn app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("agm-bot-requests-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("test.sqlite")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        let app = App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false);
        store::get_or_init(&app.db).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(crate::db::now()).execute(&app.db).await.unwrap();
        for (id, name) in [("patrol", "AGM"), ("resp", "AGM-responder"), ("w1", "fixer"), ("w2", "builder")] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,identity,hook_token,created_at) VALUES (?,'p',?,'claude','cc0',?,?)")
                .bind(id)
                .bind(name)
                .bind(format!("tok-{id}"))
                .bind(crate::db::now())
                .execute(&app.db)
                .await
                .unwrap();
        }
        store::set_env(&app.db, "patrol", "p", "/tmp").await.unwrap();
        app
    }

    pub(crate) async fn configure_responder(app: &Arc<App>) {
        roles::set_env(&app.db, Role::Responder, "resp", "p", "/tmp").await.unwrap();
    }

    async fn inbox(app: &Arc<App>) -> Vec<(String, String, Option<String>, Option<i64>)> {
        sqlx::query_as("SELECT kind, event_key, role, wake FROM supervisor_inbox ORDER BY created_at, id").fetch_all(&app.db).await.unwrap()
    }

    #[tokio::test]
    async fn without_a_responder_nothing_is_intercepted() {
        let app = app().await;
        assert!(intercept(&app, "patrol", "w1", "請核准", Some("r1"), &[], true, "api").await.unwrap().is_none());
        assert!(inbox(&app).await.is_empty());
    }

    #[tokio::test]
    async fn a_hundred_identical_requests_are_one_event_and_never_touch_patrol() {
        let app = app().await;
        configure_responder(&app).await;
        let mut ids = std::collections::HashSet::new();
        for _ in 0..100 {
            let out = intercept(&app, "patrol", "w1", "請核准重建 abc123", Some("rebuild-abc123"), &[], true, "api").await.unwrap().unwrap();
            assert_eq!(out["routed"], "responder");
            ids.insert(out["inbox_event_id"].as_str().unwrap().to_string());
        }
        assert_eq!(ids.len(), 1, "同一個 request id 只會有一筆");
        let rows = inbox(&app).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2.as_deref(), Some("responder"));
        assert_eq!(roles::get(&app.db, Role::Responder).await.unwrap().duplicates, 99);
        // 巡檢這邊：沒有任何它該被叫醒的事件，也沒有任何 turn。
        assert!(roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap().is_empty());
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 0, "攔下來的申請不會開任何回合");
        assert_eq!(roles::get(&app.db, Role::Patrol).await.unwrap().wakes, 0);
    }

    #[tokio::test]
    async fn users_daemon_and_ordinary_targets_are_left_alone() {
        let app = app().await;
        configure_responder(&app).await;
        assert!(intercept(&app, "patrol", crate::agent_relay::DAEMON_SENDER, "tick", None, &[], false, "api").await.unwrap().is_none());
        assert!(intercept(&app, "w2", "w1", "幫我看一下", None, &[], true, "api").await.unwrap().is_none(), "bot 對一般 bot 不關 AGM 的事");
        assert!(intercept(&app, "patrol", "patrol", "自言自語", None, &[], true, "api").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn roles_can_hand_over_but_a_reply_to_a_notice_does_not_wake_anyone() {
        let app = app().await;
        configure_responder(&app).await;
        let out = intercept(&app, "resp", "patrol", "巡檢發現 builder 卡住", Some("p-1"), &[], true, "api").await.unwrap().unwrap();
        assert_eq!(out["routed"], "responder");
        assert_eq!(out["wake"], true);

        // w1 正在跑一件協調者發給它的通知；它在那個回合裡回「收到」。
        let now = crate::db::now();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('run-w1','w1','running','working',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('c-w1','w1',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,created_at) VALUES ('t-w1','c-w1','run-w1','web','in_flight',?)").bind(&now).execute(&app.db).await.unwrap();
        let a = store::insert_assignment(&app.db, None, "w1", "note-1", "核准，去做", &[], None, false).await.unwrap();
        store::mark_delivered(&app.db, &a.id, "t-w1", "ok").await.unwrap();
        let out = intercept(&app, "resp", "w1", "收到，開始做", None, &[], true, "shim").await.unwrap().unwrap();
        assert_eq!(out["wake"], false, "對通知的回覆只記錄");
        assert!(
            roles::due_for(&app.db, Role::Responder, true, "2999-01-01T00:00:00Z", 0).await.unwrap().iter().filter(|e| e.wake == Some(1)).count() == 1,
            "只有巡檢那一筆會叫醒協調者"
        );
    }

    /// 協調者被一批含巡檢交接的事件叫醒，在那個回合裡回巡檢一句：記下來，但不叫醒巡檢，
    /// 否則兩個角色會互相請示到額度用完。
    async fn turn_for(app: &Arc<App>, bot: &str, run: &str, conv: &str, turn: &str) {
        let now = crate::db::now();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES (?,?,'running','working',?)")
            .bind(run).bind(bot).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES (?,?,?)")
            .bind(conv).bind(bot).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,created_at) VALUES (?,?,?,'web','in_flight',?)")
            .bind(turn).bind(conv).bind(run).bind(&now).execute(&app.db).await.unwrap();
    }

    /// 端到端：巡檢用 `assign`（persona 教的那條路）交接給協調者 → 進協調者的佇列，不是一件
    /// 交辦、也不開任何回合；協調者在收到那一批的回合裡回一句 → 只記錄，不再叫醒巡檢。
    #[tokio::test]
    async fn an_assignment_addressed_to_a_role_goes_through_the_queue_and_the_reply_wakes_nobody() {
        let app = app().await;
        configure_responder(&app).await;
        let out = crate::supervisor::assign(
            &app, "resp", "交接：builder 卡在 rebase，請你接手協調", "handover-1", None, &[], None, false, None, None, Some(Role::Patrol),
        )
        .await
        .unwrap();
        assert_eq!(out["kind"], "handover");
        assert_eq!(out["routed"], "responder");
        assert_eq!(out["wake"], true);
        assert_eq!(out["delivery"], "queued");
        assert!(out["turn_id"].is_null());
        let assignments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_assignments").fetch_one(&app.db).await.unwrap();
        assert_eq!(assignments, 0, "交接不是交辦，不會留下要驗收的工作");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 0, "沒有任何模型被叫醒");
        let ev = roles::due_for(&app.db, Role::Responder, true, "2999-01-01T00:00:00Z", 0).await.unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].wake, Some(1));
        let payload: serde_json::Value = serde_json::from_str(&ev[0].payload_json).unwrap();
        assert_eq!(payload["from_bot_id"], "patrol");
        assert_eq!(payload["via"], "assignment");

        // 協調者被那一批叫醒；它在同一個回合裡回巡檢一句「收到，我接手」。
        turn_for(&app, "resp", "run-r", "c-r", "t-r").await;
        roles::mark_delivered(&app.db, &[ev[0].id.clone()], Role::Responder, "t-r", "ok").await.unwrap();
        let back = crate::supervisor::assign(
            &app, "patrol", "收到，我接手 builder", "handover-1-ack", None, &[], None, false, None, None, Some(Role::Responder),
        )
        .await
        .unwrap();
        assert_eq!(back["routed"], "patrol");
        assert_eq!(back["wake"], false, "回信只記錄，不叫醒巡檢");
        let patrol_due = roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap();
        assert!(patrol_due.iter().all(|e| e.wake == Some(0)));

        // 對自己交辦仍然是錯的。
        let err = crate::supervisor::assign(
            &app, "resp", "自言自語", "self-1", None, &[], None, false, None, None, Some(Role::Responder),
        )
        .await;
        assert!(err.is_err());
    }

    /// 同一個 request id 換一段話**不是**重播：回 409 而不是假裝送到了（那會讓申請靜靜消失）。
    #[tokio::test]
    async fn the_same_request_id_with_different_content_is_refused_not_swallowed() {
        let app = app().await;
        configure_responder(&app).await;
        let first = intercept(&app, "patrol", "w1", "請核准重建 abc123", Some("r-1"), &[], true, "api").await.unwrap().unwrap();
        let again = intercept(&app, "patrol", "w1", "請核准重建 abc123", Some("r-1"), &[], true, "api").await.unwrap().unwrap();
        assert_eq!(again["duplicate"], true);
        assert_eq!(again["inbox_event_id"], first["inbox_event_id"]);

        let err = intercept(&app, "patrol", "w1", "請核准重建 def456", Some("r-1"), &[], true, "api").await.unwrap_err();
        match err {
            LcError::Conflict(v) => {
                assert_eq!(v["reason"], "request_mismatch");
                assert_eq!(v["inbox_event_id"], first["inbox_event_id"]);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox").fetch_one(&app.db).await.unwrap();
        assert_eq!(rows, 1, "被擋下來的那一筆不會偷偷寫進去");
        // 附件也算內容。
        let err = intercept(&app, "patrol", "w1", "請核准重建 abc123", Some("r-1"), &["img-1".into()], true, "api").await;
        assert!(err.is_err());
    }

    /// 協調者的 bot 被刪掉之後，申請仍然排給它（不倒回巡檢），狀態說得出是 `missing`，
    /// 並且巡檢會收到一則「它不見了」。
    #[tokio::test]
    async fn a_deleted_responder_bot_does_not_send_its_queue_back_to_patrol() {
        let app = app().await;
        configure_responder(&app).await;
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id='resp'").bind(crate::db::now()).execute(&app.db).await.unwrap();

        let out = intercept(&app, "patrol", "w1", "請協調 ownership", Some("r-9"), &[], true, "api").await.unwrap().unwrap();
        assert_eq!(out["routed"], "responder");
        assert!(roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap().is_empty(), "不倒回巡檢");
        assert_eq!(roles::due_for(&app.db, Role::Responder, true, "2999-01-01T00:00:00Z", 0).await.unwrap().len(), 1);

        crate::supervisor::responder::notify(&app).await;
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM supervisor_inbox ORDER BY created_at").fetch_all(&app.db).await.unwrap();
        assert!(kinds.contains(&"responder_bot_missing".to_string()), "{kinds:?}");
        let status = crate::supervisor::responder::status_json(&app).await.unwrap();
        assert_eq!(status["status"], "missing");
        assert_eq!(status["configured"], true);
        assert_eq!(status["bot_present"], false);
        // 再跑一次不會重複報。
        crate::supervisor::responder::notify(&app).await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='responder_bot_missing'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 1);
    }

    /// 雙角色啟用前由巡檢收走、還沒 ack 的協調事件：啟用後仍然是巡檢的，協調者不會也拿去送
    /// （那會變成同一則事件兩邊各送一次，而且協調者永遠標不成 delivered）。
    #[tokio::test]
    async fn an_event_already_claimed_by_patrol_stays_with_patrol_after_the_switch() {
        let app = app().await;
        let id = store::push_inbox(&app.db, "approval:A1:requested", "approval_requested", None, None, None, &json!({})).await.unwrap().unwrap();
        roles::classify(&app.db).await.unwrap();
        // 單角色時代：巡檢收走了它，但還沒 ack（通知回合失敗 → recover 放回 pending）。
        roles::mark_delivered(&app.db, &[id.clone()], Role::Patrol, "t-old", "ok").await.unwrap();
        assert!(store::requeue_inbox(&app.db, &id, "notify turn did not complete").await.unwrap());
        configure_responder(&app).await;

        assert!(roles::due_for(&app.db, Role::Responder, true, "2999-01-01T00:00:00Z", 0).await.unwrap().is_empty(), "協調者不會撿走它");
        let patrol = roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap();
        assert_eq!(patrol.len(), 1, "還是巡檢的");
        assert_eq!(roles::mark_delivered(&app.db, &[id.clone()], Role::Responder, "t-new", "ok").await.unwrap(), 0);
        assert_eq!(roles::ack(&app.db, &id, Some(Role::Responder), true).await.unwrap(), roles::AckOutcome::ClaimedByOther("patrol".into()));
        assert_eq!(roles::ack(&app.db, &id, Some(Role::Patrol), true).await.unwrap(), roles::AckOutcome::Acked);
    }

    /// inbox 清單的角色條件要在 SQL 的 LIMIT 之前：最舊的一整頁都是別人的事件時，自己的待辦
    /// 還是要翻得到。
    #[tokio::test]
    async fn the_inbox_filter_happens_before_the_limit() {
        let app = app().await;
        configure_responder(&app).await;
        for i in 0..220 {
            store::push_inbox(&app.db, &format!("incident:I{i}:opened"), "incident_opened", None, None, None, &json!({})).await.unwrap();
        }
        let mine = store::push_inbox(&app.db, "approval:A1:requested", "approval_requested", None, None, None, &json!({})).await.unwrap().unwrap();
        roles::classify(&app.db).await.unwrap();

        let listed = roles::list_for(&app.db, Some(Role::Responder), false, 200).await.unwrap();
        assert_eq!(listed.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec![mine.clone()]);
        let audit = roles::list_for(&app.db, Some(Role::Responder), true, 200).await.unwrap();
        assert_eq!(audit.len(), 1, "--all 也一樣");
        let patrol = roles::list_for(&app.db, Some(Role::Patrol), false, 200).await.unwrap();
        assert_eq!(patrol.len(), 200);
        assert!(patrol.iter().all(|e| e.kind == "incident_opened"));
        assert_eq!(roles::list_for(&app.db, None, false, 1000).await.unwrap().len(), 221);
    }

    #[tokio::test]
    async fn a_reply_between_roles_inside_the_wake_that_carried_it_does_not_ping_back() {
        let app = app().await;
        configure_responder(&app).await;
        let handover = intercept(&app, "resp", "patrol", "巡檢：builder 卡住三小時", Some("p-2"), &[], true, "api").await.unwrap().unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('run-r','resp','running','working',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('c-r','resp',?)").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,created_at) VALUES ('t-r','c-r','run-r','web','in_flight',?)").bind(&now).execute(&app.db).await.unwrap();
        roles::mark_delivered(&app.db, &[handover["inbox_event_id"].as_str().unwrap().to_string()], Role::Responder, "t-r", "ok").await.unwrap();
        let reply = intercept(&app, "patrol", "resp", "收到，已派 builder 續作", None, &[], true, "api").await.unwrap().unwrap();
        assert_eq!(reply["routed"], "patrol");
        assert_eq!(reply["wake"], false);
        let due = roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap();
        assert!(due.iter().all(|e| e.wake == Some(0)), "巡檢不會因為這句被叫醒");
    }
}
