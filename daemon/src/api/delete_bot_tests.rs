use super::*;

async fn a_bot(e: &crate::testing::Env, name: &str, managed_by: &str) -> String {
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
    )
    .bind(&id)
    .bind(&e.project_id)
    .bind(name)
    .bind(managed_by)
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    id
}

/// user bot 由 config.toml 管：測試要把它寫進 config，刪除才會在 TOML 裡找到目標。
async fn in_config(e: &crate::testing::Env, bots: &[(&str, &str)]) {
    let (pid, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
    let bots: Vec<crate::config::BotCfg> = bots
        .iter()
        .map(|(id, name)| {
            toml::from_str(&format!("id = '{id}'\nname = '{name}'\nkind = 'claude'\n")).unwrap()
        })
        .collect();
    e.app
        .cfg
        .update(move |cfg| {
            cfg.projects = vec![crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                handed_off_to: None,
                id: Some(pid),
                path: repo,
                label: "proj".into(),
                host: crate::config::LOCAL_HOST.into(),
                bots,
            }];
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn deleting_a_project_revokes_its_shared_bot_capabilities() {
    let e = crate::testing::env().await;
    let bot_id = a_bot(&e, "shared-project-bot", "user").await;
    in_config(&e, &[(&bot_id, "shared-project-bot")]).await;
    let root = crate::testing::scratch_dir("api-project-share-root");
    let root = root.to_string_lossy().into_owned();
    e.app.cfg.update(|cfg| {
        cfg.share.folders_root = Some(root.clone());
        Ok(())
    }).await.unwrap();
    let folder = crate::share::folder::ShareFolderIn::New { name: "shared-project-bot".into() };
    let (workspace, created_folder) = crate::share::admin::reserve_restricted(&e.app, &bot_id, &folder, false).await.unwrap();
    crate::share::admin::finish_restricted(&e.app, &bot_id, &workspace, created_folder, true).await;
    let token = crate::share::store::enable(&e.app.db, &bot_id).await.unwrap().unwrap();

    delete_project(State(e.app.clone()), Path(e.project_id.clone())).await.unwrap();

    assert_eq!(crate::share::store::resolve(&e.app.db, &token).await.unwrap(), None);
    assert!(crate::share::store::share(&e.app.db, &bot_id).await.unwrap().is_none(), "project deletion removes the token row");
}

async fn a_running_run(app: &Arc<App>, bot_id: &str) -> String {
    let run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','pane-x','agent','no-such-session',?)",
    )
    .bind(&run)
    .bind(bot_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    run
}

fn reason(e: LcError) -> String {
    match e {
        LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
        other => format!("{other:?}"),
    }
}

/// 閘門沒過就什麼都不動：不停 bot、不停也不刪 child、不 purge、config 不寫（sol 三輪）。
/// 1 個專案、2 顆 bot，TOML 被外部拿掉 b2 之後刪 b1：小資料集也不能靠「沒到門檻」放行。
#[tokio::test]
async fn a_refused_delete_stops_and_removes_nothing() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    let b2 = a_bot(&e, "bravo", "user").await;
    let child = a_bot(&e, "alfa-kid", "child").await;
    sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&b1).bind(&child).execute(&app.db).await.unwrap();
    let (r1, rc) = (a_running_run(&app, &b1).await, a_running_run(&app, &child).await);
    in_config(&e, &[(&b1, "alfa")]).await; // bravo 被外部拿掉了

    let err = delete_bot(State(app.clone()), Path(b1.clone())).await.unwrap_err();
    assert_eq!(reason(err), "delete_refused");
    for (bot, run) in [(&b1, &r1), (&child, &rc)] {
        assert!(db::bot(&app.db, bot).await.unwrap().unwrap().deleted_at.is_none(), "{bot} 不該被刪");
        assert_eq!(db::run(&app.db, run).await.unwrap().unwrap().state, "running", "{bot} 不該被停");
    }
    assert!(db::bot(&app.db, &b2).await.unwrap().unwrap().deleted_at.is_none());
    assert!(app.cfg.get().await.projects[0].bots.iter().any(|b| b.id.as_deref() == Some(b1.as_str())), "config 沒寫");
}

/// 停機期間外部改 TOML：定案已經在停機之前做完，所以不會 409、不會留下「已停、未刪」（sol 四輪）。
/// 舊的「預檢 → 停 → 再定案」在這裡會因為 bravo 不見了而 409，留下停掉卻沒刪的 alfa。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_toml_changing_while_the_bot_stops_does_not_strand_it() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (b1, b2) = (a_bot(&e, "alfa", "user").await, a_bot(&e, "bravo", "user").await);
    in_config(&e, &[(&b1, "alfa"), (&b2, "bravo")]).await;
    let run = crate::testing::fake_run(&app, &b1).await; // session `test`＝mock herdr，stop 會真的送鍵

    let task = tokio::spawn({
        let (app, b1) = (app.clone(), b1.clone());
        async move { delete_bot(State(app), Path(b1)).await.map(|_| ()).map_err(reason) }
    });
    let _ = crate::testing::eventually!(e.herdr.methods().iter().any(|m| m == "agent.send_keys"));
    assert!(e.herdr.methods().iter().any(|m| m == "agent.send_keys"), "stop 應該已經開始");
    // 停機進行中，外面把 bravo 從 TOML 拿掉。
    let text = std::fs::read_to_string(&app.cfg.path).unwrap();
    let pruned: crate::config::ConfigFile = {
        let mut c: crate::config::ConfigFile = toml::from_str(&text).unwrap();
        c.projects[0].bots.retain(|b| b.id.as_deref() != Some(b2.as_str()));
        c
    };
    std::fs::write(&app.cfg.path, toml::to_string(&pruned).unwrap()).unwrap();

    assert_eq!(task.await.unwrap(), Ok(()), "定案在停機前已經做完，不該 409");
    assert!(db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_some(), "alfa 刪掉了");
    assert_ne!(db::run(&app.db, &run).await.unwrap().unwrap().state, "running", "而且停了");
    assert!(db::bot(&app.db, &b2).await.unwrap().unwrap().deleted_at.is_none(), "bravo 沒被順手刪掉");
}

/// 刪專案與 start 並發：start 的關鍵段是「拿 per-bot 鎖 → 確認沒被刪 → 建 run」（`start_bot_locked_with`，
/// 中間還有開 workspace 那段時間）。刪專案若在鎖外檢查 stopped，會刪掉剛啟動的 bot（sol 四輪）。
/// 不變式：不會出現「bot 已刪、run 還在跑」。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_delete_never_removes_a_bot_that_just_started() {
    for round in 0..10 {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;

        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let starter = tokio::spawn({
            let (app, b1, gate) = (app.clone(), b1.clone(), gate.clone());
            async move {
                gate.wait().await;
                let lock = app.bot_lock(&b1).await;
                let _g = lock.lock().await;
                if db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_some() {
                    return false;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await; // 開 workspace 那段
                a_running_run(&app, &b1).await;
                true
            }
        });
        let deleter = tokio::spawn({
            let (app, pid, gate) = (app.clone(), e.project_id.clone(), gate.clone());
            async move {
                gate.wait().await;
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                delete_project(State(app), Path(pid)).await.map(|_| ()).map_err(reason)
            }
        });
        let started = starter.await.unwrap();
        let deleted = deleter.await.unwrap();
        let bot_gone = db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_some();
        let running = db::active_run(&app.db, &b1).await.unwrap().is_some();
        assert!(!(bot_gone && running), "round {round}: 刪掉了正在跑的 bot（started={started} deleted={deleted:?}）");
        assert_eq!(started, running, "round {round}");
        assert_eq!(deleted.is_ok(), bot_gone, "round {round}: {deleted:?}");
    }
}

