//! Starting and restarting a bot: pane acquisition, `agent.start`, and resume.

use super::*;

/// Identity CLI args on `host`. Discovered `ccN` identities carry none: alias flags are the user's shell habit.
async fn identity_args(app: &Arc<App>, bot: &db::Bot, host: &str) -> Vec<String> {
    let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) else { return vec![] };
    crate::tools::identity_for_host(app, host, idn).await.map(|i| i.args).unwrap_or_default()
}


/// `resume_native` continues the bot's last native session (batch update restart).
/// `fork_session`：從這個 native session 分出一個新 session（`POST /bots/:id/fork` 的第一次啟動，SPEC §6.10）。
/// `require_idle`：restart 在**拿到 bot 鎖之後**再確認一次閒置，不閒置回 409 `not_idle`、什麼都不動（一鍵重啟用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartOpts {
    pub resume_native: bool,
    /// 跟 `resume_native` 一起用：接不回原本的對話就**不啟動**，回 409 `resumed:false`＋原因，
    /// 由呼叫端決定要不要改成開新對話（`?resume=native`，herdr 升級 2026-09-17）。沒設的話照舊退回開新對話。
    pub resume_required: bool,
    pub fork_session: Option<String>,
    pub require_idle: bool,
}

/// 這顆 bot 現在為什麼不能被重啟；`None` ＝ 閒置。呼叫端持 bot 鎖。理由的代碼與 `bulk_restart::Skip` 一致。
///
/// 一鍵重啟在排到這顆時 recheck 過一次，但 recheck 到這裡拿到鎖之間仍有空檔：使用者剛好在那幾毫秒送出一則，
/// 那一回合會被 ctrl+c 砍掉（review2 quota #5）。鎖內再看一次才真的關掉。
async fn busy_reason_locked(app: &Arc<App>, bot_id: &str) -> LcResult<Option<&'static str>> {
    let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? else { return Ok(Some("not_running")) };
    Ok(if run.state != "running" {
        Some("not_running")
    } else if run.agent_status == "working" {
        Some("working")
    } else if run.agent_status == "blocked" {
        Some("blocked")
    } else if run.agent_status != "idle" {
        Some("unknown_status")
    } else if db::in_flight_turn(&app.db, &run.id).await.map_err(up)?.is_some() {
        Some("turn_in_flight")
    } else {
        None
    })
}

fn not_idle(bot_id: &str, why: &str) -> LcError {
    LcError::conflict("not_idle", json!({"bot_id": bot_id, "busy": why}))
}

pub async fn start_bot(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    start_bot_with(app, bot_id, StartOpts::default()).await
}

pub async fn start_bot_with(app: &Arc<App>, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    start_bot_locked_with(app, bot_id, opts).await
}

