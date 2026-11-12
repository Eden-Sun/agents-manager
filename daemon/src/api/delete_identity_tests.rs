use super::*;

fn ident(name: &str, kind: &str, host: Option<&str>) -> crate::config::IdentityCfg {
    crate::config::IdentityCfg { name: name.into(), kind: kind.into(), host: host.map(String::from), env: Default::default(), args: vec![] }
}

/// 沒寫 host 的身份在遠端也生效（quota 那顆 `dca3c4c`），除非那台有自己同名的（config 明寫、或 shell 的 `ccN`）。
#[test]
fn a_hostless_identity_is_in_effect_on_a_remote_host_unless_that_host_has_its_own() {
    let hostless = vec![ident("work", "codex", None)];
    assert!(hostless_identity_in_effect(&hostless, "mac_builder", None, "work"));
    let shadowed = vec![ident("work", "codex", None), ident("work", "codex", Some("mac_builder"))];
    assert!(!hostless_identity_in_effect(&shadowed, "mac_builder", None, "work"), "那台有明寫的");
    let cc = vec![ident("cc1", "claude", None)];
    let mac_builder_shell = vec![ident("cc1", "claude", None)];
    assert!(!hostless_identity_in_effect(&cc, "mac_builder", Some(&mac_builder_shell), "cc1"), "那台 shell 自己有 cc1");
    assert!(hostless_identity_in_effect(&cc, "mac_builder", Some(&[]), "cc1"), "那台偵測過、沒有 cc1");
    assert!(hostless_identity_in_effect(&cc, "mac_builder", None, "cc1"), "還沒偵測：照最保守的算");
}

async fn cache_put(app: &Arc<App>, key: &str) {
    app.models_cache.lock().await.insert(key.to_string(), (std::time::Instant::now(), json!({"models": []})));
}

async fn cache_has(app: &Arc<App>, key: &str) -> bool {
    app.models_cache.lock().await.contains_key(key)
}

/// #1349：刪掉再重建同名身份（家目錄變了）之後，那個名字的模型清單快取要作廢；別的身份的不動。
#[tokio::test]
async fn deleting_or_recreating_an_identity_drops_its_cached_model_list() {
    let env = crate::testing::env().await;
    let app = &env.app;
    crate::testing::seed_project_config(&env).await;
    cache_put(app, "local/codex/work").await;
    cache_put(app, "mac_builder/codex/work").await;
    cache_put(app, "local/codex/other").await;
    let new = NewIdentity { name: "work".into(), kind: "codex".into(), env: Default::default(), args: vec![], host: None };
    create_identity(State(app.clone()), Json(new)).await.unwrap();
    assert!(!cache_has(app, "local/codex/work").await, "重建同名身份：舊家目錄的清單不能留著");
    assert!(!cache_has(app, "mac_builder/codex/work").await, "同名的別台那格一起作廢（不逐台判斷）");
    assert!(cache_has(app, "local/codex/other").await, "別的身份不動");

    cache_put(app, "local/codex/work").await;
    delete_identity(State(app.clone()), Path("work".into()), Query(IdentityHostQuery { host: None })).await.unwrap();
    assert!(!cache_has(app, "local/codex/work").await, "刪掉之後退回預設家目錄，不能再回舊帳號的清單");
    assert!(cache_has(app, "local/codex/other").await);
    assert!(crate::models::cached(app.as_ref(), "local", "codex", Some("work")).await.is_none());
}

fn reading(host: &str) -> crate::quota::Quota {
    crate::quota::Quota {
        five_hour: None,
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: crate::db::now(),
        source: "test".into(),
        account: None,
        host: host.into(),
    }
}

/// #1350：刪掉身份之後，它的額度讀數（記憶體與 quota_cache）一起清掉；別的身份不動。
#[tokio::test]
async fn deleting_an_identity_forgets_its_quota_reading() {
    let env = crate::testing::env().await;
    let app = &env.app;
    crate::testing::seed_project_config(&env).await;
    app.cfg.update(|f| { f.identities.push(ident("work", "claude", None)); f.identities.push(ident("other", "claude", None)); Ok(()) }).await.unwrap();
    for key in ["claude:work", "claude:other"] {
        app.quotas.lock().await.insert(key.into(), reading("local"));
        crate::quota::persist_cache_for_test(app, key, &reading("local")).await;
    }
    delete_identity(State(app.clone()), Path("work".into()), Query(IdentityHostQuery { host: None })).await.unwrap();
    assert!(!app.quotas.lock().await.contains_key("claude:work"), "已刪帳號的額度格不能留著");
    assert!(app.quotas.lock().await.contains_key("claude:other"), "別的身份不動");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quota_cache WHERE key = 'claude:work'").fetch_one(&app.db).await.unwrap();
    assert_eq!(rows, 0, "重啟後也不能再載回來");
    let snap = crate::quota::snapshot(app.as_ref()).await;
    assert!(snap["kinds"].get("claude:work").is_none(), "額度列不再畫那一格：{snap}");
}