/// child 的軟刪 DB 寫入失敗時，不能繼續砍它的 runtime 目錄：以前那個 `UPDATE` 的結果被
/// `let _ =` 吃掉，寫不進去也照樣 purge，變成「DB 說它還活著、檔案已經被砍光」，事後救不回來
/// （issue #87）。整支 API 要跟著失敗，DB 那一列也要維持 `deleted_at IS NULL`。
#[tokio::test]
async fn a_child_whose_soft_delete_write_fails_keeps_its_runtime_dir() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&b1, "alfa")]).await;
    let child = a_bot(&e, "alfa-kid", "child").await;

    let dir = app.bot_dir(&child).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("marker"), b"keep me").unwrap();

    // 故障注入：只讓這顆 child 的軟刪 UPDATE 失敗，其他查詢不受影響。
    sqlx::query(&format!(
        "CREATE TRIGGER am_test_fail_child_delete BEFORE UPDATE OF deleted_at ON bots
             WHEN NEW.id = '{child}' BEGIN SELECT RAISE(ABORT, 'boom'); END"
    ))
    .execute(&app.db)
    .await
    .unwrap();

    assert!(delete_project(State(app.clone()), Path(e.project_id.clone())).await.is_err(), "DB 寫不進去，API 不能回成功");
    assert!(dir.join("marker").exists(), "DB 寫不進去卻把 runtime 目錄砍了");
    assert!(db::bot(&app.db, &child).await.unwrap().unwrap().deleted_at.is_none(), "DB 沒寫成功，不該說它已經刪除");
}

/// #284：第一顆 child 的軟刪寫不進去時，後面的 child 仍要收掉，失敗的那顆背景重試到成功——專案已經定案刪除，
/// 使用者再按一次只會 not_in_config，不能留下永遠沒人回收的活 bot。
#[tokio::test]
async fn a_child_that_fails_to_soft_delete_does_not_strand_its_siblings() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&b1, "alfa")]).await;
    let mut kids = vec![];
    for n in ["alfa-k1", "alfa-k2", "alfa-k3"] {
        kids.push(a_bot(&e, n, "child").await);
    }
    // 「寫不進去」要綁在 intent 的嘗試次數上，不能等測試自己 DROP TRIGGER：測試模式背景重試每 20ms 一次、
    // 最多 MAX_ATTEMPTS 次，負載一高，斷言還沒跑完就把次數用光、intent 收成 failed，之後再也沒人補（#773）。
    // handler 認領＝第 1 次、背景第 1 次重試＝第 2 次都失敗；第 3 次才寫得進去。
    sqlx::query(&format!(
        "CREATE TRIGGER am_test_fail_first BEFORE UPDATE OF deleted_at ON bots WHEN NEW.id = '{}'
               AND (SELECT attempts FROM intents WHERE kind = 'delete_project' AND subject_id = '{}') < 3
             BEGIN SELECT RAISE(ABORT, 'boom'); END",
        kids[0], e.project_id
    ))
    .execute(&app.db)
    .await
    .unwrap();
    assert!(delete_project(State(app.clone()), Path(e.project_id.clone())).await.is_err());
    for k in &kids[1..] {
        assert!(db::bot(&app.db, k).await.unwrap().unwrap().deleted_at.is_some(), "失敗的那顆後面的 child 也要收掉");
    }
    assert!(crate::testing::eventually!(db::bot(&app.db, &kids[0]).await.unwrap().unwrap().deleted_at.is_some()), "寫得進去之後背景補上");
    let intent = || async {
        sqlx::query_as::<_, (String, i64)>("SELECT status, attempts FROM intents WHERE kind = 'delete_project' AND subject_id = ?")
            .bind(&e.project_id)
            .fetch_one(&app.db)
            .await
            .unwrap()
    };
    assert!(crate::testing::eventually!(intent().await.0 == "done"), "補完之後 intent 收成 done：{:?}", intent().await);
    assert_eq!(intent().await.1, 3, "真的經過一次失敗的背景重試才補上");
}

/// #296：定案之後某顆 child 的軟刪寫不進去，不能 early return 把母 bot 的 run 留著不停。
#[tokio::test]
async fn a_child_soft_delete_failure_after_the_decision_does_not_strand_the_parent_run() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let parent = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&parent, "alfa")]).await;
    let child = a_bot(&e, "alfa-kid", "child").await;
    sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&child).execute(&app.db).await.unwrap();
    crate::testing::fake_run(&app, &parent).await;
    sqlx::query(&format!(
        "CREATE TRIGGER am_test_fail_kid BEFORE UPDATE OF deleted_at ON bots WHEN NEW.id = '{child}' BEGIN SELECT RAISE(ABORT, 'boom'); END"
    ))
    .execute(&app.db)
    .await
    .unwrap();
    let res = delete_bot(State(app.clone()), Path(parent.clone())).await;
    assert!(res.is_ok(), "定案之後的失敗記進 kept_dirs，不能回錯：{:?}", res.err().map(|e| format!("{e:?}")));
    assert!(db::active_run(&app.db, &parent).await.unwrap().is_none(), "母 bot 已刪，它的 run 一定要停");
    sqlx::query("DROP TRIGGER am_test_fail_kid").execute(&app.db).await.unwrap();
    assert!(crate::testing::eventually!(db::bot(&app.db, &child).await.unwrap().unwrap().deleted_at.is_some()), "寫得進去之後背景補上");
}

/// #298：專案已刪的 child 不能還原成看不到的活 bot。
#[tokio::test]
async fn a_child_of_a_deleted_project_cannot_be_restored() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&b1, "alfa")]).await;
    let child = a_bot(&e, "alfa-kid", "child").await;
    delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap();
    assert!(restore_bot(State(app.clone()), Path(child.clone())).await.is_err());
    assert!(db::bot(&app.db, &child).await.unwrap().unwrap().deleted_at.is_some());
}

/// Project deletion has no per-project lock. Its liveness check can pass, then the project can be deleted before the child row is restored.
#[tokio::test]
async fn a_project_deleted_after_the_child_restore_check_still_blocks_the_restore() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let parent = a_bot(&e, "live-parent", "user").await;
    let child = a_bot(&e, "deleted-child-race", "child").await;
    sqlx::query("UPDATE bots SET parent_bot_id = ?, deleted_at = ? WHERE id = ?")
        .bind(&parent)
        .bind(db::now())
        .bind(&child)
        .execute(&app.db)
        .await
        .unwrap();

    let (a, project) = (app.clone(), e.project_id.clone());
    crate::lifecycle::race_point::arm("restore_child_after_liveness_check", &child, move || async move {
        sqlx::query("UPDATE projects SET deleted_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(project)
            .execute(&a.db)
            .await
            .unwrap();
    });
    let restored = restore_bot(State(app.clone()), Path(child.clone())).await;
    assert!(restored.is_err(), "the project ceased to be live after the check, so restore must not succeed");
    assert!(db::bot(&app.db, &child).await.unwrap().unwrap().deleted_at.is_some(), "child must remain deleted");
}