pub async fn start_bot_locked_with(app: &Arc<App>, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    // A child only exists as the pane its parent opened; starting here would open an unrelated pane.
    if bot.managed_by == "child" {
        return Err(LcError::conflict(
            "a spawned child is started by its parent agent, not from here",
            json!({"parent_bot_id": bot.parent_bot_id}),
        ));
    }
    refuse_default_session(&bot)?;
    if let Some(existing) = db::active_run(&app.db, bot_id).await.map_err(up)? {
        return Err(LcError::conflict("active run already exists", json!({"run_id": existing.id})));
    }
    // 要求接回原對話：在任何副作用（run 列、pane、shim）之前就判斷，接不回就整個不啟動。
    if opts.resume_native && opts.resume_required {
        let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
        if let Err(why) = native_resume_plan(app, &bot, &host, false).await? {
            return Err(cannot_resume(bot_id, why));
        }
    }
    let project = db::project(&app.db, &bot.project_id)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let session = app
        .session_for_bot(&bot, &project.host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{}` is not configured", project.host)))?;
    // An unknown identity would silently run as the host's default login (m4p: `cc1` bot answered
    // as cc0). Refuse; a confirmed-logged-out explicit identity fails closed the same way instead of
    // falling back to the host's default account (GH #83: it used to just warn and start anyway,
    // which quietly billed usage to the wrong account).
    if let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if crate::tools::identity_for_host(app, &project.host, idn).await.is_none() {
            return Err(LcError::conflict(
                "identity is not known on this host",
                json!({"identity": idn, "host": project.host,
                       "hint": format!("主機 {} 沒有 `{idn}` 這個身份（config.toml 的 [[identities]] 或該機 zshrc 的 ccN alias）；先在那台建好，或到主機設定按「重新偵測」", project.host)}),
            ));
        }
        let mut not_logged_in = app
            .tools
            .lock()
            .await
            .get(&project.host)
            .and_then(|t| t.identities.get(idn))
            .map(|i| i.logged_in == Some(false))
            .unwrap_or(false);
        // The cache can be ~30 min stale after a login; ask the CLI before saying not logged in.
        if not_logged_in {
            if let Some(fresh) = crate::tools::recheck_identity_login(app, &project.host, idn).await {
                not_logged_in = !fresh;
            }
        }
        if not_logged_in {
            tracing::warn!(bot = %bot.name, identity = idn, host = %project.host, "explicit identity not logged in on host; refusing to start (fail closed)");
            if let Ok(conv) = db::conversation_id(&app.db, bot_id).await {
                let _ = insert_message(
                    app,
                    &conv,
                    None,
                    "system",
                    &format!(
                        "身份 `{idn}` 在 {} 沒有登入，不啟動（不會退回這台機器的預設帳號）。請到 {} 用 `{idn}` 登入後再啟動這顆 bot。",
                        project.host, project.host
                    ),
                    "system",
                    false,
                    None,
                )
                .await;
            }
            return Err(LcError::conflict(
                "identity_not_logged_in",
                json!({
                    "identity": idn, "host": project.host, "login_required": true,
                    "hint": format!("身份 `{idn}` 在 {} 沒有登入；先在該主機用 `{idn}` 登入（例如 claude 的 `/login`），再重試啟動。", project.host),
                }),
            ));
        }
        // Logged in headlessly but never onboarded: the TUI would open on the login menu.
        if bot.kind == "claude" && project.host == crate::config::LOCAL_HOST {
            if let Some(i) = crate::tools::identity_for_host(app, &project.host, idn).await {
                let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                let dir = i
                    .env
                    .get("CLAUDE_CONFIG_DIR")
                    .map(|d| crate::config::expand_home(d, &home))
                    .unwrap_or_else(|| format!("{home}/.claude"));
                if crate::tools::ensure_claude_onboarded(std::path::Path::new(&dir)) {
                    tracing::info!(bot = %bot.name, identity = idn, dir, "marked claude onboarding complete so the TUI skips the login menu");
                }
            }
        }
    }

    // 1. INSERT Run before touching herdr (SPEC §6.2.1).
    let run_id = db::ulid();
    let ins = sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at) VALUES (?,?,'starting','unknown',?,?)")
        .bind(&run_id)
        .bind(bot_id)
        .bind(&session)
        .bind(db::now())
        .execute(&app.db)
        .await;
    if let Err(e) = ins {
        if let Some(dbe) = e.as_database_error() {
            if dbe.is_unique_violation() {
                let existing = db::active_run(&app.db, bot_id).await.map_err(up)?.map(|r| r.id);
                return Err(LcError::conflict("active run already exists", json!({ "run_id": existing })));
            }
        }
        return Err(up(e));
    }
    app.emit_bot_status(bot_id).await;

    match start_inner(app, &bot, &project, &run_id, opts.clone()).await {
        Ok(()) => Ok(run_id),
        Err(e) => {
            let _ = sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE id=?")
                .bind(db::now())
                .bind(&run_id)
                .execute(&app.db)
                .await;
            app.emit_bot_status(bot_id).await;
            Err(e)
        }
    }
}

/// Native-session continuation args. codex `resume` is a subcommand (must come first).
/// grok 1.0.34 起 `--resume <id>` 接得回（2026-09-17 herdr 0.9.0 隔離實測：server 重啟後記得先前的暗號）。
fn resume_args_by_kind(kind: &str, session_id: &str) -> Result<Vec<String>, &'static str> {
    if session_id.trim().is_empty() {
        return Err("no_session_id");
    }
    match kind {
        "claude" | "grok" => Ok(vec!["--resume".into(), session_id.into()]),
        "codex" => Ok(vec!["resume".into(), session_id.into()]),
        _ => Err("unsupported_kind"),
    }
}

/// 這顆 bot 接得回哪一段原生對話：`Ok((session id, argv))`，接不回就是原因代碼
/// （`no_session_id`／`transcript_missing`／`unsupported_kind`）。`include_active`：重啟前查——
/// 那時現在這一個 run 還沒結束，它的 session 才是要接的那段。
///
/// 換身分（`PATCH /bots/:id` 改 `identity`）後 `bot.identity` 已經是新的，但上一段對話的 jsonl
/// 還躺在**舊**身分的 `CLAUDE_CONFIG_DIR/projects/…` 下；兩個目錄不是同一份（symlink 除外）時
/// `--resume` 在新身分下找不到檔案。先把檔案複製過去再放行，複製不了就回退開新對話，不硬失敗
/// （2026-09-17 AGM 手動搶救的兩顆 bot：`resume_session_id` 一直是空的）。
pub(crate) async fn native_resume_plan(
    app: &Arc<App>,
    bot: &db::Bot,
    host: &str,
    include_active: bool,
) -> LcResult<Result<(String, Vec<String>), &'static str>> {
    let last = if include_active {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT native_session_id, transcript_path FROM runs
              WHERE bot_id = ? AND native_session_id IS NOT NULL ORDER BY started_at DESC LIMIT 1",
        )
        .bind(&bot.id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
    } else {
        db::last_native_session(&app.db, &bot.id).await.map_err(up)?
    };
    let Some((session_id, transcript)) = last else { return Ok(Err("no_session_id")) };
    if session_id.trim().is_empty() {
        return Ok(Err("no_session_id"));
    }
    // No transcript = cannot resume: `--resume` prints "No conversation found" and exits
    // right after a "successful" restart (2026-09-11 restart-idle repro). Also stages the file
    // into the current identity's config dir when it moved.
    if let Some(transcript) = transcript.filter(|t| !t.trim().is_empty()) {
        if host == LOCAL_HOST {
            if let Err(why) = stage_cross_identity_transcript(app, bot, host, &transcript).await {
                return Ok(Err(why));
            }
        }
    }
    Ok(resume_args_by_kind(&bot.kind, &session_id).map(|a| (session_id, a)))
}

/// 這個身分實際用的 `CLAUDE_CONFIG_DIR`（沒設、或身分未知都算預設帳號 `~/.claude`）。
async fn identity_config_dir(app: &Arc<App>, host: &str, identity: Option<&str>) -> String {
    let home = crate::tools::host_home(app, host).await;
    let dir = match identity.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => crate::tools::identity_for_host(app, host, name)
            .await
            .and_then(|i| i.env.get("CLAUDE_CONFIG_DIR").map(|v| crate::config::expand_home(v, &home))),
        None => None,
    };
    dir.unwrap_or_else(|| format!("{home}/.claude"))
}

/// Local only：把 `transcript` 複製到目前身分的 `projects/<同一個 cwd 目錄名>/` 下，讓 `--resume` 在
/// 那個身分底下找得到。兩邊 `projects/`（canonicalize 後）本來就是同一份（例如 symlink）時什麼都不做。
/// 來源檔不在了，或建目錄／複製失敗，都回傳 `transcript_missing` 讓呼叫端退回開新對話，不 panic、不硬擋重啟。
async fn stage_cross_identity_transcript(app: &Arc<App>, bot: &db::Bot, host: &str, transcript: &str) -> Result<(), &'static str> {
    let src = std::path::Path::new(transcript);
    if !src.exists() {
        return Err("transcript_missing");
    }
    let (Some(cwd_dir), Some(fname)) = (src.parent(), src.file_name()) else { return Err("transcript_missing") };
    let Some(old_projects) = cwd_dir.parent() else { return Err("transcript_missing") };
    let new_dir = identity_config_dir(app, host, bot.identity.as_deref()).await;
    let new_projects = std::path::Path::new(&new_dir).join("projects");
    let same = match (std::fs::canonicalize(old_projects), std::fs::canonicalize(&new_projects)) {
        (Ok(a), Ok(b)) => a == b,
        _ => old_projects == new_projects.as_path(),
    };
    if same {
        return Ok(());
    }
    let Some(cwd_key) = cwd_dir.file_name() else { return Err("transcript_missing") };
    let dest_dir = new_projects.join(cwd_key);
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        tracing::warn!(bot = %bot.name, dest = %dest_dir.display(), error = %e, "identity switch：建不出新身分的 projects 目錄，改開新對話");
        return Err("transcript_missing");
    }
    let dest_file = dest_dir.join(fname);
    if let Err(e) = std::fs::copy(src, &dest_file) {
        tracing::warn!(bot = %bot.name, from = %src.display(), to = %dest_file.display(), error = %e, "identity switch：複製 session 檔到新身分失敗，改開新對話");
        return Err("transcript_missing");
    }
    // 檔名同名的附屬目錄（有些 CLI 版本會在 jsonl 旁邊放一份）一起搬，搬不動不影響主對話。
    if let Some(stem) = src.file_stem() {
        let companion_src = cwd_dir.join(stem);
        if companion_src.is_dir() {
            if let Err(e) = copy_dir_recursive(&companion_src, &dest_dir.join(stem)) {
                tracing::warn!(bot = %bot.name, error = %e, "identity switch：session 附屬目錄複製失敗（不影響主對話檔）");
            }
        }
    }
    tracing::info!(bot = %bot.name, from = %src.display(), to = %dest_file.display(), "identity switch：session 檔已搬到新身分的 projects 目錄，可以接回對話");
    Ok(())
}

fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

fn cannot_resume(bot_id: &str, why: &str) -> LcError {
    LcError::conflict("cannot_resume", json!({"bot_id": bot_id, "resumed": false, "resume_reason": why}))
}

/// 分出新 session 的參數。claude、grok 是 `--resume <id> --fork-session`；codex 的 `fork` 跟 `resume`
/// 一樣是子命令，要排最前面（三家 CLI 的 help 2026-09-14 實測）。
pub(crate) fn fork_args_by_kind(kind: &str, session_id: &str) -> Result<Vec<String>, &'static str> {
    if session_id.trim().is_empty() {
        return Err("no_session_id");
    }
    match kind {
        "claude" | "grok" => Ok(vec!["--resume".into(), session_id.into(), "--fork-session".into()]),
        "codex" => Ok(vec!["fork".into(), session_id.into()]),
        _ => Err("unsupported_kind"),
    }
}

/// The last native session could not be continued, so this start opens a new conversation.
pub(crate) async fn context_lost(_app: &Arc<App>, bot: &db::Bot, why: &str) -> LcResult<()> {
    tracing::info!(bot = %bot.name, why, "native session continuation unavailable; starting a new conversation");
    Ok(())
}

/// A bot pane's start dir: `bots.cwd` (adopted child) or the project path.
pub fn bot_cwd<'a>(bot: &'a db::Bot, project: &'a db::Project) -> &'a str {
    match bot.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => c,
        None => project.path.as_str(),
    }
}

/// The bot's herdr tab label: its nickname, instead of herdr's `1`, `2`, `3`.
pub fn tab_label(bot: &db::Bot) -> String {
    let n = bot.name.trim();
    if n.is_empty() {
        "bot".to_string()
    } else {
        n.to_string()
    }
}

/// Close `tab_id` when empty — the single decider for every caller removing a pane from a tab.
/// Quiet and idempotent: herdr often reaps the tab itself. A tab still holding panes (pre
/// one-bot-one-tab runs, user layouts) is left alone.
pub(crate) async fn close_tab_if_empty(client: &crate::herdr::HerdrClient, workspace_id: &str, tab_id: &str) {
    let tabs = match client.tab_list(workspace_id).await {
        Ok(t) => t,
        // Never guess: closing a tab we cannot see could take the user's pane with it.
        Err(e) => {
            tracing::debug!(workspace_id, tab_id, error = %e, "tab.list failed; leaving the tab alone");
            return;
        }
    };
    match tabs.iter().find(|t| t.tab_id == tab_id) {
        None => {} // herdr already reaped it
        Some(t) if t.pane_count == 0 => {
            if let Err(e) = client.tab_close(tab_id).await {
                tracing::debug!(tab_id, error = %e, "tab.close failed");
            }
        }
        Some(_) => {}
    }
}

/// Close a run's pane and its tab if now empty; `tab_id` is `None` for pre one-bot-one-tab runs.
pub(crate) async fn close_pane_and_tab(
    client: &crate::herdr::HerdrClient,
    workspace_id: Option<&str>,
    tab_id: Option<&str>,
    pane_id: &str,
) {
    let _ = client.pane_close(pane_id).await;
    let ws = workspace_id.filter(|w| !w.trim().is_empty());
    if let (Some(ws), Some(tab)) = (ws, tab_id.filter(|t| !t.trim().is_empty())) {
        close_tab_if_empty(client, ws, tab).await;
    }
}

/// One bot, one tab. Splitting one tab shrank panes below ~31 columns, where TUIs reflow without
/// spaces (`is_shredded`); tabs don't share width. `focus` is false so starting a bot doesn't
/// yank the user's tab; `fresh_root` (a just-created workspace's pane, opened in `root_cwd`) is used
/// as-is only when the bot's own `cwd` is that same directory.
///
/// A bot with a directory of its own (`bots.cwd`, e.g. the AGM responder living in the patrol's
/// project) must not inherit a root pane opened in the project path: the CLI would start in the other
/// role's directory, read its CLAUDE.md/persona and miss its own `--resume` session (review 2026-09-16
/// c5 M1). It gets its own tab in its own directory, and the root pane is closed *after* that tab
/// exists, so the workspace is never left empty.
async fn acquire_run_pane(
    client: &crate::herdr::HerdrClient,
    workspace_id: &str,
    cwd: &str,
    label: &str,
    env: &Value,
    fresh_root: Option<(crate::herdr::PaneInfo, &str)>,
) -> anyhow::Result<crate::herdr::PaneInfo> {
    match fresh_root {
        Some((p, root_cwd)) if root_cwd == cwd => Ok(p),
        Some((p, _)) => {
            let pane = client.tab_create(workspace_id, cwd, label, env.clone()).await?;
            close_pane_and_tab(client, Some(workspace_id), Some(&p.tab_id), &p.pane_id).await;
            Ok(pane)
        }
        None => client.tab_create(workspace_id, cwd, label, env.clone()).await,
    }
}

/// Pane cleanup for startup after a pane exists. Async, not `Drop`: `pane.close` and the tab
/// tidy-up must finish before the start error returns.
struct StartPaneGuard<'a> {
    client: &'a crate::herdr::HerdrClient,
    workspace_id: &'a str,
    tab_id: &'a str,
    pane_id: &'a str,
    armed: bool,
}

impl<'a> StartPaneGuard<'a> {
    fn new(client: &'a crate::herdr::HerdrClient, workspace_id: &'a str, tab_id: &'a str, pane_id: &'a str) -> Self {
        Self { client, workspace_id, tab_id, pane_id, armed: true }
    }

    async fn protect<T>(&mut self, result: LcResult<T>) -> LcResult<T> {
        if result.is_err() {
            self.cleanup().await;
        }
        result
    }

    async fn cleanup(&mut self) {
        if self.armed {
            close_pane_and_tab(self.client, Some(self.workspace_id), Some(self.tab_id), self.pane_id).await;
            self.armed = false;
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

async fn start_inner(
    app: &Arc<App>,
    bot: &db::Bot,
    project: &db::Project,
    run_id: &str,
    opts: StartOpts,
) -> LcResult<()> {
    let host = project.host.clone();
    let session = app
        .session_for_bot(bot, &host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{host}` is not configured")))?;
    let client = app
        .herdr_for_session(&host, &session)
        .await
        .ok_or_else(|| LcError::Upstream(format!("Herdr session `{session}` for host `{host}` is not configured")))?;
    if !app.session_connected(&host, &session).await {
        return Err(LcError::Upstream(format!("host `{host}` is not connected")));
    }
    // 1b. preflight: a missing CLI would sit in `launch_pending` for the full 60 s silently.
    if let Err(reason) = ensure_kind_installed(app, &host, &bot.kind).await {
        let conv = db::conversation_id(&app.db, &bot.id).await.map_err(up)?;
        let _ = insert_message(app, &conv, None, "system", &reason, "system", false, None).await;
        return Err(LcError::Bad(reason));
    }
    let agent = crate::config::agent_name(&project.label, &bot.id);
    let shim_dir = install_shim(app, bot, project).await;
    let env = pane_env(app, bot, &host, run_id, &agent, shim_dir.as_deref()).await;
    // SPEC §6.5c: claude learns herdr from a skill (the CLI's own doc), not the persona.
    install_herdr_skill(app, bot, project, &env, &agent).await;

    // Remote hook injection may ssh-upload, so it must happen before workspace/tab creation.
    let injected = injected_args(app, bot, project, &env).await.map_err(up)?;
    let mut args = injected;
    args.extend(persona_args(bot, &agent));
    args.extend(model_args(&effort_checked(app, bot, &project.host).await));
    args.extend(identity_args(app, bot, &project.host).await);
    args.extend(bot.args());

    // Reopen only: resolve the previous native session after preflight. The requested id is
    // persisted before `agent.start`; hookrecv uses it to detect a provider that ignored resume.
    let resume = if opts.resume_native {
        match native_resume_plan(app, bot, &host, false).await? {
            Ok(plan) => Some(plan),
            // 預設退回開新對話；`resume_required` 的呼叫在前面就擋掉了。
            Err(why) => {
                context_lost(app, bot, why).await?;
                None
            }
        }
    } else {
        None
    };
    // fork：換一個新 session 接著同一段脈絡。不寫 `resume_session_id`——那是「應該回到同一個 id」的檢查，
    // fork 本來就會拿到新 id。
    if let Some(from) = opts.fork_session.as_deref() {
        let fork = fork_args_by_kind(&bot.kind, from).map_err(|why| LcError::Bad(format!("cannot fork: {why}")))?;
        if bot.kind == "codex" {
            let mut forked = fork;
            forked.extend(args);
            args = forked;
        } else {
            args.extend(fork);
        }
    }
    if let Some((session_id, resume_args)) = resume {
        if bot.kind == "codex" {
            let mut resumed = resume_args;
            resumed.extend(args);
            args = resumed;
        } else {
            args.extend(resume_args);
        }
        sqlx::query("UPDATE runs SET resume_session_id = ? WHERE id = ?")
            .bind(&session_id)
            .bind(run_id)
            .execute(&app.db)
            .await
            .map_err(up)?;
    }

    let cwd = bot_cwd(bot, project);
    // A new dir opens on "trust this project?" with the cursor on *No*: claude quits, codex eats
    // the first message. Record trust first (local, only when not yet trusted).
    if project.host == LOCAL_HOST {
        let mut b = bot.clone();
        b.cwd = Some(cwd.to_string());
        for w in crate::trust::pretrust_bots(app, std::slice::from_ref(&b)).await {
            tracing::warn!(bot = %bot.name, cwd, warning = %w, "could not pre-trust the working directory");
        }
    }

    // 2. workspace
    // `(root pane, 它開在哪個目錄)`：bot 有自己的 cwd 時不能沿用開在專案目錄的 root（見 `acquire_run_pane`）。
    let mut fresh_root: Option<(crate::herdr::PaneInfo, &str)> = None;
    // `projects.workspace_id` is the configured session's; an imported bot in `default` must not
    // overwrite it (the next reconcile would clear it).
    let workspace_id = match (session.as_str() != "default", project.workspace_id.as_deref()) {
        (true, Some(ws)) if client.workspace_get(ws).await.map_err(up)?.is_some() => ws.to_string(),
        _ => {
            let (ws, root) = client.workspace_create(&project.path, &project.label, env.clone()).await.map_err(up)?;
            if session != "default" {
                sqlx::query("UPDATE projects SET workspace_id = ? WHERE id = ?")
                    .bind(&ws.workspace_id)
                    .bind(&project.id)
                    .execute(&app.db)
                    .await
                    .map_err(up)?;
            }
            fresh_root = Some((root, project.path.as_str()));
            ws.workspace_id
        }
    };

    // 3. pane
    let root = acquire_run_pane(&client, &workspace_id, cwd, &tab_label(bot), &env, fresh_root).await.map_err(up)?;
    let pane_id = root.pane_id;
    let tab_id = root.tab_id;
    let mut pane_guard = StartPaneGuard::new(&client, &workspace_id, &tab_id, &pane_id);

    // 4. persist mapping. Until `agent.start` succeeds, every error must close the pane and tab.
    pane_guard
        .protect(
            sqlx::query("UPDATE runs SET workspace_id = ?, pane_id = ?, tab_id = ? WHERE id = ?")
                .bind(&workspace_id)
                .bind(&pane_id)
                .bind(&tab_id)
                .bind(run_id)
                .execute(&app.db)
                .await
                .map_err(up),
        )
        .await?;

    // 5. agent.start under `<project>-<bot>`
    pane_guard
        .protect(
            sqlx::query("UPDATE runs SET agent_name = ? WHERE id = ?")
                .bind(&agent)
                .bind(run_id)
                .execute(&app.db)
                .await
                .map_err(up),
        )
        .await?;
    // SPEC §4.4a: stamp model/effort read back off the argv, not `bots` — `effort_checked` and
    // `bot.args` can differ, and `bots` changes under a live run.
    let (rt_model, rt_effort) = crate::models::model_effort_from_argv(&bot.kind, &args);
    let rt_fast = i64::from(args.iter().any(|a| a.contains("service_tier=\"priority\"")));
    pane_guard
        .protect(
            sqlx::query("UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ? WHERE id = ?")
                .bind(&rt_model)
                .bind(&rt_effort)
                .bind(rt_fast)
                .bind(run_id)
                .execute(&app.db)
                .await
                .map_err(up),
        )
        .await?;
    // A fresh pane answers `agent_pane_busy: … is not an available shell` until its shell settles
    // (2026-09-06: one of six back-to-back starts lost, 300 ms in); hence the retry.
    // SPEC §6.5b: the login shell's profile rebuilds PATH after the pane env (macOS 2026-09-07:
    // `path_helper` + `brew shellenv`), so the shim prepend is typed into the shell before
    // `agent.start`; the pty buffers it if the shell isn't up yet.
    if let Some(dir) = shim_dir.as_deref() {
        let line = format!(" export PATH={}:\"$PATH\"\n", sh_quote(dir));
        if let Err(e) = client.pane_send_text(&pane_id, &line).await {
            tracing::warn!(bot = %bot.name, pane = %pane_id, error = %e, "could not prepend the herdr shim to the pane PATH");
        }
    }
    let mut started = false;
    for attempt in 0..10u32 {
        match client.agent_start(&agent, &bot.kind, &pane_id, &args, 60_000).await {
            Ok(_) => {
                started = true;
                break;
            }
            Err(e) if pane_not_ready(&e) => {
                tracing::debug!(bot = %bot.name, attempt, error = %e, "pane is not an available shell yet");
                tokio::time::sleep(Duration::from_millis(300 + 200 * u64::from(attempt))).await;
            }
            Err(e) => {
                pane_guard.cleanup().await;
                return Err(up(e));
            }
        }
    }
    if !started {
        pane_guard.cleanup().await;
        return Err(up(format!("pane {pane_id} never became an available shell")));
    }
    pane_guard.disarm();

    // 6. per-run status subscription
    crate::events::watch_pane_on_session(app, &host, &session, &pane_id).await;

    // 7. wait for readiness
    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    match client.agent_wait(&agent, &until, 60_000).await {
        Ok(info) => {
            let st = info.agent_status.normalized();
            set_run(app, run_id, "running", st.as_str()).await;
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "agent.wait did not settle");
            // Do NOT close the pane on timeout (SPEC §6.2.7).
            match client.agent_get(&agent).await {
                Ok(Some(info)) => set_run(app, run_id, "running", info.agent_status.normalized().as_str()).await,
                _ => {
                    close_pane_and_tab(&client, Some(&workspace_id), Some(&tab_id), &pane_id).await;
                    return Err(up(e));
                }
            }
        }
    }
    // Codex account notices are TUI history rows, not in `notify`'s last message: snapshot the pane later.
    if bot.kind == "codex" {
        schedule_codex_notice_capture(app, &bot.id, run_id);
    }
    app.emit_bot_status(&bot.id).await;
    Ok(())
}

