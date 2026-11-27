//! `PATCH /api/projects/:id {"path"}`（專案搬家）。
use super::*;
use crate::project_relocate::{normalize_path, rewrite_alias_key, rewrite_md_path, swap_prefix};

async fn seed(e: &crate::testing::Env, md: Vec<String>) {
    let (pid, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
    e.app.cfg.update(move |cfg| {
        cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, handed_off_to: None, id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![] });
        cfg.agents.projects.insert("proj".into(), crate::config::AgentMdFiles::Many(md));
        Ok(())
    }).await.unwrap();
}

async fn add(e: &crate::testing::Env, name: &str) -> db::Bot {
    let res = create_bot(State(e.app.clone()), Path(e.project_id.clone()), Json(serde_json::from_value(json!({"name": name, "kind": "claude"})).unwrap())).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let id = serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string();
    db::bot(&e.app.db, &id).await.unwrap().unwrap()
}

async fn relocate(e: &crate::testing::Env, who: RequestPrincipal, path: &str) -> Result<Value, LcError> {
    let body: PatchProject = serde_json::from_value(json!({"path": path})).unwrap();
    let res = patch_project(State(e.app.clone()), Path(e.project_id.clone()), Extension(who), Json(body)).await?;
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    Ok(serde_json::from_slice(&bytes).unwrap())
}

fn moved(e: &crate::testing::Env) -> String {
    let p = e.dir.join("moved-here");
    std::fs::create_dir_all(&p).unwrap();
    p.to_string_lossy().to_string()
}

async fn project_path(e: &crate::testing::Env) -> (String, String) {
    let db_path = db::project(&e.app.db, &e.project_id).await.unwrap().unwrap().path;
    let cfg = e.app.cfg.get().await;
    let cfg_path = cfg.projects.iter().find(|p| p.id.as_deref() == Some(e.project_id.as_str())).unwrap().path.clone();
    (db_path, cfg_path)
}

#[test]
fn only_an_exact_prefix_is_swapped() {
    assert_eq!(swap_prefix("/a/wt", "/a/wt", "/b/wt").as_deref(), Some("/b/wt"));
    assert_eq!(swap_prefix("/a/wt/sub/x", "/a/wt", "/b/wt").as_deref(), Some("/b/wt/sub/x"));
    assert_eq!(swap_prefix("/a/wt2", "/a/wt", "/b/wt"), None);
    assert_eq!(swap_prefix("/a/wt2/x", "/a/wt", "/b/wt"), None);
    assert_eq!(swap_prefix("/a", "/a/wt", "/b/wt"), None);
}

#[test]
fn tilde_instruction_paths_keep_their_tilde() {
    let home = "/Users/m4p";
    let old = "/Users/m4p/project/hermes-agents/projects/wt";
    let new = "/Users/m4p/project/wt";
    assert_eq!(rewrite_md_path("~/project/hermes-agents/projects/wt/AGENTS.md", old, new, home).as_deref(), Some("~/project/wt/AGENTS.md"));
    assert_eq!(rewrite_md_path("/Users/m4p/project/hermes-agents/projects/wt/CLAUDE.md", old, new, home).as_deref(), Some("/Users/m4p/project/wt/CLAUDE.md"));
    assert_eq!(rewrite_md_path("~/project/hermes-agents/projects/wt2/AGENTS.md", old, new, home), None);
    assert_eq!(rewrite_md_path("~/notes.md", old, new, home), None);
    assert_eq!(rewrite_md_path("~/project/hermes-agents/projects/wt/A.md", old, "/srv/wt", home).as_deref(), Some("/srv/wt/A.md"), "moved outside home: absolute");
}