#[tokio::test]
async fn a_parent_deleted_after_the_child_restore_check_still_blocks_the_restore() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let parent = a_bot(&e, "parent-live-at-check", "user").await;
    let child = a_bot(&e, "deleted-child-parent-race", "child").await;
    sqlx::query("UPDATE bots SET parent_bot_id = ?, deleted_at = ? WHERE id = ?")
        .bind(&parent)
        .bind(db::now())
        .bind(&child)
        .execute(&app.db)
        .await
        .unwrap();

    let (a, parent_id) = (app.clone(), parent.clone());
    crate::lifecycle::race_point::arm("restore_child_after_liveness_check", &child, move || async move {
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(parent_id)
            .execute(&a.db)
            .await
            .unwrap();
    });
    let restored = restore_bot(State(app.clone()), Path(child.clone())).await;
    assert!(restored.is_err(), "the parent ceased to be live after the check, so restore must not succeed");
    assert!(db::bot(&app.db, &child).await.unwrap().unwrap().deleted_at.is_some(), "child must remain deleted");
}

/// #757：刪母 bot 會連它的 child 一起軟刪；復原母 bot 之後 child 仍是已刪、可以再個別復原
/// （母 bot 活了就過得了 #298 的檢查）。母 bot 復原後沒有 run（不自動重開），清單裡也不再有它。
#[tokio::test]
async fn restoring_a_deleted_parent_leaves_its_children_deleted_but_restorable() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let parent = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&parent, "alfa")]).await;
    let kid = a_bot(&e, "alfa-kid", "child").await;
    sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&kid).execute(&app.db).await.unwrap();

    delete_bot(State(app.clone()), Path(parent.clone())).await.unwrap();
    assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_some(), "child 跟著母 bot 一起軟刪");

    restore_bot(State(app.clone()), Path(parent.clone())).await.unwrap();
    assert!(db::bot(&app.db, &parent).await.unwrap().unwrap().deleted_at.is_none());
    assert!(db::active_run(&app.db, &parent).await.unwrap().is_none(), "復原不自動開 run");
    assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_some(), "child 不跟著復原（由父 bot／AGM 自己處理）");

    restore_bot(State(app.clone()), Path(kid.clone())).await.expect("母 bot 活了，child 可以個別復原");
    assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_none());
}

/// A restore is deliberately stopped, not auto-started. A restart intent left in its retry window must not undo that choice.
#[tokio::test]
async fn restoring_a_deleted_bot_abandons_its_pre_delete_restart_intent() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let bot = a_bot(&e, "restore-with-pending-restart", "user").await;
    in_config(&e, &[(&bot, "restore-with-pending-restart")]).await;
    let run = crate::testing::fake_run(&app, &bot).await;
    let intent = match crate::intents::insert(
        &app.db,
        "restart",
        &bot,
        crate::config::LOCAL_HOST,
        &json!({"opts": {"resume_native": true}, "from_run_id": run}),
        900,
    )
    .await
    .unwrap()
    {
        crate::intents::Inserted::New(i) => i.id,
        crate::intents::Inserted::AlreadyOpen(_) => panic!("test starts without an earlier restart"),
    };

    delete_bot(State(app.clone()), Path(bot.clone())).await.unwrap();
    assert!(db::active_run(&app.db, &bot).await.unwrap().is_none(), "delete ended the old run");
    restore_bot(State(app.clone()), Path(bot.clone())).await.unwrap();

    let status: String = sqlx::query_scalar("SELECT status FROM intents WHERE id = ?").bind(&intent).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "abandoned", "restoring does not authorize replaying the restart that was pending before deletion");
    assert!(db::active_run(&app.db, &bot).await.unwrap().is_none(), "restore remains stopped");
}

/// #301：delete_bot 定案後、停機／purge 之前，restore_bot 必須等鎖，不能插進來。
#[tokio::test]
async fn a_restore_waits_for_the_delete_that_holds_the_bot_lock() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let id = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&id, "alfa")]).await;
    let outcome = Arc::new(std::sync::Mutex::new(None));
    let (a, oid, o2) = (app.clone(), id.clone(), outcome.clone());
    crate::lifecycle::race_point::arm("delete_bot_after_decided", &id, move || async move {
        let r = tokio::time::timeout(std::time::Duration::from_millis(300), restore_bot(State(a), Path(oid))).await;
        *o2.lock().unwrap() = Some(r.is_ok());
    });
    delete_bot(State(app.clone()), Path(id.clone())).await.unwrap();
    assert_eq!(*outcome.lock().unwrap(), Some(false), "delete 還持著鎖的時候，restore 不能完成");
    restore_bot(State(app.clone()), Path(id.clone())).await.unwrap();
}

/// 輸入框草稿跟著 bot／專案走：刪掉之後 `GET /api/drafts` 不再列它，其他瀏覽器收到清除事件。
#[tokio::test]
async fn deleting_a_bot_or_project_clears_its_composer_drafts() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    let b2 = a_bot(&e, "bravo", "user").await;
    in_config(&e, &[(&b1, "alfa"), (&b2, "bravo")]).await;
    let (k1, k2, kg) = (format!("bot:{b1}"), format!("bot:{b2}"), format!("group:{}", e.project_id));
    for k in [&k1, &k2, &kg] {
        crate::drafts::put(&app.db, k, "草稿").await.unwrap();
    }
    let mut rx = app.subscribe();
    delete_bot(State(app.clone()), Path(b1.clone())).await.unwrap();
    let keys: Vec<String> = crate::drafts::list(&app.db).await.unwrap().into_iter().map(|d| d.key).collect();
    assert_eq!(keys, {
        let mut v = vec![k2.clone(), kg.clone()];
        v.sort();
        v
    });
    let mut cleared = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.kind == "draft_updated" {
            assert_eq!(ev.data["text"], "");
            cleared.push(ev.data["key"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(cleared, [k1.clone()]);
    delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap();
    assert!(crate::drafts::list(&app.db).await.unwrap().is_empty(), "專案刪除帶走群組草稿與剩下那顆 bot 的草稿");
}

/// #313：刪專案要一併清掉 user bot 的 runtime 目錄；清不掉（遠端主機不明）不能當成清掉了。
#[tokio::test]
async fn deleting_a_project_purges_its_user_bot_dirs_and_reports_failures() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&b1, "alfa")]).await;
    let dir = runtime_dir(&app, &b1);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("marker"), b"x").unwrap();
    let out = body_of(delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap()).await;
    assert!(!dir.exists(), "user bot 的目錄要跟著清掉");
    assert!(out.get("kept_dirs").is_none(), "{out}");

    // 遠端專案、主機不明：ssh 清不了，不能回成「清掉了」。
    let e2 = crate::testing::env().await;
    let app2 = e2.app.clone();
    let b2 = a_bot(&e2, "beta", "user").await;
    in_config(&e2, &[(&b2, "beta")]).await;
    sqlx::query("UPDATE projects SET host='ghost' WHERE id=?").bind(&e2.project_id).execute(&app2.db).await.unwrap();
    let out = body_of(delete_project(State(app2.clone()), Path(e2.project_id.clone())).await.unwrap()).await;
    assert_eq!(out["kept_dirs"], json!([{"bot_id": b2, "reason": "purge_failed"}]), "{out}");
}

