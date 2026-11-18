//! Building the AGM environment: a dedicated directory, project and bot.
//!
//! Deliberately *not* the agents-manager checkout: a manager whose cwd is a source tree reads
//! that tree's CLAUDE.md and starts editing code. It gets its own directory, its own project
//! row (so config projection and hooks work exactly as for any other bot), and nothing else.

use crate::config::{BotCfg, ProjectCfg};
use crate::lifecycle::LcError;
use crate::supervisor::ports::ConfigProjection;
use sqlx::SqlitePool;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::store;

/// The persona, compiled in from the plan so a deployment cannot drift from the reviewed text.
pub const PERSONA_DOC: &str = include_str!("../../../../docs/goals/agm-supervisor-persona.md");

pub const BOT_NAME: &str = "AGM";

/// `--remote-control <name>`（或 `--remote-control=<name>`）從 args 拿掉，其他 args 原樣；沒有這個旗標就回 `None`。
///
/// 使用者 2026-10-12 裁示廢掉手機 Remote Control（#1477）：以前 setup 把 `["--remote-control","AGM"]` 寫進巡檢的 args，
/// 既有安裝裡還留著這一對。旗標後面不是另一個旗標才算它的名字；其餘 args 是使用者的，不動。
pub fn without_remote_control(args: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(args.len());
    let mut found = false;
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        if arg == "--remote-control" {
            found = true;
            if it.peek().is_some_and(|name| !name.starts_with('-')) {
                it.next();
            }
        } else if arg.starts_with("--remote-control=") {
            found = true;
        } else {
            out.push(arg.clone());
        }
    }
    found.then_some(out)
}

