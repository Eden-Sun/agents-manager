//! session 血統 API（issue #1943 子票 A）：權限（讀＝同專案；寫＝manager／AGM／User）、跨專案拒絕、狀態轉移與錯誤碼。
use super::*;
use crate::session_lineage::{abandon, advance, claim, create, list, retire, ClaimBody, ListQuery, RetireBody, SessionBody};

struct Cast {
    env: crate::testing::Env,
    manager: db::Bot,
    executor: db::Bot,
    other_project: String,
    outsider_manager: db::Bot,
}

async fn cast() -> Cast {
    let env = crate::testing::env().await;
    let manager = crate::testing::claude_bot(&env.app, &env.project_id, "lin-manager").await;
    let executor = crate::testing::claude_bot(&env.app, &env.project_id, "lin-exec").await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&executor.id).execute(&env.app.db).await.unwrap();
    let other_project = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at,mode) VALUES(?,?,'other','local',?,'expert')")
        .bind(&other_project).bind(format!("/tmp/{other_project}")).bind(db::now()).execute(&env.app.db).await.unwrap();
    let outsider_manager = crate::testing::claude_bot(&env.app, &other_project, "lin-outsider").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&outsider_manager.id).execute(&env.app.db).await.unwrap();
    Cast { env, manager, executor, other_project, outsider_manager }
}

fn bot(b: &db::Bot) -> Extension<RequestPrincipal> {
    Extension(RequestPrincipal::Bot(b.id.clone()))
}

fn body(v: Value) -> Json<SessionBody> {
    Json(serde_json::from_value(v).unwrap())
}

fn reason(e: &LcError) -> String {
    match e {
        LcError::Forbidden(v) | LcError::Conflict(v) | LcError::BadValue(v) | LcError::NotFoundValue(v) => v["reason"].as_str().unwrap_or_default().to_string(),
        other => format!("{other:?}"),
    }
}

async fn listed(c: &Cast, project: &str, who: Extension<RequestPrincipal>, q: ListQuery) -> Result<Vec<(String, String, String)>, LcError> {
    let Json(v) = list(State(c.env.app.clone()), Path(project.to_string()), Query(q), who).await?;
    Ok(v["sessions"].as_array().unwrap().iter().map(|s| (s["line"].as_str().unwrap().to_string(), s["session_id"].as_str().unwrap().to_string(), s["state"].as_str().unwrap().to_string())).collect())
}

fn row(line: &str, session: &str, state: &str) -> (String, String, String) {
    (line.to_string(), session.to_string(), state.to_string())
}

