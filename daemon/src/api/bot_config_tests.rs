use super::*;
use crate::testing as tt;
use crate::testing::{env, Env};

async fn add(e: &Env, body: Value) -> Result<String, LcError> {
    let res = create_bot(State(e.app.clone()), Path(e.project_id.clone()), Json(serde_json::from_value(body).unwrap())).await?;
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    Ok(serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string())
}

async fn patch(e: &Env, id: &str, body: Value) -> Result<Value, LcError> {
    let res = patch_bot(State(e.app.clone()), Path(id.to_string()), Extension(RequestPrincipal::User), Json(serde_json::from_value(body).unwrap())).await?;
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    Ok(serde_json::from_slice(&bytes).unwrap())
}

fn replace_config_bot_name(text: &str, old: &str, new: &str) -> String {
    let needle = format!("name = \"{old}\"");
    assert_eq!(text.matches(&needle).count(), 1, "config.toml should contain exactly one {needle:?}");
    text.replacen(&needle, &format!("name = \"{new}\""), 1)
}

/// 這顆 bot 的 `persona` 在 config.toml 與 DB 兩邊各是什麼。
async fn stored(e: &Env, id: &str) -> (Option<String>, Option<String>) {
    let cfg = e.app.cfg.get().await;
    let in_cfg = cfg.projects[0].bots.iter().find(|b| b.id.as_deref() == Some(id)).unwrap().persona.clone();
    (in_cfg, db::bot(&e.app.db, id).await.unwrap().unwrap().persona)
}

/// 輸入檢查（`bot_input`）真的接在建立與修改 bot 的入口上：model 開頭是 `-`（會被當成 CLI 旗標）、env 設 `AM_*`、
/// 名稱含終端機控制字元，都是 400，而且 config.toml 與 DB 一個字都不動。
#[tokio::test]
async fn bot_create_and_patch_refuse_flag_like_models_reserved_env_and_control_characters_in_names() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let cfg_before = e.app.cfg.get().await;
    for body in [
        json!({"name": "m1", "kind": "claude", "model": "--dangerously-skip-permissions"}),
        json!({"name": "m2", "kind": "codex", "model": "-c"}),
        json!({"name": "m3", "kind": "claude", "model": "opus extra"}),
        json!({"name": "e1", "kind": "claude", "env": {"AM_BOT_TOKEN": "x"}}),
        json!({"name": "e2", "kind": "claude", "env": {"BAD-NAME": "x"}}),
        json!({"name": "e3", "kind": "claude", "env": {"OK": "a\nb"}}),
        json!({"name": "n1\u{1b}[31m", "kind": "claude"}),
        json!({"name": "n2\u{202e}x", "kind": "claude"}),
        json!({"name": "a1", "kind": "claude", "args": ["x\u{0}y"]}),
    ] {
        let r = add(&e, body.clone()).await;
        assert!(matches!(r, Err(LcError::Bad(_))), "{body}: {r:?}");
    }
    assert_eq!(e.app.cfg.get().await, cfg_before, "被拒的請求不能留下任何東西");

    let id = add(&e, json!({"name": "ok", "kind": "claude"})).await.unwrap();
    for body in [
        json!({"model": "--model"}),
        json!({"env": {"AM_RUN_ID": "x"}}),
        json!({"env": {"PATH": "a\u{7}"}}),
        json!({"name": "bell\u{7}"}),
        json!({"args": ["a\u{0}"]}),
    ] {
        let r = patch(&e, &id, body.clone()).await;
        assert!(matches!(r, Err(LcError::Bad(_))), "{body}: {r:?}");
    }
    // 合法的照常：model 清空、env 設 CLAUDE_CONFIG_DIR。
    patch(&e, &id, json!({"model": "", "env": {"CLAUDE_CONFIG_DIR": "$HOME/.claude-x"}})).await.unwrap();
}

/// #1658：可解析但語意不合法的磁碟設定，應能由合法 PATCH 修復；若 mutation 後仍有其他非法值，照樣拒絕且不落盤。
#[tokio::test]
async fn patch_repairs_a_semantically_invalid_external_config_and_keeps_final_validation() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let target = add(&e, json!({"name": "repair-me", "kind": "claude"})).await.unwrap();
    add(&e, json!({"name": "other", "kind": "claude"})).await.unwrap();
    let path = e.app.cfg.path.clone();

    let disk = std::fs::read_to_string(&path).unwrap();
    let invalid = replace_config_bot_name(&disk, "repair-me", "bad@name");
    std::fs::write(&path, &invalid).unwrap();

    patch(&e, &target, json!({"name": "fixed-name"})).await.unwrap();
    let disk = std::fs::read_to_string(&path).unwrap();
    assert!(disk.contains("name = \"fixed-name\""), "合法修正要寫進 TOML");
    assert!(!disk.contains("bad@name"), "修正後的非法舊值不能殘留");
    let in_memory = e.app.cfg.get().await;
    assert_eq!(in_memory.projects[0].bots.iter().find(|b| b.id.as_deref() == Some(target.as_str())).unwrap().name, "fixed-name");
    assert_eq!(db::bot(&e.app.db, &target).await.unwrap().unwrap().name, "fixed-name", "TOML 與 DB 要同步");

    // 另一筆仍非法時，修 target 不能把整份無效設定寫出去。
    let disk = replace_config_bot_name(&disk, "other", "bad@name");
    std::fs::write(&path, &disk).unwrap();
    let err = patch(&e, &target, json!({"name": "fixed-again"})).await.unwrap_err();
    match err {
        LcError::BadValue(body) => {
            assert_eq!(body["error"], "config_invalid", "仍要回設定驗證錯誤：{body}");
            assert_eq!(body["config_written"], false, "拒絕時不可落盤：{body}");
        }
        other => panic!("仍有非法設定時應拒絕 PATCH，得到 {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), disk, "失敗的 mutation 不可改 TOML");
    assert_eq!(db::bot(&e.app.db, &target).await.unwrap().unwrap().name, "fixed-name", "失敗的 mutation 不可改 DB");
}

/// 專案：本機路徑要絕對（相對路徑會解成 daemon 自己的工作目錄）、要是目錄、標籤不得含控制字元。
#[tokio::test]
async fn project_create_refuses_relative_paths_files_and_control_characters_in_the_label() {
    let e = env().await;
    let file = crate::testing::track(std::env::temp_dir().join(format!("am-botval-file-{}", db::ulid())));
    std::fs::write(&file, "x").unwrap();
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-botval-dir-{}", db::ulid())));
    std::fs::create_dir_all(&dir).unwrap();
    let create = |path: String, label: Option<&str>| {
        let app = e.app.clone();
        let body: NewProject = serde_json::from_value(json!({"path": path, "label": label})).unwrap();
        async move { create_project(State(app), Json(body)).await }
    };
    for (path, label) in [
        (".".to_string(), None),
        ("..".to_string(), None),
        ("relative/dir".to_string(), None),
        (file.to_string_lossy().into_owned(), None),
        (dir.to_string_lossy().into_owned(), Some("bad\nlabel")),
        (dir.to_string_lossy().into_owned(), Some("x\u{202e}y")),
    ] {
        let r = create(path.clone(), label).await;
        assert!(matches!(r, Err(LcError::Bad(_))), "{path} {label:?}: {r:?}");
    }
    assert!(create(dir.to_string_lossy().into_owned(), Some("fine 專案")).await.is_ok());
    let remote_relative = NewProject { path: "relative/dir".into(), label: None, host: Some("unconfigured-remote".into()) };
    let remote_err = create_project(State(e.app.clone()), Json(remote_relative)).await.unwrap_err();
    assert!(matches!(&remote_err, LcError::Bad(message) if message.contains("absolute")), "遠端專案也要先拒絕相對路徑：{remote_err:?}");
    let pid = e.app.cfg.get().await.projects.last().and_then(|p| p.id.clone()).unwrap();
    let r = patch_project(State(e.app.clone()), Path(pid), Extension(RequestPrincipal::User), Json(serde_json::from_value(json!({"label": "a\u{1b}b"})).unwrap())).await;
    assert!(matches!(r, Err(LcError::Bad(_))), "{r:?}");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_file(&file).ok();
}

/// 建立專案存的 label 要跟驗的一樣是 trim 過的：空白不進 config.toml，64 字上限算的是實際存的字串。
#[tokio::test]
async fn project_create_stores_the_trimmed_label() {
    let e = env().await;
    let dir_a = crate::testing::track(std::env::temp_dir().join(format!("am-label-{}", db::ulid())));
    std::fs::create_dir_all(&dir_a).unwrap();
    let dir_b = crate::testing::track(std::env::temp_dir().join(format!("am-label-{}", db::ulid())));
    std::fs::create_dir_all(&dir_b).unwrap();
    let body = |path: &std::path::Path, label: &str| Json(NewProject { path: path.to_string_lossy().into_owned(), label: Some(label.into()), host: None });

    create_project(State(e.app.clone()), body(&dir_a, "  spaced  ")).await.unwrap();
    assert_eq!(e.app.cfg.get().await.projects.last().unwrap().label, "spaced");

    // 空白 label 照舊退回目錄名。
    create_project(State(e.app.clone()), body(&dir_b, "   ")).await.unwrap();
    let dir_b_name = dir_b.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(e.app.cfg.get().await.projects.last().unwrap().label, dir_b_name);

    // 64 字加前後空白：驗的是 trim 後的 64 字，存的也是 64 字。
    let dir_c = crate::testing::track(std::env::temp_dir().join(format!("am-label-{}", db::ulid())));
    std::fs::create_dir_all(&dir_c).unwrap();
    create_project(State(e.app.clone()), body(&dir_c, &format!(" {} ", "長".repeat(64)))).await.unwrap();
    assert_eq!(e.app.cfg.get().await.projects.last().unwrap().label.chars().count(), 64);
}

#[tokio::test]
async fn deprecated_models_are_remapped_at_create_and_patch_and_response_says_so() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let create = create_bot(
        State(e.app.clone()),
        Path(e.project_id.clone()),
        Json(serde_json::from_value(json!({"name":"old-codex", "kind":"codex", "model":"gpt-5.6-luna"})).unwrap()),
    ).await.unwrap();
    let created: Value = serde_json::from_slice(&axum::body::to_bytes(create.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(created["remapped"]["model"]["from"], "gpt-5.6-luna");
    assert_eq!(created["remapped"]["model"]["to"], "gpt-6-luna");
    let codex_id = created["bot_id"].as_str().unwrap();
    assert_eq!(db::bot(&e.app.db, codex_id).await.unwrap().unwrap().model.as_deref(), Some("gpt-6-luna"));

    for (name, old, replacement) in [
        ("previous-codex-sol", "gpt-6-sol", "gpt-6.1-sol"),
        ("old-codex-sol", "gpt-5.6-sol", "gpt-6.1-sol"),
        ("old-codex-terra", "gpt-5.6-terra", "gpt-6.1-sol"),
    ] {
        let response = create_bot(
            State(e.app.clone()),
            Path(e.project_id.clone()),
            Json(serde_json::from_value(json!({"name":name, "kind":"codex", "model":old})).unwrap()),
        )
        .await
        .unwrap();
        let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(body["remapped"]["model"]["from"], old);
        assert_eq!(body["remapped"]["model"]["to"], replacement);
        assert_eq!(db::bot(&e.app.db, body["bot_id"].as_str().unwrap()).await.unwrap().unwrap().model.as_deref(), Some(replacement));
    }

    let patched = patch(&e, codex_id, json!({"model":"gpt-6-sol"})).await.unwrap();
    assert_eq!(patched["remapped"]["model"]["from"], "gpt-6-sol");
    assert_eq!(patched["remapped"]["model"]["to"], "gpt-6.1-sol");
    assert_eq!(db::bot(&e.app.db, codex_id).await.unwrap().unwrap().model.as_deref(), Some("gpt-6.1-sol"));

    let create_claude = create_bot(
        State(e.app.clone()),
        Path(e.project_id.clone()),
        Json(serde_json::from_value(json!({"name":"old-claude-create", "kind":"claude", "model":"opus"})).unwrap()),
    ).await.unwrap();
    let created_claude: Value = serde_json::from_slice(&axum::body::to_bytes(create_claude.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(created_claude["remapped"]["model"]["to"], "claude-opus-5-5");
    assert_eq!(db::bot(&e.app.db, created_claude["bot_id"].as_str().unwrap()).await.unwrap().unwrap().model.as_deref(), Some("claude-opus-5-5"));

    let claude_id = add(&e, json!({"name":"old-claude", "kind":"claude"})).await.unwrap();
    let patched = patch(&e, &claude_id, json!({"model":"opus"})).await.unwrap();
    assert_eq!(patched["remapped"]["model"]["from"], "opus");
    assert_eq!(patched["remapped"]["model"]["to"], "claude-opus-5-5");
    assert_eq!(db::bot(&e.app.db, &claude_id).await.unwrap().unwrap().model.as_deref(), Some("claude-opus-5-5"));
    let exact = patch(&e, &claude_id, json!({"model":"claude-opus-4-1"})).await.unwrap();
    assert!(exact.get("remapped").is_none(), "versioned model was explicitly chosen: {exact}");
    assert_eq!(db::bot(&e.app.db, &claude_id).await.unwrap().unwrap().model.as_deref(), Some("claude-opus-4-1"));
}

/// 最近一次 `agent.start` 的 `--settings` 檔案內容：daemon 真的交給 claude 的那包設定。
fn settings_of_last_start(e: &Env) -> Value {
    let call = e.herdr.calls_to("agent.start").pop().expect("有起過 agent");
    let args: Vec<String> = call["args"].as_array().unwrap().iter().filter_map(|a| a.as_str().map(String::from)).collect();
    let at = args.iter().position(|a| a == "--settings").expect("claude 帶 --settings") + 1;
    serde_json::from_slice(&std::fs::read(&args[at]).unwrap()).unwrap()
}

/// 2026-10-02 使用者決定拿掉「專案指示檔」：新版 claude 自己吃 AGENTS.md，daemon 不再釘 `agents-md` plugin 的 `instructionFiles`，
/// `--settings` 裡沒有 `pluginConfigs`，CLI 用它自己的預設。舊網頁還送 `instruction_files` 時照收、當沒這個欄位，其他欄位照存。
#[tokio::test]
async fn claude_starts_without_pinning_instruction_files_and_a_legacy_field_is_ignored() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "legacy", "kind": "claude", "persona": "P", "instruction_files": "managed-only"})).await.unwrap();
    assert_eq!(stored(&e, &id).await, (Some("P".into()), Some("P".into())), "舊欄位不影響其他欄位");

    lifecycle::start_bot(&e.app, &id).await.unwrap();
    assert!(settings_of_last_start(&e).get("pluginConfigs").is_none(), "不再釘 instructionFiles");

    let out = patch(&e, &id, json!({"instruction_files": "claude-md-and-agents-md"})).await.unwrap();
    assert_eq!(out["needs_restart"], json!(false), "舊欄位單獨送：什麼都沒改，不要求重啟：{out}");
    let out = patch(&e, &id, json!({"persona": "Q", "instruction_files": "claude-md"})).await.unwrap();
    assert_eq!(out["needs_restart"], json!(true), "{out}");
    assert_eq!(stored(&e, &id).await, (Some("Q".into()), Some("Q".into())));

    let st = state_json(&e.app).await.unwrap();
    let bot = st["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap().clone();
    assert!(bot.get("instruction_files").is_none(), "state 不再輸出這個欄位：{bot}");
}

/// #1796：前端把「建立中」佔位列對到真 bot 靠建立請求的冪等鍵，不靠名字；state 要把 config 裡的鍵帶出來（沒帶鍵的 bot 是 null）。
#[tokio::test]
async fn state_carries_the_create_request_id_so_a_placeholder_can_be_matched_without_the_name() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let with = add(&e, json!({"name": "clone-a", "kind": "claude", "client_request_id": "crid-clone-1"})).await.unwrap();
    let without = add(&e, json!({"name": "clone-b", "kind": "claude"})).await.unwrap();
    let st = state_json(&e.app).await.unwrap();
    let find = |id: &str| st["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap().clone();
    assert_eq!(find(&with)["create_request_id"], json!("crid-clone-1"));
    assert_eq!(find(&without)["create_request_id"], Value::Null);
}

#[tokio::test]
async fn patch_accepts_full_claude_model_names_and_codex_catalog_models() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let claude = add(&e, json!({"name": "claude", "kind": "claude"})).await.unwrap();
    let codex = add(&e, json!({"name": "codex", "kind": "codex"})).await.unwrap();

    for (id, model) in [(&claude, "claude-opus-5-5"), (&codex, "gpt-6-luna")] {
        let out = patch(&e, id, json!({"model": model})).await.unwrap();
        assert_eq!(out["needs_restart"], json!(false), "idle bot accepts {model}: {out}");
        assert_eq!(db::bot(&e.app.db, id).await.unwrap().unwrap().model.as_deref(), Some(model));
    }
}