/// 快照（鎖之前列的 bot）之後才被認領進專案的 child：以前不在快照裡，專案定案刪除後它的列仍是 `deleted_at IS NULL`——
/// UI 看不到、reconcile 也不掃（專案已刪），pane 與目錄永遠沒人收。定案之後要再列一次，連它一起軟刪。
#[tokio::test]
async fn a_child_adopted_after_the_snapshot_is_soft_deleted_with_the_project() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&b1, "alfa")]).await;
    let late = db::ulid();
    {
        let (app, pid, late) = (app.clone(), e.project_id.clone(), late.clone());
        // 快照與定案之間（持久 intent 寫完、config 還沒改）：另一條路認領了一顆新的 child。
        crate::lifecycle::race_point::arm("delete_after_intent", &e.project_id, move || async move {
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
                     VALUES (?,?,'late-kid','claude','[]',0,1,'tok','child',?,?)",
            )
            .bind(&late)
            .bind(&pid)
            .bind(&b1)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        });
    }
    delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap();
    assert!(deleted(&app, &late).await, "專案刪了，快照之後才出現的 child 不能留成活列");
}

/// 同一個窗口提早一步：列完 bot、還沒拿鎖時被認領的 child。拿到鎖之後重列發現變了，放掉重來，這一顆就進了快照（有 intent、
/// 有鎖、有「都已停止」的檢查），而不是靠定案之後的補收。
#[tokio::test]
async fn a_child_adopted_before_the_locks_are_taken_joins_the_snapshot() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&b1, "alfa")]).await;
    let early = db::ulid();
    {
        let (app, pid, early) = (app.clone(), e.project_id.clone(), early.clone());
        crate::lifecycle::race_point::arm("delete_project_before_lock", &e.project_id, move || async move {
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
                     VALUES (?,?,'early-kid','claude','[]',0,1,'tok','child',?,?)",
            )
            .bind(&early)
            .bind(&pid)
            .bind(&b1)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        });
    }
    delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap();
    assert!(deleted(&app, &early).await);
    let snap: String = sqlx::query_scalar("SELECT payload_json FROM intents WHERE subject_id = ? AND kind = 'delete_project'")
        .bind(&e.project_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert!(snap.contains(&early), "重列之後它在持久 intent 的快照裡：{snap}");
}

// ---- #355 P3：刪除在定案之後（或之前）行程死掉，開機補完 ----

/// 模擬行程死亡：刪除走到 `point` 就卡住，再把整個 future abort 掉。
async fn die_in_delete(e: &crate::testing::Env, key: &str, point: &'static str, project: bool) {
    let reached = Arc::new(tokio::sync::Notify::new());
    let r2 = reached.clone();
    crate::lifecycle::race_point::arm(point, key, move || async move {
        r2.notify_one();
        std::future::pending::<()>().await
    });
    let (app, k) = (e.app.clone(), key.to_string());
    let h = tokio::spawn(async move {
        if project {
            delete_project(State(app), Path(k)).await.map(|_| ())
        } else {
            delete_bot(State(app), Path(k)).await.map(|_| ())
        }
    });
    reached.notified().await;
    // 行程死亡：定案之後的工作已脫離 handler，要連它一起殺掉（否則它握著鎖，卡在 pending 上）。
    for _ in 0..200 {
        if kill_delete_work_for_test(key) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    h.abort();
    let _ = h.await;
}

async fn deleted(app: &Arc<App>, id: &str) -> bool {
    db::bot(&app.db, id).await.unwrap().unwrap().deleted_at.is_some()
}

async fn intent_states(app: &Arc<App>, subject: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT status FROM intents WHERE subject_id = ? ORDER BY created_at").bind(subject).fetch_all(&app.db).await.unwrap()
}

async fn parent_and_child(e: &crate::testing::Env) -> (String, String) {
    let parent = a_bot(e, "alfa", "user").await;
    in_config(e, &[(&parent, "alfa")]).await;
    let kid = a_bot(e, "alfa-kid", "child").await;
    sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&kid).execute(&e.app.db).await.unwrap();
    (parent, kid)
}

/// #296：母 bot 已定案刪除、child 還沒處理就死。以前 child 永遠活著（reconcile 不掃已刪母 bot 的 child）；現在開機補完，而且補兩次／併發補都一樣。
#[tokio::test]
async fn a_delete_bot_killed_after_the_decision_is_completed_on_boot() {
    let e = crate::testing::env().await;
    let (parent, kid) = parent_and_child(&e).await;
    die_in_delete(&e, &parent, "delete_bot_after_decided", false).await;
    assert!(deleted(&e.app, &parent).await, "定案了");
    assert!(!deleted(&e.app, &kid).await, "child 還活著——這就是 #296 的半套");
    assert_eq!(intent_states(&e.app, &parent).await, vec!["running"]);

    let app2 = crate::testing::restart_app(&e).await;
    crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
    assert!(deleted(&app2, &kid).await, "開機補完：child 收掉了");
    assert_eq!(intent_states(&app2, &parent).await, vec!["done"]);
    crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
    tokio::join!(crate::delete_intents::recover_host(&app2, LOCAL_HOST), crate::delete_intents::recover_host(&app2, LOCAL_HOST));
    assert_eq!(intent_states(&app2, &parent).await, vec!["done"], "冪等");
}

/// **#508**：定案之後 daemon 停超過 delete intent 的 TTL（一小時）才開回來。開機對帳的真實順序是
/// `restart_intents::recover_host`（第一步就是收過期的 intent）→ `delete_intents::recover_host`（補完）。
/// 以前第一步會把它收成 `failed`＋推 AGM inbox，補完那一步的 `intents::open()` 再也撈不到它，留下
/// 「母 bot 已軟刪、child 還活著」，而且再按一次刪除只得 404——期限的時鐘在 daemon 死著時照走，
/// 而 daemon 死著正是 intent 存在的理由。現在要照樣補完。
#[tokio::test]
async fn a_delete_interrupted_by_a_daemon_outage_longer_than_the_ttl_is_still_completed_on_boot() {
    let e = crate::testing::env().await;
    let (parent, kid) = parent_and_child(&e).await;
    die_in_delete(&e, &parent, "delete_bot_after_decided", false).await;
    assert!(deleted(&e.app, &parent).await && !deleted(&e.app, &kid).await, "前提：定案了、child 還活著");
    // daemon 躺了一整夜：expires_at 早就過了。
    sqlx::query("UPDATE intents SET expires_at = '2020-01-01T00:00:00.000Z' WHERE subject_id = ?")
        .bind(&parent)
        .execute(&e.app.db)
        .await
        .unwrap();

    let app2 = crate::testing::restart_app(&e).await;
    crate::runners::restart_intents::recover_host(&app2, LOCAL_HOST).await;
    assert_eq!(intent_states(&app2, &parent).await, vec!["running"], "還沒補過就不准收成 failed");
    crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
    assert!(deleted(&app2, &kid).await, "開機補完：child 收掉了");
    assert_eq!(intent_states(&app2, &parent).await, vec!["done"]);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'intent_failed' AND bot_id = ?")
        .bind(&parent)
        .fetch_one(&app2.db)
        .await
        .unwrap();
    assert_eq!(n, 0, "沒有人被通知『補不完』——它補完了");
}