#[tokio::test]
async fn manager_registers_advances_and_retires_and_everyone_in_the_project_reads() {
    let c = cast().await;
    let (app, pid) = (c.env.app.clone(), c.env.project_id.clone());
    let Json(first) = advance(State(app.clone()), Path(pid.clone()), bot(&c.manager), body(json!({"line": "web", "session_id": "sess-1", "head_sha": "0B43A28E4", "summary": " seed\n checkpoint "}))).await.unwrap();
    assert_eq!((first["session"]["state"].as_str(), first["session"]["head_sha"].as_str(), first["session"]["summary"].as_str()), (Some("checkpoint"), Some("0b43a28e4"), Some("seed checkpoint")));
    assert_eq!((first["session"]["role"].as_str(), first["session"]["kind"].as_str(), first["session"]["project_id"].as_str()), (Some("worker"), Some("codex"), Some(pid.as_str())));
    assert!(first["retired"].is_null());

    let Json(active) = create(State(app.clone()), Path(pid.clone()), bot(&c.manager), body(json!({"line": "web", "session_id": "sess-2", "parent_session_id": "sess-1", "role": "worker", "kind": "codex", "context_tokens": 208169, "last_api_at": "2026-10-24T01:25:32Z"}))).await.unwrap();
    assert_eq!((active["session"]["state"].as_str(), active["session"]["context_tokens"].as_i64()), (Some("active"), Some(208169)));
    let dup = create(State(app.clone()), Path(pid.clone()), bot(&c.manager), body(json!({"line": "web", "session_id": "sess-2"}))).await.unwrap_err();
    assert!(matches!(dup, LcError::Conflict(_)) && reason(&dup) == "lineage_session_exists", "{dup:?}");

    let Json(moved) = advance(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), body(json!({"line": "web", "session_id": "sess-2"}))).await.unwrap();
    assert_eq!((moved["session"]["state"].as_str(), moved["retired"]["session_id"].as_str(), moved["retired"]["state"].as_str()), (Some("checkpoint"), Some("sess-1"), Some("retired")));

    // 別條線的 session 不能拿來當這條線的檢查點。
    advance(State(app.clone()), Path(pid.clone()), bot(&c.manager), body(json!({"line": "quota", "session_id": "q-1", "role": "thinker"}))).await.unwrap();
    let cross_line = advance(State(app.clone()), Path(pid.clone()), bot(&c.manager), body(json!({"line": "web", "session_id": "q-1"}))).await.unwrap_err();
    assert!(matches!(cross_line, LcError::Conflict(_)) && reason(&cross_line) == "lineage_line_mismatch", "{cross_line:?}");

    // 執行者、child 都讀得到；`line`／`state` 可篩。
    let all = listed(&c, &pid, bot(&c.executor), ListQuery::default()).await.unwrap();
    assert_eq!(all, vec![row("quota", "q-1", "checkpoint"), row("web", "sess-2", "checkpoint"), row("web", "sess-1", "retired")]);
    let child = crate::testing::claude_bot(&app, &pid, "lin-child").await;
    sqlx::query("UPDATE bots SET managed_by='child',parent_bot_id=? WHERE id=?").bind(&c.manager.id).bind(&child.id).execute(&app.db).await.unwrap();
    let as_child = Extension(RequestPrincipal::Child { child_id: child.id.clone(), parent_bot_id: c.manager.id.clone() });
    assert_eq!(listed(&c, &pid, as_child, ListQuery { line: Some("web".into()), state: Some("checkpoint".into()) }).await.unwrap(), vec![row("web", "sess-2", "checkpoint")]);

    let Json(gone) = retire(State(app.clone()), Path(pid.clone()), bot(&c.manager), Json(RetireBody { session_id: "sess-2".into() })).await.unwrap();
    assert_eq!(gone["session"]["state"], "retired");
    let missing = retire(State(app.clone()), Path(pid.clone()), bot(&c.manager), Json(RetireBody { session_id: "nope".into() })).await.unwrap_err();
    assert!(matches!(missing, LcError::NotFoundValue(_)) && reason(&missing) == "lineage_session_not_found", "{missing:?}");
    assert_eq!(listed(&c, &pid, Extension(RequestPrincipal::User), ListQuery { line: Some("web".into()), state: Some("checkpoint".into()) }).await.unwrap(), vec![], "檢查點退役後這條線沒有檢查點");
}