/// 單顆 bot 的 restart 連點兩下（c2 看到 `agent.start` 3 次）：同一顆 bot 已有 restart **在進行中**，
/// 第二個請求回同一個結果，不再重啟一次（比照一鍵重啟的合併，SPEC §6.9）；做完之後再來的是新的一次。
#[tokio::test]
async fn a_second_restart_of_the_same_bot_is_coalesced_not_run_again() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "dbl", "kind": "claude"})).await.unwrap();
    lifecycle::start_bot(&e.app, &id).await.unwrap();
    let starts = || e.herdr.calls_to("agent.start").len();
    let q = |resume: Option<&str>| Query(StartQuery { resume: resume.map(String::from), session: None });
    let body = |r: Response| async move {
        assert_eq!(r.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };

    let before = starts();
    let (a, b) = tokio::join!(
        restart_bot(State(e.app.clone()), Path(id.clone()), q(None)),
        restart_bot(State(e.app.clone()), Path(id.clone()), q(None)),
    );
    let (a, b) = (body(a.unwrap()).await, body(b.unwrap()).await);
    assert_eq!(starts() - before, 1, "連點兩下只重啟一次：{a} {b}");
    assert_eq!(a["run_id"], b["run_id"], "兩個請求拿到同一個結果");
    let coalesced = [&a, &b].iter().filter(|v| v["coalesced"] == json!(true)).count();
    assert_eq!(coalesced, 1, "被合併的那個說一聲：{a} {b}");

    // 剛做完又來一個：那是另一次重啟，不是同一次（只合併「仍在進行中」的）——照做，換新的 run。
    let second = body(restart_bot(State(e.app.clone()), Path(id.clone()), q(None)).await.unwrap()).await;
    assert_eq!(starts() - before, 2, "做完之後的重啟不合併：{second}");
    assert_ne!(second["run_id"], a["run_id"]);
    assert!(second.get("coalesced").is_none(), "{second}");

    // 改設定後馬上（不到一秒）重啟：必須真的重啟，而且新 run 帶的是新設定（2026-10-02：被當成同一次就沒套到）。
    let needs_restart = |app: Arc<App>, id: String| async move {
        let st = state_json(&app).await.unwrap();
        st["projects"].as_array().unwrap().iter().flat_map(|p| p["bots"].as_array().unwrap().clone()).find(|b| b["id"] == json!(id)).unwrap()["needs_restart"].clone()
    };
    let _ = patch(&e, &id, json!({"persona": "換一份人設 B"})).await.unwrap();
    assert_eq!(needs_restart(e.app.clone(), id.clone()).await, json!(true), "前提：改了設定、執行中的 run 還是舊的");
    let third = body(restart_bot(State(e.app.clone()), Path(id.clone()), q(None)).await.unwrap()).await;
    assert_eq!(starts() - before, 3, "改設定後的重啟一定要做：{third}");
    assert_ne!(third["run_id"], second["run_id"]);
    assert_eq!(needs_restart(e.app.clone(), id.clone()).await, json!(false), "新 run 載入的是新設定");

    // 不一樣的重啟（明確要開新對話）本來就不是同一件事：照做。
    let fresh = body(restart_bot(State(e.app.clone()), Path(id.clone()), q(Some("fresh"))).await.unwrap()).await;
    assert_eq!(starts() - before, 4, "不同的重啟種類不合併：{fresh}");
    assert_ne!(fresh["run_id"], third["run_id"]);
}

/// 身份／bot 的 env 名稱只收合法的環境變數名稱：不合法的以前照樣存下，到用的時候才被 `tools::valid_env_name` 靜靜濾掉——
/// 少打一個 `=`、名字帶空白，身份的 env 就悄悄變成空的＝用了預設帳號，沒有任何錯誤。現在存之前就擋，說出是哪個 key。
#[tokio::test]
async fn an_invalid_env_variable_name_is_refused_up_front_not_dropped_at_use_time() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let reason = |err: LcError| match err {
        LcError::Bad(m) => m,
        other => panic!("要 400：{other:?}"),
    };
    let identity = |env: Value| NewIdentity { name: "cc9".into(), kind: "claude".into(), env: serde_json::from_value(env).unwrap(), args: vec![], host: None };
    for bad in ["CLAUDE CONFIG DIR", "9LIVES", "A-B", "", "A=B"] {
        let err = create_identity(State(e.app.clone()), Json(identity(json!({ bad: "/x" })))).await.unwrap_err();
        assert!(reason(err).contains(&format!("`{bad}`")), "身份：{bad:?} 要被點名");
        let err = add(&e, json!({"name": "ebad", "kind": "claude", "env": { bad: "1" }})).await.unwrap_err();
        assert!(reason(err).contains(&format!("`{bad}`")), "新 bot：{bad:?}");
    }
    let err = create_identity(State(e.app.clone()), Json(identity(json!({"AM_RUN_ID": "forged-run"}))))
        .await
        .expect_err("身份 env 不能覆蓋由 daemon 管理的 AM_* 狀態");
    assert!(reason(err).contains("reserved"), "identity.env: AM_RUN_ID 應回 400 reserved");
    let nul_args = NewIdentity {
        name: "cc10".into(),
        kind: "claude".into(),
        env: BTreeMap::new(),
        args: vec!["--flag\0injected".into()],
        host: None,
    };
    let err = create_identity(State(e.app.clone()), Json(nul_args))
        .await
        .expect_err("identity args 也會成為 bot 啟動 argv，必須拒絕 NUL");
    assert!(reason(err).contains("NUL"), "identity.args 的錯誤要指出 NUL");
    assert!(e.app.cfg.get().await.identities.iter().all(|i| i.name != "cc9"), "被拒的身份什麼都沒存");
    assert!(e.app.cfg.get().await.identities.iter().all(|i| i.name != "cc10"), "含 NUL args 的身份什麼都沒存");
    let id = add(&e, json!({"name": "eok", "kind": "claude"})).await.unwrap();
    let err = patch(&e, &id, json!({"env": {"BAD NAME": "1"}})).await.unwrap_err();
    assert!(reason(err).contains("`BAD NAME`"), "patch bot");
    // 值留空（#1750）：身份被點名拒絕、什麼都沒存；bot 自己的 env 空值照舊可以。
    for empty in ["", "   "] {
        let err = create_identity(State(e.app.clone()), Json(identity(json!({"CLAUDE_CONFIG_DIR": empty})))).await.unwrap_err();
        let m = reason(err);
        assert!(m.contains("`CLAUDE_CONFIG_DIR`") && m.contains("empty"), "身份空值要點名 key：{m}");
    }
    assert!(e.app.cfg.get().await.identities.iter().all(|i| i.name != "cc9"), "空值被拒的身份什麼都沒存");
    assert!(patch(&e, &id, json!({"env": {"HTTP_PROXY": ""}})).await.is_ok(), "bot 的 env 空值不受影響");
    // 合法的照收。
    assert!(create_identity(State(e.app.clone()), Json(identity(json!({"CLAUDE_CONFIG_DIR": "$HOME/.claude-cc9", "_X1": "y"})))).await.is_ok());
    assert!(patch(&e, &id, json!({"env": {"GOOD_NAME": "1"}})).await.is_ok());
}

