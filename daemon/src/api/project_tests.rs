use super::*;

#[tokio::test]
async fn patch_renames_the_project_and_rejects_a_blank_label() {
    let e = crate::testing::env().await;
    let (app, pid) = (e.app.clone(), e.project_id.clone());
    // `testing::env` only seeds the db row; the rename edits config.toml.
    app.cfg
        .update(|cfg| {
            cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                handed_off_to: None,
                id: Some(pid.clone()),
                path: e.repo.to_string_lossy().to_string(),
                label: "proj".into(),
                host: "local".into(),
                bots: vec![],
            });
            Ok(())
        })
        .await
        .unwrap();

    let res = patch_project(
        State(app.clone()),
        Path(pid.clone()),
        Extension(RequestPrincipal::User),
        Json(PatchProject { mode: None, manager_bot_id: None, label: Some("  改過的名字  ".into()), handed_off_to: None, ..Default::default() }),
    )
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let p = db::project(&app.db, &pid).await.unwrap().unwrap();
    assert_eq!(p.label, "改過的名字", "the label is trimmed and projected into the db");
    assert!(app.cfg.get().await.projects.iter().any(|x| x.label == "改過的名字"), "and written to config.toml");

    let err = patch_project(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), Json(PatchProject { mode: None, manager_bot_id: None, label: Some("   ".into()), handed_off_to: None, ..Default::default() }))
        .await
        .unwrap_err();
    assert!(matches!(err, LcError::Bad(_)), "blank label is a 400, got {err:?}");
    assert_eq!(db::project(&app.db, &pid).await.unwrap().unwrap().label, "改過的名字");

    let err = patch_project(State(app), Path("nope".into()), Extension(RequestPrincipal::User), Json(PatchProject { mode: None, manager_bot_id: None, label: Some("x".into()), handed_off_to: None, ..Default::default() }))
        .await
        .unwrap_err();
    assert!(matches!(err, LcError::NotFound(_)), "unknown project is a 404, got {err:?}");
}

/// #709：`GET /api/hosts` 的 `shared_session` 讀當下的設定；`POST /api/hosts` 沒帶這個欄位就沿用，不悄悄關掉。
#[tokio::test]
async fn the_shared_session_flag_is_listed_live_and_kept_when_a_host_update_omits_it() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    // 每個測試各自的 host 名（見 shared_host 的 unique_host）：行程級狀態以 host 名為 key，不能寫死 "sh1"。
    let host = crate::runners::am_base_tests::shared_host::tests::shared_host(&e, true).await.host;
    let flag = |app: Arc<App>| {
        let host = host.clone();
        async move { hosts_list(&app, &app.cfg.get().await).await.into_iter().find(|h| h["name"] == host.as_str()).unwrap()["shared_session"].clone() }
    };
    assert_eq!(flag(app.clone()).await, json!(true));

    let update = |shared: Option<bool>| NewHost {
        name: host.clone(),
        ssh: "sh1.invalid".into(),
        ssh_port: None,
        ssh_opts: None,
        herdr_session: Some("test".into()),
        remote_path: None,
        shared_session: shared,
    };
    create_host(State(app.clone()), Query(DeleteQuery::default()), Extension(RequestPrincipal::User), Json(update(None))).await.unwrap();
    assert!(app.cfg.get().await.hosts.iter().any(|h| h.name == host && h.shared_session), "沒帶就沿用");
    create_host(State(app.clone()), Query(DeleteQuery::default()), Extension(RequestPrincipal::User), Json(update(Some(false)))).await.unwrap();
    assert_eq!(flag(app.clone()).await, json!(false));
}