/// Find the kind's executable via the user's login shell (`$SHELL -lic`), else PATH. Returns a
/// user-facing reason when missing; a lookup that itself fails passes.
async fn ensure_kind_installed(app: &Arc<App>, host: &str, kind: &str) -> Result<(), String> {
    if !crate::config::valid_kind(kind) {
        return Err(format!("未知的 bot kind `{kind}`"));
    }
    let probe = format!(
        "( \"${{SHELL:-/bin/sh}}\" -lic 'command -v {kind}' 2>/dev/null || command -v {kind} 2>/dev/null ) | tail -1"
    );
    let found: Option<String> = if host == LOCAL_HOST {
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("/bin/sh").arg("-c").arg(&probe).output(),
        )
        .await;
        match out {
            Ok(Ok(o)) => Some(String::from_utf8_lossy(&o.stdout).trim().to_string()),
            _ => None, // could not probe → do not block the start
        }
    } else {
        match app.hosts.get(host).await {
            Some(conn) => match conn.ssh_exec_path(&probe).await {
                Ok(o) => Some(o.trim().to_string()),
                Err(e) => {
                    tracing::warn!(host, kind, error = %e, "kind preflight could not run; continuing");
                    None
                }
            },
            None => None,
        }
    };
    match found {
        Some(path) if path.is_empty() => {
            let where_ = if host == LOCAL_HOST { "本機".to_string() } else { format!("主機 {host}") };
            Err(format!(
                "{where_}上找不到 `{kind}` 執行檔（用登入 shell 檢查 `command -v {kind}` 沒有結果）。請先在該主機安裝 {kind}，或確認它在登入 shell 的 PATH 中；遠端主機也可在主機設定的 remote_path 補上路徑。"
            ))
        }
        Some(path) => {
            tracing::debug!(host, kind, %path, "kind preflight ok");
            Ok(())
        }
        None => Ok(()),
    }
}