/// 既有安裝的遷移（#1477）：巡檢與協調者兩顆 bot 的 args 若還帶著 `--remote-control <name>` 就拿掉那一對。
/// 冪等（沒有就不寫 config）；只改 config.toml 的 args——執行中的 session 照舊，`launch_rev` 一變就是「需重啟」
/// （既有機制），**不**自動重啟 AGM。回傳改了幾顆。daemon 開機與每次 `setup` 都會跑。
pub async fn strip_remote_control_args(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + ConfigProjection)) -> Result<usize, LcError> {
    let up = |e: &dyn std::fmt::Display| LcError::Upstream(e.to_string());
    let mut ids = Vec::new();
    if let Some(id) = store::get_or_init(app.db()).await.map_err(|e| up(&e))?.bot_id {
        ids.push(id);
    }
    if let Some(id) = super::roles::bot_for(app.db(), super::roles::Role::Responder).await.map_err(|e| up(&e))? {
        ids.push(id);
    }
    if ids.is_empty() {
        return Ok(0);
    }
    let cfg = app.cfg().get().await;
    let stale: Vec<String> = cfg
        .projects
        .iter()
        .flat_map(|p| p.bots.iter())
        .filter(|b| without_remote_control(&b.args).is_some())
        .filter_map(|b| b.id.clone())
        .filter(|id| ids.contains(id))
        .collect();
    if stale.is_empty() {
        return Ok(0);
    }
    let targets = stale.clone();
    app.update_and_project(move |cfg| {
        for bot in cfg.projects.iter_mut().flat_map(|p| p.bots.iter_mut()) {
            if bot.id.as_ref().is_some_and(|id| targets.contains(id)) {
                if let Some(args) = without_remote_control(&bot.args) {
                    bot.args = args;
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| up(&e))?;
    for id in &stale {
        tracing::info!(bot = %id, "removed the retired --remote-control argument from the supervisor bot's args (restart to apply)");
        app.emit("bot_changed", json!({"bot_id": id})).await;
    }
    Ok(stale.len())
}

/// 巡檢可用的模型。`fable`／`opus` 由控制器依 Fable 週桶自動互換；`sonnet` 是使用者
/// 指定的固定模型（2026-10-30），控制器不會把它切走，只有 `fallback {model}` 改得動。
pub const CANDIDATES: [&str; 3] = ["fable", "opus", "sonnet"];

/// 不認得的值一律當 `fable`（舊行為）。
pub fn candidate(model: &str) -> &'static str {
    CANDIDATES.into_iter().find(|c| *c == model).unwrap_or("fable")
}

/// Resolve the supervisor candidate and apply the shared retired-model policy before launch.
pub fn model_arg(candidate: &str) -> &'static str {
    crate::models::canonical_model("claude", self::candidate(candidate))
}

pub fn other_candidate(candidate: &str) -> &'static str {
    if candidate == "fable" {
        "opus"
    } else {
        "fable"
    }
}

pub fn agm_dir(app: &impl crate::capabilities::DataDir) -> PathBuf {
    app.data_dir().join("supervisor").join(BOT_NAME)
}

/// Everything after the `---` separator: the header above it is guidance for humans reading
/// the plan, not part of what the manager is told about itself.
///
/// This is the *embedded* text — the seed for a first install and nothing more. What the
/// manager runs on is the stored persona (`supervisors.persona_text`); see `persona.rs`.
pub fn persona_body() -> String {
    match PERSONA_DOC.split_once("\n---\n") {
        Some((_, body)) => body.trim().to_string(),
        None => PERSONA_DOC.trim().to_string(),
    }
}

/// The persona the manager actually runs on: stored if there is one, the embedded text only as
/// a seed. Returns `(text, seeded)`.
pub async fn effective_persona(db: &SqlitePool) -> Result<(String, bool), LcError> {
    let embedded = persona_body();
    let current = store::get_or_init(db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let legacy = if current.persona_text.as_deref().is_none_or(str::is_empty) {
        match current.bot_id.as_deref() {
            Some(id) => crate::db::bot(db, id).await
                .map_err(|e| LcError::Upstream(e.to_string()))?
                .and_then(|b| b.persona).filter(|t| !t.trim().is_empty()),
            None => None,
        }
    } else { None };
    let seeded = match legacy {
        Some(ref text) => store::seed_persona_from(db, text, "legacy_bot", &embedded).await,
        None => store::seed_persona_if_empty(db, &embedded).await,
    }.map_err(|e| LcError::Upstream(e.to_string()))?;
    let sup = store::get_or_init(db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok((sup.persona_text.filter(|t| !t.is_empty()).unwrap_or(embedded), seeded))
}

/// Both roles share one project, so its instruction file must not name either role's id or cwd.
pub(super) fn shared_agent_md() -> String {
    r#"# AGM 共用作業規則

此檔由巡檢與協調者共用，不用來判定你的角色、bot id 或工作目錄。
- 角色由這顆 bot 的 persona 與自己工作目錄的 `runtime.json` 決定；自己的 id 以 `AM_BOT_ID`／`runtime.json` 的 `self_bot_id` 為準。
- 只讀自己工作目錄的 `runtime.json`、`handoff.md`；不要因為這份指示檔位於另一個角色的目錄就切換過去。
- daemon 位址讀自己 `runtime.json` 的 `daemon_url`（loopback URL，含實際 port）；不要沿用另一個角色的位址或自行猜 port。
- 子 agent 不因繼承這份文件取得 AGM 身份或權限，仍只處理 parent 指派的任務。
- 先用自己工作目錄的 `bin/agm --help` 核對可用命令；沒列出的不假設存在，也不自行拼 API 繞過。
- 恢復時先讀自己的 `handoff.md`，再依 `runtime.json` 的 role 查 inbox：patrol 查未結案 assignments；responder 查 `assignments --awaiting-review`，最後對帳即時狀態。
- 舊 pending 不等於尚未送達，重試前先對帳；管理摘要的權威在資料庫，檔案只是可重建副本。
- 模型與強度遵守全域派工政策；角色 bot 自己的固定模型不是 worker 的預設。
- 此目錄不是產品原始碼；不在這裡改專案程式，不把 token、登入秘密或完整環境變數寫進檔案或回覆。
- Remote Control 由各角色的啟動環境建立（巡檢名稱是 `AGM`）；不要自行開關。
"#.to_string()
}

/// 測試接縫（#1862 S2a）：`shared_agent_md` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫（不改行為）；`#[doc(hidden)]`，正式 build 不編。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub fn shared_agent_md_for_test() -> String {
    shared_agent_md()
}

fn claude_md(_dir: &Path, _bot_id: &str, _port: u16) -> String {
    shared_agent_md()
}

#[cfg(any(test, feature = "test-hooks"))]
/// 測試接縫（#1862 S1）：`claude_md` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫；`#[doc(hidden)]`，正式 build 不編。
#[doc(hidden)]
pub fn claude_md_for_test(dir: &Path, bot_id: &str, port: u16) -> String {
    claude_md(dir, bot_id, port)
}

/// The manager's tool entry point, compiled into the daemon binary. Deploying it from a
/// path on the build machine would leave every release install without a CLI.
pub const AGM_CLI: &str = include_str!("../../../../scripts/agm.py");

/// What actually got written, so the caller can report honestly instead of assuming.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Deployed {
    pub cwd: String,
    pub agm_cli: String,
}

/// The CLI's runtime configuration.
///
/// No token, by design: `bin/agm` asks the daemon for one over loopback at run time, so the
/// credential never sits in a file, in argv, or in a handoff note.
///
/// `responder_id`：協調者的 bot（沒建立就是 null）。巡檢目錄的 ops 腳本（`claude-release-kick.sh`）派工給它——
/// 巡檢不能對自己下交辦。以前只有協調者自己的目錄寫這一欄，全新安裝的巡檢目錄沒有，腳本每一輪都跳過（review2 sup #1）。
fn runtime_json(port: u16, bot_id: &str, responder_id: Option<&str>, data_dir: &str) -> Value {
    json!({
        "daemon_url": format!("http://127.0.0.1:{port}"),
        "manager_bot_id": bot_id,
        "responder_bot_id": responder_id,
        // The CLI also accepts `bot_id`, which an earlier draft wrote. Both are emitted so a
        // deployment cannot be broken by whichever half is upgraded first.
        "bot_id": bot_id,
        // 雙角色（SPEC §18.15）：這個目錄是巡檢的。`bin/agm` 用它決定預設看哪個角色的 inbox、
        // 回報時以誰的名義；真正的身份驗證靠 pane 環境裡的 bot token，不靠這個字串。
        "role": "patrol",
        "self_bot_id": bot_id,
        "data_dir": data_dir,
        "supervisor_id": store::SUPERVISOR_ID,
    })
}

#[cfg(any(test, feature = "test-hooks"))]
/// 測試接縫（#1862 S1）：`runtime_json` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫；`#[doc(hidden)]`，正式 build 不編。
#[doc(hidden)]
pub fn runtime_json_for_test(port: u16, bot_id: &str, responder_id: Option<&str>, data_dir: &str) -> Value {
    runtime_json(port, bot_id, responder_id, data_dir)
}

/// Write the directory contents. Idempotent: every file is rewritten from the current
/// daemon, except `handoff.md`, which belongs to the manager once it exists.
///
/// `persona` is the **stored** text, passed in rather than read from the binary: `persona.md`
/// is a readable copy of what the manager is actually running on, and regenerating it from the
/// embedded default would make the copy disagree with the original.
pub fn deploy_files(app: &(impl crate::capabilities::DataDir + crate::capabilities::ListenPort), bot_id: &str, responder_id: Option<&str>, persona: &str) -> std::io::Result<Deployed> {
    let dir = agm_dir(app);
    std::fs::create_dir_all(dir.join("bin"))?;
    // 原子寫（#919）：`fs::write` 的 `O_TRUNC` 窗口會讓讀的人（`bin/agm` 每次執行都讀 runtime.json）看到空的或半截。
    write_file(&dir.join("CLAUDE.md"), claude_md(&dir, bot_id, app.port()).as_bytes())?;
    write_file(&dir.join("persona.md"), persona.as_bytes())?;
    write_runtime_json(app, bot_id, responder_id)?;
    if !dir.join("handoff.md").exists() {
        write_file(&dir.join("handoff.md"), "# AGM 管理摘要\n\n（尚未寫入。權威紀錄在 AG Man 資料庫。）\n".as_bytes())?;
    }
    // 原子寫（issue #520）：這支 CLI 一直有人在跑（所有 kick 每 5～30 分鐘 exec 一次），
    // `fs::write` 的 `O_TRUNC` 窗口會讓它們讀到半截的 python。跟 `cli_refresh` 用同一個 helper，
    // 免得同一個檔兩條路兩種寫法（這正是 #520 的形狀）。
    let bin = dir.join("bin").join("agm");
    super::cli_refresh::install_executable(&bin, AGM_CLI.as_bytes())?;
    Ok(Deployed { cwd: dir.to_string_lossy().to_string(), agm_cli: "deployed".into() })
}

/// 只重寫巡檢目錄的 `runtime.json`。協調者在巡檢之後才建立時由 `responder::ensure_env` 呼叫，
/// 巡檢目錄還不存在（沒 setup 過）就什麼都不做。
pub fn write_runtime_json(app: &(impl crate::capabilities::DataDir + crate::capabilities::ListenPort), bot_id: &str, responder_id: Option<&str>) -> std::io::Result<()> {
    let dir = agm_dir(app);
    if !dir.is_dir() {
        return Ok(());
    }
    write_file(
        &dir.join("runtime.json"),
        serde_json::to_string_pretty(&runtime_json(app.port(), bot_id, responder_id, &app.data_dir().to_string_lossy()))?.as_bytes(),
    )
}

/// AGM 目錄裡的文字檔（CLAUDE.md、persona.md、runtime.json、handoff.md）：0644，原子寫入（[`am_base::atomic_file`]）。
pub fn write_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    am_base::atomic_file::write(path, data, am_base::atomic_file::Mode::Exact(0o644))
}