#[tokio::test]
async fn only_manager_agm_role_or_user_may_write() {
    let c = cast().await;
    let (app, pid) = (c.env.app.clone(), c.env.project_id.clone());
    let advisor = crate::testing::claude_bot(&app, &pid, "lin-advisor").await;
    sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&advisor.id).execute(&app.db).await.unwrap();
    let child = crate::testing::claude_bot(&app, &pid, "lin-kid").await;
    sqlx::query("UPDATE bots SET managed_by='child',role='manager',parent_bot_id=? WHERE id=?").bind(&c.manager.id).bind(&child.id).execute(&app.db).await.unwrap();
    let agm = crate::testing::claude_bot(&app, &pid, "lin-patrol").await;

    let readers: Vec<(&str, Extension<RequestPrincipal>)> = vec![
        ("executor", bot(&c.executor)),
        ("advisor", bot(&advisor)),
        ("not an AGM role yet", bot(&agm)),
        ("child, even with a manager role", Extension(RequestPrincipal::Child { child_id: child.id.clone(), parent_bot_id: c.manager.id.clone() })),
        ("child presenting as a bot", bot(&child)),
    ];
    for (what, who) in readers {
        let e = create(State(app.clone()), Path(pid.clone()), who.clone(), body(json!({"line": "web", "session_id": "s"}))).await.unwrap_err();
        assert!(matches!(e, LcError::Forbidden(_)) && reason(&e) == "role_required", "create by {what}: {e:?}");
        let e = advance(State(app.clone()), Path(pid.clone()), who.clone(), body(json!({"line": "web", "session_id": "s"}))).await.unwrap_err();
        assert!(matches!(e, LcError::Forbidden(_)) && reason(&e) == "role_required", "advance by {what}: {e:?}");
        let e = retire(State(app.clone()), Path(pid.clone()), who.clone(), Json(RetireBody { session_id: "s".into() })).await.unwrap_err();
        assert!(matches!(e, LcError::Forbidden(_)) && reason(&e) == "role_required", "retire by {what}: {e:?}");
        assert!(listed(&c, &pid, who, ListQuery::default()).await.unwrap().is_empty(), "{what} reads, and nothing was written");
    }
    let service = create(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::Service("daemon-swap".into())), body(json!({"line": "web", "session_id": "s"}))).await.unwrap_err();
    assert!(matches!(service, LcError::Forbidden(_)), "{service:?}");

    // 登記成 AGM 角色（巡檢）後可寫。
    crate::supervisor::store::get_or_init(&app.db).await.unwrap();
    crate::supervisor::store::set_env(&app.db, &agm.id, &pid, "/tmp").await.unwrap();
    advance(State(app.clone()), Path(pid.clone()), bot(&agm), body(json!({"line": "web", "session_id": "s"}))).await.map_err(|e| format!("{e:?}")).unwrap();
    assert_eq!(listed(&c, &pid, bot(&c.manager), ListQuery::default()).await.unwrap(), vec![row("web", "s", "checkpoint")]);
}