async fn set_run(app: &Arc<App>, run_id: &str, state: &str, agent_status: &str) {
    let _ = sqlx::query("UPDATE runs SET state = ?, agent_status = ? WHERE id = ?")
        .bind(state)
        .bind(agent_status)
        .bind(run_id)
        .execute(&app.db)
        .await;
}


pub async fn restart_bot(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    restart_bot_with(app, bot_id, StartOpts::default()).await
}

/// Stop + start under one hold of the bot's lock. Two holds let a reconcile adopt the just-stopped
/// agent in between (2026-09-10 23:02, `restart-idle`: AGM + three bots down 5.5 h).
/// If a run whose pane is gone still blocks the start, it is ended and the start retried once.
pub async fn restart_bot_with(app: &Arc<App>, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    // Checked before the stop, or the user's agent gets ctrl+c for nothing.
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    refuse_default_session(&bot)?;
    if opts.require_idle {
        if let Some(why) = busy_reason_locked(app, bot_id).await? {
            return Err(not_idle(bot_id, why));
        }
    }
    // 停之前就確定接得回，免得 ctrl+c 掉之後才發現只能開新對話。
    if opts.resume_native && opts.resume_required {
        let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
        if let Err(why) = native_resume_plan(app, &bot, &host, true).await? {
            return Err(cannot_resume(bot_id, why));
        }
    }
    let stopping = db::active_run(&app.db, bot_id).await.map_err(up)?.map(|r| r.id);
    stop_bot_locked(app, bot_id).await?;
    let started = restart_start(app, bot_id, opts).await;
    if started.is_err() {
        if let Some(run_id) = stopping.as_deref() {
            left_down_by_restart(app, bot_id, run_id).await;
        }
    }
    started
}

/// 重啟停掉了 bot、卻沒能把它開回來（start 在前置檢查就失敗，連新的 run 都沒建）：剛停掉的那個 run 改記
/// `exited`。`stopped` 的意思是「使用者要它停」——incident 探針靠它分辨故意停的 bot，留著的話一顆
/// `autostart=1` 的 bot 從此沒在跑、卻永遠不開 `bot_stopped`，health 一直是綠的（review 2026-09-16 c1 L3）。
async fn left_down_by_restart(app: &Arc<App>, bot_id: &str, run_id: &str) {
    let res = sqlx::query(
        "UPDATE runs SET state='exited' WHERE id=? AND state='stopped'
           AND NOT EXISTS (SELECT 1 FROM runs WHERE bot_id=? AND state IN ('starting','running','stopping'))",
    )
    .bind(run_id)
    .bind(bot_id)
    .execute(&app.db)
    .await;
    if matches!(res, Ok(r) if r.rows_affected() > 0) {
        tracing::warn!(bot = bot_id, run = run_id, "restart stopped the bot but could not start it again; recorded as exited, not as a user stop");
        app.emit_bot_status(bot_id).await;
    }
}

async fn restart_start(app: &Arc<App>, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    match start_bot_locked_with(app, bot_id, opts.clone()).await {
        Err(LcError::Conflict(v)) if v.get("reason").and_then(|r| r.as_str()) == Some("active run already exists") => {
            let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? else {
                return start_bot_locked_with(app, bot_id, opts.clone()).await;
            };
            let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
            if run_alive(app, &run, &bot).await {
                return Err(LcError::Conflict(v));
            }
            tracing::warn!(bot = %bot.name, run = %run.id, pane = ?run.pane_id, "restart found a run with no live pane in its way; ending it and starting again");
            mark_run_exited(app, &run.id, "its pane was gone when the bot restarted").await;
            start_bot_locked_with(app, bot_id, opts).await
        }
        other => other,
    }
}

/// 子 agent 原地重啟（SPEC §6.5a / §6.9）：在它自己的 pane 裡 `ctrl+c` 收掉、**不關 pane**，
/// 同名 `agent.start --resume <上一個 session>`。pane 是父 agent 開的，且 shell 裡的環境
/// （`CLAUDE_CONFIG_DIR`、shim）daemon 重建不了。沒注入 hook，回覆照舊走終端快照。
/// 過程中 pane 不見了就不重開。
pub async fn restart_child_in_pane(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    restart_child_in_pane_with(app, bot_id, false).await
}

