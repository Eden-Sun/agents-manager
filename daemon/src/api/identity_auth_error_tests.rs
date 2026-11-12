use super::*;
use axum::response::IntoResponse;

async fn body_of(err: LcError) -> (StatusCode, Value) {
    let resp = err.into_response();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn with_identity(app: &Arc<App>, name: &str, kind: &str) {
    let ident = crate::config::IdentityCfg {
        name: name.into(),
        kind: kind.into(),
        host: None,
        env: Default::default(),
        args: vec![],
    };
    app.cfg
        .update(move |cfg| {
            cfg.identities.push(ident);
            Ok(())
        })
        .await
        .unwrap();
}

/// API.md：CLI 不在 PATH → `409 {"reason":"identity_login_unavailable"}`。
/// extra 裡不能再寫 `reason`，否則會蓋掉機器 key，前端對不到這條。
#[tokio::test]
async fn missing_cli_is_409_identity_login_unavailable() {
    let e = crate::testing::env().await;
    with_identity(&e.app, "cc1", "claude").await;
    let err = login_identity(State(e.app.clone()), Path(("local".into(), "cc1".into())), Extension(RequestPrincipal::User))
        .await
        .expect_err("no CLI on PATH must not open a pane");
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "conflict");
    assert_eq!(body["reason"], "identity_login_unavailable", "{body}");
    assert_eq!(body["identity"], "cc1");
    assert_eq!(body["kind"], "claude");
}

#[tokio::test]
async fn identity_auth_stops_before_opening_a_shell_when_remote_home_is_unreadable() {
    use std::sync::atomic::Ordering;

    let e = crate::testing::env().await;
    let host = format!("identity-api-home-616-{}", crate::db::ulid().to_ascii_lowercase());
    let conn = e.app.hosts.insert_remote_for_test(HostCfg { daemon_url: None,
        shared_session: false,
        name: host.clone(),
        ssh: "unused".into(),
        ssh_port: 22,
        ssh_opts: vec![],
        herdr_session: "agents-manager".into(),
        remote_path: String::new(),
    }).await;
    conn.connected.store(true, Ordering::SeqCst);
    e.app.cfg.update(|cfg| {
        cfg.identities.push(crate::config::IdentityCfg {
            name: "cx1".into(),
            kind: "codex".into(),
            host: Some(host.clone()),
            env: [("CODEX_HOME".into(), "~/.codex-cx1".into())].into(),
            args: vec![],
        });
        Ok(())
    }).await.unwrap();
    e.app.tools.lock().await.insert(host.clone(), crate::tools::HostTools {
        tools: [("codex".into(), crate::tools::ToolInfo { installed: true, path: Some("/usr/bin/codex".into()), version: None, logged_in: Some(true) })].into(),
        identities: Default::default(),
        shell_identities: vec![],
        utc_offset_secs: None,
        herdr_cli: None,
        checked_at: crate::db::now(),
    });
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let calls2 = calls.clone();
    crate::hosts::set_ssh_fake(&host, move |script| {
        calls2.lock().unwrap().push(script.to_string());
        Err(anyhow::anyhow!("injected remote HOME read failure"))
    });

    let err = identity_auth(e.app.clone(), host.clone(), "cx1".into(), false).await.expect_err("unknown HOME must not start identity authentication");
    assert!(matches!(err, LcError::Upstream(ref message) if message.contains("HOME")), "return a retryable upstream failure: {err:?}");
    assert_eq!(calls.lock().unwrap().len(), 1, "only HOME resolution may run; no auth pane command is sent");
    assert!(e.app.host_shells.lock().await.iter().all(|shell| shell.host != host), "no shell is opened on an unrelated path");
}