/// 母子模式（parent_child）沒有 manager：同專案任何存活的頂層 bot 都可寫，child 仍唯讀，別的專案照樣碰不到。
#[tokio::test]
async fn in_a_parent_child_project_any_top_level_bot_writes_and_children_only_read() {
    let c = cast().await;
    let (app, pid, other) = (c.env.app.clone(), c.env.project_id.clone(), c.other_project.clone());
    sqlx::query("UPDATE projects SET mode='parent_child' WHERE id IN (?,?)").bind(&pid).bind(&other).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role=NULL").execute(&app.db).await.unwrap();
    let parent = crate::testing::claude_bot(&app, &pid, "lin-parent").await;
    let child = crate::testing::claude_bot(&app, &pid, "lin-pc-child").await;
    sqlx::query("UPDATE bots SET managed_by='child',parent_bot_id=? WHERE id=?").bind(&parent.id).bind(&child.id).execute(&app.db).await.unwrap();

    // 頂層 bot（沒有角色）：add／advance／retire 都可以；不必是登記那顆 session 的 bot。
    let Json(added) = create(State(app.clone()), Path(pid.clone()), bot(&parent), body(json!({"line": "web", "session_id": "pc-1"}))).await.map_err(|e| format!("{e:?}")).unwrap();
    assert_eq!(added["session"]["state"], "active");
    let Json(moved) = advance(State(app.clone()), Path(pid.clone()), bot(&c.executor), body(json!({"line": "web", "session_id": "pc-1"}))).await.map_err(|e| format!("{e:?}")).unwrap();
    assert_eq!(moved["session"]["state"], "checkpoint");
    advance(State(app.clone()), Path(pid.clone()), bot(&parent), body(json!({"line": "web", "session_id": "pc-2"}))).await.map_err(|e| format!("{e:?}")).unwrap();
    let Json(gone) = retire(State(app.clone()), Path(pid.clone()), bot(&c.manager), Json(RetireBody { session_id: "pc-1".into() })).await.map_err(|e| format!("{e:?}")).unwrap();
    assert_eq!(gone["session"]["state"], "retired");

    // child：不論用 child 憑證還是 bot 憑證，都只能讀。
    let as_child = || Extension(RequestPrincipal::Child { child_id: child.id.clone(), parent_bot_id: parent.id.clone() });
    for (what, who) in [("child credential", as_child()), ("child presenting as a bot", bot(&child))] {
        let e = create(State(app.clone()), Path(pid.clone()), who.clone(), body(json!({"line": "web", "session_id": "kid"}))).await.unwrap_err();
        assert!(matches!(e, LcError::Forbidden(_)) && reason(&e) == "role_required", "create by {what}: {e:?}");
        let e = advance(State(app.clone()), Path(pid.clone()), who.clone(), body(json!({"line": "web", "session_id": "pc-1"}))).await.unwrap_err();
        assert!(matches!(e, LcError::Forbidden(_)) && reason(&e) == "role_required", "advance by {what}: {e:?}");
        let e = retire(State(app.clone()), Path(pid.clone()), who.clone(), Json(RetireBody { session_id: "pc-2".into() })).await.unwrap_err();
        assert!(matches!(e, LcError::Forbidden(_)) && reason(&e) == "role_required", "retire by {what}: {e:?}");
        assert_eq!(listed(&c, &pid, who, ListQuery::default()).await.unwrap(), vec![row("web", "pc-2", "checkpoint"), row("web", "pc-1", "retired")], "{what} reads");
    }
    // parent 欄位還在、只是 managed_by 沒標成 child 的 bot 也不算頂層。
    sqlx::query("UPDATE bots SET managed_by='user' WHERE id=?").bind(&child.id).execute(&app.db).await.unwrap();
    let e = retire(State(app.clone()), Path(pid.clone()), bot(&child), Json(RetireBody { session_id: "pc-2".into() })).await.unwrap_err();
    assert!(reason(&e) == "role_required", "{e:?}");
    // 被刪的頂層 bot 不行。
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&c.executor.id).execute(&app.db).await.unwrap();
    let e = retire(State(app.clone()), Path(pid.clone()), bot(&c.executor), Json(RetireBody { session_id: "pc-2".into() })).await.unwrap_err();
    assert!(matches!(e, LcError::Forbidden(_)), "{e:?}");

    // 跨專案：別的（同為母子模式的）專案的頂層 bot 寫不進來，路由層與 handler 都擋；反向亦然。
    for (method, tail) in [("GET", ""), ("POST", ""), ("POST", "/advance"), ("POST", "/retire")] {
        assert!(matches!(authorize_bot_path(&app, &c.outsider_manager.id, method, &format!("/api/projects/{pid}/lineage{tail}")).await, Err(LcError::Forbidden(_))), "{method} {tail}");
        assert!(matches!(authorize_bot_path(&app, &parent.id, method, &format!("/api/projects/{other}/lineage{tail}")).await, Err(LcError::Forbidden(_))), "{method} {tail}");
    }
    let e = advance(State(app.clone()), Path(pid.clone()), bot(&c.outsider_manager), body(json!({"line": "web", "session_id": "hijack"}))).await.unwrap_err();
    assert!(reason(&e) == "cross_project", "{e:?}");
    let e = create(State(app.clone()), Path(other.clone()), bot(&parent), body(json!({"line": "web", "session_id": "hijack"}))).await.unwrap_err();
    assert!(reason(&e) == "cross_project", "{e:?}");
    let e = retire(State(app.clone()), Path(pid.clone()), bot(&c.outsider_manager), Json(RetireBody { session_id: "pc-2".into() })).await.unwrap_err();
    assert!(reason(&e) == "cross_project", "{e:?}");
    assert_eq!(listed(&c, &pid, Extension(RequestPrincipal::User), ListQuery::default()).await.unwrap(), vec![row("web", "pc-2", "checkpoint"), row("web", "pc-1", "retired")]);
    assert!(listed(&c, &other, Extension(RequestPrincipal::User), ListQuery::default()).await.unwrap().is_empty());

    // 切回 expert：沒有角色的頂層 bot 不再能寫（expert 維持原規則）。
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&pid).execute(&app.db).await.unwrap();
    let e = retire(State(app.clone()), Path(pid.clone()), bot(&parent), Json(RetireBody { session_id: "pc-2".into() })).await.unwrap_err();
    assert!(reason(&e) == "role_required", "{e:?}");
}