/// Create (or find) the AGM project and bot in config.toml, then project it into SQLite.
/// Never starts anything — `POST /api/supervisor/start` is a separate, explicit step.
pub async fn ensure_env(app: &(impl crate::capabilities::Cfg + crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::ListenPort + crate::supervisor::ports::HostProbes + ConfigProjection)) -> Result<(String, String, Deployed), LcError> {
    let sup = store::get_or_init(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let dir = agm_dir(app);
    std::fs::create_dir_all(&dir).map_err(|e| LcError::Bad(format!("{}: {e}", dir.display())))?;
    let path = crate::config::canonical_path(&dir.to_string_lossy()).map_err(|e| LcError::Bad(e.to_string()))?;

    // The identity has to exist already: silently running the manager under whatever account
    // happens to be default is exactly the kind of guess that produces a bot nobody can log in.
    // #2002: admit before validating, held until the config write is committed (see `identity_fence`).
    let identity_admit = am_base::identity_fence::admit(crate::config::LOCAL_HOST, "claude", Some(&sup.identity)).await;
    if app.identity_for_host(crate::config::LOCAL_HOST, &sup.identity).await.is_none() {
        return Err(LcError::conflict(
            "supervisor identity is not configured on this host",
            json!({"reason": "identity_missing", "identity": sup.identity}),
        ));
    }

    // The persisted ids win over the name. The user may have renamed AGM in the sidebar, and
    // an unrelated bot may since have been called `AGM` — matching on the name would either
    // create a duplicate manager or quietly take over somebody else's bot.
    let known_project = sup.project_id.clone();
    let known_bot = sup.bot_id.clone();
    let fresh_project = crate::db::ulid();
    let fresh_bot = crate::db::ulid();
    let model = model_arg(&sup.active_model).to_string();
    // Stored wins. An older binary running `setup` used to write its own compiled-in text back
    // over the bot, which is how a persona that had just been updated could be silently rolled
    // back; the embedded copy is now only ever a seed for an install that has none.
    let (persona, seeded) = effective_persona(app.db()).await?;
    // The closure below moves its copy into the config update; `persona` itself is still needed
    // afterwards to write the readable copy.
    let persona_for_cfg = persona.clone();
    if seeded {
        tracing::info!("seeded the AGM persona from the existing bot or embedded first-install default");
    }
    let effort = sup.effort.clone();
    let identity = sup.identity.clone();
    let p2 = path.clone();

    let (project_id, bot_id) = app.update_and_project(move |cfg| {
            // Project: persisted id first, then the directory, then a new one.
            let pidx = cfg
                .projects
                .iter()
                .position(|p| known_project.is_some() && p.id == known_project)
                .or_else(|| cfg.projects.iter().position(|p| p.path == p2 && p.host == crate::config::LOCAL_HOST));
            let pidx = match pidx {
                Some(i) => i,
                None => {
                    cfg.projects.push(ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                        handed_off_to: None,
                        id: Some(fresh_project.clone()),
                        path: p2.clone(),
                        label: BOT_NAME.to_string(),
                        host: crate::config::LOCAL_HOST.to_string(),
                        bots: vec![],
                    });
                    cfg.projects.len() - 1
                }
            };
            if cfg.projects[pidx].id.is_none() {
                cfg.projects[pidx].id = Some(fresh_project.clone());
            }
            let project_id = cfg.projects[pidx].id.clone().expect("set above");

            // Bot: only the persisted id identifies the manager. Without one, the name must be
            // free — an existing `AGM` we have never managed belongs to someone else.
            let proj = &mut cfg.projects[pidx];
            let bidx = match known_bot.as_ref() {
                Some(id) => proj.bots.iter().position(|b| b.id.as_ref() == Some(id)),
                // 還沒記過 bot：這時找到的專案只會是 daemon 自己目錄裡的那一個，同名的 `AGM` 只可能是上一次設定寫進 config、
                // 還沒來得及記進角色列就失敗的那一顆——接著用它（issue #181）。當成別人的回 `name_taken`，就永遠設定不起來。
                // 只認 setup 自己留了記號的那一顆（`bot_requests::ROLE_SETUP_MARK_PREFIX`），不看名字：同名但沒記號的是別人建的，
                // 而這顆被使用者改了名之後重跑也要找得回來。
                None => proj.bots.iter().position(|b| b.create_request_id.as_deref() == Some(super::bot_requests::ROLE_SETUP_MARK_PREFIX_PATROL)),
            };
            let bidx = match bidx {
                Some(i) => i,
                None => {
                    if proj.bots.iter().any(|b| b.name == BOT_NAME) {
                        anyhow::bail!("name-taken");
                    }
                    proj.bots.push(BotCfg { role: None, manager_use: true, track: None, runtime_override: None, fork_from: None,
                        id: Some(fresh_bot.clone()),
                        name: BOT_NAME.to_string(),
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
                        create_request_id: Some(super::bot_requests::ROLE_SETUP_MARK_PREFIX_PATROL.into()),
                        create_fingerprint: None,
                        ..Default::default()
                    });
                    proj.bots.len() - 1
                }
            };
            let bot = &mut proj.bots[bidx];
            if bot.id.is_none() {
                bot.id = Some(fresh_bot.clone());
            }
            // The name is left alone on purpose: a rename in the sidebar is the user's, and the
            // supervisor is identified by its id, not by what it is called.
            bot.kind = "claude".into();
            bot.model = Some(model.clone());
            bot.effort = Some(effort.clone());
            bot.identity = Some(identity.clone());
            bot.persona = Some(persona_for_cfg.clone());
            // 手機 Remote Control 已廢（#1477）：舊安裝留下的 `--remote-control <name>` 在這裡拿掉，使用者自己加的其他 args 不動。
            if let Some(args) = without_remote_control(&bot.args) {
                bot.args = args;
            }
            // Verified by hand before it is allowed to come back by itself.
            bot.autostart = false;
            bot.inject_hooks = true;
        Ok((project_id, bot.id.clone().expect("set above")))
    })
    .await
    .map_err(|e| {
        if e.to_string() == "name-taken" {
            LcError::conflict(
                "a different bot is already called AGM in the supervisor project",
                json!({"reason": "name_taken", "name": BOT_NAME}),
            )
        } else {
            LcError::Upstream(e.to_string())
        }
    })?;
    drop(identity_admit);

    let responder = super::roles::responder_bot(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?.map(|b| b.id);
    let deployed = deploy_files(app, &bot_id, responder.as_deref(), &persona).map_err(|e| LcError::Upstream(e.to_string()))?;
    store::set_env(app.db(), &bot_id, &project_id, &deployed.cwd)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok((project_id, bot_id, deployed))
}