/// 前端 `identityEnv.rustTrim` 照這一組空白判空（#1883）：U+0085／U+3000 算空白被拒，U+FEFF 不算空白所以放行。
#[test]
fn identity_env_emptiness_uses_rust_whitespace() {
    for empty in ["\u{85}", "\u{3000}", " \t\u{a0}\u{2003}\r\n"] {
        let env = BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), empty.to_string())]);
        assert!(check_identity_env_values(&env).is_err(), "{empty:?} 應判空");
    }
    let env = BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), "\u{feff}".to_string())]);
    assert!(check_identity_env_values(&env).is_ok(), "U+FEFF 不是 Rust 空白");
}

#[tokio::test]
async fn identity_args_reject_nul_before_persistence() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let input = NewIdentity {
        name: "cc-nul".into(),
        kind: "claude".into(),
        env: BTreeMap::new(),
        args: vec!["--flag\0injected".into()],
        host: None,
    };
    let err = create_identity(State(e.app.clone()), Json(input)).await.unwrap_err();
    assert!(matches!(&err, LcError::Bad(message) if message.contains("NUL")), "identity args 要拒絕 NUL：{err:?}");
    assert!(e.app.cfg.get().await.identities.iter().all(|identity| identity.name != "cc-nul"), "被拒的身份不能落設定");
}

#[tokio::test]
async fn a_bot_token_cannot_switch_its_identity_or_cli_config_directory() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "identity-guard", "kind": "claude"})).await.unwrap();
    let before = db::bot(&e.app.db, &id).await.unwrap().unwrap();
    for body in [json!({"identity": null}), json!({"env": {"CLAUDE_CONFIG_DIR": e.dir.join("foreign").to_string_lossy()}})] {
        let err = patch_bot(State(e.app.clone()), Path(id.clone()), Extension(RequestPrincipal::Bot(id.clone())), Json(serde_json::from_value(body).unwrap()))
            .await
            .unwrap_err();
        assert!(matches!(err, LcError::Forbidden(_)), "{err:?}");
    }
    let after = db::bot(&e.app.db, &id).await.unwrap().unwrap();
    assert_eq!(after.identity, before.identity);
    assert_eq!(after.env_json, before.env_json);
}

// ───────── 目錄瀏覽（DirPicker 背後的 `/fs/dirs`）的安全審查 ─────────

fn dirs_user() -> Extension<RequestPrincipal> {
    Extension(RequestPrincipal::User)
}

fn dirs_query(path: &std::path::Path) -> Query<DirsQuery> {
    Query(DirsQuery { path: Some(path.to_string_lossy().into_owned()), host: None, hidden: None })
}

/// 列目錄是 UI 的 DirPicker 在用；bot（pane 裡的 agent 拿得到自己的 token）不該能用 daemon 的身份去列本機、
/// 更不該能經 daemon 的 ssh 去列別台主機。
#[tokio::test]
async fn dirs_only_the_user_may_browse_directories() {
    let e = env().await;
    let tmp = crate::testing::track(std::env::temp_dir().join(format!("am-test-dirs-{}", db::ulid())));
    std::fs::create_dir_all(tmp.join("sub")).unwrap();
    assert!(list_dirs(State(e.app.clone()), dirs_user(), dirs_query(&tmp)).await.is_ok());
    for principal in [RequestPrincipal::Bot("b1".into()), RequestPrincipal::Service("daemon-swap".into())] {
        let err = list_dirs(State(e.app.clone()), Extension(principal), dirs_query(&tmp)).await.unwrap_err();
        assert!(matches!(err, LcError::Forbidden(_)), "{err:?}");
    }
}

#[test]
fn dirs_path_expands_only_tilde_and_tilde_slash() {
    let home = std::path::Path::new("/h/me");
    for blank in [None, Some(""), Some("  "), Some("~")] {
        assert_eq!(expand_dir_path(blank, home).unwrap(), "/h/me", "{blank:?}");
    }
    assert_eq!(expand_dir_path(Some("~/a/b"), home).unwrap(), "/h/me/a/b");
    assert_eq!(expand_dir_path(Some("  ~/a/b "), home).unwrap(), "/h/me/a/b");
    assert_eq!(expand_dir_path(Some("/abs/x"), home).unwrap(), "/abs/x");
    // 現況 `~2` 會得到 `/h/me2`、`~backup/x` 會得到 `/h/mebackup/x`：兩者都是家目錄的兄弟，不是使用者指的地方。
    for bad in ["~2", "~backup/x", "rel/dir", "./x", ".."] {
        assert!(matches!(expand_dir_path(Some(bad), home), Err(LcError::Bad(_))), "{bad} 要 400");
    }
}

/// 列目錄與新建資料夾共用同一條展開規則：`~user` 與相對路徑在兩邊都是 400，不會落到別的地方。
#[tokio::test]
async fn dirs_listing_refuses_tilde_user_and_relative_paths() {
    let e = env().await;
    for bad in ["~no-such", "relative"] {
        let query = Query(DirsQuery { path: Some(bad.into()), host: None, hidden: None });
        let err = list_dirs(State(e.app.clone()), dirs_user(), query).await.unwrap_err();
        assert!(matches!(&err, LcError::Bad(m) if m.contains("absolute")), "{bad}: {err:?}");
        let mkdir = Json(MkdirIn { host: None, parent: Some(bad.into()), name: "n".into() });
        let err = make_dir(State(e.app.clone()), dirs_user(), mkdir).await.unwrap_err();
        assert!(matches!(&err, LcError::Bad(m) if m.contains("absolute")), "{bad}: {err:?}");
    }
}

/// 憑證、金鑰、daemon 自己的資料目錄不列（只回子目錄名字，但 `bots/<id>` 之類的結構也不該外流）。
#[tokio::test]
async fn dirs_credential_and_daemon_directories_are_not_listed() {
    let home = std::path::Path::new("/home/u");
    let data = std::path::Path::new("/srv/am-data");
    for denied in [
        "/home/u/.ssh", "/home/u/.ssh/keys", "/home/u/.gnupg", "/home/u/.aws", "/home/u/.kube", "/home/u/.config/agents-manager",
        "/home/u/.config/agents-manager/bots/B1", "/home/u/.claude", "/home/u/.claude-cc1/projects", "/home/u/.codex", "/home/u/.grok",
        "/home/u/.gemini", "/home/u/.gemini/antigravity-cli",
        "/srv/am-data", "/srv/am-data/bots",
    ] {
        assert!(fs_dir_denied(std::path::Path::new(denied), home, data), "{denied} 要擋");
    }
    for ok in ["/", "/home", "/home/u", "/home/u/project", "/home/u/.config", "/home/u/.sshx", "/home/u/.claudeish", "/home/u/.geminix", "/srv", "/srv/am-data2"] {
        assert!(!fs_dir_denied(std::path::Path::new(ok), home, data), "{ok} 不該擋");
    }
    // 真的走 handler：daemon 自己的資料目錄。
    let e = env().await;
    let err = list_dirs(State(e.app.clone()), dirs_user(), dirs_query(&e.app.data_dir)).await.unwrap_err();
    assert!(matches!(err, LcError::Forbidden(_)), "{err:?}");
}

/// 上萬個子目錄的資料夾：每一項都要 stat 一次 `.git`、整份塞進一個 JSON。設上限、回 `truncated`，不拖死 daemon。
#[tokio::test]
async fn dirs_a_huge_directory_is_capped_and_says_so() {
    let e = env().await;
    let tmp = crate::testing::track(std::env::temp_dir().join(format!("am-test-dirs-many-{}", db::ulid())));
    for i in 0..2100 {
        std::fs::create_dir_all(tmp.join(format!("d{i:04}"))).unwrap();
    }
    let Json(v) = list_dirs(State(e.app.clone()), dirs_user(), dirs_query(&tmp)).await.unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 2000);
    assert_eq!(v["truncated"], json!(true));
    let small = crate::testing::track(std::env::temp_dir().join(format!("am-test-dirs-few-{}", db::ulid())));
    std::fs::create_dir_all(small.join("a")).unwrap();
    let Json(v) = list_dirs(State(e.app.clone()), dirs_user(), dirs_query(&small)).await.unwrap();
    assert_eq!(v["truncated"], json!(false));
}

#[test]
fn dirs_does_not_follow_a_path_replaced_by_a_symlink_after_authorization() {
    let base = tt::scratch_dir("am-test-dirs-swap");
    let browse = base.join("browse");
    let protected = base.join("protected");
    std::fs::create_dir_all(browse.join("visible")).unwrap();
    std::fs::create_dir_all(protected.join("credential-dir")).unwrap();
    let authorized_path = std::fs::canonicalize(&browse).unwrap();
    std::fs::rename(&browse, base.join("browse-before-swap")).unwrap();
    std::os::unix::fs::symlink(&protected, &browse).unwrap();

    assert!(local_dir_entries(&authorized_path, false).is_err(), "the reopened path must refuse the swapped symlink");
}

/// 錯誤訊息不替人把符號連結解開、講出真正的位置。
#[tokio::test]
async fn dirs_the_not_a_directory_error_does_not_reveal_where_a_symlink_points() {
    let e = env().await;
    let tmp = crate::testing::track(std::env::temp_dir().join(format!("am-test-dirs-link-{}", db::ulid())));
    std::fs::create_dir_all(&tmp).unwrap();
    let secret = tmp.join("secret-target-file.txt");
    std::fs::write(&secret, "x").unwrap();
    let link = tmp.join("innocent-link");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let err = list_dirs(State(e.app.clone()), dirs_user(), dirs_query(&link)).await.unwrap_err();
    let LcError::Bad(msg) = err else { panic!("要 400") };
    assert!(!msg.contains("secret-target-file"), "不能講出符號連結指到哪：{msg}");
}

/// 遠端列目錄的 sh 輸出是靠 `AM_*` 開頭的行解析的：目錄名字帶換行就能偽造 `AM_PATH=…`（把「目前路徑」改成別處，
/// 後面所有項目的 `path` 跟著錯）或偽造項目。名字含控制字元（換行、tab、CR…）的一律不列；真的拿去 `sh` 跑。
#[test]
fn dirs_a_directory_name_cannot_forge_the_remote_listing_protocol() {
    let tmp = crate::testing::track(std::env::temp_dir().join(format!("am-test-dirs-inject-{}", db::ulid())));
    for name in ["ok1", "a\nAM_PATH=/etc", "x\nAM_D\tfake\t1", "tab\tname", "cr\rname", ".hidden"] {
        std::fs::create_dir_all(tmp.join(name)).unwrap();
    }
    let run = |path: &str, hidden: bool| {
        let script = crate::hosts::dir_list_script(Some(path), hidden);
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).env("HOME", &tmp).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let canon = std::fs::canonicalize(&tmp).unwrap();
    let v = crate::hosts::parse_dir_listing(&run(tmp.to_str().unwrap(), false), false).unwrap();
    assert_eq!(v["path"], json!(canon.to_string_lossy()), "目前路徑不能被偽造：{v}");
    let names: Vec<&str> = v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["ok1"], "{v}");
    let v = crate::hosts::parse_dir_listing(&run(tmp.to_str().unwrap(), true), true).unwrap();
    let names: Vec<&str> = v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, [".hidden", "ok1"], "hidden=1 才多出隱藏的：{v}");
}