/// issue #544：`POST /api/hosts` 同時是新增與更新。把 `ssh` 改成指到**另一台機器**時，
/// 要跟 `delete_host` 一樣先確認主機上沒有活著的專案——`apply_config` 換掉連線但不動 `runs`，
/// 那些 run 還帶著舊機器的 pane id。只改 `remote_path` 不會換機器，照樣放行；`ssh_opts` 可能切換登入身份。
#[tokio::test]
async fn repointing_a_host_with_live_projects_is_refused_but_cosmetic_edits_are_not() {
    let e = crate::testing::env().await;
    let host = NewHost {
        name: "zz92".into(),
        ssh: "old-box".into(),
        ssh_port: None,
        ssh_opts: None,
        herdr_session: None,
        remote_path: None,
        shared_session: None,
    };
    let mk = |h: &NewHost| NewHost {
        name: h.name.clone(),
        ssh: h.ssh.clone(),
        ssh_port: h.ssh_port,
        ssh_opts: h.ssh_opts.clone(),
        herdr_session: h.herdr_session.clone(),
        remote_path: h.remote_path.clone(),
        shared_session: h.shared_session,
    };
    // 先把主機寫進 config（不必等它真的連上：這條測的是閘門，不是連線）。
    e.app.cfg
        .update(move |f| {
            f.hosts.push(HostCfg { daemon_url: None,
                shared_session: false,
                name: "zz92".into(),
                ssh: "old-box".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
            });
            Ok(())
        })
        .await
        .unwrap();
    // 這台上放一個活著的專案。
    crate::projection::update_and_project(&e.app.cfg, &e.app.db, |cfg| {
        cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
            handed_off_to: None,
            id: Some("01PROJZZ92".into()),
            path: "/srv/work".into(),
            label: "work".into(),
            host: "zz92".into(),
            bots: vec![],
        });
        Ok(())
    })
    .await
    .unwrap();

    // 1. 改 ssh（換機器）→ 409，而且什麼都沒寫進去。
    let mut repoint = mk(&host);
    repoint.ssh = "new-box".into();
    let err = create_host(State(e.app.clone()), Query(DeleteQuery::default()), Extension(RequestPrincipal::User), Json(repoint))
        .await
        .expect_err("repointing a host in use must be refused");
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "host_repoint_in_use", "{body}");
    assert_eq!(body["projects"][0]["id"], "01PROJZZ92", "{body}");
    let after = e.app.cfg.get().await.hosts.into_iter().find(|h| h.name == "zz92").unwrap();
    assert_eq!(after.ssh, "old-box", "被擋下來就不能改到 config：{body}");

    // 2. 只改不會換機器的欄位（remote_path）→ 放行。
    let mut cosmetic = mk(&host);
    cosmetic.remote_path = Some("/opt/bin".into());
    create_host(State(e.app.clone()), Query(DeleteQuery::default()), Extension(RequestPrincipal::User), Json(cosmetic))
        .await
        .expect("cosmetic edits are not a repoint");
    let after = e.app.cfg.get().await.hosts.into_iter().find(|h| h.name == "zz92").unwrap();
    assert_eq!((after.ssh.as_str(), after.remote_path.as_str()), ("old-box", "/opt/bin"));

    // 3. 明確確認就放行（沿用 ?confirm= 的先例）。
    let mut forced = mk(&host);
    forced.ssh = "new-box".into();
    create_host(State(e.app.clone()), Query(DeleteQuery { confirm: Some("repoint".into()) }), Extension(RequestPrincipal::User), Json(forced))
        .await
        .expect("?confirm=repoint is the documented escape hatch");
    let after = e.app.cfg.get().await.hosts.into_iter().find(|h| h.name == "zz92").unwrap();
    assert_eq!(after.ssh, "new-box");
}

#[tokio::test]
async fn xreview_deleting_a_host_is_refused_while_a_deleted_bots_remote_shim_is_unpurged() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let host = "zz92-cleanup";
    app.cfg
        .update(|f| {
            f.hosts.push(HostCfg { daemon_url: None,
                shared_session: false,
                name: host.into(),
                ssh: "unused-test-host".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
            });
            Ok(())
        })
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET host = ?, deleted_at = ? WHERE id = ?")
        .bind(host)
        .bind(crate::db::now())
        .bind(&e.project_id)
        .execute(&app.db)
        .await
        .unwrap();
    let bot = crate::testing::claude_bot(&app, &e.project_id, "deleted-on-remote").await;
    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
        .bind(crate::db::now())
        .bind(&bot.id)
        .execute(&app.db)
        .await
        .unwrap();
    crate::remote_purge::record(&app, &bot.id, host, false, Some("ssh unavailable")).await;

    let err = delete_host(State(app.clone()), Path(host.into()), Extension(RequestPrincipal::User))
        .await
        .expect_err(
        "do not forget the only host route to a deleted bot's still-live directory and shims",
    );
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "host_bot_cleanup_pending", "{body}");
    assert!(
        body["bot_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &bot.id),
        "{body}"
    );
    assert!(
        app.cfg.get().await.hosts.iter().any(|h| h.name == host),
        "a failed cleanup check must not remove the host config"
    );
}