#[tokio::test]
async fn a_bot_cannot_read_or_write_another_projects_lineage() {
    let c = cast().await;
    let (app, pid, other) = (c.env.app.clone(), c.env.project_id.clone(), c.other_project.clone());
    advance(State(app.clone()), Path(pid.clone()), bot(&c.manager), body(json!({"line": "web", "session_id": "mine"}))).await.unwrap();
    advance(State(app.clone()), Path(other.clone()), bot(&c.outsider_manager), body(json!({"line": "web", "session_id": "theirs"}))).await.unwrap();

    // 路由層：別的專案的路徑一律 403（AGM 角色也一樣，專案範圍不因角色放寬）。
    crate::supervisor::store::get_or_init(&app.db).await.unwrap();
    crate::supervisor::store::set_env(&app.db, &c.executor.id, &pid, "/tmp").await.unwrap();
    for caller in [&c.manager, &c.executor] {
        for (method, tail) in [("GET", ""), ("POST", ""), ("POST", "/advance"), ("POST", "/retire")] {
            let denied = authorize_bot_path(&app, &caller.id, method, &format!("/api/projects/{other}/lineage{tail}")).await;
            assert!(matches!(denied, Err(LcError::Forbidden(_))), "{} {method} {tail}", caller.name);
            assert!(authorize_bot_path(&app, &caller.id, method, &format!("/api/projects/{pid}/lineage{tail}")).await.is_ok(), "own project {method} {tail}");
        }
    }
    // handler 自己也核專案，不靠路由層。
    let who = bot(&c.manager);
    let e = listed(&c, &other, who.clone(), ListQuery::default()).await.unwrap_err();
    assert!(matches!(e, LcError::Forbidden(_)) && reason(&e) == "cross_project", "{e:?}");
    let e = advance(State(app.clone()), Path(other.clone()), who.clone(), body(json!({"line": "web", "session_id": "hijack"}))).await.unwrap_err();
    assert!(reason(&e) == "cross_project", "{e:?}");
    let e = create(State(app.clone()), Path(other.clone()), who.clone(), body(json!({"line": "web", "session_id": "hijack"}))).await.unwrap_err();
    assert!(reason(&e) == "cross_project", "{e:?}");
    let e = retire(State(app.clone()), Path(other.clone()), who, Json(RetireBody { session_id: "theirs".into() })).await.unwrap_err();
    assert!(reason(&e) == "cross_project", "{e:?}");
    // 自己專案裡退役不到別的專案的同名 session。
    let e = retire(State(app.clone()), Path(pid.clone()), bot(&c.manager), Json(RetireBody { session_id: "theirs".into() })).await.unwrap_err();
    assert!(matches!(e, LcError::NotFoundValue(_)), "{e:?}");
    assert_eq!(listed(&c, &other, bot(&c.outsider_manager), ListQuery::default()).await.unwrap(), vec![row("web", "theirs", "checkpoint")]);
    assert_eq!(listed(&c, &pid, bot(&c.manager), ListQuery::default()).await.unwrap(), vec![row("web", "mine", "checkpoint")]);
}

