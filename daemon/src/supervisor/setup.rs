//! Building the AGM environment: a dedicated directory, project and bot.
//!
//! Deliberately *not* the agents-manager checkout: a manager whose cwd is a source tree reads
//! that tree's CLAUDE.md and starts editing code. It gets its own directory, its own project
//! row (so config projection and hooks work exactly as for any other bot), and nothing else.

use crate::config::{BotCfg, ProjectCfg};
use crate::lifecycle::LcError;
use crate::state::App;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::store;

/// The persona, compiled in from the plan so a deployment cannot drift from the reviewed text.
pub const PERSONA_DOC: &str = include_str!("../../../docs/goals/agm-supervisor-persona.md");

pub const BOT_NAME: &str = "AGM";
pub const REMOTE_NAME: &str = "AGM";

/// `fable` / `opus` are the two candidates the plan fixes. The alias — not a pinned
/// `claude-<x>-5.y` id — is what goes on the command line: the concrete id behind each alias
/// moves with the CLI, and a supervisor that stops starting after a model release is worse
/// than one that follows the alias.
pub fn model_arg(candidate: &str) -> &'static str {
    match candidate {
        "opus" => "opus",
        _ => "fable",
    }
}

pub fn other_candidate(candidate: &str) -> &'static str {
    if candidate == "fable" {
        "opus"
    } else {
        "fable"
    }
}