/// #1320：專案搬到別台之後，舊主機上還欠著清理的已刪 bot 仍是那台主機的刪除條件（不能因為 `projects.host` 已經不是它就漏掉）。
#[tokio::test]
async fn a_deleted_bot_owed_on_the_old_host_still_blocks_that_host_after_its_project_moved() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (old, new) = ("zz92-moved-old", "zz92-moved-new");
    app.cfg
        .update(|f| {
            for name in [old, new] {
                f.hosts.push(HostCfg { daemon_url: None,
                    shared_session: false,
                    name: name.into(),
                    ssh: "unused-test-host".into(),
                    ssh_port: 22,
                    ssh_opts: vec![],
                    herdr_session: "agents-manager".into(),
                    remote_path: String::new(),
                });
            }
            Ok(())
        })
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(old).bind(&e.project_id).execute(&app.db).await.unwrap();
    let bot = crate::testing::claude_bot(&app, &e.project_id, "deleted-then-moved").await;
    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(crate::db::now()).bind(&bot.id).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(new).bind(&e.project_id).execute(&app.db).await.unwrap();

    assert!(host_bots_requiring_attention(&app, old).await.unwrap().contains(&bot.id), "舊主機還欠著它的目錄，不能拿掉");
    assert!(!host_bots_requiring_attention(&app, new).await.unwrap().contains(&bot.id), "新主機不因為它被擋");
}

#[tokio::test]
async fn xreview_deleting_a_host_is_refused_for_a_live_orphan_bot_in_a_deleted_project() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let host = "zz92-orphan";
    app.cfg
        .update(|f| {
            f.hosts.push(HostCfg { daemon_url: None,
                shared_session: false,
                name: host.into(),
                ssh: "unused-test-host".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
            });
            Ok(())
        })
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET host = ?, deleted_at = ? WHERE id = ?")
        .bind(host)
        .bind(crate::db::now())
        .bind(&e.project_id)
        .execute(&app.db)
        .await
        .unwrap();
    let bot = crate::testing::claude_bot(&app, &e.project_id, "orphan-on-remote").await;
    crate::testing::fake_run(&app, &bot.id).await;

    let err = delete_host(State(app.clone()), Path(host.into()), Extension(RequestPrincipal::User))
        .await
        .expect_err("a live orphan run may still be using its remote bot shim");
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "host_bot_cleanup_pending", "{body}");
    assert!(
        body["bot_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &bot.id),
        "{body}"
    );
    assert!(app.cfg.get().await.hosts.iter().any(|h| h.name == host));
}

#[tokio::test]
async fn xreview_delete_host_rechecks_projects_while_removing_the_host_config() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let host = "zz92-config-project-race";
    app.cfg
        .update(|f| {
            f.hosts.push(HostCfg { daemon_url: None,
                shared_session: false,
                name: host.into(),
                ssh: "unused-test-host".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
            });
            f.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                id: Some("01CONFIGRACE".into()),
                path: "/tmp/xreview-host-config-race".into(),
                label: "config race".into(),
                host: host.into(),
                bots: vec![],
                handed_off_to: None,
            });
            Ok(())
        })
        .await
        .unwrap();
    // Model a project commit after the early DB snapshot. Host removal shares project
    // creation's config lock, so this staged config entry must be detected before removal.

    let err = delete_host(State(app.clone()), Path(host.into()), Extension(RequestPrincipal::User))
        .await
        .expect_err(
            "the config-lock recheck must catch a project published after the DB snapshot",
        );
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["project_id"], "01CONFIGRACE", "{body}");
    assert!(
        app.cfg.get().await.hosts.iter().any(|h| h.name == host),
        "failed removal must preserve the host config"
    );
}