/// 同上；`require_idle` 見 [`StartOpts::require_idle`]。
pub async fn restart_child_in_pane_with(app: &Arc<App>, bot_id: &str, require_idle: bool) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    if require_idle {
        if let Some(why) = busy_reason_locked(app, bot_id).await? {
            return Err(not_idle(bot_id, why));
        }
    }
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.managed_by != "child" {
        return Err(LcError::Bad("這不是 agent spawn 出來的子 agent".into()));
    }
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let Some(pane_id) = run.pane_id.clone() else {
        return Err(LcError::Bad("這個子 agent 的 run 沒有記到 pane".into()));
    };
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    let client = client_for_run(app, &run).await?;
    let agent = run.agent_name.clone().unwrap_or_else(|| bot.name.clone());

    let _ = sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run.id).execute(&app.db).await;
    app.emit_bot_status(bot_id).await;
    fail_in_flight(app, &run.id, "restarted to apply the CLI update").await;

    let target = db::run_target(&run, &bot);
    for _ in 0..2 {
        let _ = client.agent_send_keys(&target, &["ctrl+c".to_string()]).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut empty = false;
    for _ in 0..20 {
        match client.pane_get(&pane_id).await {
            // pane 沒了：子 agent 結束，run 收掉，不在別處重開。
            Ok(None) => {
                mark_run_exited(app, &run.id, "子 agent 的 pane 在重啟過程中被關掉").await;
                app.emit_bot_status(bot_id).await;
                return Err(LcError::Bad("這個子 agent 的 pane 已經被關掉了".into()));
            }
            Ok(Some(p)) if p.agent.is_none() => {
                empty = true;
                break;
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !empty {
        // agent 還在：run 轉回 running。留在 `stopping` 會讓 prompt 409、start 拒絕、reconcile
        // 也不救——永遠黃燈（2026-09-12 review #1）。
        let _ = sqlx::query("UPDATE runs SET state='running' WHERE id=? AND state='stopping'")
            .bind(&run.id)
            .execute(&app.db)
            .await;
        app.emit_bot_status(bot_id).await;
        return Err(LcError::Upstream("子 agent 十秒內沒有退出，沒有動它的 pane".into()));
    }
    let _ = sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(db::now())
        .bind(&run.id)
        .execute(&app.db)
        .await;

    let mut args: Vec<String> = Vec::new();
    if bot.auto_approve != 0 {
        match bot.kind.as_str() {
            "claude" => args.push("--dangerously-skip-permissions".into()),
            "codex" => args.push("--yolo".into()),
            "grok" => args.push("--always-approve".into()),
            _ => {}
        }
    }
    // 模型／強度用 `bots` 上 §4.4a 從子 agent argv 讀回的；讀不到就讓 CLI 用預設。
    args.extend(model_args(&effort_checked(app, &bot, &host).await));
    args.extend(bot.args());
    let resume = match db::last_native_session(&app.db, bot_id).await.map_err(up)? {
        Some((sid, _)) => resume_args_by_kind(&bot.kind, &sid).ok().map(|a| (sid, a)),
        None => None,
    };
    if let Some((_, resume_args)) = resume.clone() {
        if bot.kind == "codex" {
            let mut resumed = resume_args;
            resumed.extend(args);
            args = resumed;
        } else {
            args.extend(resume_args);
        }
    } else {
        tracing::info!(bot = %bot.name, "子 agent 沒有可接續的 session，重啟後從新的對話開始");
    }

    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, resume_session_id, started_at)
         VALUES (?,?,'starting','unknown',?,?,?,1,?,?,?,?)",
    )
    .bind(&run_id)
    .bind(bot_id)
    .bind(&run.workspace_id)
    .bind(&pane_id)
    .bind(&run.tab_id)
    .bind(&agent)
    .bind(&run.herdr_session)
    .bind(resume.as_ref().map(|(sid, _)| sid.clone()))
    .bind(db::now())
    .execute(&app.db)
    .await
    .map_err(up)?;

    let mut started = false;
    for attempt in 0..10u32 {
        match client.agent_start(&agent, &bot.kind, &pane_id, &args, 60_000).await {
            Ok(_) => {
                started = true;
                break;
            }
            Err(e) if pane_not_ready(&e) => {
                tracing::debug!(bot = %bot.name, attempt, error = %e, "子 agent 的 pane 還沒回到可用的 shell");
                tokio::time::sleep(Duration::from_millis(300 + 200 * u64::from(attempt))).await;
            }
            Err(e) => {
                mark_run_exited(app, &run_id, "子 agent 重啟時 agent.start 失敗").await;
                app.emit_bot_status(bot_id).await;
                return Err(up(e));
            }
        }
    }
    if !started {
        mark_run_exited(app, &run_id, "子 agent 的 pane 一直不是可用的 shell").await;
        app.emit_bot_status(bot_id).await;
        return Err(LcError::Upstream(format!("pane {pane_id} never became an available shell")));
    }
    let _ = sqlx::query("UPDATE runs SET state='running' WHERE id=?").bind(&run_id).execute(&app.db).await;
    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    if let Err(e) = client.agent_wait(&agent, &until, 60_000).await {
        tracing::warn!(bot = %bot.name, error = %e, "子 agent 重啟後沒等到 ready，run 留著讓對帳接手");
    }
    // 沒有 hook 的 run，畫面是唯一來源（同收編）。
    spawn_adopted_capture(app, &run_id, bot_id);
    app.emit_bot_status(bot_id).await;
    app.emit("bot_changed", json!({"bot_id": bot_id})).await;
    tracing::info!(bot = %bot.name, pane = %pane_id, run = %run_id, "子 agent 在原本的 pane 裡重啟完成");
    Ok(run_id)
}

/// Is this run's pane open and its agent listed? A failed RPC counts as alive: ending a live bot
/// over a hiccup is the worse mistake.
#[cfg(test)]
mod resume_args_tests {
    use super::{resume_args_by_kind, restart_bot_with, start_bot, start_bot_with, stop_bot, LcError, StartOpts};
    use crate::db;
    use crate::testing::{claude_bot, env, Env};
    use serde_json::Value;

    fn started_args(e: &Env) -> Vec<Vec<String>> {
        e.herdr
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "agent.start")
            .filter_map(|(_, params)| {
                params
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|args| args.iter().filter_map(Value::as_str).map(String::from).collect())
            })
            .collect()
    }

    #[test]
    fn provider_resume_arguments_are_exact() {
        assert_eq!(resume_args_by_kind("claude", "sid-1").unwrap(), vec!["--resume", "sid-1"]);
        assert_eq!(resume_args_by_kind("codex", "sid-1").unwrap(), vec!["resume", "sid-1"]);
        assert_eq!(resume_args_by_kind("grok", "sid-1").unwrap(), vec!["--resume", "sid-1"]);
        assert_eq!(resume_args_by_kind("gemini", "sid-1"), Err("unsupported_kind"));
        assert_eq!(resume_args_by_kind("claude", ""), Err("no_session_id"));
    }

    /// Continuation is opt-in: only `resume_native` gets `--resume <id>`.
    #[tokio::test]
    async fn native_resume_is_opt_in() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
             VALUES (?,?,'stopped','idle',?,?,?)",
        )
        .bind(db::ulid())
        .bind(&pm.id)
        .bind("native-previous")
        .bind("2026-09-07T00:00:00Z")
        .bind("2026-09-07T00:01:00Z")
        .execute(&e.app.db)
        .await
        .unwrap();

        start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        assert!(args.windows(2).any(|w| w == ["--resume", "native-previous"]));
        stop_bot(&e.app, &pm.id).await.unwrap();

        start_bot(&e.app, &pm.id).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        assert!(!args.contains(&"--resume".into()));
        stop_bot(&e.app, &pm.id).await.unwrap();
    }

    /// `?resume=native`（resume_required）：接不回就**整個不啟動**，回 `resumed:false`＋原因，不默默開新對話；
    /// 接得回就帶 `--resume <sid>` 並記下 `resume_session_id`。反向：沒要求的照舊退回開新對話（上面那條）。
    #[tokio::test]
    async fn a_required_resume_refuses_instead_of_starting_fresh() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
        let err = start_bot_with(&e.app, &pm.id, strict.clone()).await.unwrap_err();
        match err {
            LcError::Conflict(v) => {
                assert_eq!((v["reason"].as_str(), v["resumed"].as_bool(), v["resume_reason"].as_str()), (Some("cannot_resume"), Some(false), Some("no_session_id")))
            }
            other => panic!("expected 409, got {other:?}"),
        }
        assert!(started_args(&e).is_empty(), "nothing was started");
        assert!(db::active_run(&e.app.db, &pm.id).await.unwrap().is_none(), "no run row left behind");

        let transcript = e.dir.join("pm.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle','sid-pm',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&pm.id)
        .bind(transcript.to_str().unwrap())
        .execute(&e.app.db)
        .await
        .unwrap();
        let run_id = start_bot_with(&e.app, &pm.id, strict.clone()).await.unwrap();
        assert!(started_args(&e).pop().unwrap().windows(2).any(|w| w == ["--resume", "sid-pm"]));
        let sid: Option<String> = sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?").bind(&run_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(sid.as_deref(), Some("sid-pm"));

        // restart：停之前就判斷（看現在這個 run 的 session），接不回就連停都不停。
        sqlx::query("UPDATE runs SET native_session_id='sid-live' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();
        restart_bot_with(&e.app, &pm.id, strict.clone()).await.unwrap();
        assert!(started_args(&e).pop().unwrap().windows(2).any(|w| w == ["--resume", "sid-live"]));
        std::fs::remove_file(&transcript).unwrap();
        let before = db::active_run(&e.app.db, &pm.id).await.unwrap().unwrap().id;
        sqlx::query("UPDATE runs SET native_session_id='sid-gone', transcript_path=? WHERE id=?")
            .bind(transcript.to_str().unwrap())
            .bind(&before)
            .execute(&e.app.db)
            .await
            .unwrap();
        let err = restart_bot_with(&e.app, &pm.id, strict).await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(ref v) if v["resume_reason"] == "transcript_missing"), "{err:?}");
        assert_eq!(db::active_run(&e.app.db, &pm.id).await.unwrap().map(|r| r.id), Some(before), "the running agent was not stopped");
        stop_bot(&e.app, &pm.id).await.unwrap();
    }

    /// No transcript → not resumed: `claude --resume` would exit with "No conversation found".
    #[tokio::test]
    async fn a_session_without_a_transcript_on_disk_is_not_resumed() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        let ended = |sid: &str, transcript: &str, at: &str| {
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'stopped','idle',?,?,?,?)",
            )
            .bind(db::ulid())
            .bind(pm.id.clone())
            .bind(sid.to_string())
            .bind(transcript.to_string())
            .bind(at.to_string())
            .bind(at.to_string())
        };
        let missing = e.dir.join("never-written.jsonl");
        ended("sid-unwritten", missing.to_str().unwrap(), "2026-09-11T00:00:00Z").execute(&e.app.db).await.unwrap();
        start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        assert!(!args.contains(&"--resume".into()), "resumed a session that has no transcript: {args:?}");
        stop_bot(&e.app, &pm.id).await.unwrap();

        let written = e.dir.join("written.jsonl");
        std::fs::write(&written, "{}\n").unwrap();
        ended("sid-written", written.to_str().unwrap(), "2026-09-11T00:10:00Z").execute(&e.app.db).await.unwrap();
        start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        assert!(args.windows(2).any(|w| w == ["--resume", "sid-written"]), "{args:?}");
        stop_bot(&e.app, &pm.id).await.unwrap();
    }

    /// 換身分後重啟要接得回原對話：新舊身分的 `CLAUDE_CONFIG_DIR` 是同一份（symlink）就不用搬檔，
    /// 不是同一份就要把 jsonl 複製過去。兩顆都用 `native_resume_plan` 直接驗，不用真的啟動一次 CLI。
    mod cross_identity_transcript_tests {
        use super::*;
        use crate::config::LOCAL_HOST;
        use crate::lifecycle::native_resume_plan;
        use std::os::unix::fs::MetadataExt;

        fn write_jsonl(dir: &std::path::Path, cwd_key: &str, sid: &str) -> std::path::PathBuf {
            let cwd_dir = dir.join("projects").join(cwd_key);
            std::fs::create_dir_all(&cwd_dir).unwrap();
            let f = cwd_dir.join(format!("{sid}.jsonl"));
            std::fs::write(&f, "{}\n").unwrap();
            f
        }

        async fn set_identity_dir(app: &std::sync::Arc<crate::state::App>, name: &str, dir: &std::path::Path) {
            app.cfg
                .update(|c| {
                    c.identities.push(crate::config::IdentityCfg {
                        name: name.into(),
                        kind: "claude".into(),
                        host: None,
                        env: [("CLAUDE_CONFIG_DIR".to_string(), dir.to_str().unwrap().to_string())].into(),
                        args: vec![],
                    });
                    Ok(())
                })
                .await
                .unwrap();
        }

        /// (a) 新身分的 `projects/` 是舊身分那份的 symlink → 不搬檔，直接接得回。
        #[tokio::test]
        async fn symlinked_projects_dir_needs_no_copy() {
            let e = env().await;
            let pm = claude_bot(&e.app, &e.project_id, "pm").await;

            let old_dir = e.dir.join("cc-old");
            let transcript = write_jsonl(&old_dir, "-Users-m4p-project-x", "sid-same");

            // 新身分：自己的目錄下 `projects` 是指到舊身分那份 `projects` 的 symlink（同一份東西）。
            let new_dir = e.dir.join("cc-symlinked");
            std::fs::create_dir_all(&new_dir).unwrap();
            std::os::unix::fs::symlink(old_dir.join("projects"), new_dir.join("projects")).unwrap();
            set_identity_dir(&e.app, "cc-sym", &new_dir).await;
            sqlx::query("UPDATE bots SET identity='cc-sym' WHERE id=?").bind(&pm.id).execute(&e.app.db).await.unwrap();

            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'stopped','idle','sid-same',?,'2026-09-17T00:00:00Z','2026-09-17T00:01:00Z')",
            )
            .bind(db::ulid())
            .bind(&pm.id)
            .bind(transcript.to_str().unwrap())
            .execute(&e.app.db)
            .await
            .unwrap();

            let bot = db::bot(&e.app.db, &pm.id).await.unwrap().unwrap();
            let plan = native_resume_plan(&e.app, &bot, LOCAL_HOST, false).await.unwrap();
            let (sid, args) = plan.expect("resumable");
            assert_eq!(sid, "sid-same");
            assert!(args.windows(2).any(|w| w == ["--resume", "sid-same"]));

            // 沒有另外複製出一份：透過 symlink 看到的還是原本那個 inode。
            let via_new = new_dir.join("projects").join("-Users-m4p-project-x").join("sid-same.jsonl");
            assert_eq!(std::fs::metadata(&via_new).unwrap().ino(), std::fs::metadata(&transcript).unwrap().ino());
            assert_eq!(std::fs::read_dir(old_dir.join("projects").join("-Users-m4p-project-x")).unwrap().count(), 1, "沒有多出檔案");
        }

        /// (b) 新舊身分的 `projects/` 是不相干的兩個目錄 → 接回前先把 jsonl 複製過去，複製出的是獨立的檔案。
        #[tokio::test]
        async fn different_config_dirs_copy_the_transcript_before_resuming() {
            let e = env().await;
            let pm = claude_bot(&e.app, &e.project_id, "pm").await;

            let old_dir = e.dir.join("cc-old2");
            let transcript = write_jsonl(&old_dir, "-Users-m4p-project-y", "sid-move");

            let new_dir = e.dir.join("cc2-new");
            set_identity_dir(&e.app, "cc2", &new_dir).await;
            sqlx::query("UPDATE bots SET identity='cc2' WHERE id=?").bind(&pm.id).execute(&e.app.db).await.unwrap();

            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'stopped','idle','sid-move',?,'2026-09-17T00:00:00Z','2026-09-17T00:01:00Z')",
            )
            .bind(db::ulid())
            .bind(&pm.id)
            .bind(transcript.to_str().unwrap())
            .execute(&e.app.db)
            .await
            .unwrap();

            let bot = db::bot(&e.app.db, &pm.id).await.unwrap().unwrap();
            let plan = native_resume_plan(&e.app, &bot, LOCAL_HOST, false).await.unwrap();
            let (sid, args) = plan.expect("resumable after copy");
            assert_eq!(sid, "sid-move");
            assert!(args.windows(2).any(|w| w == ["--resume", "sid-move"]));

            let dest = new_dir.join("projects").join("-Users-m4p-project-y").join("sid-move.jsonl");
            assert!(dest.exists(), "jsonl 沒有被複製到新身分的 projects 目錄");
            assert_eq!(std::fs::read_to_string(&dest).unwrap(), std::fs::read_to_string(&transcript).unwrap());
            assert_ne!(std::fs::metadata(&dest).unwrap().ino(), std::fs::metadata(&transcript).unwrap().ino(), "應該是獨立複製出的檔案，不是同一個 inode");
        }

        /// 來源檔不見了：不硬擋，退回開新對話（跟原本沒換身分時「transcript_missing」一致）。
        #[tokio::test]
        async fn missing_source_falls_back_to_a_new_conversation() {
            let e = env().await;
            let pm = claude_bot(&e.app, &e.project_id, "pm").await;
            let new_dir = e.dir.join("cc3-new");
            set_identity_dir(&e.app, "cc3", &new_dir).await;
            sqlx::query("UPDATE bots SET identity='cc3' WHERE id=?").bind(&pm.id).execute(&e.app.db).await.unwrap();

            let gone = e.dir.join("cc-old3/projects/-Users-m4p-project-z/sid-gone.jsonl");
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'stopped','idle','sid-gone',?,'2026-09-17T00:00:00Z','2026-09-17T00:01:00Z')",
            )
            .bind(db::ulid())
            .bind(&pm.id)
            .bind(gone.to_str().unwrap())
            .execute(&e.app.db)
            .await
            .unwrap();

            let bot = db::bot(&e.app.db, &pm.id).await.unwrap().unwrap();
            let plan = native_resume_plan(&e.app, &bot, LOCAL_HOST, false).await.unwrap();
            assert_eq!(plan, Err("transcript_missing"));
        }
    }

    /// 2026-09-10 23:02: restart repeatedly beside a tight reconcile loop; every restart must
    /// return the run it started, alive (no adoption in a stop/start gap).
    #[tokio::test]
    async fn restarts_racing_a_reconcile_loop_always_come_back() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        start_bot(&e.app, &pm.id).await.unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (app2, stop2) = (e.app.clone(), stop.clone());
        let looper = tokio::spawn(async move {
            let mut rounds = 0u32;
            while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = crate::reconcile::reconcile_host(&app2, crate::config::LOCAL_HOST).await;
                rounds += 1;
                tokio::task::yield_now().await;
            }
            rounds
        });

        for i in 0..15 {
            let started = match crate::lifecycle::restart_bot_with(&e.app, &pm.id, StartOpts::default()).await {
                Ok(id) => id,
                Err(_) => panic!("restart {i} was refused while a reconcile was running"),
            };
            let active = db::active_run(&e.app.db, &pm.id).await.unwrap().expect("a run after the restart");
            assert_eq!(active.id, started, "restart {i}: the active run is the one the restart started, not an adopted leftover");
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let rounds = looper.await.unwrap();
        assert!(rounds > 0, "the reconcile loop actually ran alongside the restarts");

        let run = db::active_run(&e.app.db, &pm.id).await.unwrap().unwrap();
        let bot = db::bot(&e.app.db, &pm.id).await.unwrap().unwrap();
        assert!(crate::lifecycle::run_alive(&e.app, &run, &bot).await, "the surviving run has a live pane and agent");
        let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=? AND state IN ('starting','running','stopping')")
            .bind(&pm.id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(live, 1, "exactly one live run");
    }
}