/// #1752：`POST /api/hosts` 更新既有主機時沒帶的選填欄位沿用舊值（`remote_path` 被清成空字串，mac_builder 的 PATH 就找不到 herdr）；
/// 明確帶了才改（包含明確帶空字串清掉）。
#[tokio::test]
async fn a_host_update_that_omits_optional_fields_keeps_the_stored_values() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    app.cfg
        .update(|f| {
            f.hosts.push(HostCfg { daemon_url: None,
                name: "keep1".into(),
                ssh: "keep1.invalid".into(),
                ssh_port: 2222,
                ssh_opts: vec!["-o".into(), "ConnectTimeout=5".into()],
                herdr_session: "agents-manager".into(),
                remote_path: "/opt/homebrew/bin:$HOME/.local/bin".into(),
                shared_session: false,
            });
            Ok(())
        })
        .await
        .unwrap();
    let stored = |app: Arc<App>| async move { app.cfg.get().await.hosts.into_iter().find(|h| h.name == "keep1").unwrap() };
    let post = |ssh_port, ssh_opts, herdr_session, remote_path| NewHost {
        name: "keep1".into(),
        ssh: "keep1.invalid".into(),
        ssh_port,
        ssh_opts,
        herdr_session,
        remote_path,
        shared_session: None,
    };
    create_host(State(app.clone()), Query(DeleteQuery::default()), Extension(RequestPrincipal::User), Json(post(None, None, None, None)))
        .await
        .unwrap();
    let kept = stored(app.clone()).await;
    assert_eq!(kept.remote_path, "/opt/homebrew/bin:$HOME/.local/bin", "沒帶 remote_path 不能被清空");
    assert_eq!(kept.ssh_port, 2222);
    assert_eq!(kept.ssh_opts, vec!["-o".to_string(), "ConnectTimeout=5".to_string()]);
    assert_eq!(kept.herdr_session, "agents-manager");

    create_host(
        State(app.clone()),
        Query(DeleteQuery::default()),
        Extension(RequestPrincipal::User),
        Json(post(Some(22), Some(vec![]), None, Some(String::new()))),
    )
    .await
    .unwrap();
    let cleared = stored(app.clone()).await;
    assert_eq!((cleared.ssh_port, cleared.remote_path.as_str(), cleared.ssh_opts.len()), (22, "", 0), "明確帶了就照帶的");
}

/// #708：`handed_off_to` 設得上、清得掉，寫進 config.toml、投影進 DB、`GET /api/state` 看得到；空字串與非字串是 400。
#[tokio::test]
async fn patch_hands_a_project_off_and_takes_it_back() {
    let e = crate::testing::env().await;
    let (app, pid) = (e.app.clone(), e.project_id.clone());
    let path = e.repo.to_string_lossy().to_string();
    app.cfg
        .update(move |cfg| {
            cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                id: Some(pid.clone()),
                path,
                label: "proj".into(),
                host: "local".into(),
                bots: vec![],
                handed_off_to: None,
            });
            Ok(())
        })
        .await
        .unwrap();
    let pid = e.project_id.clone();
    let patch = |v: Value| PatchProject { mode: None, manager_bot_id: None, label: None, handed_off_to: Some(v), ..Default::default() };
    let state_flag = |app: Arc<App>| async move {
        let s = state_json(&app).await.unwrap();
        s["projects"].as_array().unwrap()[0]["handed_off_to"].clone()
    };

    patch_project(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), Json(patch(json!(" agm-host ")))).await.unwrap();
    assert_eq!(db::project(&app.db, &pid).await.unwrap().unwrap().handed_off_to.as_deref(), Some("agm-host"));
    assert_eq!(app.cfg.get().await.projects[0].handed_off_to.as_deref(), Some("agm-host"), "written to config.toml");
    assert_eq!(state_flag(app.clone()).await, json!("agm-host"));

    for bad in [json!("  "), json!(3)] {
        let err = patch_project(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), Json(patch(bad))).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "got {err:?}");
    }
    patch_project(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), Json(PatchProject { mode: None, manager_bot_id: None, label: Some("renamed".into()), handed_off_to: None, ..Default::default() }))
        .await
        .unwrap();
    assert_eq!(state_flag(app.clone()).await, json!("agm-host"), "a label-only patch leaves the handoff alone");

    patch_project(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), Json(patch(Value::Null))).await.unwrap();
    assert!(db::project(&app.db, &pid).await.unwrap().unwrap().handed_off_to.is_none());
    assert!(app.cfg.get().await.projects[0].handed_off_to.is_none());
    assert_eq!(state_flag(app.clone()).await, Value::Null);
}

