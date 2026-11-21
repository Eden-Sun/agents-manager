//! 承諾帳本 API（T2）：管理入口收件 → 清單出現 `untriaged`、訊息清單附掛、版本不符 409、權限（執行者／child 只讀、跨專案拒絕）。
use super::*;
use crate::testing::{claude_bot, env};
use am_base::ledger::ingest_if_management_message;

struct Cast {
    /// 留著：`Env` 掉了會刪掉測試資料夾（DB 檔也在裡面）。
    _env: crate::testing::Env,
    app: Arc<App>,
    pid: String,
    manager: db::Bot,
    executor: db::Bot,
    outsider: db::Bot,
}

async fn cast() -> Cast {
    let env = env().await;
    let app = env.app.clone();
    let pid = env.project_id.clone();
    let manager = claude_bot(&app, &pid, "led-manager").await;
    let executor = claude_bot(&app, &pid, "led-exec").await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&pid).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&executor.id).execute(&app.db).await.unwrap();
    let other = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at,mode) VALUES(?,?,'other','local',?,'expert')")
        .bind(&other)
        .bind(format!("/tmp/{other}"))
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let outsider = claude_bot(&app, &other, "led-outsider").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&outsider.id).execute(&app.db).await.unwrap();
    Cast { _env: env, app, pid, manager, executor, outsider }
}

fn bot(b: &db::Bot) -> Extension<RequestPrincipal> {
    Extension(RequestPrincipal::Bot(b.id.clone()))
}

/// 在這顆 bot 的對話裡放一則使用者訊息（`messages` 需要 conversation）。
async fn user_message(app: &Arc<App>, bot_id: &str, text: &str) -> String {
    let now = db::now();
    sqlx::query("INSERT OR IGNORE INTO conversations(id,bot_id,created_at) VALUES(?,?,?)").bind(format!("conv-{bot_id}")).bind(bot_id).bind(&now).execute(&app.db).await.unwrap();
    let id = db::ulid();
    sqlx::query("INSERT INTO messages(id,conversation_id,role,content,source,created_at) VALUES(?,?,'user',?,'web',?)")
        .bind(&id)
        .bind(format!("conv-{bot_id}"))
        .bind(text)
        .bind(&now)
        .execute(&app.db)
        .await
        .unwrap();
    id
}

fn reason(e: &LcError) -> String {
    match e {
        LcError::Forbidden(v) | LcError::Conflict(v) | LcError::BadValue(v) | LcError::NotFoundValue(v) => v["reason"].as_str().or(v["error"].as_str()).unwrap_or_default().to_string(),
        other => format!("{other:?}"),
    }
}

async fn list_items(c: &Cast, who: Extension<RequestPrincipal>, q: ListQuery) -> Result<Value, LcError> {
    let Json(v) = list(State(c.app.clone()), Path(c.pid.clone()), Query(q), who).await?;
    Ok(v)
}

#[tokio::test]
async fn a_management_message_becomes_an_untriaged_item_visible_to_the_user_and_attached_to_its_bubble() {
    let c = cast().await;
    let m1 = user_message(&c.app, &c.manager.id, "請查 CI 失敗，順便更新文件").await;
    let got = ingest_if_management_message(&c.app.db, &c.manager.id, &m1, None, "請查 CI 失敗，順便更新文件").await.unwrap();
    assert_eq!(got.map(|i| i.code_no), Some(1));
    // 執行者收到的同類訊息不入帳。
    let m2 = user_message(&c.app, &c.executor.id, "做這個").await;
    assert!(ingest_if_management_message(&c.app.db, &c.executor.id, &m2, None, "做這個").await.unwrap().is_none());

    let v = list_items(&c, Extension(RequestPrincipal::User), ListQuery::default()).await.unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!((items[0]["code"].as_str(), items[0]["status"].as_str(), items[0]["version"].as_i64()), (Some("L1"), Some("untriaged"), Some(1)));
    assert_eq!(v["counts"]["untriaged"].as_i64(), Some(1));

    let mut msgs = vec![json!({"id": m1}), json!({"id": m2})];
    attach_ledger_items_to_messages(&c.app.db, &mut msgs).await.unwrap();
    assert_eq!(msgs[0]["ledger_items"][0]["code"].as_str(), Some("L1"), "the bubble carries its item");
    assert!(msgs[1].get("ledger_items").is_none(), "a message without items gets no field");
}