/// GH #83: an explicit identity confirmed logged out must fail closed, not fall back to the host's
/// default account.
#[cfg(test)]
mod identity_login_gate_tests {
    use super::*;
    use crate::config::IdentityCfg;
    use crate::testing as tt;
    use crate::tools::{HostTools, IdentityInfo, ToolInfo, SOURCE_CONFIG};
    use std::os::unix::fs::PermissionsExt;

    /// Registers `name` as a known local `claude` identity whose cache says logged out, and points
    /// its CLI at a fake script (never the real `claude`) that answers `{"loggedIn": fresh_logged_in}`
    /// when `start_bot` rechecks — so the test never depends on what is actually installed or logged
    /// in on the machine running it.
    async fn identity_with_recheck_answer(app: &Arc<App>, dir: &std::path::Path, name: &str, fresh_logged_in: bool) {
        let config_dir = dir.join(format!("{name}-config"));
        app.cfg
            .update(|c| {
                c.identities.push(IdentityCfg {
                    name: name.into(),
                    kind: "claude".into(),
                    host: None,
                    env: [("CLAUDE_CONFIG_DIR".to_string(), config_dir.to_string_lossy().to_string())].into(),
                    args: vec![],
                });
                Ok(())
            })
            .await
            .unwrap();
        let script = dir.join(format!("fake-cli-{name}.sh"));
        std::fs::write(&script, format!("#!/bin/sh\nprintf '{{\"loggedIn\": {fresh_logged_in}}}'\n")).unwrap();
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            HostTools {
                tools: [("claude".to_string(), ToolInfo { installed: true, path: Some(script.to_string_lossy().to_string()), version: None, logged_in: None })].into(),
                identities: [(
                    name.to_string(),
                    IdentityInfo {
                        name: name.to_string(),
                        kind: "claude".to_string(),
                        logged_in: Some(false),
                        reason: None,
                        account: None,
                        plan: None,
                        source: SOURCE_CONFIG,
                        config_dir: None,
                    },
                )]
                .into(),
                shell_identities: vec![],
                checked_at: db::now(),
            },
        );
    }

    fn conflict(e: LcError) -> Value {
        match e {
            LcError::Conflict(v) => v,
            other => panic!("expected a 409 conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_explicit_logged_out_identity_fails_closed_instead_of_falling_back() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "cc-lock").await;
        sqlx::query("UPDATE bots SET identity='cc-lock' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        identity_with_recheck_answer(&e.app, &e.dir, "cc-lock", false).await;

        let err = start_bot(&e.app, &bot.id).await.expect_err("身分確認未登入：不能啟動");
        let body = conflict(err);
        assert_eq!(body["reason"], "identity_not_logged_in");
        assert_eq!(body["identity"], "cc-lock");
        assert_eq!(body["host"], "local");

        assert!(db::active_run(&e.app.db, &bot.id).await.unwrap().is_none(), "沒有 run 被建起來");
        assert!(e.herdr.methods().is_empty(), "沒有碰 herdr，CLI 沒被啟動：{:?}", e.herdr.methods());

        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        let note: String = sqlx::query_scalar(
            "SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&conv)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert!(note.contains("cc-lock") && note.contains("沒有登入"), "{note}");
    }

    /// 快取覺得沒登入，但 CLI 重驗說有登入（例如剛登入、30 分鐘的快取還沒更新）：照樣啟動，不擋。
    #[tokio::test]
    async fn a_stale_logged_out_cache_does_not_block_a_start_the_cli_confirms() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "cc-fresh").await;
        sqlx::query("UPDATE bots SET identity='cc-fresh' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        identity_with_recheck_answer(&e.app, &e.dir, "cc-fresh", true).await;

        start_bot(&e.app, &bot.id).await.expect("CLI 重驗說有登入：照常啟動");
        assert!(db::active_run(&e.app.db, &bot.id).await.unwrap().is_some());
    }
}

#[cfg(test)]
mod child_restart_tests {
    //! `restart_child_in_pane` when the agent will not leave (review 2026-09-12 #1).
    use super::*;
    use crate::testing as tt;

    /// Agent ignores ctrl+c: after 20 polls the `stopping` run must return to `running` (else 409s
    /// and a permanently yellow bot).
    #[tokio::test]
    async fn a_child_that_will_not_exit_gets_its_run_back() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let parent = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'alfa','claude','[]',0,1,'tok',?)",
        )
        .bind(&parent)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let kid = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
             VALUES (?,?,'ui','claude','[]',0,0,'tok','child',?,?)",
        )
        .bind(&kid)
        .bind(&env.project_id)
        .bind(&parent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,'proj-alfa-ui','test',1,?)",
        )
        .bind(&run_id)
        .bind(&kid)
        .bind(&ws.workspace_id)
        .bind(&kid_pane.tab_id)
        .bind(&kid_pane.pane_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        // The mock drops an agent on ctrl+c only by name; `name: null` (herdr 0.8.2 after a same-named
        // restart) stays put — a stand-in for one ignoring ctrl+c.
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": null, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id,
            "cwd": "/tmp/p"})];

        let err = restart_child_in_pane(&app, &kid).await.expect_err("the agent never left");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");

        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, "running", "the agent is still in its pane, so the run is still live");
        assert!(run.ended_at.is_none());
        assert_eq!(db::active_run(&app.db, &kid).await.unwrap().map(|r| r.id), Some(run_id));
        // Nothing touched the pane.
        assert!(env.herdr.tab(&kid_pane.tab_id).unwrap().panes.contains(&kid_pane.pane_id));
        assert!(!env.herdr.methods().iter().any(|m| m == "pane.close"));
    }
}