// ───────── 「新資料夾」`POST /fs/dirs`（issue #877） ─────────

fn mkdir_in(parent: &std::path::Path, name: &str) -> Json<MkdirIn> {
    Json(MkdirIn { host: None, parent: Some(parent.to_string_lossy().into_owned()), name: name.into() })
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let tmp = crate::testing::track(std::env::temp_dir().join(format!("am-test-mkdir-{tag}-{}", db::ulid())));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::canonicalize(&tmp).unwrap()
}

/// 在上層目錄建出新資料夾，回完整路徑（UI 直接拿來當 Project 路徑）；中文與空白的名字也行。
#[tokio::test]
async fn mkdir_creates_a_folder_under_the_browsed_parent_and_returns_its_path() {
    let e = env().await;
    let tmp = scratch("ok");
    for name in ["new-proj", "新 專案", ".hidden", "-dash", "  padded  "] {
        let Json(v) = make_dir(State(e.app.clone()), dirs_user(), mkdir_in(&tmp, name)).await.unwrap();
        let want = tmp.join(name.trim());
        assert!(want.is_dir(), "{name:?} 沒建出來");
        assert_eq!(v["path"], json!(want.to_string_lossy()));
        assert_eq!(v["name"], json!(name.trim()), "名字去掉頭尾空白");
        assert_eq!(v["parent"], json!(tmp.to_string_lossy()));
    }
    // 建好的資料夾本來就能被目錄瀏覽列出來、也能當 Project 路徑。
    let Json(listing) = list_dirs(State(e.app.clone()), dirs_user(), dirs_query(&tmp)).await.unwrap();
    assert!(listing["entries"].as_array().unwrap().iter().any(|x| x["name"] == json!("new-proj")));
}

/// 已經有同名的東西（資料夾、檔案、符號連結）→ 409 `already_exists`，一個字都不動。
#[tokio::test]
async fn mkdir_says_so_when_the_name_already_exists_and_touches_nothing() {
    let e = env().await;
    let tmp = scratch("exists");
    std::fs::create_dir(tmp.join("dir")).unwrap();
    std::fs::write(tmp.join("file"), "keep").unwrap();
    let elsewhere = scratch("exists-target");
    std::os::unix::fs::symlink(&elsewhere, tmp.join("link")).unwrap();
    for name in ["dir", "file", "link"] {
        let err = make_dir(State(e.app.clone()), dirs_user(), mkdir_in(&tmp, name)).await.unwrap_err();
        let LcError::Conflict(v) = err else { panic!("{name}: 要 409") };
        assert_eq!(v["reason"], json!("already_exists"), "{v}");
        assert!(v["message"].as_str().unwrap().contains(name), "提示要講是哪個名字：{v}");
    }
    assert_eq!(std::fs::read_to_string(tmp.join("file")).unwrap(), "keep");
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none(), "不能穿過符號連結建到別處");
}

/// 名字驗證：空白、`.`／`..`、含 `/`、控制字元、過長 → 400，而且什麼都沒建（尤其不能靠 `../x` 跳出上層）。
#[tokio::test]
async fn mkdir_refuses_bad_names_and_never_escapes_the_parent() {
    let e = env().await;
    let tmp = scratch("badname");
    let inner = tmp.join("inner");
    std::fs::create_dir(&inner).unwrap();
    let long = "x".repeat(256);
    for name in ["", "   ", ".", "..", "../escaped", "a/b", "/abs", "a\nb", "a\0b", "tab\tname", long.as_str()] {
        let err = make_dir(State(e.app.clone()), dirs_user(), mkdir_in(&inner, name)).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "{name:?} 要 400：{err:?}");
    }
    assert!(!tmp.join("escaped").exists());
    assert!(std::fs::read_dir(&inner).unwrap().next().is_none(), "被拒的名字不留任何東西");
}

/// 只有 User 能建；bot／service 一律 403；憑證與 daemon 資料目錄不給建（跟列目錄同一份規則）。
#[tokio::test]
async fn mkdir_is_user_only_and_keeps_the_browse_denylist() {
    let e = env().await;
    let tmp = scratch("authz");
    for principal in [RequestPrincipal::Bot("b1".into()), RequestPrincipal::Service("daemon-swap".into())] {
        let err = make_dir(State(e.app.clone()), Extension(principal), mkdir_in(&tmp, "x")).await.unwrap_err();
        assert!(matches!(err, LcError::Forbidden(_)), "{err:?}");
    }
    assert!(!tmp.join("x").exists());
    let err = make_dir(State(e.app.clone()), dirs_user(), mkdir_in(&e.app.data_dir, "x")).await.unwrap_err();
    assert!(matches!(err, LcError::Forbidden(_)), "daemon 資料目錄底下不能建：{err:?}");
    // 新路徑本身落在不給進的地方（在家目錄底下建 `.ssh`）也擋：純判斷。
    let home = std::path::Path::new("/home/u");
    let data = std::path::Path::new("/srv/am-data");
    for target in ["/home/u/.ssh", "/home/u/.aws", "/home/u/.claude-cc9", "/home/u/.config/agents-manager"] {
        assert!(fs_dir_denied(std::path::Path::new(target), home, data), "{target}");
    }
}

#[tokio::test]
async fn mkdir_needs_an_existing_directory_as_parent() {
    let e = env().await;
    let tmp = scratch("parent");
    std::fs::write(tmp.join("f"), "x").unwrap();
    for parent in [tmp.join("missing"), tmp.join("f")] {
        let err = make_dir(State(e.app.clone()), dirs_user(), mkdir_in(&parent, "n")).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "{parent:?}: {err:?}");
    }
    // 上層是符號連結：建在它指到的那一層（canonicalize 之後），不是建在連結本身。
    let real = scratch("parent-real");
    let link = tmp.join("lnk");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let Json(v) = make_dir(State(e.app.clone()), dirs_user(), mkdir_in(&link, "via-link")).await.unwrap();
    assert!(real.join("via-link").is_dir());
    assert_eq!(v["path"], json!(real.join("via-link").to_string_lossy()));
}

/// 遠端：同一段 sh 在本機的 `sh` 上真的跑（`HOME` 指到暫存目錄，不碰真的家目錄）。
#[test]
fn mkdir_remote_script_creates_reports_exists_and_refuses_credential_dirs() {
    let home = scratch("remote-home");
    std::fs::create_dir_all(home.join("work")).unwrap();
    std::fs::create_dir_all(home.join(".ssh/inner")).unwrap();
    let run = |parent: &str, name: &str| {
        let script = crate::hosts::make_dir_script(parent, name);
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).env("HOME", &home).output().unwrap();
        crate::hosts::parse_make_dir(&String::from_utf8_lossy(&out.stdout))
    };
    let work = home.join("work");
    // 絕對路徑、`~/`、`~` 三種上層寫法。
    assert_eq!(run(work.to_str().unwrap(), "a").unwrap(), crate::hosts::MakeDirOutcome::Created(work.join("a").to_string_lossy().into()));
    assert_eq!(run("~/work", "b").unwrap(), crate::hosts::MakeDirOutcome::Created(work.join("b").to_string_lossy().into()));
    assert_eq!(run("~", "c").unwrap(), crate::hosts::MakeDirOutcome::Created(home.join("c").to_string_lossy().into()));
    assert!(work.join("a").is_dir() && work.join("b").is_dir() && home.join("c").is_dir());
    // 已存在：資料夾、檔案、符號連結。
    std::fs::write(work.join("file"), "keep").unwrap();
    std::os::unix::fs::symlink("/tmp", work.join("link")).unwrap();
    for name in ["a", "file", "link"] {
        assert_eq!(run("~/work", name).unwrap(), crate::hosts::MakeDirOutcome::Exists, "{name}");
    }
    assert_eq!(std::fs::read_to_string(work.join("file")).unwrap(), "keep");
    // 名字裡的 shell 元字元不能被執行或拆開：整個當成一個名字。
    for name in ["$(touch pwned)", "a b", "x';touch pwned;'", "`touch pwned`", "*"] {
        let Ok(crate::hosts::MakeDirOutcome::Created(p)) = run("~/work", name) else { panic!("{name}") };
        assert_eq!(std::path::Path::new(&p).file_name().unwrap().to_string_lossy(), name);
    }
    assert!(!work.join("pwned").exists() && !home.join("pwned").exists(), "名字被當成命令跑了");
    // 憑證目錄：在底下建、或建出憑證目錄本身都擋；上層不存在回錯。
    assert!(run("~/.ssh", "x").unwrap_err().to_string().contains("forbidden"));
    assert!(run("~/.ssh/inner", "x").unwrap_err().to_string().contains("forbidden"));
    assert!(run("~", ".aws").unwrap_err().to_string().contains("forbidden"), "建出 ~/.aws 本身也擋");
    assert!(run("~", ".claude-new").unwrap_err().to_string().contains("forbidden"));
    assert!(!home.join(".aws").exists() && !home.join(".claude-new").exists());
    assert!(run("~/nope", "x").unwrap_err().to_string().contains("no such directory"));
}

#[test]
fn mkdir_remote_output_parser_covers_every_outcome() {
    use crate::hosts::{parse_make_dir, MakeDirOutcome};
    assert_eq!(parse_make_dir("AM_PATH=/srv/p/new\n").unwrap(), MakeDirOutcome::Created("/srv/p/new".into()));
    assert_eq!(parse_make_dir("AM_EXISTS=1\n").unwrap(), MakeDirOutcome::Exists);
    assert!(parse_make_dir("AM_ERR=forbidden directory\n").unwrap_err().to_string().contains("forbidden"));
    assert!(parse_make_dir("").is_err(), "沒有任何輸出不能當成功");
    assert!(parse_make_dir("garbage\n").is_err());
}

/// 遠端也一樣：憑證目錄不列、超大目錄有上限。
#[test]
fn dirs_the_remote_listing_refuses_credential_dirs_and_caps_a_huge_one() {
    let home = crate::testing::track(std::env::temp_dir().join(format!("am-test-dirs-home-{}", db::ulid())));
    std::fs::create_dir_all(home.join(".ssh/inner")).unwrap();
    std::fs::create_dir_all(home.join(".config/agents-manager/bots")).unwrap();
    std::fs::create_dir_all(home.join("many")).unwrap();
    for i in 0..2100 {
        std::fs::create_dir_all(home.join("many").join(format!("d{i:04}"))).unwrap();
    }
    let run = |path: &str| {
        let script = crate::hosts::dir_list_script(Some(path), true);
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).env("HOME", &home).output().unwrap();
        crate::hosts::parse_dir_listing(&String::from_utf8_lossy(&out.stdout), true)
    };
    for denied in ["~/.ssh", "~/.ssh/inner", "~/.config/agents-manager", "~/.config/agents-manager/bots"] {
        let err = run(denied).unwrap_err().to_string();
        assert!(err.contains("forbidden"), "{denied}: {err}");
    }
    let secret = home.join("secret-store");
    std::fs::create_dir_all(secret.join("inner")).unwrap();
    std::fs::remove_dir_all(home.join(".ssh")).unwrap();
    std::os::unix::fs::symlink(&secret, home.join(".ssh")).unwrap();
    for denied in ["~/secret-store", "~/secret-store/inner"] {
        let err = run(denied).unwrap_err().to_string();
        assert!(err.contains("forbidden"), "symlink alias {denied}: {err}");
    }
    let v = run("~/many").unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 2000);
    assert_eq!(v["truncated"], json!(true));
    assert_eq!(run("~").unwrap()["truncated"], json!(false));
}