/// 定案之前就死：世界沒變（bot 還活著、還在 config），abandoned。
#[tokio::test]
async fn a_delete_bot_killed_before_the_decision_is_abandoned() {
    let e = crate::testing::env().await;
    let (parent, kid) = parent_and_child(&e).await;
    die_in_delete(&e, &parent, "delete_after_intent", false).await;
    let app2 = crate::testing::restart_app(&e).await;
    crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
    assert_eq!(intent_states(&app2, &parent).await, vec!["abandoned"]);
    assert!(!deleted(&app2, &parent).await && !deleted(&app2, &kid).await, "什麼都沒刪");
}

/// #284：專案已定案刪除、child 還沒軟刪就死。
#[tokio::test]
async fn a_delete_project_killed_after_the_decision_is_completed_on_boot() {
    let e = crate::testing::env().await;
    let (_parent, kid) = parent_and_child(&e).await;
    die_in_delete(&e, &e.project_id, "delete_project_after_commit", true).await;
    assert!(!deleted(&e.app, &kid).await, "child 還活著、專案已刪——這就是 #284 的半套");
    let app2 = crate::testing::restart_app(&e).await;
    crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
    assert!(deleted(&app2, &kid).await, "開機補完");
    assert_eq!(intent_states(&app2, &e.project_id).await, vec!["done"]);
}

/// #1063：定案之後死掉、草稿還沒清：開機補完要連草稿一起清（bot 與專案版）。
#[tokio::test]
async fn a_delete_killed_after_the_decision_also_clears_the_drafts_on_boot() {
    let e = crate::testing::env().await;
    let (parent, kid) = parent_and_child(&e).await;
    for key in [format!("bot:{parent}"), format!("bot:{kid}"), format!("group:{}", e.project_id)] {
        crate::drafts::put(&e.app.db, &key, "unsent").await.unwrap();
    }
    die_in_delete(&e, &parent, "delete_bot_after_decided", false).await;
    let app2 = crate::testing::restart_app(&e).await;
    crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
    let keys: Vec<String> = crate::drafts::list(&app2.db).await.unwrap().into_iter().map(|d| d.key).collect();
    assert_eq!(keys, vec![format!("group:{}", e.project_id)], "母 bot 與 child 的草稿清掉；專案的草稿不屬於這次刪除");

    let e2 = crate::testing::env().await;
    let (_p2, kid2) = parent_and_child(&e2).await;
    crate::drafts::put(&e2.app.db, &format!("group:{}", e2.project_id), "unsent").await.unwrap();
    crate::drafts::put(&e2.app.db, &format!("bot:{kid2}"), "unsent").await.unwrap();
    die_in_delete(&e2, &e2.project_id, "delete_project_after_commit", true).await;
    let app3 = crate::testing::restart_app(&e2).await;
    crate::delete_intents::recover_host(&app3, LOCAL_HOST).await;
    assert!(crate::drafts::list(&app3.db).await.unwrap().is_empty(), "專案與它的 bot 草稿都清掉");
}

#[tokio::test]
async fn a_delete_project_killed_before_the_decision_is_abandoned() {
    let e = crate::testing::env().await;
    let (_parent, kid) = parent_and_child(&e).await;
    die_in_delete(&e, &e.project_id, "delete_after_intent", true).await;
    let app2 = crate::testing::restart_app(&e).await;
    crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
    assert_eq!(intent_states(&app2, &e.project_id).await, vec!["abandoned"]);
    assert!(!deleted(&app2, &kid).await);
}

/// #1058：收尾沒做完的專案，呼叫端再刪一次＝404，不能沿用並 abandon 背景還在補的 intent（child 還沒軟刪、分享還有效）。
#[tokio::test]
async fn a_second_delete_of_an_already_deleted_project_does_not_abandon_the_cleanup_still_owed() {
    let e = crate::testing::env().await;
    let (_parent, kid) = parent_and_child(&e).await;
    sqlx::query(&format!(
        "CREATE TRIGGER am_test_kid_stuck BEFORE UPDATE OF deleted_at ON bots WHEN NEW.id = '{kid}' BEGIN SELECT RAISE(ABORT, 'boom'); END"
    ))
    .execute(&e.app.db)
    .await
    .unwrap();
    let first = delete_project(State(e.app.clone()), Path(e.project_id.clone())).await;
    assert!(first.is_err(), "收尾沒做完：第一次要 5xx");
    let second = delete_project(State(e.app.clone()), Path(e.project_id.clone())).await;
    assert!(matches!(second, Err(LcError::NotFound(_))), "已刪的專案再刪＝404");
    assert!(!intent_states(&e.app, &e.project_id).await.iter().any(|s| s == "abandoned"), "背景那件不能被 abandon");
}

/// #1065：定案之後 client 斷線（handler future 被 drop）：工作脫離出去、照常跑完，intent 收成 done，不停在 running。
#[tokio::test]
async fn a_bot_delete_whose_handler_is_dropped_after_the_decision_still_finishes() {
    let e = crate::testing::env().await;
    let (parent, kid) = parent_and_child(&e).await;
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (r2, go) = (reached.clone(), release.clone());
    crate::lifecycle::race_point::arm("delete_bot_after_decided", &parent, move || async move {
        r2.notify_one();
        go.notified().await;
    });
    let (app, p) = (e.app.clone(), parent.clone());
    let h = tokio::spawn(async move { delete_bot(State(app), Path(p)).await.map(|_| ()) });
    reached.notified().await;
    h.abort(); // 瀏覽器斷線：handler future 被 drop（脫離出去的工作不受影響）
    let _ = h.await;
    release.notify_one();
    assert!(crate::testing::eventually!(intent_states(&e.app, &parent).await == vec!["done"]), "{:?}", intent_states(&e.app, &parent).await);
    assert!(deleted(&e.app, &kid).await, "child 照樣軟刪");
}

/// #1065：專案版，同上。
#[tokio::test]
async fn a_project_delete_whose_handler_is_dropped_after_the_decision_still_finishes() {
    let e = crate::testing::env().await;
    let (_parent, kid) = parent_and_child(&e).await;
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (r2, go) = (reached.clone(), release.clone());
    crate::lifecycle::race_point::arm("delete_project_after_commit", &e.project_id, move || async move {
        r2.notify_one();
        go.notified().await;
    });
    let (app, p) = (e.app.clone(), e.project_id.clone());
    let h = tokio::spawn(async move { delete_project(State(app), Path(p)).await.map(|_| ()) });
    reached.notified().await;
    h.abort();
    let _ = h.await;
    release.notify_one();
    assert!(crate::testing::eventually!(intent_states(&e.app, &e.project_id).await == vec!["done"]), "{:?}", intent_states(&e.app, &e.project_id).await);
    assert!(deleted(&e.app, &kid).await, "child 照樣軟刪");
}