#[tokio::test]
async fn bad_fields_and_missing_projects_are_refused_before_anything_is_written() {
    let c = cast().await;
    let (app, pid) = (c.env.app.clone(), c.env.project_id.clone());
    let user = || Extension(RequestPrincipal::User);
    for (field, v) in [
        ("line", json!({"line": "ci/ops", "session_id": "s"})),
        ("session_id", json!({"line": "web", "session_id": "has space"})),
        ("role", json!({"line": "web", "session_id": "s", "role": "boss"})),
        ("kind", json!({"line": "web", "session_id": "s", "kind": "claude-api"})),
        ("kind", json!({"line": "web", "session_id": "s", "kind": "claude"})),
        ("parent_session_id", json!({"line": "web", "session_id": "s", "parent_session_id": "s"})),
        ("context_tokens", json!({"line": "web", "session_id": "s", "context_tokens": -1})),
        ("last_api_at", json!({"line": "web", "session_id": "s", "last_api_at": "yesterday"})),
        ("head_sha", json!({"line": "web", "session_id": "s", "head_sha": "HEAD"})),
        ("summary", json!({"line": "web", "session_id": "s", "summary": "x".repeat(501)})),
    ] {
        let e = advance(State(app.clone()), Path(pid.clone()), user(), body(v.clone())).await.unwrap_err();
        assert!(matches!(&e, LcError::BadValue(b) if b["field"] == field && b["reason"] == "invalid_lineage_field"), "{field}: {e:?}");
        assert!(matches!(create(State(app.clone()), Path(pid.clone()), user(), body(v)).await, Err(LcError::BadValue(_))), "{field}");
    }
    assert!(serde_json::from_value::<SessionBody>(json!({"line": "web", "session_id": "s", "state": "checkpoint"})).is_err(), "未知欄位（例如想直接指定 state）不收");
    let e = listed(&c, &pid, user(), ListQuery { line: None, state: Some("paused".into()) }).await.unwrap_err();
    assert!(matches!(&e, LcError::BadValue(b) if b["field"] == "state"), "{e:?}");
    assert!(listed(&c, &pid, user(), ListQuery::default()).await.unwrap().is_empty());

    assert!(matches!(listed(&c, "no-such-project", user(), ListQuery::default()).await, Err(LcError::NotFound(_))));
    sqlx::query("UPDATE projects SET deleted_at=? WHERE id=?").bind(db::now()).bind(&c.other_project).execute(&app.db).await.unwrap();
    let e = advance(State(app.clone()), Path(c.other_project.clone()), user(), body(json!({"line": "web", "session_id": "s"}))).await.unwrap_err();
    assert!(matches!(e, LcError::NotFound(_)), "已刪的專案：{e:?}");
}