/// #1350：同名的 shell alias（`cc1`）接手時，那把 key 仍然有效，不清。
#[tokio::test]
async fn an_identity_whose_name_a_shell_alias_still_carries_keeps_its_quota_reading() {
    let env = crate::testing::env().await;
    let app = &env.app;
    crate::testing::seed_project_config(&env).await;
    app.cfg.update(|f| { f.identities.push(ident("cc1", "claude", None)); Ok(()) }).await.unwrap();
    app.tools.lock().await.insert("local".into(), crate::tools::HostTools {
        tools: Default::default(), identities: Default::default(), shell_identities: vec![ident("cc1", "claude", None)],
        utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
    });
    app.quotas.lock().await.insert("claude:cc1".into(), reading("local"));
    delete_identity(State(app.clone()), Path("cc1".into()), Query(IdentityHostQuery { host: None })).await.unwrap();
    assert!(app.quotas.lock().await.contains_key("claude:cc1"), "shell 的 cc1 還在用這把 key");
}

/// 刪掉沒寫 host 的 `work`：mac_builder 上的 bot 正在用它（那台沒有自己的 `work`）→ 409，不能只看本機的 bot。
#[tokio::test]
async fn deleting_a_hostless_identity_is_refused_while_a_remote_bot_relies_on_it() {
    let env = crate::testing::env().await;
    let app = &env.app;
    let text = |own: &str| {
        format!(
            "[server]\nlisten = '127.0.0.1:7788'\n\n[[identities]]\nname = 'work'\nkind = 'codex'\n{own}\n\
                 [[projects]]\nid = 'p9'\npath = '/Users/me/wt'\nlabel = 'wt'\nhost = 'mac_builder'\n\n\
                 [[projects.bots]]\nid = 'b9'\nname = 'worker'\nkind = 'codex'\nidentity = 'work'\n"
        )
    };
    std::fs::write(&app.cfg.path, text("")).unwrap();
    app.cfg.update(|_| Ok(())).await.unwrap();
    crate::projection::project_config(&app.cfg, &app.db).await.unwrap();

    let del = || delete_identity(State(app.clone()), Path("work".into()), Query(IdentityHostQuery { host: None }));
    match del().await {
        Err(LcError::Conflict(body)) => assert_eq!((body["bot_id"].as_str(), body["host"].as_str()), (Some("b9"), Some("mac_builder"))),
        other => panic!("遠端 bot 還靠它：{:?}", other.map(|r| r.status())),
    }
    assert_eq!(app.cfg.get().await.identities.len(), 1, "什麼都沒刪");

    // mac_builder 有自己的 work：刪沒寫 host 的那筆不影響那台的 bot。
    std::fs::write(&app.cfg.path, text("\n[[identities]]\nname = 'work'\nkind = 'codex'\nhost = 'mac_builder'\n")).unwrap();
    app.cfg.update(|_| Ok(())).await.unwrap();
    crate::projection::project_config(&app.cfg, &app.db).await.unwrap();
    del().await.expect("那台用的是自己的 work");
    let left = app.cfg.get().await.identities;
    assert_eq!((left.len(), left[0].host.as_deref()), (1, Some("mac_builder")));
}