pub fn agm_dir(app: &Arc<App>) -> PathBuf {
    app.data_dir.join("supervisor").join(BOT_NAME)
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
pub async fn effective_persona(app: &Arc<App>) -> Result<(String, bool), LcError> {
    let embedded = persona_body();
    let current = store::get_or_init(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let legacy = if current.persona_text.as_deref().is_none_or(str::is_empty) {
        match current.bot_id.as_deref() {
            Some(id) => crate::db::bot(&app.db, id).await
                .map_err(|e| LcError::Upstream(e.to_string()))?
                .and_then(|b| b.persona).filter(|t| !t.trim().is_empty()),
            None => None,
        }
    } else { None };
    let seeded = match legacy {
        Some(ref text) => store::seed_persona_from(&app.db, text, "legacy_bot", &embedded).await,
        None => store::seed_persona_if_empty(&app.db, &embedded).await,
    }.map_err(|e| LcError::Upstream(e.to_string()))?;
    let sup = store::get_or_init(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok((sup.persona_text.filter(|t| !t.is_empty()).unwrap_or(embedded), seeded))
}

fn claude_md(dir: &Path, bot_id: &str, port: u16) -> String {
    format!(
        r#"# AGM — agents-manager 總管

你是 AGM。角色前導詞由 daemon 以 persona 注入（同目錄 `persona.md` 是同一份的可讀副本，
不要再把它整份貼進對話）。

## 執行期
- AG Man daemon：`http://127.0.0.1:{port}`
- 你自己的 bot id：`{bot_id}`
- 工作目錄：`{dir}`
- 執行期設定：`runtime.json`（沒有 token；CLI 自己去 `GET /api/session` 拿）
- 管理摘要：`handoff.md`（可重建的副本，權威在資料庫 `GET /api/supervisor/handoff`）

## 可用工具
- `bin/agm`：結構化 JSON CLI，是你操作 AG Man 的入口。先跑 `bin/agm --help` 看目前**實際**有哪些子命令。
- 沒有出現在 `--help` 裡的子命令就是還沒實作，不要假設它存在，也不要自己拼 curl 繞過去。

## 恢復流程
接班或重啟後，依序：讀 `handoff.md` → `bin/agm assignments` 未結案 → `bin/agm inbox` 待處理
→ 再查即時狀態。舊的 pending 不等於還沒送出，一律先對帳再決定。

## 邊界
- 這個目錄不是任何專案的原始碼，不要在這裡改程式。
- 不要把 token、登入秘密或完整環境變數寫進任何檔案或回覆。
- Remote Control 名稱是 `{REMOTE_NAME}`，由啟動環境建立；不要自己開關。
"#,
        dir = dir.display(),
    )
}

/// The manager's tool entry point, compiled into the daemon binary. Deploying it from a
/// path on the build machine would leave every release install without a CLI.
pub const AGM_CLI: &str = include_str!("../../../scripts/agm.py");

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
fn runtime_json(port: u16, bot_id: &str, data_dir: &str) -> Value {
    json!({
        "daemon_url": format!("http://127.0.0.1:{port}"),
        "manager_bot_id": bot_id,
        // The CLI also accepts `bot_id`, which an earlier draft wrote. Both are emitted so a
        // deployment cannot be broken by whichever half is upgraded first.
        "bot_id": bot_id,
        "data_dir": data_dir,
        "supervisor_id": store::SUPERVISOR_ID,
        "remote_name": REMOTE_NAME,
    })
}

/// Write the directory contents. Idempotent: every file is rewritten from the current
/// daemon, except `handoff.md`, which belongs to the manager once it exists.
///
/// `persona` is the **stored** text, passed in rather than read from the binary: `persona.md`
/// is a readable copy of what the manager is actually running on, and regenerating it from the
/// embedded default would make the copy disagree with the original.
pub fn deploy_files(app: &Arc<App>, bot_id: &str, persona: &str) -> std::io::Result<Deployed> {
    let dir = agm_dir(app);
    std::fs::create_dir_all(dir.join("bin"))?;
    std::fs::write(dir.join("CLAUDE.md"), claude_md(&dir, bot_id, app.port))?;
    std::fs::write(dir.join("persona.md"), persona)?;
    std::fs::write(
        dir.join("runtime.json"),
        serde_json::to_string_pretty(&runtime_json(app.port, bot_id, &app.data_dir.to_string_lossy()))?,
    )?;
    if !dir.join("handoff.md").exists() {
        std::fs::write(dir.join("handoff.md"), "# AGM 管理摘要\n\n（尚未寫入。權威紀錄在 AG Man 資料庫。）\n")?;
    }
    let bin = dir.join("bin").join("agm");
    std::fs::write(&bin, AGM_CLI)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(Deployed { cwd: dir.to_string_lossy().to_string(), agm_cli: "deployed".into() })
}

/// Create (or find) the AGM project and bot in config.toml, then project it into SQLite.
/// Never starts anything — `POST /api/supervisor/start` is a separate, explicit step.
pub async fn ensure_env(app: &Arc<App>) -> Result<(String, String, Deployed), LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let dir = agm_dir(app);
    std::fs::create_dir_all(&dir).map_err(|e| LcError::Bad(format!("{}: {e}", dir.display())))?;
    let path = crate::config::canonical_path(&dir.to_string_lossy()).map_err(|e| LcError::Bad(e.to_string()))?;

    // The identity has to exist already: silently running the manager under whatever account
    // happens to be default is exactly the kind of guess that produces a bot nobody can log in.
    if crate::tools::identity_for_host(app, crate::config::LOCAL_HOST, &sup.identity).await.is_none() {
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
    let (persona, seeded) = effective_persona(app).await?;
    // The closure below moves its copy into the config update; `persona` itself is still needed
    // afterwards to write the readable copy.
    let persona_for_cfg = persona.clone();
    if seeded {
        tracing::info!("seeded the AGM persona from the existing bot or embedded first-install default");
    }
    let effort = sup.effort.clone();
    let identity = sup.identity.clone();
    let p2 = path.clone();

    let (project_id, bot_id) = app
        .cfg
        .update(move |cfg| {
            // Project: persisted id first, then the directory, then a new one.
            let pidx = cfg
                .projects
                .iter()
                .position(|p| known_project.is_some() && p.id == known_project)
                .or_else(|| cfg.projects.iter().position(|p| p.path == p2 && p.host == crate::config::LOCAL_HOST));
            let pidx = match pidx {
                Some(i) => i,
                None => {
                    cfg.projects.push(ProjectCfg {
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
            let bidx = known_bot.as_ref().and_then(|id| proj.bots.iter().position(|b| b.id.as_ref() == Some(id)));
            let bidx = match bidx {
                Some(i) => i,
                None => {
                    if proj.bots.iter().any(|b| b.name == BOT_NAME) {
                        anyhow::bail!("name-taken");
                    }
                    proj.bots.push(BotCfg {
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
            // The Remote Control entry point the phone looks for. Setup only *configures* it;
            // whether a remote session actually came up is decided by observation, never argv.
            bot.args = vec!["--remote-control".into(), REMOTE_NAME.into()];
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

    crate::projection::project_config(&app.cfg, &app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;

    let deployed = deploy_files(app, &bot_id, &persona).map_err(|e| LcError::Upstream(e.to_string()))?;
    store::set_env(&app.db, &bot_id, &project_id, &deployed.cwd)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok((project_id, bot_id, deployed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_persona_body_drops_the_human_preamble() {
        let body = persona_body();
        assert!(body.starts_with("你是 AGM"), "persona body starts at the role line: {:?}", &body[..40.min(body.len())]);
        assert!(!body.contains("以下正文供 AGM bot"), "the note to the human reader is not part of the persona");
        assert!(body.contains("cc0/fable/low"), "the fixed candidate order survives the split");
    }

    #[test]
    fn candidates_stay_aliases_so_a_model_release_cannot_strand_the_supervisor() {
        assert_eq!(model_arg("fable"), "fable");
        assert_eq!(model_arg("opus"), "opus");
        assert_eq!(other_candidate("fable"), "opus");
        assert_eq!(other_candidate("opus"), "fable");
    }

    /// The one file the CLI reads on every call. A token leaking in here would end up in a
    /// directory the manager itself can read and quote back.
    #[test]
    fn the_runtime_file_names_the_manager_and_carries_no_secret() {
        let v = runtime_json(7788, "botULID", "/data");
        assert_eq!(v["daemon_url"], "http://127.0.0.1:7788");
        assert_eq!(v["manager_bot_id"], "botULID");
        assert_eq!(v["bot_id"], "botULID", "the older key stays, so either half can upgrade first");
        let text = v.to_string().to_lowercase();
        assert!(!text.contains("token"), "no token, and no field that could hold one: {text}");
    }

    #[test]
    fn the_cli_is_compiled_in_not_read_off_the_build_machine() {
        assert!(AGM_CLI.contains("add_parser(\"assignments\""), "the deployed CLI is the real agm.py");
    }
}