/// #353：改了要重啟才生效的設定，`needs_restart` 不能只存在 PATCH 的 HTTP 回應裡——回應掉了（或 daemon 之後重啟）
/// 就永遠沒人知道執行中的 CLI 還拿著舊設定。現在從資料算：run 啟動時記下載入的版本，bot 目前的版本對不上＝要重啟。
#[tokio::test]
async fn a_config_change_under_a_running_bot_stays_visible_after_the_patch_response_is_lost() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "a", "kind": "claude"})).await.unwrap();
    let run = lifecycle::start_bot(&e.app, &id).await.unwrap();
    let stamped: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run).fetch_one(&e.app.db).await.unwrap();
    assert!(stamped.is_some(), "run 啟動時記下載入的版本");
    let needs = |e: &Env, id: String| {
        let app = e.app.clone();
        async move {
            let st = state_json(&app).await.unwrap();
            st["projects"].as_array().unwrap().iter().flat_map(|p| p["bots"].as_array().unwrap().clone()).find(|b| b["id"] == json!(id)).unwrap()["needs_restart"].clone()
        }
    };
    assert_eq!(needs(&e, id.clone()).await, json!(false));

    // 「回應掉了」：呼叫端沒拿到 needs_restart，我們也不看它——只看資料。
    let _lost = patch(&e, &id, json!({"persona": "換一份人設 A"})).await.unwrap();
    assert_eq!(needs(&e, id.clone()).await, json!(true), "改了設定、run 還是舊版：從資料看得出要重啟");

    // 重啟＝新 run 載入新版本，不再過期。
    lifecycle::restart_bot(&e.app, &id).await.unwrap();
    assert_eq!(needs(&e, id.clone()).await, json!(false));

    // 只改可以當場套用的欄位（claude 的 model → /model）：套用成功就是已生效，不能被誤判成過期。
    let out = patch(&e, &id, json!({"model": "opus"})).await.unwrap();
    if out.get("live_apply").is_some_and(|l| l["applied"] == json!(true)) {
        assert_eq!(needs(&e, id.clone()).await, json!(false), "當場套用成功：run 的版本跟著更新");
    }

    // 升版前的舊 run（沒記版本）：改設定那一刻補記「改之前」的版本，之後照樣看得出。
    sqlx::query("UPDATE runs SET launch_rev = NULL WHERE bot_id = ? AND state IN ('starting','running')").bind(&id).execute(&e.app.db).await.unwrap();
    assert_eq!(needs(&e, id.clone()).await, json!(false), "沒記版本＝不誤報");
    let _ = patch(&e, &id, json!({"persona": "換一份人設"})).await.unwrap();
    assert_eq!(needs(&e, id.clone()).await, json!(true), "舊 run 也在改設定的那刻開始被追蹤");
}

/// 稽核：沒有當場套用可走的欄位（persona 等），PATCH 回應的 `needs_restart` 只看「有 active run 且改了這類欄位」，
/// 不看值真的變沒變：改成跟現在一樣、或改回 run 啟動時的值，回應說要重啟，`/state` 卻（照 launch_rev）說不用。
#[tokio::test]
async fn a_patch_that_leaves_the_running_bots_settings_as_loaded_does_not_ask_for_a_restart() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "a", "kind": "claude", "persona": "P"})).await.unwrap();
    lifecycle::start_bot(&e.app, &id).await.unwrap();

    let out = patch(&e, &id, json!({"persona": "P"})).await.unwrap();
    assert_eq!(out["needs_restart"], json!(false), "跟載入的一樣：不必重啟 {out}");
    let out = patch(&e, &id, json!({"persona": "Q"})).await.unwrap();
    assert_eq!(out["needs_restart"], json!(true), "{out}");
    let out = patch(&e, &id, json!({"persona": "P"})).await.unwrap();
    assert_eq!(out["needs_restart"], json!(false), "改回 run 載入的值：不必重啟 {out}");
}

#[tokio::test]
async fn patch_does_not_commit_when_the_old_launch_revision_cannot_be_recorded() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "baseline", "kind": "claude", "persona": "A"})).await.unwrap();
    let run_id = lifecycle::start_bot(&e.app, &id).await.unwrap();
    let old_rev = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
    sqlx::query("UPDATE runs SET launch_rev = NULL WHERE id = ?").bind(&run_id).execute(&e.app.db).await.unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER refuse_launch_baseline BEFORE UPDATE OF launch_rev ON runs
             WHEN OLD.id = '{}' AND OLD.launch_rev IS NULL AND NEW.launch_rev IS NOT NULL
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
        run_id
    ))
    .execute(&e.app.db)
    .await
    .unwrap();

    let before = stored(&e, &id).await;
    let err = patch(&e, &id, json!({"persona": "B"})).await.unwrap_err();
    assert!(matches!(err, LcError::Unavailable(ref v) if v["reason"] == "launch_revision_baseline_failed"), "expected retryable refusal: {err:?}");
    assert_eq!(stored(&e, &id).await, before, "failed baseline write must leave TOML and DB projection unchanged");
    let launch_rev: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run_id).fetch_one(&e.app.db).await.unwrap();
    assert!(launch_rev.is_none(), "the failed stamp must not manufacture a baseline");

    sqlx::query("DROP TRIGGER refuse_launch_baseline").execute(&e.app.db).await.unwrap();
    let retried = patch(&e, &id, json!({"persona": "B"})).await.unwrap();
    assert_eq!(retried["needs_restart"], json!(true));
    assert_eq!(stored(&e, &id).await, (Some("B".into()), Some("B".into())), "retry stamps A before committing B");
    let launch_rev: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(launch_rev.as_deref(), Some(old_rev.as_str()), "retry must stamp the revision from config A before committing config B");
}

/// `GET /api/bots/{id}/terminal`：`format=ansi` 走 `pane.read` 的 ansi 並標 `format`；文字版形狀不變；`detection` 不接 ansi。
#[tokio::test]
async fn get_terminal_reads_ansi_only_when_asked_and_refuses_it_for_detection() {
    let e = crate::testing::env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "term-fmt", "kind": "claude", "model": "claude-sonnet-4-5"})).await.unwrap();
    tt::fake_run(&e.app, &id).await;
    let pane = format!("pane-{id}");
    e.herdr.set_screen(&pane, "\x1b[1mhi\x1b[0m plain");
    let get = |pairs: &[(&str, &str)]| {
        let q: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        get_terminal(State(e.app.clone()), Path(id.clone()), Query(q))
    };

    let text = get(&[("source", "visible")]).await.unwrap().0;
    assert!(text.get("format").is_none(), "文字版不加欄位：{text}");
    assert_eq!(text["text"], "\x1b[1mhi\x1b[0m plain", "fake herdr 原樣回傳");
    assert!(e.herdr.calls_to("pane.read").last().unwrap().get("format").is_none(), "預設不送 format 給 herdr");

    for source in ["visible", "recent", "recent_unwrapped"] {
        let ansi = get(&[("source", source), ("format", "ansi")]).await.unwrap().0;
        assert_eq!(ansi["format"], "ansi", "{source}: {ansi}");
        assert_eq!(ansi["source"], source);
        assert_eq!(e.herdr.calls_to("pane.read").last().unwrap()["format"], "ansi", "{source} 要向 herdr 要 ansi");
    }
    assert!(matches!(get(&[("format", "ansi"), ("source", "detection")]).await, Err(LcError::Bad(_))), "detection 不回假 ansi");
    assert!(matches!(get(&[("format", "html")]).await, Err(LcError::Bad(_))));
}

#[tokio::test]
async fn live_apply_stamps_the_run_selected_after_the_patch_snapshot() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "run-bound", "kind": "claude", "model": "claude-sonnet-4-5"})).await.unwrap();
    let run_a = tt::fake_run(&e.app, &id).await;
    let old_rev = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
    sqlx::query("UPDATE runs SET launch_rev = ?, runtime_model = 'claude-sonnet-4-5' WHERE id = ?")
        .bind(&old_rev)
        .bind(&run_a)
        .execute(&e.app.db)
        .await
        .unwrap();

    let run_b = db::ulid();
    let pane_b = format!("pane-{run_b}");
    e.herdr.set_screen(&pane_b, "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch to Claude Opus 5.5\n  2. No, go back\n");
    let screens = e.herdr.screens.clone();
    let pane_after_answer = pane_b.clone();
    crate::lifecycle::race_point::arm("slash_after_answer_before_read", &pane_b, move || async move {
        screens.lock().unwrap().insert(pane_after_answer, "Claude Code\n❯\n".into());
    });

    let app = e.app.clone();
    let bot_for_rollover = id.clone();
    let run_a_for_rollover = run_a.clone();
    let run_b_for_rollover = run_b.clone();
    let pane_for_rollover = pane_b.clone();
    let baseline = old_rev.clone();
    crate::lifecycle::race_point::arm("patch_after_active_snapshot", &id, move || async move {
        sqlx::query("UPDATE runs SET state = 'exited', ended_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(&run_a_for_rollover)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, started_at, launch_rev, runtime_model)
                 VALUES (?, ?, 'running', 'idle', ?, ?, ?, 'claude-sonnet-4-5')",
        )
        .bind(&run_b_for_rollover)
        .bind(&bot_for_rollover)
        .bind(&pane_for_rollover)
        .bind(db::now())
        .bind(&baseline)
        .execute(&app.db)
        .await
        .unwrap();
    });

    let out = patch(&e, &id, json!({"model": "claude-opus-5-5"})).await.unwrap();
    assert_eq!(out["live_apply"]["applied"], json!(true), "the replacement run accepted the live model: {out}");
    assert_eq!(out["needs_restart"], json!(false), "the run receiving the live apply must not keep a false restart badge");
    let a_rev: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run_a).fetch_one(&e.app.db).await.unwrap();
    let b_rev: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run_b).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(a_rev.as_deref(), Some(old_rev.as_str()), "bookkeeping must not stamp the stale PATCH snapshot");
    assert_eq!(b_rev.as_deref(), Some(crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap()).as_str()), "bookkeeping must stamp the actual live target run");
    let state = state_json(&e.app).await.unwrap();
    let needs_restart = state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap()["needs_restart"].clone();
    assert_eq!(needs_restart, json!(false));
}