/// 並行：`create_bot` 驗過身份存在之後、寫進 config 之前，那個身份被刪掉。`projection::validate` 在 config 鎖裡重驗，
/// 所以不會建出帶孤兒身份的 bot（config 與 DB 都沒有它）；錯誤碼是 404 identity（以前是籠統的 400 `config_invalid`）。
#[tokio::test]
async fn a_bot_created_while_its_identity_is_deleted_is_refused() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let (spid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
    app.cfg
        .update(move |c| {
            c.identities.push(ident("racer", "codex", None));
            c.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, id: Some(spid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![], handed_off_to: None });
            Ok(())
        })
        .await
        .unwrap();
    let (a2, pid) = (app.clone(), env.project_id.clone());
    crate::lifecycle::race_point::arm("create_bot_after_identity_check", &env.project_id, move || async move {
        delete_identity(State(a2.clone()), Path("racer".into()), Query(IdentityHostQuery { host: None }))
            .await
            .expect("沒有 bot 在用，刪得掉");
        let _ = pid;
    });
    let body = json!({"name": "late", "kind": "codex", "identity": "racer"});
    let res = create_bot(State(app.clone()), Path(env.project_id.clone()), Json(serde_json::from_value(body).unwrap())).await;
    assert!(matches!(res, Err(LcError::NotFound(_))), "身份已被刪：不能建出帶孤兒身份的 bot：{:?}", res.map(|r| r.status()));
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE name = 'late'").fetch_one(&app.db).await.unwrap();
    assert_eq!(n, 0);
    assert!(app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).all(|b| b.name != "late"), "config 也不能留下它");
}

/// 並行：`delete_identity` 查完「沒有 bot 在用」之後、真的刪除之前，有 bot 開始用它。刪除在 config 鎖裡被 `projection::validate`
/// 擋下（身份還在、bot 也在）；錯誤碼是 409 identity still used by bots（以前是籠統的 400 `config_invalid`）。
#[tokio::test]
async fn an_identity_taken_by_a_bot_while_it_is_being_deleted_survives() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let (spid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
    app.cfg
        .update(move |c| {
            c.identities.push(ident("racer2", "codex", None));
            c.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, id: Some(spid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![], handed_off_to: None });
            Ok(())
        })
        .await
        .unwrap();
    let (a2, pid) = (app.clone(), env.project_id.clone());
    crate::lifecycle::race_point::arm("delete_identity_after_check", "racer2", move || async move {
        let body = json!({"name": "early", "kind": "codex", "identity": "racer2"});
        create_bot(State(a2.clone()), Path(pid), Json(serde_json::from_value(body).unwrap())).await.expect("身份還在：建得起來");
    });
    let res = delete_identity(State(app.clone()), Path("racer2".into()), Query(IdentityHostQuery { host: None })).await;
    assert!(matches!(res, Err(LcError::Conflict(_))), "有 bot 剛開始用它：不能刪：{:?}", res.map(|r| r.status()));
    assert!(app.cfg.get().await.identities.iter().any(|i| i.name == "racer2"), "身份還在");
}

/// 刪 mac_builder 那一台的 `work`：本機另有一筆自己的 `work`，它的快取列不能跟著消失（快取只在下次偵測才會補回來）。
#[tokio::test]
async fn deleting_one_hosts_identity_leaves_the_other_hosts_cached_row() {
    let env = crate::testing::env().await;
    let app = &env.app;
    std::fs::write(
        &app.cfg.path,
        "[server]\nlisten = '127.0.0.1:7788'\n\n[[identities]]\nname = 'work'\nkind = 'codex'\nhost = 'mac_builder'\n\n\
             [[identities]]\nname = 'work'\nkind = 'codex'\nhost = 'local'\n\n\
             [[projects]]\nid = 'p9'\npath = '/Users/me/wt'\nlabel = 'wt'\nhost = 'mac_builder'\n",
    )
    .unwrap();
    app.cfg.update(|_| Ok(())).await.unwrap();
    crate::projection::project_config(&app.cfg, &app.db).await.unwrap();
    for host in ["local", "mac_builder"] {
        let mut ht = crate::tools::HostTools {
            tools: Default::default(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        ht.identities.insert("work".into(), crate::tools::IdentityInfo::shell("work", "codex", None));
        app.tools.lock().await.insert(host.into(), ht);
    }
    delete_identity(State(app.clone()), Path("work".into()), Query(IdentityHostQuery { host: Some("mac_builder".into()) }))
        .await
        .expect("沒有 bot 在用");
    let tools = app.tools.lock().await;
    assert!(!tools["mac_builder"].identities.contains_key("work"), "被刪的那一台的快取列要拿掉");
    assert!(tools["local"].identities.contains_key("work"), "本機自己的 work 沒被刪，快取列不能一起消失");
}
