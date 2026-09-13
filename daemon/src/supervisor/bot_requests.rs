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

/// 沒帶 request id 時的去重鍵：寄件者 + 內容雜湊 + 十分鐘一格。
///
/// 規格裡「兩個一樣的要求就是兩個要求」說的是**使用者**的話（`supervisor_requests`）。bot 的
/// 重送多半是逾時重試或 shim 重跑，一字不差又在同一格裡的，當成同一件。
pub fn event_key(from: &str, crid: Option<&str>, text: &str, now_unix: i64) -> String {
    match crid.map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => format!("bot_request:{from}:crid:{c}"),
        None => {
            let norm: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("bot_request:{from}:text:{}:{}", super::persona::hash(&norm), now_unix.div_euclid(DEDUPE_BUCKET_SECS))
        }
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
    if from_bot_id == crate::agent_relay::DAEMON_SENDER || roles::responder_bot(&app.db).await.map_err(up)?.is_none() {
        return Ok(None);
    }
    let Some(target) = roles::role_of_bot(&app.db, target_bot_id).await.map_err(up)? else { return Ok(None) };
    let sender = roles::role_of_bot(&app.db, from_bot_id).await.map_err(up)?;
    let Some(to) = recipient(target, sender) else { return Ok(None) };
    let to_bot = roles::bot_for(&app.db, to).await.map_err(up)?;
    let quiet = quiet_reason(app, from_bot_id, to_bot.as_deref()).await?;
    let from_bot = crate::db::bot(&app.db, from_bot_id).await.map_err(up)?;
    let key = event_key(from_bot_id, client_request_id, text, chrono::Utc::now().timestamp());
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
            let _ = roles::count_duplicate(&app.db, to).await;
            let id: String = sqlx::query_scalar("SELECT id FROM supervisor_inbox WHERE supervisor_id=? AND event_key=?")
                .bind(store::SUPERVISOR_ID)
                .bind(&key)
                .fetch_one(&app.db)
                .await
                .map_err(up)?;
            (id, true)
        }
    };
    let state: String = sqlx::query_scalar("SELECT state FROM supervisor_inbox WHERE id=?").bind(&id).fetch_one(&app.db).await.map_err(up)?;
    if !duplicate {
        app.emit("supervisor_changed", json!({"bot_request": id, "to_role": to.as_str()})).await;
    }
    tracing::info!(from = from_bot_id, to = to.as_str(), duplicate, quiet = ?quiet, via, "bot request queued for AGM");
    Ok(Some(json!({
        "routed": to.as_str(),
        "queued": true,
        "duplicate": duplicate,
        "wake": quiet.is_none(),
        "inbox_event_id": id,
        "state": state,
        // 與 prompt 的回應同形：沒有 turn，因為這一刻沒有任何模型被叫醒。
        "delivery": "queued",
        "turn_id": null,
        "message_id": null,
        "note": "已排入 AGM 協調佇列；有 pending 的申請就等裁示或用 inbox 查狀態，不要重複催問",
    })))
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
    fn a_request_id_is_the_key_and_a_bare_resend_is_folded_inside_ten_minutes() {
        assert_eq!(event_key("b1", Some("r-1"), "x", 0), event_key("b1", Some(" r-1 "), "totally different", 999_999));
        assert_ne!(event_key("b1", Some("r-1"), "x", 0), event_key("b2", Some("r-1"), "x", 0), "不同寄件者不互相吃掉");
        let t = 1_757_750_400; // 某個十分鐘格子的開頭
        assert_eq!(event_key("b1", None, "請核准  重建\nabc", t), event_key("b1", None, "請核准 重建 abc", t + 599));
        assert_ne!(event_key("b1", None, "請核准重建", t), event_key("b1", None, "請核准重建", t + 600));
        assert_ne!(event_key("b1", None, "請核准重建 A", t), event_key("b1", None, "請核准重建 B", t));
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