#[tokio::test]
async fn a_deferred_live_apply_keeps_its_stamp_retry_after_launch_write_fails() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(
        &e,
        json!({"name": "deferred-stamp-retry", "kind": "codex", "model": "gpt-6-luna", "effort": "max", "fast": false}),
    )
    .await
    .unwrap();
    let run_id = tt::fake_run(&e.app, &id).await;
    let pane = format!("pane-{id}");
    let baseline = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
    sqlx::query(
        "UPDATE runs SET pane_id = ?, launch_rev = ?, runtime_model = 'gpt-6-luna', runtime_effort = 'max', runtime_fast = 0, agent_status = 'working' WHERE id = ?",
    )
    .bind(&pane)
    .bind(&baseline)
    .bind(&run_id)
    .execute(&e.app.db)
    .await
    .unwrap();

    // 回合中只切 fast 會直接送（#712）；輸入框裡有使用者的草稿就不碰，排到回合結束——這條測的是排隊那條路。
    e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
    let out = patch(&e, &id, json!({"fast": true}))
        .await
        .unwrap();
    assert_eq!(out["live_apply"]["deferred"], json!(true), "busy run should queue this live apply: {out}");
    assert!(e.herdr.calls_to("pane.send_text").is_empty(), "草稿還在框裡：一個字都不打");
    assert_eq!(out["needs_restart"], json!(false), "waiting for idle must not ask for restart: {out}");
    let pending_state = state_json(&e.app).await.unwrap();
    let pending_bot = pending_state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap();
    assert_eq!(pending_bot["needs_restart"], json!(false));
    assert_eq!(pending_bot["live_apply_deferred"], json!(true));
    assert_eq!(db::bot(&e.app.db, &id).await.unwrap().unwrap().fast, 1);
    let target = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
    sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?")
        .bind(&run_id)
        .execute(&e.app.db)
        .await
        .unwrap();
    e.herdr.set_screen(&pane, "gpt-6-luna max · /tmp · Context 43% used · 5h 12% left\n");
    let replace_screen = e.herdr.set_screen_later();
    let toggled_screen = "gpt-6-luna max fast · /tmp · Context 43% used · 5h 12% left\n";
    let pane_after_toggle = pane.clone();
    crate::lifecycle::race_point::arm(
        "codex_after_fast_toggle",
        &pane,
        move || async move {
            replace_screen(&pane_after_toggle, toggled_screen);
        },
    );
    sqlx::query(&format!(
        "CREATE TRIGGER refuse_deferred_launch_stamp BEFORE UPDATE OF launch_rev ON runs
             WHEN OLD.id = '{}' AND NEW.launch_rev = '{}'
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
        run_id, target
    ))
    .execute(&e.app.db)
    .await
    .unwrap();
    assert_eq!(
        crate::codex_live::parse_status_line(toggled_screen).map(|seen| seen.fast),
        Some(true),
        "the pane fixture must show Codex's confirmed fast tier"
    );

    let deferred_outcome = lifecycle::apply_deferred_once(&e.app, &id).await;
    assert!(
        matches!(deferred_outcome, Some(lifecycle::LiveApplyOutcome::Applied { .. })),
        "the deferred TUI readback must succeed before stamp debt exists: {deferred_outcome:?}"
    );
    let applied_state = state_json(&e.app).await.unwrap();
    let applied_bot = applied_state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap();
    assert_eq!(applied_bot["live_apply_deferred"], json!(false));

    let sent_before_retry = e.herdr.calls_to("pane.send_keys").len();
    assert_eq!(sent_before_retry, 1, "the deferred fast toggle ran exactly once");
    assert_eq!(
        e.herdr.screens.lock().unwrap().get(&pane).map(String::as_str),
        Some(toggled_screen),
        "the exact post-toggle pane revision must be visible at readback"
    );

    let (runtime, runtime_fast, launch_rev, live_rev): (
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT runtime_model, runtime_fast, launch_rev, live_rev FROM runs WHERE id = ?",
    )
    .bind(&run_id)
    .fetch_one(&e.app.db)
    .await
    .unwrap();
    assert_eq!(runtime.as_deref(), Some("gpt-6-luna"));
    assert_eq!(
        live_rev.as_deref(),
        Some(target.as_str()),
        "a successful live readback must retain durable stamp retry debt"
    );
    assert_eq!(launch_rev.as_deref(), Some(baseline.as_str()));
    assert_eq!(runtime_fast, Some(1), "the readback snapshot includes the changed fast tier");
    let state = state_json(&e.app).await.unwrap();
    let shown = state["projects"][0]["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == json!(id))
        .unwrap();
    assert_eq!(
        shown["needs_restart"],
        json!(false),
        "the matching persisted live revision must prevent a false restart badge"
    );

    sqlx::query("DROP TRIGGER refuse_deferred_launch_stamp")
        .execute(&e.app.db)
        .await
        .unwrap();
    let _ = lifecycle::retry_live_apply_bookkeeping_once(&e.app, &run_id)
        .await
        .unwrap();
    let (runtime_fast, launch_rev, live_rev): (Option<i64>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT runtime_fast, launch_rev, live_rev FROM runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
    assert_eq!(runtime_fast, Some(1));
    assert_eq!(launch_rev.as_deref(), Some(target.as_str()));
    assert_eq!(live_rev, None);
    assert_eq!(
        e.herdr.calls_to("pane.send_keys").len(),
        sent_before_retry,
        "retry changes bookkeeping only and never resends to the TUI"
    );
}

const CODEX_WORKING_ANSI: &str = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-working.ansi");
const CODEX_DRAFT_ANSI: &str = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-draft.ansi");

/// 真的回合中畫面（輸入框空的），最後一列換成 0.157 的狀態列：印的是顯示名 `GPT-6-Luna`，不是 id。
fn codex_working_screen(status: &str) -> String {
    let mut rows: Vec<&str> = CODEX_WORKING_ANSI.lines().collect();
    rows.pop();
    rows.push(status);
    rows.join("\n") + "\n"
}

/// 在跑一個回合的 codex bot（#712）：model／effort 已對上，fast 關。回 (bot id, run id, pane)。
async fn busy_codex(e: &tt::Env, name: &str) -> (String, String, String) {
    crate::testing::seed_project_config(e).await;
    let id = add(e, json!({"name": name, "kind": "codex", "model": "gpt-6-luna", "effort": "max", "fast": false})).await.unwrap();
    let run_id = tt::fake_run(&e.app, &id).await;
    let pane = format!("pane-{id}");
    let baseline = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
    sqlx::query(
        "UPDATE runs SET pane_id = ?, launch_rev = ?, runtime_model = 'gpt-6-luna', runtime_effort = 'max', runtime_fast = 0, agent_status = 'working' WHERE id = ?",
    )
    .bind(&pane)
    .bind(&baseline)
    .bind(&run_id)
    .execute(&e.app.db)
    .await
    .unwrap();
    (id, run_id, pane)
}

/// #712：回合中只切 fast——codex 0.157.1 的 `/fast` 回合中照樣生效，不排隊、不要求重啟、不按 Esc（Esc＝中斷回合）。
/// 狀態列印顯示名 `GPT-6-Luna` 也要讀回得過（以前大小寫不同就判 `readback_model_mismatch`、退回重啟）。
#[tokio::test]
async fn a_busy_fast_only_change_goes_in_during_the_turn() {
    let e = env().await;
    let (id, run_id, pane) = busy_codex(&e, "fast-mid-turn").await;
    e.herdr.set_screen(&pane, &codex_working_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left"));
    let replace_screen = e.herdr.set_screen_later();
    let toggled = codex_working_screen("  GPT-6-Luna max fast · /tmp · Context 43% used · 5h 12% left");
    let pane_after = pane.clone();
    crate::lifecycle::race_point::arm("codex_after_fast_toggle", &pane, move || async move {
        replace_screen(&pane_after, &toggled);
    });

    let out = patch(&e, &id, json!({"fast": true})).await.unwrap();
    assert_eq!(out["live_apply"]["applied"], json!(true), "回合中直接套用: {out}");
    assert_eq!(out["live_apply"]["deferred"], json!(false), "{out}");
    assert_eq!(out["needs_restart"], json!(false), "{out}");
    let typed: Vec<Value> = e.herdr.calls_to("pane.send_text");
    assert_eq!(typed.len(), 1, "一次 /fast: {typed:?}");
    assert_eq!(typed[0]["text"], json!("/fast"));
    let keys = e.herdr.calls_to("pane.send_keys");
    assert!(!format!("{keys:?}").contains("Escape"), "回合中絕不按 Esc（會中斷回合）: {keys:?}");
    let (runtime_model, runtime_fast): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT runtime_model, runtime_fast FROM runs WHERE id = ?").bind(&run_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(runtime_model.as_deref(), Some("gpt-6-luna"), "顯示名轉成 id 的寫法");
    assert_eq!(runtime_fast, Some(1));
    let state = state_json(&e.app).await.unwrap();
    let shown = state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap();
    assert_eq!(shown["needs_restart"], json!(false));
    assert_eq!(shown["live_apply_deferred"], json!(false));
}

/// #712：回合中輸入框有使用者的草稿——打 `/fast` 會接在草稿後面、Enter 把整段送出去。不碰，排到回合結束，也不要求重啟。
#[tokio::test]
async fn a_busy_fast_change_waits_when_the_composer_holds_a_draft() {
    let e = env().await;
    let (id, _run_id, pane) = busy_codex(&e, "fast-draft").await;
    e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
    let out = patch(&e, &id, json!({"fast": true})).await.unwrap();
    assert_eq!(out["live_apply"]["deferred"], json!(true), "{out}");
    assert_eq!(out["live_apply"]["reason"], json!("codex: busy_not_ready"), "{out}");
    assert_eq!(out["needs_restart"], json!(false), "{out}");
    assert!(e.herdr.calls_to("pane.send_text").is_empty(), "一個字都不打");
    assert!(e.herdr.calls_to("pane.send_keys").is_empty(), "一個鍵都不按");
    assert!(lifecycle::is_deferred(&id));
    lifecycle::apply_deferred_once(&e.app, &id).await; // 清掉全域排程，不留給別的測試
}

/// 稽核：閒著的 codex 輸入框裡有使用者的草稿時，`apply`（model／effort／fast 當場套用）不能把 `/model`、`/fast` 打在草稿後面——
/// 接在草稿後面的 Enter 會把整段「草稿/fast」當 prompt 送出去。回合中那條（#712）有這道檢查，閒著這條沒有。
#[tokio::test]
async fn an_idle_codex_live_apply_never_types_after_a_users_draft() {
    for patch_body in [json!({"fast": true}), json!({"effort": "high"})] {
        let e = env().await;
        let (id, run_id, pane) = busy_codex(&e, "idle-draft").await;
        sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?").bind(&run_id).execute(&e.app.db).await.unwrap();
        e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
        let out = patch(&e, &id, patch_body.clone()).await.unwrap();
        assert_eq!(out["live_apply"]["applied"], json!(false), "{patch_body}: {out}");
        assert!(e.herdr.calls_to("pane.send_text").is_empty(), "{patch_body}: 草稿後面不能再打字: {:?}", e.herdr.calls_to("pane.send_text"));
        assert!(e.herdr.calls_to("pane.send_keys").is_empty(), "{patch_body}: 一個鍵都不按（Enter 會送出草稿）: {:?}", e.herdr.calls_to("pane.send_keys"));
    }
}

const CODEX_IDLE_ANSI: &str = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-idle.ansi");

/// 回合被 Esc 停下之後的輸入列（空的、沒有 Working），最後一列換成指定的狀態列。
fn codex_idle_screen(status: &str) -> String {
    let mut rows: Vec<&str> = CODEX_IDLE_ANSI.lines().collect();
    rows.pop();
    rows.push(status);
    rows.join("\n") + "\n"
}

/// codex 0.162 的選單印顯示名（`GPT-6-Luna`），設定裡是 id（`gpt-6-luna`）。
const CODEX_MODEL_MENU: &str = "\
  Select Model and Effort

› 1. GPT-6.1-Sol (current)  Latest workhorse model for coding and everyday work.
  2. GPT-6-Astra            Frontier intelligence for the most demanding work.
  3. GPT-6-Luna             Fast and affordable model for easier tasks.

  enter select · esc back
";
const CODEX_EFFORT_MENU: &str = "\
  Select Reasoning Level for GPT-6-Luna

  1. Low (default)     Fast responses with lighter reasoning
  2. Medium            Balances speed and reasoning depth for everyday tasks
  3. High              Greater reasoning depth for complex problems
  4. Extra high        Extra high reasoning depth for complex problems
  5. More reasoning…   Max and Ultra consume usage limits faster

  enter default · s session · esc back
";

/// 在飛的回合（已確定送達），讓 `busy_codex` 的 bot 回合進行中。回 turn id。
async fn in_flight_turn_for(e: &tt::Env, id: &str, run_id: &str, delivery: &str) -> String {
    let conv = db::conversation_id(&e.app.db, id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight',?,?)")
        .bind(&turn_id)
        .bind(&conv)
        .bind(run_id)
        .bind(delivery)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    turn_id
}

async fn sent_texts(e: &tt::Env) -> Vec<String> {
    e.herdr.calls_to("pane.send_text").iter().map(|v| v["text"].as_str().unwrap_or_default().to_string()).collect()
}

fn sent_keys(e: &tt::Env) -> Vec<String> {
    e.herdr
        .calls_to("pane.send_keys")
        .iter()
        .flat_map(|v| v["keys"].as_array().cloned().unwrap_or_default())
        .map(|k| k.as_str().unwrap_or_default().to_string())
        .collect()
}

async fn turn_status(e: &tt::Env, turn_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(turn_id).fetch_one(&e.app.db).await.unwrap()
}

/// 回合中改強度的整段流程用的畫面腳本：Esc 之後回到輸入列、`/model` 兩層選單、選完回到輸入列（狀態列換成新強度）。
fn script_interrupt_apply(e: &tt::Env, pane: &str, after_esc: String, after_apply: String) {
    let screens = e.herdr.screens.clone();
    let (pane_a, screens_a) = (pane.to_string(), screens.clone());
    crate::lifecycle::race_point::arm("codex_interrupt_after_esc", pane, move || async move {
        screens_a.lock().unwrap().insert(pane_a, after_esc);
    });
    let (pane_b, screens_b) = (pane.to_string(), screens.clone());
    crate::lifecycle::race_point::arm("codex_apply_before_model_picker_read", pane, move || async move {
        screens_b.lock().unwrap().insert(pane_b, CODEX_MODEL_MENU.into());
    });
    let (pane_c, screens_c) = (pane.to_string(), screens.clone());
    crate::lifecycle::race_point::arm("codex_apply_before_effort_picker_read", pane, move || async move {
        screens_c.lock().unwrap().insert(pane_c, CODEX_EFFORT_MENU.into());
    });
    let (pane_d, screens_d) = (pane.to_string(), screens);
    crate::lifecycle::race_point::arm("codex_picker_before_final_read", pane, move || async move {
        screens_d.lock().unwrap().insert(pane_d, after_apply);
    });
}

/// #1681：回合中改強度——Esc 停下回合 → 套用 → 送 continue 接著跑，立即生效、不排到回合結束。
/// 回合不收（不是使用者中止）：continue 記成它的補充，Stop 之後同一個回合收成 completed。
#[tokio::test]
async fn a_busy_effort_change_interrupts_applies_and_resumes_the_same_turn() {
    let e = env().await;
    let (id, run_id, pane) = busy_codex(&e, "effort-interrupt").await;
    let turn_id = in_flight_turn_for(&e, &id, &run_id, "ok").await;
    e.herdr.set_screen(&pane, &codex_working_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left"));
    script_interrupt_apply(
        &e,
        &pane,
        codex_idle_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left"),
        codex_idle_screen("  GPT-6-Luna high · /tmp · Context 43% used · 5h 12% left"),
    );

    let out = patch(&e, &id, json!({"effort": "high"})).await.unwrap();
    assert_eq!(out["live_apply"]["applied"], json!(true), "立即套用: {out}");
    assert_eq!(out["live_apply"]["interrupted"], json!(true), "{out}");
    assert_eq!(out["live_apply"]["deferred"], json!(false), "{out}");
    assert_eq!(out["needs_restart"], json!(false), "{out}");
    assert!(!lifecycle::is_deferred(&id), "不排到回合結束");
    assert_eq!(sent_keys(&e).first().map(String::as_str), Some("Escape"), "第一個鍵是 Esc: {:?}", sent_keys(&e));
    assert_eq!(sent_texts(&e).await, vec!["/model", "3", "3", "continue"], "選單兩層（顯示名大小寫不同也認得）再接著跑");
    assert_eq!(sent_keys(&e).last().map(String::as_str), Some("Enter"), "最後送出 continue");
    assert_eq!(turn_status(&e, &turn_id).await, "in_flight", "回合沒被記成失敗／使用者中止");
    let users: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT content, sent_via FROM messages WHERE turn_id = ? AND role = 'user'").bind(&turn_id).fetch_all(&e.app.db).await.unwrap();
    assert_eq!(users, vec![("continue".to_string(), Some("supplement".to_string()))], "continue 是原回合的補充");
    let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'").bind(&turn_id).fetch_all(&e.app.db).await.unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("Esc") && notes[0].contains("強度"), "{notes:?}");
    let runtime_effort: Option<String> = sqlx::query_scalar("SELECT runtime_effort FROM runs WHERE id = ?").bind(&run_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(runtime_effort.as_deref(), Some("high"));
}

/// #1681：套用失敗（選單開不出來）也不能讓回合停在被打斷的狀態——照樣送 continue，留下需重啟與原因。
#[tokio::test]
async fn a_failed_apply_after_the_interrupt_still_resumes_the_turn() {
    let e = env().await;
    let (id, run_id, pane) = busy_codex(&e, "effort-interrupt-fail").await;
    let turn_id = in_flight_turn_for(&e, &id, &run_id, "ok").await;
    e.herdr.set_screen(&pane, &codex_working_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left"));
    let replace = e.herdr.set_screen_later();
    let idle = codex_idle_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left");
    let pane_after = pane.clone();
    crate::lifecycle::race_point::arm("codex_interrupt_after_esc", &pane, move || async move {
        replace(&pane_after, &idle);
    });

    let out = patch(&e, &id, json!({"effort": "high"})).await.unwrap();
    assert_eq!(out["live_apply"]["applied"], json!(false), "{out}");
    assert_eq!(out["live_apply"]["interrupted"], json!(true), "{out}");
    assert_eq!(out["live_apply"]["reason"], json!("codex: picker_failed"), "{out}");
    assert_eq!(out["needs_restart"], json!(true), "套不上就要看得到需重啟: {out}");
    assert_eq!(sent_texts(&e).await, vec!["/model", "continue"], "套用失敗仍送 continue");
    assert_eq!(turn_status(&e, &turn_id).await, "in_flight");
    let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'").bind(&turn_id).fetch_all(&e.app.db).await.unwrap();
    assert!(notes.iter().any(|n| n.contains("沒套上") && n.contains("picker_failed")), "{notes:?}");
}

/// #1681：continue 送不出去時 bot 不能卡在「被打斷、沒人接著跑」：回合收成 failed、對話寫明原因。設定照常算套用。
#[tokio::test]
async fn a_resume_that_cannot_be_typed_fails_the_turn_with_a_visible_reason() {
    let e = env().await;
    let (id, run_id, pane) = busy_codex(&e, "effort-interrupt-resume-fail").await;
    let turn_id = in_flight_turn_for(&e, &id, &run_id, "ok").await;
    e.herdr.set_screen(&pane, &codex_working_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left"));
    script_interrupt_apply(
        &e,
        &pane,
        codex_idle_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left"),
        codex_idle_screen("  GPT-6-Luna high · /tmp · Context 43% used · 5h 12% left"),
    );
    let fail = e.herdr.fail_later();
    let pane_hook = pane.clone();
    let screens = e.herdr.screens.clone();
    let after_apply = codex_idle_screen("  GPT-6-Luna high · /tmp · Context 43% used · 5h 12% left");
    crate::lifecycle::race_point::arm("codex_picker_before_final_read", &pane, move || async move {
        screens.lock().unwrap().insert(pane_hook, after_apply);
        fail("pane.send_text", tt::Fault::Refuse);
    });

    let out = patch(&e, &id, json!({"effort": "high"})).await.unwrap();
    assert_eq!(out["live_apply"]["applied"], json!(true), "設定已經落地: {out}");
    assert_eq!(out["live_apply"]["interrupted"], json!(true), "{out}");
    assert_eq!(turn_status(&e, &turn_id).await, "failed", "沒人接著跑：回合要收掉，不能掛在 in_flight");
    let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'").bind(&turn_id).fetch_all(&e.app.db).await.unwrap();
    assert!(notes.iter().any(|n| n.contains("continue") && n.contains("沒能送出")), "{notes:?}");
}

/// #1681：Esc 之後畫面確認不了回到可輸入——不盲打，退回排到回合結束（沒有 /model、沒有 continue）。
#[tokio::test]
async fn an_esc_that_never_settles_falls_back_to_waiting_for_the_turn_end() {
    let e = env().await;
    let (id, run_id, pane) = busy_codex(&e, "effort-interrupt-unsettled").await;
    in_flight_turn_for(&e, &id, &run_id, "ok").await;
    e.herdr.set_screen(&pane, &codex_working_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left"));

    let out = patch(&e, &id, json!({"effort": "high"})).await.unwrap();
    assert_eq!(out["live_apply"]["deferred"], json!(true), "{out}");
    assert_eq!(out["live_apply"]["interrupted"], json!(false), "{out}");
    assert_eq!(out["live_apply"]["reason"], json!("codex: busy_not_ready"), "{out}");
    assert_eq!(out["needs_restart"], json!(false), "{out}");
    assert_eq!(sent_keys(&e), vec!["Escape"], "只按過那一次 Esc");
    assert!(sent_texts(&e).await.is_empty(), "不盲打");
    lifecycle::apply_deferred_once(&e.app, &id).await;
}

/// #1681：該排著的照舊排著、一個鍵都不按——blocked（等人回答）、送達結果沒定案、輸入框有使用者的草稿。
#[tokio::test]
async fn blocked_unsettled_delivery_and_a_draft_are_never_interrupted() {
    for case in ["blocked", "pending_delivery", "draft"] {
        let e = env().await;
        let (id, run_id, pane) = busy_codex(&e, &format!("noint-{case}")).await;
        let delivery = if case == "pending_delivery" { "pending" } else { "ok" };
        in_flight_turn_for(&e, &id, &run_id, delivery).await;
        if case == "blocked" {
            sqlx::query("UPDATE runs SET agent_status = 'blocked' WHERE id = ?").bind(&run_id).execute(&e.app.db).await.unwrap();
        }
        let screen = if case == "draft" { CODEX_DRAFT_ANSI.to_string() } else { codex_working_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left") };
        e.herdr.set_screen(&pane, &screen);

        let out = patch(&e, &id, json!({"effort": "high"})).await.unwrap();
        assert_eq!(out["live_apply"]["deferred"], json!(true), "{case}: {out}");
        assert_eq!(out["live_apply"]["interrupted"], json!(false), "{case}: {out}");
        assert_eq!(out["needs_restart"], json!(false), "{case}: {out}");
        assert!(sent_keys(&e).is_empty(), "{case}: 一個鍵都不按: {:?}", sent_keys(&e));
        assert!(sent_texts(&e).await.is_empty(), "{case}: 一個字都不打");
        lifecycle::apply_deferred_once(&e.app, &id).await;
    }
}

/// #1681：只有 PATCH 那一刻會打斷；idle 邊補套（`apply_live_setting_with_revision`）回合又起來時照舊排回去，不去打斷新回合。
#[tokio::test]
async fn the_deferred_retry_never_interrupts_a_new_turn() {
    let e = env().await;
    let (id, run_id, pane) = busy_codex(&e, "effort-retry-no-interrupt").await;
    in_flight_turn_for(&e, &id, &run_id, "ok").await;
    e.herdr.set_screen(&pane, &codex_working_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left"));
    let rev = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());

    let outcome = lifecycle::apply_live_setting_with_revision(&e.app, &id, &["effort"], &rev, &rev).await;
    assert_eq!(outcome, lifecycle::LiveApplyOutcome::Failed("slash_gate: agent_busy".into()));
    assert!(sent_keys(&e).is_empty(), "{:?}", sent_keys(&e));
}

/// 排著的 live 套用在 `limit` 內被取走（`schedule_deferred_live` 延遲 1.5 秒後 `take`）。
async fn deferred_taken_within(id: &str, limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if !lifecycle::is_deferred(id) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

/// #712：agent 早就 idle、只剩回合紀錄還在飛時排下的，等不到 idle 邊——回合收掉（`emit_turn`）就要套。
#[tokio::test]
async fn a_fast_change_deferred_behind_an_in_flight_turn_applies_when_the_turn_closes() {
    let e = env().await;
    let (id, run_id, pane) = busy_codex(&e, "fast-turn-close").await;
    let conv = db::conversation_id(&e.app.db, &id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?").bind(&run_id).execute(&e.app.db).await.unwrap();
    e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
    let out = patch(&e, &id, json!({"fast": true})).await.unwrap();
    assert_eq!(out["live_apply"]["deferred"], json!(true), "{out}");
    assert!(!deferred_taken_within(&id, std::time::Duration::from_millis(2500)).await, "回合還在飛：不動");

    e.herdr.set_screen(&pane, "  gpt-6-luna max · /tmp · Context 43% used · 5h 12% left\n");
    sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?").bind(&turn_id).execute(&e.app.db).await.unwrap();
    lifecycle::emit_turn(&e.app, &turn_id).await;
    assert!(deferred_taken_within(&id, std::time::Duration::from_secs(6)).await, "回合收掉那一刻就該套");
}

/// #712：blocked（權限框）→ idle 也是一條 idle 邊：排著的 fast 要套，不是只等 working → idle。
#[tokio::test]
async fn a_deferred_fast_change_applies_after_a_blocked_prompt_closes() {
    let e = env().await;
    let (id, run_id, pane) = busy_codex(&e, "fast-after-blocked").await;
    e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
    let out = patch(&e, &id, json!({"fast": true})).await.unwrap();
    assert_eq!(out["live_apply"]["deferred"], json!(true), "{out}");
    sqlx::query("UPDATE runs SET agent_status = 'blocked' WHERE id = ?").bind(&run_id).execute(&e.app.db).await.unwrap();
    e.herdr.set_screen(&pane, "  gpt-6-luna max · /tmp · Context 43% used · 5h 12% left\n");
    let ev = crate::herdr::Event {
        event: "pane_agent_status_changed".into(),
        data: json!({"pane_id": pane, "agent_status": "idle"}),
    };
    crate::runners::events::handle_status(&e.app, crate::config::LOCAL_HOST, "test", &ev).await;
    assert!(deferred_taken_within(&id, std::time::Duration::from_secs(6)).await, "blocked → idle 就該套");
}

/// #712：claude 忙的時候改 model 排到回合結束、不要求重啟；再改 effort（slash 指令一次一個值）不合併，照舊要重啟。
#[tokio::test]
async fn a_busy_claude_defers_one_field_and_asks_for_restart_on_a_second() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "claude-busy", "kind": "claude", "model": "claude-sonnet-4-5", "effort": "high"})).await.unwrap();
    let run_id = tt::fake_run(&e.app, &id).await;
    let baseline = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
    sqlx::query("UPDATE runs SET pane_id = ?, launch_rev = ?, agent_status = 'working' WHERE id = ?")
        .bind(format!("pane-{id}"))
        .bind(&baseline)
        .bind(&run_id)
        .execute(&e.app.db)
        .await
        .unwrap();
    let first = patch(&e, &id, json!({"model": "claude-opus-5-5"})).await.unwrap();
    assert_eq!(first["live_apply"]["deferred"], json!(true), "{first}");
    assert_eq!(first["needs_restart"], json!(false), "{first}");
    let second = patch(&e, &id, json!({"effort": "low"})).await.unwrap();
    assert_eq!(second["live_apply"]["deferred"], json!(false), "{second}");
    assert_eq!(second["needs_restart"], json!(true), "{second}");
    lifecycle::apply_deferred_once(&e.app, &id).await;
}

#[tokio::test]
async fn a_live_apply_with_a_failed_runtime_write_is_not_reported_as_applied() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(
        &e,
        json!({"name": "runtime-write-fails", "kind": "claude", "model": "claude-sonnet-4-5"}),
    )
    .await
    .unwrap();
    let run_id = tt::fake_run(&e.app, &id).await;
    let pane = format!("pane-{id}");
    let baseline = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
    sqlx::query("UPDATE runs SET pane_id = ?, launch_rev = ?, runtime_model = 'claude-sonnet-4-5' WHERE id = ?")
        .bind(&pane)
        .bind(&baseline)
        .bind(&run_id)
        .execute(&e.app.db)
        .await
        .unwrap();
    e.herdr.set_screen(&pane, "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch to Claude Opus 5.5\n  2. No, go back\n");
    let screens = e.herdr.screens.clone();
    let pane_after_answer = pane.clone();
    crate::lifecycle::race_point::arm(
        "slash_after_answer_before_read",
        &pane,
        move || async move {
            screens
                .lock()
                .unwrap()
                .insert(pane_after_answer, "Claude Code\n❯\n".into());
        },
    );
    sqlx::query(&format!(
        "CREATE TRIGGER refuse_live_runtime BEFORE UPDATE OF runtime_model ON runs
             WHEN OLD.id = '{}' AND NEW.runtime_model = 'claude-opus-5-5'
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
        run_id
    ))
    .execute(&e.app.db)
    .await
    .unwrap();

    let mut events = e.app.subscribe();
    let out = patch(&e, &id, json!({"model": "claude-opus-5-5"}))
        .await
        .unwrap();

    assert_eq!(
        out["live_apply"]["applied"],
        json!(false),
        "runtime bookkeeping failed, so PATCH cannot claim a completed live apply: {out}"
    );
    assert_eq!(
        out["live_apply"]["pending_bookkeeping"],
        json!(true),
        "the observed TUI result must become DB-only retry debt: {out}"
    );
    assert_eq!(
        out["needs_restart"],
        json!(true),
        "failed runtime persistence cannot clear launch drift: {out}"
    );
    let runtime: Option<String> =
        sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
    assert_eq!(
        runtime.as_deref(),
        Some("claude-sonnet-4-5"),
        "the rejected UPDATE leaves the prior runtime snapshot intact"
    );
    while let Ok(event) = events.try_recv() {
        assert_ne!(
            event.kind, "bot_status",
            "runtime status cannot be published before the runtime row commits"
        );
    }
    let debt: (String, String, String) = sqlx::query_as(
        "SELECT run_id, target_rev, runtime_model FROM live_apply_debts WHERE run_id = ?",
    )
    .bind(&run_id)
    .fetch_one(&e.app.db)
    .await
    .unwrap();
    assert_eq!(debt.0, run_id);
    assert_eq!(
        debt.1,
        crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap())
    );
    assert_eq!(
        debt.2, "claude-opus-5-5",
        "the TUI readback snapshot, not a new TUI command, is the retry input"
    );
    let sent_before = e.herdr.calls_to("pane.send_text").len();
    assert_eq!(sent_before, 1, "the live command ran once");

    sqlx::query("DROP TRIGGER refuse_live_runtime")
        .execute(&e.app.db)
        .await
        .unwrap();
    let _ = lifecycle::retry_live_apply_bookkeeping_once(&e.app, &run_id)
        .await
        .unwrap();

    let (runtime, launch_rev, live_rev): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT runtime_model, launch_rev, live_rev FROM runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
    assert_eq!(runtime.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(
        launch_rev.as_deref(),
        Some(debt.1.as_str()),
        "retry stamps the exact run after its runtime snapshot commits"
    );
    assert_eq!(live_rev, None, "successful stamp clears its proof marker");
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM live_apply_debts WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
    assert_eq!(
        remaining, 0,
        "the debt clears only with the successful runtime transaction"
    );
    assert_eq!(
        e.herdr.calls_to("pane.send_text").len(),
        sent_before,
        "recovery is bookkeeping only; no slash resend"
    );
    let mut committed_status = None;
    while let Ok(event) = events.try_recv() {
        if event.kind == "bot_status" {
            committed_status = Some(event);
        }
    }
    let committed_status = committed_status.expect("committed runtime row should publish bot_status");
    assert_eq!(
        committed_status.data["run"]["runtime_model"],
        json!("claude-opus-5-5"),
        "bot_status must only carry the committed runtime snapshot"
    );
    let state = state_json(&e.app).await.unwrap();
    let shown = state["projects"][0]["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == json!(id))
        .unwrap();
    assert_eq!(shown["needs_restart"], json!(false));
}

/// 刪掉再還原：設定跟著回來（還原是從 DB 那一列重建 config 條目，漏抄欄位會在這裡悄悄變回預設）。
#[tokio::test]
async fn restoring_a_deleted_bot_keeps_its_persona() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "keep", "kind": "claude", "persona": "P"})).await.unwrap();

    delete_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();
    assert!(e.app.cfg.get().await.projects[0].bots.iter().all(|b| b.id.as_deref() != Some(id.as_str())), "刪掉之後 config 裡沒有了");
    restore_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();

    assert_eq!(stored(&e, &id).await, (Some("P".into()), Some("P".into())));
}

/// #295：還原不能把讀不懂的 args／env 當成空的寫進 config，也不能把 herdr_session 洗成 NULL。
#[tokio::test]
async fn restoring_keeps_env_args_and_session_and_refuses_unreadable_json() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "keep2", "kind": "claude"})).await.unwrap();
    delete_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();
    sqlx::query(r#"UPDATE bots SET herdr_session='sess', env_json='{"A":"1"}', args_json='["--x"]' WHERE id=?"#).bind(&id).execute(&e.app.db).await.unwrap();
    restore_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();
    let cfg = e.app.cfg.get().await;
    let b = cfg.projects[0].bots.iter().find(|b| b.id.as_deref() == Some(id.as_str())).unwrap();
    assert_eq!(b.env.get("A").map(String::as_str), Some("1"));
    assert_eq!(b.args, vec!["--x".to_string()]);
    assert_eq!(b.herdr_session.as_deref(), Some("sess"));
    assert_eq!(db::bot(&e.app.db, &id).await.unwrap().unwrap().herdr_session.as_deref(), Some("sess"));

    delete_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();
    sqlx::query("UPDATE bots SET env_json='not json' WHERE id=?").bind(&id).execute(&e.app.db).await.unwrap();
    assert!(restore_bot(State(e.app.clone()), Path(id.clone())).await.is_err(), "讀不懂的 env 不能還原成空的");
    assert!(db::bot(&e.app.db, &id).await.unwrap().unwrap().deleted_at.is_some());
    assert!(e.app.cfg.get().await.projects[0].bots.iter().all(|b| b.id.as_deref() != Some(id.as_str())));
}