/// #1355：restart 的 handler 在 stop 與 start 之間被 drop（client 斷線）：脫離出去的工作照樣做完，
/// 新的 run 起來、restart intent 收成 done、鎖與 restart_hold 都放掉。
#[tokio::test]
async fn a_restart_whose_handler_is_dropped_between_stop_and_start_still_finishes() {
    let e = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "restart-drop").await;
    let run1 = lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (r2, go) = (reached.clone(), release.clone());
    crate::lifecycle::race_point::arm("restart_after_stop", &bot.id, move || async move {
        r2.notify_one();
        go.notified().await;
    });
    let (app, id) = (e.app.clone(), bot.id.clone());
    let h = tokio::spawn(async move { restart_bot(State(app), Path(id), Query(Default::default())).await.map(|_| ()) });
    reached.notified().await;
    h.abort(); // 瀏覽器斷線：handler future 被 drop（脫離出去的工作不受影響）
    let _ = h.await;
    release.notify_one();
    wait_work_done_for_test(&bot.id).await;
    let run2 = db::active_run(&e.app.db, &bot.id).await.unwrap().expect("重啟做完：bot 回來了");
    assert_ne!(run2.id, run1, "是一顆新的 run");
    assert_eq!(run2.state, "running");
    let intents: Vec<String> = sqlx::query_scalar("SELECT status FROM intents WHERE kind = 'restart' AND subject_id = ?").bind(&bot.id).fetch_all(&e.app.db).await.unwrap();
    assert_eq!(intents, vec!["done"]);
    assert!(!lifecycle::restart_hold::in_progress(&bot.id), "restart_hold 放掉了");
}

/// #1356：start 的 handler 在 agent 起來、`running` 還沒記的那一瞬被 drop：啟動照樣做完，run 收成 running。
#[tokio::test]
async fn a_start_whose_handler_is_dropped_while_the_agent_comes_up_still_records_running() {
    let e = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "start-drop").await;
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (r2, go) = (reached.clone(), release.clone());
    crate::lifecycle::race_point::arm("start_before_running", &bot.id, move || async move {
        r2.notify_one();
        go.notified().await;
    });
    let (app, id) = (e.app.clone(), bot.id.clone());
    let h = tokio::spawn(async move { start_bot(State(app), Path(id), Query(Default::default())).await.map(|_| ()) });
    reached.notified().await;
    h.abort(); // 瀏覽器斷線：handler future 被 drop（脫離出去的工作不受影響）
    let _ = h.await;
    release.notify_one();
    wait_work_done_for_test(&bot.id).await;
    let run = db::active_run(&e.app.db, &bot.id).await.unwrap().expect("啟動做完：有 active run");
    assert_eq!(run.state, "running", "agent 已經起來了，run 不能停在 starting");
}

/// #1357：stop 的 handler 在 pane 關了、`stopped` 還沒記的那一瞬被 drop：停止照樣記完，run 是 stopped 不是 stopping／exited。
#[tokio::test]
async fn a_stop_whose_handler_is_dropped_before_the_stopped_record_still_records_stopped() {
    let e = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "stop-drop").await;
    let run = lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (r2, go) = (reached.clone(), release.clone());
    crate::lifecycle::race_point::arm("stop_before_stopped", &bot.id, move || async move {
        r2.notify_one();
        go.notified().await;
    });
    let (app, id) = (e.app.clone(), bot.id.clone());
    let h = tokio::spawn(async move { stop_bot(State(app), Path(id)).await.map(|_| ()) });
    reached.notified().await;
    h.abort(); // 瀏覽器斷線：handler future 被 drop（脫離出去的工作不受影響）
    let _ = h.await;
    release.notify_one();
    wait_work_done_for_test(&bot.id).await;
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id = ?").bind(&run).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(state, "stopped", "使用者要它停：不是 stopping，也不是 exited");
    assert!(db::active_run(&e.app.db, &bot.id).await.unwrap().is_none());
}

/// 補不成（child 的軟刪一直寫不進去）：最多試 MAX_ATTEMPTS 次，failed＋AGM inbox。
#[tokio::test]
async fn a_delete_that_can_never_be_completed_gives_up_and_tells_agm() {
    let e = crate::testing::env().await;
    let (parent, kid) = parent_and_child(&e).await;
    die_in_delete(&e, &parent, "delete_bot_after_decided", false).await;
    sqlx::query(&format!(
        "CREATE TRIGGER am_test_kid_stuck BEFORE UPDATE OF deleted_at ON bots WHEN NEW.id = '{kid}' BEGIN SELECT RAISE(ABORT, 'boom'); END"
    ))
    .execute(&e.app.db)
    .await
    .unwrap();
    let app2 = crate::testing::restart_app(&e).await;
    crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
    let mut st = vec![];
    for _ in 0..200 {
        st = intent_states(&app2, &parent).await;
        if st == vec!["failed"] {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(st, vec!["failed"]);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'intent_failed' AND bot_id = ?").bind(&parent).fetch_one(&app2.db).await.unwrap();
    assert_eq!(n, 1, "AGM inbox 有一則 intent_failed");
}

/// #1059：補做失敗、而且失敗記不下來：intent 停在這顆 boot 的 running。同一顆 daemon 之後的重試要續做，不能當成「別人收了」就安靜結束。
#[tokio::test]
async fn a_delete_whose_failure_cannot_be_recorded_is_still_retried_by_the_same_boot() {
    use crate::runners::restart_intents::Outcome;
    let e = crate::testing::env().await;
    let (parent, kid) = parent_and_child(&e).await;
    die_in_delete(&e, &parent, "delete_bot_after_decided", false).await;
    sqlx::query(&format!(
        "CREATE TRIGGER am_test_kid_stuck BEFORE UPDATE OF deleted_at ON bots WHEN NEW.id = '{kid}' BEGIN SELECT RAISE(ABORT, 'boom'); END"
    ))
    .execute(&e.app.db)
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER am_test_refuse_pending BEFORE UPDATE OF status ON intents WHEN OLD.status = 'running' AND NEW.status = 'pending' BEGIN SELECT RAISE(ABORT, 'injected'); END")
        .execute(&e.app.db)
        .await
        .unwrap();
    let app2 = crate::testing::restart_app(&e).await;
    let id: String = sqlx::query_scalar("SELECT id FROM intents WHERE subject_id = ?").bind(&parent).fetch_one(&app2.db).await.unwrap();

    assert!(matches!(crate::delete_intents::drive_once(&app2, &id).await, Outcome::Retry(_)));
    assert_eq!(intent_states(&app2, &parent).await, vec!["running"]);
    assert!(matches!(crate::delete_intents::drive_once(&app2, &id).await, Outcome::Retry(_)), "同一顆 boot 的下一輪要續做，不是 Finished");

    sqlx::query("DROP TRIGGER am_test_refuse_pending").execute(&app2.db).await.unwrap();
    sqlx::query("DROP TRIGGER am_test_kid_stuck").execute(&app2.db).await.unwrap();
    assert_eq!(crate::delete_intents::drive_once(&app2, &id).await, Outcome::Finished);
    assert!(deleted(&app2, &kid).await, "child 補完軟刪");
    assert_eq!(intent_states(&app2, &parent).await, vec!["done"]);
}

/// intent 寫不進去＝什麼都還沒動：不能定案刪除。
#[tokio::test]
async fn a_delete_that_cannot_record_its_intent_decides_nothing() {
    let e = crate::testing::env().await;
    let (parent, _kid) = parent_and_child(&e).await;
    sqlx::query("CREATE TRIGGER am_test_no_intents BEFORE INSERT ON intents BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END").execute(&e.app.db).await.unwrap();
    assert!(delete_bot(State(e.app.clone()), Path(parent.clone())).await.is_err());
    assert!(!deleted(&e.app, &parent).await, "沒定案");
}

/// 死鎖回歸（sol 五輪）：child id 字典序**小於** parent。舊寫法 delete_bot 先持 parent、定案後才拿 child，
/// delete_project 依序先持 child 再等 parent → 互等。現在兩邊都依 id 排序一次拿齊，必須在時限內都結束。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_delete_and_parent_delete_never_deadlock() {
    for round in 0..10 {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let parent = format!("zz-parent-{round}");
        let child = format!("aa-child-{round}");
        for (bot_id, name, managed_by) in [(&parent, "alfa", "user"), (&child, "alfa-kid", "child")] {
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
                     VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
            )
            .bind(bot_id)
            .bind(&e.project_id)
            .bind(name)
            .bind(managed_by)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&child).execute(&app.db).await.unwrap();
        assert!(child < parent, "這條測試要 child 排在前面");
        in_config(&e, &[(&parent, "alfa")]).await;

        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let by_bot = tokio::spawn({
            let (app, parent, gate) = (app.clone(), parent.clone(), gate.clone());
            async move {
                gate.wait().await;
                delete_bot(State(app), Path(parent)).await.map(|_| ()).map_err(reason)
            }
        });
        let by_project = tokio::spawn({
            let (app, pid, gate) = (app.clone(), e.project_id.clone(), gate.clone());
            async move {
                gate.wait().await;
                delete_project(State(app), Path(pid)).await.map(|_| ()).map_err(reason)
            }
        });
        let both = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            (by_bot.await.unwrap(), by_project.await.unwrap())
        })
        .await
        .unwrap_or_else(|_| panic!("round {round}: delete_bot 與 delete_project 互等卡死"));
        // 至少有一邊成功，parent 一定刪掉了；另一邊看到的是「已經不在」而不是卡住。
        assert!(both.0.is_ok() || both.1.is_ok(), "round {round}: {both:?}");
        assert!(db::bot(&app.db, &parent).await.unwrap().unwrap().deleted_at.is_some(), "round {round}");
    }
}