#[tokio::test]
async fn patch_is_version_checked_and_short_codes_resolve() {
    let c = cast().await;
    let m1 = user_message(&c.app, &c.manager.id, "x").await;
    ingest_if_management_message(&c.app.db, &c.manager.id, &m1, None, "x").await.unwrap();
    let user = Extension(RequestPrincipal::User);

    let stale = patch(State(c.app.clone()), Path((c.pid.clone(), "L1".into())), user.clone(), Json(PatchIn { op: Some("claim".into()), expected_version: 7, owner_bot_id: Some(c.manager.id.clone()), ..Default::default() })).await.unwrap_err();
    assert!(matches!(&stale, LcError::Conflict(v) if v["error"] == "ledger_version_conflict" && v["current_version"] == 1), "{stale:?}");

    let Json(claimed) = patch(State(c.app.clone()), Path((c.pid.clone(), "L1".into())), user.clone(), Json(PatchIn { op: Some("claim".into()), expected_version: 1, owner_bot_id: Some(c.manager.id.clone()), ..Default::default() })).await.unwrap();
    assert_eq!((claimed["version"].as_i64(), claimed["owner_bot_id"].as_str()), (Some(2), Some(c.manager.id.as_str())));

    let by_exec = patch(State(c.app.clone()), Path((c.pid.clone(), claimed["id"].as_str().unwrap().to_string())), bot(&c.executor), Json(PatchIn { op: Some("cancel".into()), expected_version: 2, ..Default::default() })).await.unwrap_err();
    assert_eq!(reason(&by_exec), "role_required", "{by_exec:?}");
}

#[tokio::test]
async fn only_management_writes_and_other_projects_and_children_only_read() {
    let c = cast().await;
    let who_child = {
        let child = claude_bot(&c.app, &c.pid, "led-child").await;
        sqlx::query("UPDATE bots SET managed_by='child', parent_bot_id=? WHERE id=?").bind(&c.manager.id).bind(&child.id).execute(&c.app.db).await.unwrap();
        Extension(RequestPrincipal::Child { child_id: child.id.clone(), parent_bot_id: c.manager.id.clone() })
    };
    assert!(list_items(&c, who_child.clone(), ListQuery::default()).await.is_ok(), "child may read");
    let denied = create(State(c.app.clone()), Path(c.pid.clone()), who_child, Json(CreateIn { title: Some("x".into()), ..Default::default() })).await.unwrap_err();
    assert_eq!(reason(&denied), "role_required");

    let cross = create(State(c.app.clone()), Path(c.pid.clone()), bot(&c.outsider), Json(CreateIn { title: Some("x".into()), ..Default::default() })).await.unwrap_err();
    assert_eq!(reason(&cross), "cross_project", "{cross:?}");

    let Json(made) = create(State(c.app.clone()), Path(c.pid.clone()), bot(&c.manager), Json(CreateIn { title: Some("追蹤 PR".into()), status: Some("following".into()), ..Default::default() })).await.unwrap();
    assert_eq!((made["items"][0]["code"].as_str(), made["items"][0]["status"].as_str()), (Some("L1"), Some("following")));
    let v = list_items(&c, Extension(RequestPrincipal::User), ListQuery { status: Some("open".into()), owner: Some("unassigned".into()), ..Default::default() }).await.unwrap();
    assert_eq!(v["items"].as_array().unwrap().len(), 1, "a commitment with no owner is in the unassigned queue");
}

#[tokio::test]
async fn awaiting_review_is_visible_to_the_user_but_not_a_manager_todo() {
    let c = cast().await;
    let m1 = user_message(&c.app, &c.manager.id, "請改 CI").await;
    ingest_if_management_message(&c.app.db, &c.manager.id, &m1, None, "請改 CI").await.unwrap();
    let user = Extension(RequestPrincipal::User);
    let Json(done) = patch(State(c.app.clone()), Path((c.pid.clone(), "L1".into())), bot(&c.manager), Json(PatchIn { op: Some("update".into()), expected_version: 1, status: Some("answered".into()), deliverable: Some("CI 已修好".into()), ..Default::default() })).await.unwrap();
    assert_eq!((done["status"].as_str(), done["deliverable"].as_str()), (Some("answered"), Some("CI 已修好")));
    // 使用者的「未結」清單包含待驗收；管理者的待辦（查詢用的摘要集合）不包含。
    let open = list_items(&c, user.clone(), ListQuery { status: Some("open".into()), ..Default::default() }).await.unwrap();
    assert_eq!(open["items"].as_array().unwrap().len(), 1);
    assert_eq!(open["items"][0]["status"].as_str(), Some("answered"));
    assert!(!am_base::ledger::OPEN_STATUSES.contains(&"answered"));
    // 使用者驗收：完成
    let Json(closed) = patch(State(c.app.clone()), Path((c.pid.clone(), "L1".into())), user, Json(PatchIn { op: Some("update".into()), expected_version: 2, status: Some("done".into()), ..Default::default() })).await.unwrap();
    assert_eq!(closed["status"].as_str(), Some("done"));
}