/// 專案已刪的 user bot 還原：是 409（前提不成立，重試也一樣），不是 502 upstream，也不能把 bot 復活。
#[tokio::test]
async fn restoring_a_bot_whose_project_is_deleted_is_a_conflict_not_a_502() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, json!({"name": "orphan", "kind": "claude"})).await.unwrap();
    delete_project(State(e.app.clone()), Path(e.project_id.clone())).await.unwrap();
    let err = restore_bot(State(e.app.clone()), Path(id.clone())).await.unwrap_err();
    assert!(matches!(&err, LcError::Conflict(v) if v["reason"] == "the project of this bot is deleted" && v["project_id"] == json!(e.project_id)), "{err:?}");
    assert!(db::bot(&e.app.db, &id).await.unwrap().unwrap().deleted_at.is_some());
}

/// 舊的 config.toml 還留著 `instruction_files = …`：照樣讀得進來、投影得動，bot 不受影響。
#[tokio::test]
async fn a_leftover_instruction_files_key_in_config_toml_is_ignored() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = db::ulid();
    let bid = id.clone();
    e.app
        .cfg
        .update(move |cfg| {
            let b: crate::config::BotCfg = toml::from_str(&format!("id = '{bid}'\nname = 'old'\nkind = 'claude'\ninstruction_files = 'managed-only'\n")).unwrap();
            cfg.projects[0].bots.push(b);
            Ok(())
        })
        .await
        .unwrap();
    crate::projection::project_config(&e.app.cfg, &e.app.db).await.unwrap();
    lifecycle::start_bot(&e.app, &id).await.unwrap();
    assert!(settings_of_last_start(&e).get("pluginConfigs").is_none());
}