/// TOML 被外部清空後刪原本的專案：目標此刻不在 TOML → 拒絕，DB 裡的專案與 bot 一列都不動。
#[tokio::test]
async fn deleting_a_project_the_toml_no_longer_has_is_refused() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&b1, "alfa")]).await;
    app.cfg.update(|cfg| { cfg.projects.clear(); Ok(()) }).await.unwrap();

    let err = delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap_err();
    assert_eq!(reason(err), "not_in_config");
    assert!(db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_none());
    assert_eq!(db::live_projects(&app.db).await.unwrap().len(), 1);

    let err = delete_bot(State(app.clone()), Path(b1.clone())).await.unwrap_err();
    assert_eq!(reason(err), "not_in_config");
    assert!(db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_none());
}

/// 兩支不同的 DELETE 真的同時跑：各自只刪自己那顆，不會把對方剛寫進 config 的刪除當成未授權而拒絕，
/// 也不會替對方放行。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_deletes_at_once_each_remove_only_their_own_bot() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let ids: Vec<String> = futures::future::join_all(["a", "b", "c", "d"].map(|n| a_bot(&e, n, "user"))).await;
    in_config(&e, &[(&ids[0], "a"), (&ids[1], "b"), (&ids[2], "c"), (&ids[3], "d")]).await;

    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let spawn = |id: String| {
        let (app, gate) = (app.clone(), gate.clone());
        tokio::spawn(async move {
            gate.wait().await;
            delete_bot(State(app), Path(id)).await.map(|_| ()).map_err(reason)
        })
    };
    let (x, y) = (spawn(ids[0].clone()), spawn(ids[1].clone()));
    assert_eq!(x.await.unwrap(), Ok(()));
    assert_eq!(y.await.unwrap(), Ok(()));

    let live: Vec<String> = db::live_bots(&app.db).await.unwrap().into_iter().map(|b| b.id).collect();
    assert_eq!(live.len(), 2, "{live:?}");
    assert!(live.contains(&ids[2]) && live.contains(&ids[3]));
    let in_toml: Vec<String> =
        app.cfg.get().await.projects[0].bots.iter().filter_map(|b| b.id.clone()).collect();
    assert_eq!(in_toml, vec![ids[2].clone(), ids[3].clone()]);
}

/// review 2026-09-12 d: no reconcile walks deleted bots, so nothing else would end that run.
#[tokio::test]
async fn a_bot_deleted_while_its_host_is_down_does_not_keep_a_running_run() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let id = a_bot(&e, "remote-ish", "user").await;
    in_config(&e, &[(&id, "remote-ish")]).await;
    let run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','pane-x','agent','no-such-session',?)",
    )
    .bind(&run)
    .bind(&id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    delete_bot(State(app.clone()), Path(id.clone())).await.unwrap();

    let bot = db::bot(&app.db, &id).await.unwrap().unwrap();
    assert!(bot.deleted_at.is_some(), "the bot is gone");
    assert!(db::active_run(&app.db, &id).await.unwrap().is_none(), "no orphan run stays active");
    let r = db::run(&app.db, &run).await.unwrap().unwrap();
    assert_eq!(r.state, "exited");
    assert!(r.ended_at.is_some());
}

/// 這顆 bot 的 runtime 目錄（裡面放著 hook 設定，砍掉就補不回來）。
fn runtime_dir(app: &Arc<App>, bot_id: &str) -> std::path::PathBuf {
    let dir = app.bot_dir(bot_id).unwrap();
    std::fs::create_dir_all(dir.join("hooks")).unwrap();
    std::fs::write(dir.join("hooks/settings.json"), "{}").unwrap();
    dir
}

/// #246：遠端專案刪除定案之後 `projects` 讀不到，child 的目錄清理不能退回 local——本機同 id 的目錄一根毛都不能動。
/// host 在定案之前就讀好；定案前就讀不到則 502、什麼都沒動。
#[tokio::test]
async fn a_remote_project_delete_never_purges_local_dirs_when_the_host_cannot_be_reread() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let kid = a_bot(&e, "kid", "child").await;
    in_config(&e, &[]).await;
    sqlx::query("UPDATE projects SET host='remote1' WHERE id=?").bind(&e.project_id).execute(&app.db).await.unwrap();
    let dir = runtime_dir(&app, &kid);

    // 定案前讀不到：整支失敗、bot 沒軟刪。
    crate::testing::make_table_unreadable(&app, "projects").await;
    let err = delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap_err();
    crate::testing::make_table_readable(&app, "projects").await;
    assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
    assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_none());
    assert!(dir.exists());

    // 定案之後才讀不到（重讀的舊寫法在這裡退回 local）：本機目錄必須還在。
    let a = app.clone();
    crate::lifecycle::race_point::arm("delete_project_after_commit", &e.project_id, move || async move {
        crate::testing::make_table_unreadable(&a, "projects").await
    });
    let _ = delete_project(State(app.clone()), Path(e.project_id.clone())).await;
    crate::testing::make_table_readable(&app, "projects").await;
    assert!(dir.join("hooks/settings.json").exists(), "遠端專案的清理不能砍到本機目錄");
}

