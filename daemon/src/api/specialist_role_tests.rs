use super::*;

/// 專家模式專案：先用一般專案建 manager 與一顆成員，再把 manager 設好、專案改成 expert（expert 要求恰好一位 manager）。
async fn expert_project(env: &crate::testing::Env) -> (String, String) {
    let (pid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
    env.app.cfg.update(move |cfg| {
        cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, handed_off_to: None, id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![] });
        Ok(())
    }).await.unwrap();
    let add = |name: &'static str| {
        let app = env.app.clone();
        let pid = env.project_id.clone();
        async move {
            let res = create_bot(State(app), Path(pid), Json(serde_json::from_value(json!({"name": name, "kind": "claude"})).unwrap())).await.unwrap();
            let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string()
        }
    };
    let manager = add("spec-manager").await;
    let member = add("spec-member").await;
    let set_manager = serde_json::from_value::<PatchBot>(json!({"role": "manager"})).unwrap();
    patch_bot(State(env.app.clone()), Path(manager.clone()), Extension(RequestPrincipal::User), Json(set_manager)).await.map_err(|e| format!("{e:?}")).unwrap();
    crate::projection::update_and_project(&env.app.cfg, &env.app.db, |cfg| {
        for p in cfg.projects.iter_mut() {
            p.mode = "expert".into();
        }
        Ok(())
    }).await.map_err(|e| format!("{e:?}")).unwrap();
    (manager, member)
}

#[tokio::test]
async fn specialist_needs_a_track_and_only_the_user_sets_it() {
    let env = crate::testing::env().await;
    let (manager, member) = expert_project(&env).await;
    let patch = |caller: RequestPrincipal, v: Value| patch_bot(State(env.app.clone()), Path(member.clone()), Extension(caller), Json(serde_json::from_value::<PatchBot>(v).unwrap()));

    // 專家缺 track → 400 track_required（不寫任何東西）
    let err = patch(RequestPrincipal::User, json!({"role": "specialist"})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["error"] == "track_required"), "{err:?}");
    assert_eq!(db::bot(&env.app.db, &member).await.unwrap().unwrap().role, None);

    // 管理者不能改 track：user_only
    let err = patch(RequestPrincipal::Bot(manager.clone()), json!({"track": "infra"})).await.err().unwrap();
    assert!(matches!(err, LcError::Forbidden(ref v) if v["reason"] == "user_only"), "{err:?}");

    // 使用者同時設角色與 track：寫進 DB，也投影回 config.toml
    patch(RequestPrincipal::User, json!({"role": "specialist", "track": "infra"})).await.map_err(|e| format!("{e:?}")).unwrap();
    let b = db::bot(&env.app.db, &member).await.unwrap().unwrap();
    assert_eq!((b.role.as_deref(), b.track.as_deref()), (Some("specialist"), Some("infra")));
    let cfg_track = env.app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).find(|x| x.id.as_deref() == Some(member.as_str())).and_then(|x| x.track.clone());
    assert_eq!(cfg_track.as_deref(), Some("infra"), "track 的真相在 config.toml");

    // 清掉 track 之後又缺 track
    let err = patch(RequestPrincipal::User, json!({"track": null})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["error"] == "track_required"), "{err:?}");

    // /api/state：bot 帶 track、runtime_override（未設＝null），project 帶 role_policy
    patch(RequestPrincipal::User, json!({"track": "infra"})).await.map_err(|e| format!("{e:?}")).unwrap();
    let state = state_json(&env.app.clone()).await.unwrap();
    let project = state["projects"].as_array().unwrap().iter().find(|p| p["id"] == env.project_id.as_str()).unwrap();
    let bot = project["bots"].as_array().unwrap().iter().find(|x| x["id"] == member.as_str()).unwrap();
    assert_eq!((&bot["track"], &bot["runtime_override"]), (&json!("infra"), &Value::Null));
    assert_eq!(project["role_policy"], json!({}));
}