#[cfg(test)]
mod tab_tests {
    //! One bot, one tab (and the retrofit). The mock herdr keeps real tab/pane bookkeeping but,
    //! unlike herdr 0.8.2, does not reap empty tabs — so these see whether the daemon tidies up itself.
    use super::*;
    use crate::testing as tt;

    async fn a_bot(env: &tt::Env, name: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
        )
        .bind(&id)
        .bind(&env.project_id)
        .bind(name)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        id
    }

    /// 協調者（`bots.cwd` 是自己的目錄）比巡檢先開 workspace：root pane 開在專案目錄，不能拿來跑它——
    /// 要在自己的目錄開 tab，再把 root 關掉（review 2026-09-16 c5 M1）。沒有自己目錄的 bot 照舊直接用 root。
    #[tokio::test]
    async fn a_bot_with_its_own_directory_does_not_run_in_a_fresh_workspace_root_opened_elsewhere() {
        let env = tt::env().await;
        let own = env.dir.join("AGM-responder");
        std::fs::create_dir_all(&own).unwrap();
        let own = own.to_string_lossy().to_string();
        let resp = a_bot(&env, "responder").await;
        sqlx::query("UPDATE bots SET cwd=? WHERE id=?").bind(&own).bind(&resp).execute(&env.app.db).await.unwrap();

        start_bot(&env.app, &resp).await.unwrap();
        let root = env.herdr.first_call("workspace.create").expect("the workspace did not exist yet");
        assert_ne!(root["cwd"].as_str(), Some(own.as_str()), "workspace 仍以專案目錄建立");
        let run = db::active_run(&env.app.db, &resp).await.unwrap().unwrap();
        let tab_create = env.herdr.first_call("tab.create").expect("a tab in the bot's own directory");
        assert_eq!(tab_create["cwd"].as_str(), Some(own.as_str()));
        let ws = run.workspace_id.clone().unwrap();
        let tabs = env.herdr.tabs_in(&ws);
        assert_eq!(tabs.len(), 1, "root 的 tab 關掉了，只剩協調者自己的：{tabs:?}");
        assert!(tabs[0].panes.contains(run.pane_id.as_ref().unwrap()));
        assert!(env.herdr.methods().contains(&"pane.close".into()));

        // 沒有自己目錄的 bot：新 workspace 的 root 就是它的 pane，不多開 tab。
        let env2 = tt::env().await;
        let plain = a_bot(&env2, "plain").await;
        start_bot(&env2.app, &plain).await.unwrap();
        assert!(env2.herdr.first_call("tab.create").is_none());
        assert!(!env2.herdr.methods().contains(&"pane.close".into()));
    }

    /// A DB failure after `workspace.create` must still remove the root pane and its tab (trigger-injected).
    #[tokio::test]
    async fn a_run_mapping_failure_closes_the_new_pane_and_tab() {
        let env = tt::env().await;
        let bot_id = a_bot(&env, "alfa").await;
        sqlx::query(
            "CREATE TRIGGER fail_run_mapping BEFORE UPDATE OF workspace_id, pane_id, tab_id ON runs
             BEGIN SELECT RAISE(ABORT, 'forced run mapping failure'); END",
        )
        .execute(&env.app.db)
        .await
        .unwrap();

        assert!(start_bot(&env.app, &bot_id).await.is_err());

        let workspace_id = db::project(&env.app.db, &env.project_id)
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .expect("workspace.create ran before the mapping failure");
        assert!(env.herdr.tabs_in(&workspace_id).is_empty(), "the failed start left a tab behind");
        let methods = env.herdr.methods();
        assert!(methods.contains(&"pane.close".into()), "cleanup did not close the pane: {methods:?}");
        assert!(methods.contains(&"tab.close".into()), "cleanup did not close the empty tab: {methods:?}");

        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE bot_id = ? ORDER BY started_at DESC LIMIT 1")
            .bind(&bot_id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(state, "exited");
    }

    async fn run_row(app: &Arc<App>, id: &str) -> db::Run {
        sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id = ?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    /// A run that is `running` on `pane_id`, with whatever `tab_id` the caller says.
    async fn running_on(app: &Arc<App>, bot_id: &str, ws: &str, pane: &str, tab: Option<&str>) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'agent','test',?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(ws)
        .bind(pane)
        .bind(tab)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        id
    }

    /// Every start pre-trusts its cwd, or claude's "trust this project?" prompt (cursor on *No*) fails it.
    #[tokio::test]
    async fn a_fresh_working_directory_is_trusted_before_the_agent_starts() {
        let dir = std::env::temp_dir().join(format!("am-trust-start-{}", crate::db::ulid()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let store = dir.join(".claude.json");
        std::fs::write(&store, "{\"numStartups\":7}").unwrap();

        let cwd = repo.to_string_lossy().to_string();
        let wrote = crate::trust::mark_trusted("claude", &store, &[crate::trust::canonical(&cwd)]).unwrap();
        assert!(wrote, "a directory the CLI has not seen is recorded");

        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 7, "the CLI's own state survives the merge");
        let key = crate::trust::canonical(&cwd);
        assert_eq!(v["projects"][&key]["hasTrustDialogAccepted"], true);
        // Resolved path: macOS `/tmp` is a symlink and the CLI compares its `getcwd()`.
        assert!(!key.starts_with("/tmp/"), "the recorded path is canonical, got {key}");

        assert!(!crate::trust::mark_trusted("claude", &store, &[key]).unwrap(), "already trusted: left alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Starting a bot creates a tab (never `pane.split`): tabs don't divide a fixed width.
    #[tokio::test]
    async fn a_starting_bot_gets_a_tab_of_its_own() {
        let env = tt::env().await;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

        let e = json!({"AM_BOT_ID": "b1", "AM_HOOK_TOKEN": "tok"});
        let first = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &e, None).await.unwrap();
        let second = acquire_run_pane(&client, &ws.workspace_id, "/tmp/worktree", "bravo", &e, None).await.unwrap();

        assert!(!env.herdr.methods().iter().any(|m| m == "pane.split"), "a bot is never split into a shared tab");
        assert_ne!(first.tab_id, second.tab_id, "two bots, two tabs — they do not share a width");
        assert_ne!(first.tab_id, root.tab_id, "and neither lands in the workspace's own tab");
        for t in [&first.tab_id, &second.tab_id] {
            assert_eq!(env.herdr.tab(t).unwrap().panes.len(), 1, "each tab holds exactly the one bot's pane");
        }

        // Same cwd/env contract as `pane.split`, nickname on the tab bar, never steals focus.
        let p = env.herdr.first_call("tab.create").expect("tab.create was called");
        assert_eq!(p["workspace_id"], json!(ws.workspace_id));
        assert_eq!(p["cwd"], json!("/tmp/p"));
        assert_eq!(p["label"], json!("alfa"));
        assert_eq!(p["focus"], json!(false), "starting a bot must not yank the user's focus");
        assert_eq!(p["env"]["AM_BOT_ID"], json!("b1"));
        assert_eq!(p["env"]["AM_HOOK_TOKEN"], json!("tok"));
    }

    /// A fresh workspace is already one tab / one pane: the first bot uses the root pane.
    #[tokio::test]
    async fn the_first_bot_in_a_fresh_workspace_reuses_its_root_pane() {
        let env = tt::env().await;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

        let got =
            acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), Some((root.clone(), "/tmp/p"))).await.unwrap();

        assert_eq!(got.pane_id, root.pane_id);
        assert_eq!(got.tab_id, root.tab_id);
        assert!(env.herdr.first_call("tab.create").is_none(), "no second tab for a workspace that is one already");
        assert_eq!(env.herdr.tabs_in(&ws.workspace_id).len(), 1);
    }

    /// Stopping a bot takes its tab too, so no row of empty tabs builds up.
    #[tokio::test]
    async fn stopping_a_bot_closes_the_tab_it_owned() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), None).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        running_on(&app, &bot, &ws.workspace_id, &pane.pane_id, Some(&pane.tab_id)).await;

        assert!(stop_bot(&app, &bot).await.unwrap());

        assert!(env.herdr.tab(&pane.tab_id).is_none(), "the bot's own tab went with it");
        assert_eq!(env.herdr.tabs_in(&ws.workspace_id).len(), 1, "only the workspace's own tab is left");
    }

    /// A pane sharing its tab only loses the pane; closing the tab would kill a neighbour's agent.
    #[tokio::test]
    async fn stopping_a_bot_in_a_shared_tab_leaves_the_tab_alone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        // The old world: two bots split into the workspace's single tab.
        let neighbour = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let mine = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        assert_eq!(mine.tab_id, neighbour.tab_id);

        let bot = a_bot(&env, "alfa").await;
        // Reconcile fills `tab_id` for old runs too, so "has a tab id" can't decide closing.
        running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, Some(&mine.tab_id)).await;

        assert!(stop_bot(&app, &bot).await.unwrap());

        let tab = env.herdr.tab(&mine.tab_id).expect("the shared tab survives");
        assert!(!tab.panes.contains(&mine.pane_id), "our pane is gone");
        assert!(tab.panes.contains(&neighbour.pane_id), "the neighbour's agent is untouched");
    }

    /// Retrofit: a bot in a shared tab gets its own via a move; `pane_id` must survive.
    #[tokio::test]
    async fn moving_a_running_bot_gives_it_a_tab_without_changing_its_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let neighbour = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let mine = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        // NULL tab_id: a run from before the column existed.
        let run = running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, None).await;

        move_pane_to_own_tab(&app, &bot).await.unwrap();

        let r = run_row(&app, &run).await;
        assert_eq!(r.pane_id.as_deref(), Some(mine.pane_id.as_str()), "a move, not a restart: same pane");
        assert_eq!(r.state, "running");
        let tab = r.tab_id.expect("the new tab was recorded on the run");
        assert_ne!(tab, mine.tab_id);
        assert_eq!(env.herdr.tab(&tab).unwrap().panes, vec![mine.pane_id.clone()], "it has the tab to itself");
        assert_eq!(env.herdr.first_call("pane.move").unwrap()["destination"]["label"], json!("alfa"));
        assert_eq!(env.herdr.first_call("pane.move").unwrap()["focus"], json!(false));
        // The tab it left still holds the neighbour, so it is not closed.
        assert!(env.herdr.tab(&neighbour.tab_id).unwrap().panes.contains(&neighbour.pane_id));
    }

    /// The shared tidy-up decides from herdr's pane count, not our records. "Not found" is normal in
    /// production (herdr 0.8.2 reaps tabs); only the non-reaping mock reaches `tab.close`.
    #[tokio::test]
    async fn the_tidy_up_closes_an_empty_tab_and_only_an_empty_one() {
        let env = tt::env().await;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let alone = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), None).await.unwrap();
        let busy = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "bravo", &json!({}), None).await.unwrap();

        // Occupied: left alone.
        close_tab_if_empty(&client, &ws.workspace_id, &busy.tab_id).await;
        assert!(env.herdr.tab(&busy.tab_id).is_some());

        // Emptied: closed.
        client.pane_close(&alone.pane_id).await.unwrap();
        close_tab_if_empty(&client, &ws.workspace_id, &alone.tab_id).await;
        assert!(env.herdr.tab(&alone.tab_id).is_none());

        // Already gone: not an error, and nothing else is touched.
        close_tab_if_empty(&client, &ws.workspace_id, &alone.tab_id).await;
        assert!(env.herdr.tab(&busy.tab_id).is_some());
    }

    /// Idempotence: on herdr a second move rebuilds the tab and renumbers the tab bar.
    #[tokio::test]
    async fn moving_a_bot_that_already_owns_its_tab_changes_nothing() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let mine = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), None).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        let run = running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, None).await;

        move_pane_to_own_tab(&app, &bot).await.unwrap();

        assert!(env.herdr.first_call("pane.move").is_none(), "nothing to move");
        let r = run_row(&app, &run).await;
        assert_eq!(r.tab_id.as_deref(), Some(mine.tab_id.as_str()), "the tab it already had is recorded");
        assert_eq!(env.herdr.tab(&mine.tab_id).unwrap().panes, vec![mine.pane_id]);
    }

    /// No active run, or a run with no pane behind it, is a 404 — not a 502 and not a panic.
    #[tokio::test]
    async fn moving_a_bot_that_is_not_running_is_a_not_found() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = a_bot(&env, "alfa").await;

        assert!(matches!(move_pane_to_own_tab(&app, &bot).await, Err(LcError::NotFound(_))));

        let id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at)
             VALUES (?,?,'running','idle','test',?)",
        )
        .bind(&id)
        .bind(&bot)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert!(matches!(move_pane_to_own_tab(&app, &bot).await, Err(LcError::NotFound(_))));
    }
}