/// 走真的 router：bot 憑證（`X-AM-Bot-Id`＋token）經過中介層的專案範圍檢查，再到 handler 的角色檢查。
#[tokio::test]
async fn over_http_bot_credentials_are_scoped_by_project_then_by_role() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let c = cast().await;
    let (pid, other) = (c.env.project_id.clone(), c.other_project.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(c.env.app.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let send = |method: &'static str, path: String, who: &db::Bot, body: Value| {
        let (id, token) = (who.id.clone(), who.hook_token.clone());
        async move {
            let body = if method == "GET" { String::new() } else { body.to_string() };
            let req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nX-AM-Bot-Id: {id}\r\nX-AM-Bot-Token: {token}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
            conn.write_all(req.as_bytes()).await.unwrap();
            let mut out = String::new();
            conn.read_to_string(&mut out).await.unwrap();
            out
        }
    };
    let own = |tail: &str| format!("/api/projects/{pid}/lineage{tail}");
    let theirs = |tail: &str| format!("/api/projects/{other}/lineage{tail}");

    let out = send("POST", own("/advance"), &c.manager, json!({"line": "web", "session_id": "sess-1", "head_sha": "0b43a28e4"})).await;
    assert!(out.starts_with("HTTP/1.1 200") && out.contains("\"state\":\"checkpoint\""), "{out}");
    let out = send("POST", own("/claim"), &c.manager, json!({"line": "web", "mode": "resume", "backup_session_id": "bak-1"})).await;
    assert!(out.starts_with("HTTP/1.1 200") && out.contains("\"mode\":\"resume\""), "{out}");
    let out = send("POST", own("/claim"), &c.executor, json!({"line": "web", "mode": "fork"})).await;
    assert!(out.starts_with("HTTP/1.1 403") && out.contains("role_required"), "executor claim: {out}");
    let out = send("POST", theirs("/claim"), &c.manager, json!({"line": "web", "mode": "fork"})).await;
    assert!(out.starts_with("HTTP/1.1 403") && out.contains("bot_resource_scope"), "outsider claim: {out}");
    let out = send("GET", own("?line=web"), &c.executor, Value::Null).await;
    assert!(out.starts_with("HTTP/1.1 200") && out.contains("\"session_id\":\"sess-1\""), "executor reads: {out}");
    for tail in ["", "/advance", "/retire"] {
        let body = if tail == "/retire" { json!({"session_id": "sess-1"}) } else { json!({"line": "web", "session_id": "sess-1"}) };
        let out = send("POST", own(tail), &c.executor, body).await;
        assert!(out.starts_with("HTTP/1.1 403") && out.contains("role_required"), "executor POST {tail}: {out}");
    }
    for (method, tail) in [("GET", ""), ("POST", ""), ("POST", "/advance"), ("POST", "/retire")] {
        let out = send(method, theirs(tail), &c.manager, json!({"line": "web", "session_id": "sess-1"})).await;
        assert!(out.starts_with("HTTP/1.1 403") && out.contains("bot_resource_scope"), "other project {method} {tail}: {out}");
    }
    let out = send("POST", own("/advance"), &c.manager, json!({"line": "web", "session_id": "sess-2", "state": "checkpoint"})).await;
    assert!(out.starts_with("HTTP/1.1 422"), "unknown field: {out}");
    let out = send("POST", own("/retire"), &c.manager, json!({"session_id": "sess-1"})).await;
    assert!(out.starts_with("HTTP/1.1 200") && out.contains("\"state\":\"retired\""), "{out}");
    server.abort();
    assert_eq!(listed(&c, &other, Extension(RequestPrincipal::User), ListQuery::default()).await.unwrap(), vec![], "別的專案什麼都沒被寫進去");
}