#[tokio::test]
async fn manager_appoints_a_specialist_only_with_a_track() {
    let env = crate::testing::env().await;
    let (manager, member) = expert_project(&env).await;
    let call = |role: Option<&str>, track: Option<&str>| put_bot_role(State(env.app.clone()), Path(member.clone()), Extension(RequestPrincipal::Bot(manager.clone())), Json(PutBotRole { role: role.map(str::to_string), track: track.map(str::to_string) }));

    let err = call(Some("specialist"), None).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["error"] == "track_required"), "{err:?}");
    // 其他角色不能順手帶 track
    let err = call(Some("executor"), Some("infra")).await.err().unwrap();
    assert!(matches!(err, LcError::Forbidden(ref v) if v["reason"] == "user_only"), "{err:?}");

    call(Some("specialist"), Some("infra")).await.map_err(|e| format!("{e:?}")).unwrap();
    let b = db::bot(&env.app.db, &member).await.unwrap().unwrap();
    assert_eq!((b.role.as_deref(), b.track.as_deref()), (Some("specialist"), Some("infra")));
}

#[tokio::test]
async fn manager_hires_specialists_without_pool_limits() {
    let env = crate::testing::env().await;
    let (manager, _member) = expert_project(&env).await;
    let hire = |v: Value| hire_member(State(env.app.clone()), Path(env.project_id.clone()), Extension(RequestPrincipal::Bot(manager.clone())), Json(serde_json::from_value::<HireMember>(v).unwrap()));

    let err = hire(json!({"name": "spec-a", "kind": "claude", "role": "specialist"})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["error"] == "track_required"), "{err:?}");

    for (name, track) in [("spec-a", "infra"), ("spec-b", "testing")] {
        let response = hire(json!({"name": name, "kind": "claude", "role": "specialist", "track": track})).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let body = serde_json::from_slice::<Value>(&bytes).unwrap();
        let bot = db::bot(&env.app.db, body["bot_id"].as_str().unwrap()).await.unwrap().unwrap();
        assert_eq!((bot.role.as_deref(), bot.track.as_deref()), (Some("specialist"), Some(track)));
    }
}

#[tokio::test]
async fn role_policy_is_user_only_and_lands_in_config_and_db() {
    let env = crate::testing::env().await;
    let (manager, _member) = expert_project(&env).await;
    let patch = |caller: RequestPrincipal, v: Value| patch_project(State(env.app.clone()), Path(env.project_id.clone()), Extension(caller), Json(serde_json::from_value::<PatchProject>(v).unwrap()));

    let err = patch(RequestPrincipal::Bot(manager.clone()), json!({"role_policy": {"specialist": {"keep_warm_after_min": 30}}})).await.err().unwrap();
    assert!(matches!(err, LcError::Forbidden(ref v) if v["reason"] == "user_only"), "{err:?}");

    let err = patch(RequestPrincipal::User, json!({"role_policy": "nope"})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "invalid_role_policy"), "{err:?}");
    // key 只收 manager／executor／advisor／specialist；值一定是 object
    let err = patch(RequestPrincipal::User, json!({"role_policy": {"owner": {"max_turns": 1}}})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "invalid_role_policy" && v["role"] == "owner"), "{err:?}");
    let err = patch(RequestPrincipal::User, json!({"role_policy": {"specialist": 3}})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "invalid_role_policy"), "{err:?}");
    let err = patch(RequestPrincipal::User, json!({"role_policy": {"specialist": ["x"]}})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "invalid_role_policy"), "{err:?}");
    assert!(db::project(&env.app.db, &env.project_id).await.unwrap().unwrap().role_policy().is_empty(), "invalid input writes nothing");

    patch(RequestPrincipal::User, json!({"role_policy": {"specialist": {"keep_warm_after_min": 30}}})).await.map_err(|e| format!("{e:?}")).unwrap();
    let p = db::project(&env.app.db, &env.project_id).await.unwrap().unwrap();
    assert_eq!(p.role_policy()["specialist"]["keep_warm_after_min"], 30);
    let cfg_policy = env.app.cfg.get().await.projects.iter().find(|p| p.id.as_deref() == Some(env.project_id.as_str())).unwrap().role_policy.clone();
    assert_eq!(cfg_policy["specialist"]["keep_warm_after_min"], 30);
}