/// issue #73 reopen：需要 DB 才判得出來的大量軟刪閘門，現在也在**落盤之前**擋下非刪除的 mutation。
///
/// 情境是 config.toml 被外部改掉（少了 3 顆 bot，DB 還沒被投影追上——事故路徑，不是使用者剛刪完），
/// 這時 `patch_project` 只是改個 label、完全不碰 bot 列表。以前的順序是：`app.cfg.update` 先把
/// （已經跟 DB 對不上的）整份 config 落盤，`reproject` 投影當下才被閘門擋下，回 `config_written: true`——
/// TOML 已經被「合法化」寫入了。統一 commit boundary（`projection::update_and_project`）之後，
/// 閘門在同一個臨界區內、寫檔前就查過 DB 快照：擋下來時 TOML 位元組與 DB 都不變。
#[tokio::test]
async fn a_non_delete_mutation_the_bulk_guard_would_reject_leaves_the_file_and_the_db_untouched() {
    let e = crate::testing::env().await;
    let (app, pid) = (e.app.clone(), e.project_id.clone());
    let repo = e.repo.to_string_lossy().to_string();

    // 種 4 顆 bot 並投影進 DB：DB 的活列＝上一次投影的結果。
    app.cfg
        .update(|cfg| {
            cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                handed_off_to: None,
                id: Some(pid.clone()),
                path: repo.clone(),
                label: "proj".into(),
                host: LOCAL_HOST.into(),
                bots: ["b1", "b2", "b3", "b4"]
                    .into_iter()
                    .map(|id| crate::config::BotCfg { role: None, manager_use: true, track: None, runtime_override: None, fork_from: None,
                        id: Some(id.to_string()),
                        name: id.to_string(),
                        kind: "claude".into(),
                        model: None,
                        effort: None,
                        fast: false,
                        persona: None,
                        args: vec![],
                        autostart: false,
                        inject_hooks: true,
                        auto_approve: true,
                        identity: None,
                        env: Default::default(),
                        herdr_session: None,
                        create_request_id: None,
                        create_fingerprint: None,
                                    })
                    .collect(),
            });
            Ok(())
        })
        .await
        .unwrap();
    crate::projection::project_config(&app.cfg, &app.db).await.unwrap();
    assert_eq!(db::live_bots(&app.db).await.unwrap().len(), 4);

    // 外面把 config.toml 換掉：只剩 1 顆 bot。`ConfigStore::update` 會先重讀最新版，這裡先強迫
    // 重讀一次（不改任何東西，跟 projection.rs 的 `a_config_swapped_under_a_running_daemon_is_refused` 同一招）。
    let reduced = format!(
        "[server]\nlisten = '127.0.0.1:7788'\n\n[[projects]]\nid = '{pid}'\npath = '{repo}'\nlabel = 'proj'\nhost = 'local'\n\n[[projects.bots]]\nid = 'b1'\nname = 'b1'\nkind = 'claude'\n"
    );
    std::fs::write(&app.cfg.path, &reduced).unwrap();
    app.cfg.update(|_| Ok(())).await.unwrap();

    let err = patch_project(State(app.clone()), Path(pid.clone()), Extension(RequestPrincipal::User), Json(PatchProject { mode: None, manager_bot_id: None, label: Some("renamed".into()), handed_off_to: None, ..Default::default() }))
        .await
        .unwrap_err();
    match err {
        LcError::Conflict(body) => {
            assert_eq!(body["reason"], "projection_refused");
            assert_eq!(body["config_written"], false, "落盤前就被擋，跟身份 API 的 config_written:true 相反：{body}");
        }
        other => panic!("expected 409 projection_refused, got {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&app.cfg.path).unwrap(), reduced, "TOML 一個字都不能動");
    assert_eq!(app.cfg.get().await.projects[0].label, "proj", "記憶體裡那份也不能變（rename 沒套用）");
    assert_eq!(db::live_bots(&app.db).await.unwrap().len(), 4, "DB 不該被動到");
}