#[tokio::test]
async fn claim_abandon_and_versioned_advance_enforces_single_writer_and_permissions() {
    let c = cast().await;
    let (app, pid, other) = (c.env.app.clone(), c.env.project_id.clone(), c.other_project.clone());
    let claim_body = |v: Value| Json(serde_json::from_value::<ClaimBody>(v).unwrap());

    // 沒有檢查點不能認領。
    let e = claim(State(app.clone()), Path(pid.clone()), bot(&c.manager), claim_body(json!({"line": "web", "mode": "resume"}))).await.unwrap_err();
    assert!(matches!(e, LcError::NotFoundValue(_)) && reason(&e) == "lineage_checkpoint_not_found", "{e:?}");

    advance(State(app.clone()), Path(pid.clone()), bot(&c.manager), body(json!({"line": "web", "session_id": "s1", "head_sha": "aaaaaaa"}))).await.unwrap();

    // 權限：executor 只讀，不能 claim／abandon；outsider 跨專案被擋。
    let e = claim(State(app.clone()), Path(pid.clone()), bot(&c.executor), claim_body(json!({"line": "web", "mode": "resume"}))).await.unwrap_err();
    assert!(matches!(e, LcError::Forbidden(_)) && reason(&e) == "role_required", "{e:?}");
    let e = claim(State(app.clone()), Path(pid.clone()), bot(&c.outsider_manager), claim_body(json!({"line": "web", "mode": "resume"}))).await.unwrap_err();
    assert!(matches!(e, LcError::Forbidden(_)) && reason(&e) == "cross_project", "{e:?}");

    // 單寫者 resume 認領；第二筆 409 lineage_line_busy（回傳佔用中的 claim）。
    let Json(c1) = claim(State(app.clone()), Path(pid.clone()), bot(&c.manager), claim_body(json!({"line": "web", "mode": "resume", "backup_session_id": "bak-1"}))).await.unwrap();
    let c1_id = c1["claim"]["id"].as_str().unwrap().to_string();
    assert_eq!((c1["claim"]["holder_bot_id"].as_str(), c1["checkpoint"]["session_id"].as_str()), (Some(c.manager.id.as_str()), Some("s1")));
    let busy = claim(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), claim_body(json!({"line": "web", "mode": "resume"}))).await.unwrap_err();
    assert!(matches!(&busy, LcError::Conflict(v) if v["reason"] == "lineage_line_busy" && v["claim"]["id"] == c1_id), "{busy:?}");

    // fork 認領可多筆並存。
    let Json(f1) = claim(State(app.clone()), Path(pid.clone()), bot(&c.manager), claim_body(json!({"line": "web", "mode": "fork", "work_session_id": "w1"}))).await.unwrap();
    let Json(f2) = claim(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), claim_body(json!({"line": "web", "mode": "fork", "work_session_id": "w2"}))).await.unwrap();
    let f1_id = f1["claim"]["id"].as_str().unwrap().to_string();
    let f2_id = f2["claim"]["id"].as_str().unwrap().to_string();

    // executor 不能 abandon；跨專案不能 abandon；正確 abandon 回傳 backup_session_id 且可再認領。
    let e = abandon(State(app.clone()), Path((pid.clone(), c1_id.clone())), bot(&c.executor)).await.unwrap_err();
    assert!(reason(&e) == "role_required", "{e:?}");
    let e = abandon(State(app.clone()), Path((other, c1_id.clone())), bot(&c.outsider_manager)).await.unwrap_err();
    assert!(matches!(e, LcError::NotFoundValue(_)) && reason(&e) == "lineage_claim_not_found", "{e:?}");
    let Json(ab) = abandon(State(app.clone()), Path((pid.clone(), c1_id.clone())), bot(&c.manager)).await.unwrap();
    assert_eq!((ab["claim"]["state"].as_str(), ab["backup_session_id"].as_str()), (Some("abandoned"), Some("bak-1")));

    let Json(c2) = claim(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), claim_body(json!({"line": "web", "mode": "resume", "backup_session_id": "bak-2"}))).await.unwrap();
    assert!(c2["claim"]["holder_bot_id"].is_null());
    let c2_id = c2["claim"]["id"].as_str().unwrap().to_string();

    // 帶 claim_id 前移：resume 模式前移 base session 本身（可省略 session_id）；fork 模式前移 work_session_id。
    let Json(adv_res) = advance(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), body(json!({"line": "web", "claim_id": c2_id, "head_sha": "bbbbbbb"}))).await.unwrap();
    assert_eq!((adv_res["session"]["session_id"].as_str(), adv_res["claim"]["state"].as_str()), (Some("s1"), Some("advanced")));
    assert!(adv_res["retired"].is_null());

    let Json(adv_f1) = advance(State(app.clone()), Path(pid.clone()), bot(&c.manager), body(json!({"line": "web", "claim_id": f1_id}))).await.unwrap();
    assert_eq!((adv_f1["session"]["session_id"].as_str(), adv_f1["retired"]["session_id"].as_str()), (Some("w1"), Some("s1")));

    // 過期 claim 已在檢查點移走時自動標成 abandoned，再前移被拒（409 lineage_claim_closed）且什麼都沒改。
    let stale = advance(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), body(json!({"line": "web", "claim_id": f2_id}))).await.unwrap_err();
    assert!(matches!(&stale, LcError::Conflict(v) if v["reason"] == "lineage_claim_closed" && v["claim"]["state"] == "abandoned"), "{stale:?}");
    let closed = advance(State(app.clone()), Path(pid.clone()), bot(&c.manager), body(json!({"line": "web", "claim_id": f1_id}))).await.unwrap_err();
    assert!(matches!(&closed, LcError::Conflict(v) if v["reason"] == "lineage_claim_closed"), "{closed:?}");
    assert_eq!(listed(&c, &pid, bot(&c.executor), ListQuery { line: Some("web".into()), state: Some("checkpoint".into()) }).await.unwrap(), vec![row("web", "w1", "checkpoint")]);
}