async fn body_of(resp: Response) -> Value {
    serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap()
}

/// #210 驗收 1（讀不到 run 的狀態）：定案之前就讀不到 → 502、什麼都不動（bot 沒刪、config 沒寫、沒停、目錄在），可以原樣再按一次。
#[tokio::test]
async fn an_unreadable_run_state_refuses_the_delete_before_anything_is_decided() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let b1 = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&b1, "alfa")]).await;
    let run = crate::testing::fake_run(&app, &b1).await;
    let dir = runtime_dir(&app, &b1);

    crate::testing::make_table_unreadable(&app, "runs").await;
    let err = delete_bot(State(app.clone()), Path(b1.clone())).await.unwrap_err();
    crate::testing::make_table_readable(&app, "runs").await;

    assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
    assert!(db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_none(), "定案前不能刪");
    assert!(app.cfg.get().await.projects[0].bots.iter().any(|b| b.id.as_deref() == Some(b1.as_str())), "config 沒寫");
    assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "running", "沒停");
    assert!(dir.join("hooks/settings.json").exists());
    assert!(!e.herdr.methods().iter().any(|m| m == "agent.send_keys"), "一個鍵都沒送");
}

/// #210 驗收 1（停機失敗而且讀不到 active run）：定案之後才讀不到——停機失敗、run 收不掉——目錄必須留著，
/// 回應列在 `kept_dirs`；DB 好了 run 還活著，開機清掃也不會去刪它。
#[tokio::test]
async fn a_failed_stop_that_cannot_read_the_run_keeps_the_dir() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let id = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&id, "alfa")]).await;
    let run = a_running_run(&app, &id).await; // session 不存在：stop 一定失敗
    let dir = runtime_dir(&app, &id);
    let a = app.clone();
    crate::lifecycle::race_point::arm("delete_bot_after_decided", &id, move || async move {
        crate::testing::make_table_unreadable(&a, "runs").await
    });

    let out = body_of(delete_bot(State(app.clone()), Path(id.clone())).await.unwrap()).await;
    crate::testing::make_table_readable(&app, "runs").await;

    assert_eq!(out["kept_dirs"], json!([{"bot_id": id, "reason": "run_state_unreadable"}]), "{out}");
    assert!(dir.join("hooks/settings.json").exists(), "讀不到 run 的狀態，不能刪它的目錄");
    assert!(db::bot(&app.db, &id).await.unwrap().unwrap().deleted_at.is_some(), "刪除本身已經定案");
    assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "running", "讀不到就沒有動 run");
    assert_eq!(crate::lifecycle::purge_deleted_bot_dirs(&app).await, 0, "run 還活著：開機清掃也不刪");
    assert!(dir.exists());
}

/// #210：停機失敗（主機連不上）而 run 讀得到：run 照舊強制收成 exited，但停機沒有確認、agent 可能還活著——目錄留著、
/// 列在 `kept_dirs`；沒有 run 的 parent 照常 purge。下次開機的清掃在 run 確定結束之後把它收掉。
#[tokio::test]
async fn a_delete_whose_stop_failed_keeps_that_dir_until_the_startup_sweep() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let parent = a_bot(&e, "alfa", "user").await;
    let kid = a_bot(&e, "alfa-kid", "child").await;
    sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&kid).execute(&app.db).await.unwrap();
    in_config(&e, &[(&parent, "alfa")]).await;
    let run = a_running_run(&app, &kid).await;
    let (parent_dir, kid_dir) = (runtime_dir(&app, &parent), runtime_dir(&app, &kid));

    let out = body_of(delete_bot(State(app.clone()), Path(parent.clone())).await.unwrap()).await;

    assert_eq!(out["removed_children"], json!([kid]), "{out}");
    assert_eq!(out["kept_dirs"], json!([{"bot_id": kid, "reason": "stop_not_confirmed"}]), "{out}");
    assert!(!parent_dir.exists(), "沒有 run、確定沒有：照常 purge");
    assert!(kid_dir.join("hooks/settings.json").exists(), "停機沒有確認：目錄留著");
    assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "exited", "run 照舊被收掉，不留孤兒");

    assert_eq!(crate::lifecycle::purge_deleted_bot_dirs(&app).await, 1, "下次開機：run 確定結束了才收");
    assert!(!kid_dir.exists());
}

/// 收完再讀，run 還是 active（`exited` 寫不進去）：不是「確定沒有 active run」，目錄留著。
#[tokio::test]
async fn a_run_that_could_not_be_ended_keeps_the_dir() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let id = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&id, "alfa")]).await;
    let run = a_running_run(&app, &id).await;
    let dir = runtime_dir(&app, &id);
    sqlx::query("CREATE TRIGGER refuse_exit BEFORE UPDATE OF state ON runs WHEN NEW.state = 'exited' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
        .execute(&app.db)
        .await
        .unwrap();

    let out = body_of(delete_bot(State(app.clone()), Path(id.clone())).await.unwrap()).await;

    assert_eq!(out["kept_dirs"], json!([{"bot_id": id, "reason": "run_still_active"}]), "{out}");
    assert!(dir.join("hooks/settings.json").exists());
    assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "running");
}

/// #210 驗收 2：停好了、而且讀得到現在沒有 active run，才 purge；回應不帶 `kept_dirs`。
#[tokio::test]
async fn a_confirmed_stop_purges_the_dir() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let id = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&id, "alfa")]).await;
    let run = crate::testing::fake_run(&app, &id).await; // session `test`＝mock herdr，stop 會真的送鍵
    let dir = runtime_dir(&app, &id);

    let out = body_of(delete_bot(State(app.clone()), Path(id.clone())).await.unwrap()).await;

    assert!(out.get("kept_dirs").is_none(), "{out}");
    assert!(!dir.exists(), "確定停了、確定沒有 run：purge");
    assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "stopped");
}

/// 停好了，但「現在沒有 active run」讀不到（停機與收尾之間 DB 壞了）：不是確定，目錄留著。
#[tokio::test]
async fn a_stop_whose_result_cannot_be_read_back_keeps_the_dir() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let id = a_bot(&e, "alfa", "user").await;
    in_config(&e, &[(&id, "alfa")]).await;
    crate::testing::fake_run(&app, &id).await;
    let dir = runtime_dir(&app, &id);
    let a = app.clone();
    crate::lifecycle::race_point::arm("delete_stop_before_proof", &id, move || async move {
        crate::testing::make_table_unreadable(&a, "runs").await
    });

    let out = body_of(delete_bot(State(app.clone()), Path(id.clone())).await.unwrap()).await;
    crate::testing::make_table_readable(&app, "runs").await;

    assert_eq!(out["kept_dirs"], json!([{"bot_id": id, "reason": "run_state_unreadable"}]), "{out}");
    assert!(dir.join("hooks/settings.json").exists());
}