/// 一鍵重啟的鎖內閒置確認（review2 quota #5 的最後一段空檔）。
#[cfg(test)]
mod idle_restart_tests {
    use super::*;
    use crate::testing as tt;

    async fn bot_with_run(env: &tt::Env, managed_by: &str, agent_status: &str) -> (String, String) {
        let app = &env.app;
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(format!("busy-{bot_id}"))
        .bind(managed_by)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running',?,'pane-busy','busy','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(agent_status)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        (bot_id, run_id)
    }

    fn reason(e: LcError) -> (String, String) {
        match e {
            LcError::Conflict(v) => (v["reason"].as_str().unwrap_or_default().into(), v["busy"].as_str().unwrap_or_default().into()),
            other => panic!("expected 409, got {other:?}"),
        }
    }

    /// 重啟時 stop 成功、start 在前置檢查就失敗（這裡是身分在這台主機不存在）：剛停掉的 run 不能留成
    /// `stopped`——那代表「使用者要它停」，incident 探針就永遠不替這顆 autostart bot 開 `bot_stopped`
    /// （review 2026-09-16 c1 L3）。使用者自己 stop 的照舊是 `stopped`。
    #[tokio::test]
    async fn a_restart_that_cannot_start_again_is_not_recorded_as_a_user_stop() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id) = bot_with_run(&env, "user", "idle").await;
        sqlx::query("UPDATE bots SET identity='nope-not-on-this-host', autostart=1 WHERE id=?").bind(&bot_id).execute(&app.db).await.unwrap();
        let err = restart_bot(&app, &bot_id).await.expect_err("身分不在這台主機：start 一定失敗");
        assert!(matches!(err, LcError::Conflict(_)), "{err:?}");
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "exited", "重啟沒開回來，不是使用者停的");
        assert!(db::active_run(&app.db, &bot_id).await.unwrap().is_none());

        // 對照：使用者自己 stop 的 run 留在 `stopped`。
        let (other, other_run) = bot_with_run(&env, "user", "idle").await;
        stop_bot(&app, &other).await.unwrap();
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&other_run).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "stopped");
    }

    /// 排到它的那一刻還閒著、拿到鎖時已經在跑：不送 ctrl+c、run 原封不動。
    #[tokio::test]
    async fn a_bot_that_got_busy_before_the_lock_is_not_restarted() {
        let env = tt::env().await;
        let app = env.app.clone();
        for (status, want) in [("working", "working"), ("blocked", "blocked")] {
            let (bot_id, run_id) = bot_with_run(&env, "user", status).await;
            let opts = StartOpts { resume_native: true, require_idle: true, ..Default::default() };
            assert_eq!(reason(restart_bot_with(&app, &bot_id, opts).await.unwrap_err()), ("not_idle".into(), want.into()));
            assert_eq!(db::active_run(&app.db, &bot_id).await.unwrap().map(|r| (r.id, r.state)), Some((run_id, "running".into())));
        }
        // 閒著但還有一回合沒收掉，也不動。
        let (bot_id, run_id) = bot_with_run(&env, "user", "idle").await;
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let opts = StartOpts { resume_native: true, require_idle: true, ..Default::default() };
        assert_eq!(reason(restart_bot_with(&app, &bot_id, opts).await.unwrap_err()).1, "turn_in_flight");
        // 子 agent 的原地重啟也一樣。
        let (kid, _) = bot_with_run(&env, "child", "working").await;
        assert_eq!(reason(restart_child_in_pane_with(&app, &kid, true).await.unwrap_err()).1, "working");
        let methods = env.herdr.methods();
        assert!(!methods.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "什麼都沒動：{methods:?}");
    }
}