#[test]
fn alias_keys_and_paths_are_normalized() {
    let key = json!(["m4p", "/a/wt/web", ["port", 3000]]).to_string();
    assert_eq!(rewrite_alias_key(&key, "m4p", "/a/wt", "/b/wt"), Some(json!(["m4p", "/b/wt/web", ["port", 3000]]).to_string()));
    assert_eq!(rewrite_alias_key(&key, "local", "/a/wt", "/b/wt"), None, "other host");
    assert_eq!(rewrite_alias_key(&json!(["m4p", "/a/wt2", ["command", "x"]]).to_string(), "m4p", "/a/wt", "/b/wt"), None);
    assert_eq!(normalize_path(" /a/b/ ").unwrap(), "/a/b");
    for bad in ["a/b", "/", "/a/../b", "/a//b", "/a/./b", "~/x"] {
        assert!(normalize_path(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn the_user_relocates_config_and_db_together() {
    let e = crate::testing::env().await;
    let old = e.repo.to_string_lossy().to_string();
    seed(&e, vec![format!("{old}/AGENTS.md"), format!("{old}2/AGENTS.md"), "~/other.md".into()]).await;
    let new = moved(&e);
    let a = add(&e, "reloc-a").await;
    let b = add(&e, "reloc-b").await;
    sqlx::query("UPDATE bots SET cwd=? WHERE id=?").bind(format!("{old}/sub")).bind(&a.id).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET cwd=? WHERE id=?").bind(format!("{old}2")).bind(&b.id).execute(&e.app.db).await.unwrap();
    for (id, agent_path) in [("att-in", format!("{old}/x.png")), ("att-out", format!("{old}2/x.png"))] {
        sqlx::query("INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at) VALUES (?,?,'x','image/png',1,'/l',?,'local',?)")
            .bind(id).bind(&a.id).bind(agent_path).bind(db::now()).execute(&e.app.db).await.unwrap();
    }
    sqlx::query("INSERT INTO shared_bots (bot_id, profile, workspace, created_at) VALUES (?, 'restricted', ?, 'now')").bind(&a.id).bind(format!("{old}/ws")).execute(&e.app.db).await.unwrap();
    let in_key = json!(["local", format!("{old}/web"), ["port", 3000]]).to_string();
    let out_key = json!(["local", format!("{old}2"), ["port", 3001]]).to_string();
    for k in [&in_key, &out_key] {
        sqlx::query("INSERT INTO pane_aliases (key, alias, updated_at) VALUES (?, 'web', 'now')").bind(k).execute(&e.app.db).await.unwrap();
    }

    let v = relocate(&e, RequestPrincipal::User, &format!("{new}/")).await.map_err(|e| format!("{e:?}")).unwrap();
    assert_eq!((v["path"].as_str(), v["needs_restart"].as_bool()), (Some(new.as_str()), Some(false)));
    assert_eq!(project_path(&e).await, (new.clone(), new.clone()));
    let cwd = |id: String| { let pool = e.app.db.clone(); async move { db::bot(&pool, &id).await.unwrap().unwrap().cwd } };
    assert_eq!(cwd(a.id.clone()).await.as_deref(), Some(format!("{new}/sub").as_str()));
    assert_eq!(cwd(b.id.clone()).await.as_deref(), Some(format!("{old}2").as_str()), "sibling prefix untouched");
    let att: Vec<(String, String)> = sqlx::query_as("SELECT id, agent_path FROM attachments ORDER BY id").fetch_all(&e.app.db).await.unwrap();
    assert_eq!(att, vec![("att-in".into(), format!("{new}/x.png")), ("att-out".into(), format!("{old}2/x.png"))]);
    let ws: String = sqlx::query_scalar("SELECT workspace FROM shared_bots WHERE bot_id=?").bind(&a.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(ws, format!("{new}/ws"));
    let mut keys: Vec<String> = sqlx::query_scalar("SELECT key FROM pane_aliases").fetch_all(&e.app.db).await.unwrap();
    keys.sort();
    let mut want = vec![json!(["local", format!("{new}/web"), ["port", 3000]]).to_string(), out_key.clone()];
    want.sort();
    assert_eq!(keys, want);
    let md = e.app.cfg.get().await.agents.projects.get("proj").cloned().unwrap();
    assert_eq!(md, crate::config::AgentMdFiles::Many(vec![format!("{new}/AGENTS.md"), format!("{old}2/AGENTS.md"), "~/other.md".into()]));
}

#[tokio::test]
async fn only_the_user_or_a_fleet_operator_may_relocate() {
    let e = crate::testing::env().await;
    seed(&e, vec![]).await;
    let new = moved(&e);
    let other = add(&e, "reloc-plain").await;
    let err = relocate(&e, RequestPrincipal::Bot(other.id.clone()), &new).await.err().unwrap();
    assert!(matches!(err, LcError::Forbidden(ref v) if v["reason"] == "user_only"), "{err:?}");
    assert_eq!(project_path(&e).await.0, e.repo.to_string_lossy());
    let op = other.id.clone();
    e.app.cfg.update(move |cfg| { cfg.agents.fleet_operators = vec![op]; Ok(()) }).await.unwrap();
    let label: PatchProject = serde_json::from_value(json!({"label": "hijack"})).unwrap();
    let err = patch_project(State(e.app.clone()), Path(e.project_id.clone()), Extension(RequestPrincipal::Bot(other.id.clone())), Json(label)).await.err().unwrap();
    assert!(matches!(err, LcError::Forbidden(ref v) if v["reason"] == "user_only"), "a fleet operator gets path only: {err:?}");
    relocate(&e, RequestPrincipal::Bot(other.id.clone()), &new).await.map_err(|e| format!("{e:?}")).unwrap();
    assert_eq!(project_path(&e).await, (new.clone(), new));
}

#[tokio::test]
async fn a_running_bot_a_taken_path_or_a_missing_dir_refuses() {
    let e = crate::testing::env().await;
    seed(&e, vec![]).await;
    let old = e.repo.to_string_lossy().to_string();
    let new = moved(&e);
    let bot = add(&e, "reloc-running").await;
    crate::testing::fake_run(&e.app, &bot.id).await;
    let err = relocate(&e, RequestPrincipal::User, &new).await.err().unwrap();
    let LcError::Conflict(v) = &err else { panic!("{err:?}") };
    assert_eq!(v["reason"], "bots_running");
    assert_eq!(v["bots"][0]["id"], bot.id.as_str());
    sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE bot_id=?").bind(db::now()).bind(&bot.id).execute(&e.app.db).await.unwrap();
    assert!(db::active_run(&e.app.db, &bot.id).await.unwrap().is_none());

    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES ('p-taken', ?, 'taken', 'local', ?)").bind(&new).bind(db::now()).execute(&e.app.db).await.unwrap();
    let err = relocate(&e, RequestPrincipal::User, &new).await.err().unwrap();
    assert!(matches!(err, LcError::Conflict(ref v) if v["reason"] == "path_taken"), "{err:?}");

    let missing = e.dir.join("not-there").to_string_lossy().to_string();
    let err = relocate(&e, RequestPrincipal::User, &missing).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "path_not_found"), "{err:?}");
    assert_eq!(project_path(&e).await, (old.clone(), old));
}

/// 軟刪的 bot 的 cwd 也跟著搬：還原後才不會指向舊路徑。
#[tokio::test]
async fn a_soft_deleted_bot_moves_with_the_project() {
    let e = crate::testing::env().await;
    let old = e.repo.to_string_lossy().to_string();
    seed(&e, vec![]).await;
    let new = moved(&e);
    let gone = crate::testing::claude_bot(&e.app, &e.project_id, "reloc-gone").await;
    sqlx::query("UPDATE bots SET cwd=?, deleted_at=? WHERE id=?").bind(format!("{old}/sub")).bind(db::now()).bind(&gone.id).execute(&e.app.db).await.unwrap();
    relocate(&e, RequestPrincipal::User, &new).await.map_err(|e| format!("{e:?}")).unwrap();
    let cwd: Option<String> = sqlx::query_scalar("SELECT cwd FROM bots WHERE id=?").bind(&gone.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(cwd.as_deref(), Some(format!("{new}/sub").as_str()));
}