#[tokio::test]
async fn xreview_repoint_confirmation_covers_an_active_orphan_run() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let host = "zz92-repoint-orphan";
    app.cfg
        .update(|f| {
            f.hosts.push(HostCfg { daemon_url: None,
                shared_session: false,
                name: host.into(),
                ssh: "old-test-target".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
            });
            Ok(())
        })
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET host = ?, deleted_at = ? WHERE id = ?")
        .bind(host)
        .bind(crate::db::now())
        .bind(&e.project_id)
        .execute(&app.db)
        .await
        .unwrap();
    let bot = crate::testing::claude_bot(&app, &e.project_id, "orphan-run").await;
    crate::testing::fake_run(&app, &bot.id).await;
    crate::hosts::set_ssh_fake(host, |_script| {
        anyhow::bail!("test guard: never contact a remote host")
    });

    let err = create_host(
        State(app.clone()),
        Query(DeleteQuery::default()),
        Extension(RequestPrincipal::User),
        Json(NewHost {
            name: host.into(),
            ssh: "new-test-target".into(),
            ssh_port: None,
            ssh_opts: None,
            herdr_session: None,
            remote_path: None,
            shared_session: None,
        }),
    )
    .await
    .expect_err("an active run hidden under a deleted project must still require explicit repoint confirmation");
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "host_repoint_in_use", "{body}");
    assert_eq!(body["bot_ids"][0], bot.id, "{body}");
    assert_eq!(
        app.cfg
            .get()
            .await
            .hosts
            .iter()
            .find(|h| h.name == host)
            .unwrap()
            .ssh,
        "old-test-target"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn deleting_a_host_keeps_a_connection_recreated_after_its_config_removal() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let host = format!("race-{}", &db::ulid().to_ascii_lowercase()[..20]);
    app.cfg
        .update({
            let host = host.clone();
            move |f| {
                f.hosts.push(HostCfg { daemon_url: None,
                    shared_session: false,
                    name: host,
                    ssh: "old-test-target".into(),
                    ssh_port: 22,
                    ssh_opts: vec![],
                    herdr_session: "agents-manager".into(),
                    remote_path: String::new(),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    crate::hosts::set_ssh_fake(&host, |_script| Err(anyhow::anyhow!("test fake: no SSH")));

    let (connected_app, connected_host) = (app.clone(), host.clone());
    crate::lifecycle::race_point::arm("create_host_after_apply_config", &host, move || async move {
        if let Some(conn) = connected_app.hosts.get(&connected_host).await {
            conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let recreate_task = Arc::new(tokio::sync::Mutex::new(None));
    let callback_task = recreate_task.clone();
    let (recreate_app, recreate_host) = (app.clone(), host.clone());
    crate::lifecycle::race_point::arm("delete_host_after_config_remove", &host, move || async move {
        let task = tokio::spawn(async move {
            create_host(
                State(recreate_app),
                Query(DeleteQuery::default()),
                Extension(RequestPrincipal::User),
                Json(NewHost {
                    name: recreate_host,
                    ssh: "new-test-target".into(),
                    ssh_port: None,
                    ssh_opts: None,
                    herdr_session: None,
                    remote_path: None,
                    shared_session: None,
                }),
            )
            .await
        });
        *callback_task.lock().await = Some(task);
        tokio::task::yield_now().await;
    });

    delete_host(State(app.clone()), Path(host.clone()), Extension(RequestPrincipal::User))
        .await
        .expect("the in-flight delete itself succeeds");
    recreate_task
        .lock()
        .await
        .take()
        .expect("the overlapping POST started")
        .await
        .expect("the overlapping POST task completed")
        .expect("a create queued behind the delete can restore the host");

    assert!(app.cfg.get().await.hosts.iter().any(|h| h.name == host), "the later create committed the host config");
    assert!(app.hosts.get(&host).await.is_some(), "DELETE must not remove a connection installed by the later create");
}

/// issue #1384：不認得的主機一律 404 `what:"host"`（API.md §1，同其他 17 處）；那句話放 message，不塞進 what。
#[tokio::test]
async fn an_unknown_host_on_the_mem_and_quota_endpoints_is_404_host_like_the_rest() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let q = |pairs: &[(&str, &str)]| Query(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<String, String>>());
    let errs = [
        ("mem processes", get_mem_processes(State(app.clone()), q(&[("host", "no-such-host")])).await.err()),
        ("mem pane", get_mem_pane(State(app.clone()), q(&[("host", "no-such-host"), ("pane_id", "p1")])).await.err()),
        ("mem kill", kill_mem_process(State(app.clone()), Json(json!({"host": "no-such-host", "pid": 4242}))).await.err()),
        ("quota", get_quota(State(app.clone()), q(&[("refresh", "1"), ("host", "no-such-host")])).await.err()),
    ];
    for (name, err) in errs {
        let (status, body) = body_of(err.unwrap_or_else(|| panic!("{name} 應該回 404"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name}: {body}");
        assert_eq!(body["what"], "host", "{name}：what 是資源名：{body}");
        assert!(body["message"].as_str().unwrap_or_default().contains("no-such-host"), "{name}：原本那句話留在 message：{body}");
    }
}

/// API.md：host 不存在是 404。沒寫 host 的身份在遠端也生效，所以找不到主機時不能先走到
/// 「CLI 不在 PATH」那條 409——呼叫端會去裝 CLI，其實主機根本沒這台。
#[tokio::test]
async fn unknown_host_is_404_host_not_cli_missing() {
    let e = crate::testing::env().await;
    with_identity(&e.app, "work", "codex").await;
    let err = login_identity(State(e.app.clone()), Path(("no-such-host".into(), "work".into())), Extension(RequestPrincipal::User))
        .await
        .expect_err("unknown host");
    let (status, body) = body_of(err).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "not_found");
    assert_eq!(body["what"], "host", "{body}");
}
