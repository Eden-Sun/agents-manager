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
        // agent 已經在跑，只差 `running` 沒記下（#145）：收成 exited 會留下一顆沒有 run 的活 agent，
        // 交給 `start_inner` 排好的重試照證據收斂。
        Err(e @ LcError::Uncommitted(_)) => {
            app.emit_bot_status(bot_id).await;
            Err(e)
        }
        Err(e) => {
            // CAS：`start_inner` 途中不拿鎖的 pane-exit 事件已經收掉它的話，不覆寫。寫不進去時 pane 已經收掉了，
            // DB 卻還是 `starting`——不留一顆永遠擋住下一次 start 的 run，排對帳重試（#145 驗收 3）。
            if let Err(db_err) = super::run_state::transition(&app.db, &run_id, &["starting"], "exited", None).await {
                tracing::warn!(bot = %bot.name, run = %run_id, error = %db_err, "failed start could not be recorded as exited");
                super::run_state::schedule_settle(app, &run_id, super::run_state::Settle::Reconcile { stuck: "starting".into() });
            }
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
    // into the current identity's config dir when it moved — local and remote both (issue #95:
    // remote used to skip this entirely and just hope `--resume` found the file on its own).
    if let Some(transcript) = transcript.filter(|t| !t.trim().is_empty()) {
        if let Err(why) = stage_cross_identity_transcript(app, bot, host, &transcript).await {
            return Ok(Err(why));
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

/// 把 `transcript` 複製到目前身分的 `projects/<同一個 cwd 目錄名>/` 下，讓 `--resume` 在那個身分
/// 底下找得到。本機與遠端分開實作：遠端主機上舊/新 `projects/` 都在**那台機器**上，是同機複製，
/// 透過 ssh 執行一段 shell script，不是本機↔遠端搬檔（issue #95：以前只做本機這半，遠端完全跳過，
/// 換身分後 `--resume` 在遠端主機上一樣找不到檔案，只是要等 CLI 真的跑起來才會發現）。
async fn stage_cross_identity_transcript(app: &Arc<App>, bot: &db::Bot, host: &str, transcript: &str) -> Result<(), &'static str> {
    if host == LOCAL_HOST {
        stage_cross_identity_transcript_local(app, bot, transcript).await
    } else {
        stage_cross_identity_transcript_remote(app, bot, host, transcript).await
    }
}

/// 兩邊 `projects/`（canonicalize 後）本來就是同一份（例如 symlink）時什麼都不做。來源檔不在了，
/// 或建目錄／複製失敗，都回傳 `transcript_missing` 讓呼叫端退回開新對話，不 panic、不硬擋重啟。
async fn stage_cross_identity_transcript_local(app: &Arc<App>, bot: &db::Bot, transcript: &str) -> Result<(), &'static str> {
    let src = std::path::Path::new(transcript);
    if !src.exists() {
        return Err("transcript_missing");
    }
    let (Some(cwd_dir), Some(fname)) = (src.parent(), src.file_name()) else { return Err("transcript_missing") };
    let Some(old_projects) = cwd_dir.parent() else { return Err("transcript_missing") };
    let new_dir = identity_config_dir(app, LOCAL_HOST, bot.identity.as_deref()).await;
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

/// 跟 local 版做同一件事，但舊／新 `projects/` 都在**那台遠端主機**上：是同機複製，透過 ssh
/// 執行一段 shell script，不是本機↔遠端搬檔。主機沒連線／ssh 指令本身失敗都回 `transcript_missing`
/// ——連不上就假裝已經搬過去，比直接開新對話更糟（會讓 `--resume` 帶著錯的期待送出去）。
async fn stage_cross_identity_transcript_remote(app: &Arc<App>, bot: &db::Bot, host: &str, transcript: &str) -> Result<(), &'static str> {
    let src = std::path::Path::new(transcript);
    let (Some(cwd_dir), Some(fname)) = (src.parent(), src.file_name()) else { return Err("transcript_missing") };
    let Some(old_projects) = cwd_dir.parent() else { return Err("transcript_missing") };
    let Some(cwd_key) = cwd_dir.file_name() else { return Err("transcript_missing") };
    let Some(conn) = app.hosts.get(host).await else {
        tracing::warn!(bot = %bot.name, host, "identity switch：主機沒連線，改開新對話");
        return Err("transcript_missing");
    };
    let new_dir = identity_config_dir(app, host, bot.identity.as_deref()).await;
    let new_projects = std::path::Path::new(&new_dir).join("projects");
    let script = remote_stage_script(
        &old_projects.to_string_lossy(),
        &new_projects.to_string_lossy(),
        &cwd_key.to_string_lossy(),
        &fname.to_string_lossy(),
        src.file_stem().map(|s| s.to_string_lossy().into_owned()).as_deref(),
    );
    let out = match conn.ssh_exec(&script).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(bot = %bot.name, host, error = %e, "identity switch：搬遠端 session 檔的 ssh 指令失敗，改開新對話");
            return Err("transcript_missing");
        }
    };
    match parse_stage_output(&out) {
        Ok(()) => {
            tracing::info!(bot = %bot.name, host, "identity switch：遠端 session 檔已搬到新身分的 projects 目錄，可以接回對話");
            Ok(())
        }
        Err(why) => {
            tracing::warn!(bot = %bot.name, host, output = %out.trim(), "identity switch：遠端搬 session 檔失敗，改開新對話");
            Err(why)
        }
    }
}

/// 純函式：組出「在同一台遠端主機上把 session 檔從舊身分的 projects/ 搬到新身分」的 shell script。
/// 每個路徑各自 `sh_quote` 過再組合，不把 `cwd_key`／`fname` 未加引號地黏進雙引號字串裡——這兩個
/// 值來自 session id／專案路徑衍生的目錄名，理論上不含特殊字元，但構造 remote shell 指令本來就該
/// 每一段都當危險字串處理。`ssh_exec` 只回 stdout（沒有 exit code），靠印出的 `AM_*` 標記讓呼叫端
/// 判斷結果，跟 `install_remote_hook` 那類既有遠端安裝腳本「檢查確認字串有沒有出現」是同一套做法。
fn remote_stage_script(old_projects: &str, new_projects: &str, cwd_key: &str, fname: &str, stem: Option<&str>) -> String {
    let old_cwd_dir = format!("{old_projects}/{cwd_key}");
    let new_cwd_dir = format!("{new_projects}/{cwd_key}");
    let src = format!("{old_cwd_dir}/{fname}");
    let dest = format!("{new_cwd_dir}/{fname}");
    let companion = stem
        .map(|s| {
            let from = format!("{old_cwd_dir}/{s}");
            format!(
                "if [ -d {from} ]; then cp -a {from} {to} 2>/dev/null || true; fi\n",
                from = sh_quote(&from),
                to = sh_quote(&format!("{new_cwd_dir}/"))
            )
        })
        .unwrap_or_default();
    format!(
        "set -e\n\
         if [ \"$(readlink -f {old} 2>/dev/null || printf '%s' {old})\" = \"$(readlink -f {new} 2>/dev/null || printf '%s' {new})\" ]; then printf 'AM_SAME\\n'; exit 0; fi\n\
         [ -f {src} ] || {{ printf 'AM_MISSING\\n'; exit 0; }}\n\
         mkdir -p {new_cwd_dir} || {{ printf 'AM_MKDIR_FAILED\\n'; exit 0; }}\n\
         cp {src} {dest} || {{ printf 'AM_COPY_FAILED\\n'; exit 0; }}\n\
         {companion}printf 'AM_STAGED\\n'\n",
        old = sh_quote(old_projects),
        new = sh_quote(new_projects),
        src = sh_quote(&src),
        new_cwd_dir = sh_quote(&new_cwd_dir),
        dest = sh_quote(&dest),
    )
}

/// 純函式：解析 `remote_stage_script` 印出的標記。`AM_SAME`／`AM_STAGED` 是成功；`AM_MISSING`／
/// `AM_MKDIR_FAILED`／`AM_COPY_FAILED`，或任何看不懂的輸出（腳本本身炸掉、ssh 只回了部分內容），
/// 一律當失敗——寧可多退回開新對話，也不要在沒把握的情況下宣稱搬成功。
fn parse_stage_output(out: &str) -> Result<(), &'static str> {
    if out.contains("AM_SAME") || out.contains("AM_STAGED") {
        Ok(())
    } else {
        Err("transcript_missing")
    }
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
///
/// 這件事一定要在聊天室裡看得見：以前只寫一行 `tracing::info!`，使用者（或 AGM）看到的是
/// 一顆重啟成功、狀態正常的 bot，實際上脈絡已經悄悄斷了，要等到回覆內容不對勁才會發現
/// （issue #92：換身分接不回原對話那次，靠人工翻 log 才查到；`resume_mismatch`——hook 回報的
/// session 跟預期的不是同一個——更隱蔽，CLI 自己開了新對話卻沒有任何錯誤）。插一則系統訊息，
/// 跟别的系統通知（例如 CLI 沒裝）走同一條 `insert_message` 路，前端不用另外加邏輯就看得到。
pub(crate) async fn context_lost(app: &Arc<App>, bot: &db::Bot, why: &str) -> LcResult<()> {
    tracing::warn!(bot = %bot.name, why, "native session continuation unavailable; starting a new conversation");
    let reason = match why {
        "no_session_id" => "沒有記到上一段對話的 session id",
        "transcript_missing" => "上一段對話的紀錄檔不見了（換身分時可能沒搬過去，或檔案被清掉）",
        "resume_mismatch" => "接回去之後，實際回報的對話跟原本要接的不是同一個",
        "unsupported_kind" => "這個 CLI 種類不支援接續原生對話",
        other => other,
    };
    let conv = db::conversation_id(&app.db, &bot.id).await.map_err(up)?;
    let _ = insert_message(
        app,
        &conv,
        None,
        "system",
        &format!("⚠️ 接不回原本的對話（{reason}），已經開了新的對話——前面的脈絡沒有帶過來。"),
        "system",
        false,
        None,
    )
    .await;
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
        // 先把繼承來的 shim 目錄清掉再接自己的：子 agent 的 pane 是 `pane split` 出來的，父 pane 的
        // shim 原封不動跟著進來（`shim_path`，2026-09-18 的巢狀死鎖）。
        let line = crate::shim_path::pane_export_line(&sh_quote(dir));
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
    let status = match client.agent_wait(&agent, &until, 60_000).await {
        Ok(info) => info.agent_status.normalized(),
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "agent.wait did not settle");
            // Do NOT close the pane on timeout (SPEC §6.2.7).
            match client.agent_get(&agent).await {
                Ok(Some(info)) => info.agent_status.normalized(),
                _ => {
                    close_pane_and_tab(&client, Some(&workspace_id), Some(&tab_id), &pane_id).await;
                    return Err(up(e));
                }
            }
        }
    };
    // agent 起來了、`running` 還沒記下的那一瞬（測試在這裡插進不拿 bot 鎖的 pane-exit 事件）。
    #[cfg(test)]
    {
        super::race_point::hit("start_before_running", &bot.id).await;
    }
    // `running` 是 prompt 准入、UI、restart／reconcile 認的權威（#145）：跨過 `agent.start` 之後寫不進去，
    // 就不能回一般的成功，也不殺掉活著的 agent——回 503、排重試，讓對帳照 herdr 的證據收成 running。
    match super::run_state::transition(&app.db, run_id, &["starting"], "running", Some(status.as_str())).await {
        Ok(super::run_state::Moved::Applied) => {}
        // 不拿 bot 鎖的 pane-exit／workspace-closed 事件已經把它收成終態（pane 沒了）：不拉回 running。
        Ok(super::run_state::Moved::Lost) => {
            tracing::warn!(bot = %bot.name, run = run_id, "the run ended while it was starting; not bringing it back");
            return Err(LcError::Upstream(format!("run {run_id} ended while it was starting (its pane went away)")));
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, run = run_id, error = %e, "the agent is up but its run could not be recorded as running");
            super::run_state::schedule_settle(app, run_id, super::run_state::Settle::Reconcile { stuck: "starting".into() });
            return Err(LcError::uncommitted(
                "start_state_uncommitted",
                run_id,
                "agent 已經在跑，但 run 的狀態寫不進 DB；已排重試，會照 herdr 的狀態收斂成 running",
                e,
            ));
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
/// user-facing reason when missing; a lookup that itself fails passes. 「怎麼去問」測試可以換掉
/// （`App.kind_probe`，見 `kind_probe.rs`）；指令與判讀是同一份。
async fn ensure_kind_installed(app: &Arc<App>, host: &str, kind: &str) -> Result<(), String> {
    if !crate::config::valid_kind(kind) {
        return Err(format!("未知的 bot kind `{kind}`"));
    }
    let probe = crate::kind_probe::probe_command(kind);
    let found: Option<String> = if let Some(run) = app.kind_probe.get() {
        run(host, kind, &probe)
    } else if host == LOCAL_HOST {
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
    crate::kind_probe::verdict(host, kind, found)
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
    // 停到起之間沒有 active run，但 bot 馬上就回來：排著的派工不是孤兒（issue #106，`restart_hold`）。
    let restarting = super::restart_hold::begin(bot_id);
    stop_for_restart_locked(app, bot_id).await?;
    let started = restart_start(app, bot_id, opts).await;
    drop(restarting);
    match &started {
        // 排著的交給新的 run：`--resume` 起的 claude 由 `resume_gate` 等驗證，其他照常送。
        Ok(_) => schedule_flush_queued(app, bot_id),
        // 新 agent 起來了，只是 `running` 還沒記下（#145）：bot 回來了，不是沒開回來。對帳重試收成 running 時不叫 flush，
        // 它起來時的 idle 邊又早在 `starting` 就過了：等它收斂再叫（#165，同 start_send 的 #152）。
        Err(LcError::Uncommitted(v)) => {
            if let Some(run_id) = v.get("run_id").and_then(|r| r.as_str()) {
                super::start_send::flush_once_running(app, bot_id, run_id);
            }
        }
        Err(_) => {
            if let Some(run_id) = stopping.as_deref() {
                left_down_by_restart(app, bot_id, run_id).await;
            }
            // 沒開回來：這下排著的才真的沒有人會送。
            revoke_orphaned_queued_turns(app, bot_id, "重啟之後沒能把 bot 開回來").await;
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

    // 舊 run 跟 stop 走同一套（#146 留言）：先記「正在停」，記不下來就一步都不做；被 pane-exit 事件先收掉就不重開。
    match super::run_state::transition(&app.db, &run.id, super::run_state::LIVE, "stopping", None).await.map_err(up)? {
        super::run_state::Moved::Applied => {}
        super::run_state::Moved::Lost => {
            app.emit_bot_status(bot_id).await;
            return Err(LcError::Bad("這個子 agent 的 pane 已經被關掉了".into()));
        }
    }
    app.emit_bot_status(bot_id).await;
    // 在飛的那一筆先收，收不成就不送 ctrl+c、不重開（#156，跟 stop 同一套）。
    if let Err(e) = fail_in_flight(app, &run.id, "restarted to apply the CLI update").await {
        return Err(super::turn_unwritable(app, bot_id, &run.id, "重啟", e).await);
    }

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
        // 也不救——永遠黃燈（2026-09-12 review #1）。寫不進去就排重試。
        back_to_running(app, &run.id).await;
        app.emit_bot_status(bot_id).await;
        return Err(LcError::Upstream("子 agent 十秒內沒有退出，沒有動它的 pane".into()));
    }
    // 舊 run 停了、新 run 還沒寫進去：這一段沒有 active run，但子 agent 馬上就在同一個 pane 裡回來，
    // 排著的派工不是孤兒（#129，跟 #106 同一件事）。寫入新 run 之後就放掉：之後的失敗照舊撤。
    let restarting = super::restart_hold::begin(bot_id);
    // 舊 run 的 `stopped` 寫進去之前不寫新 run、不 `agent.start`（#146 留言）：不靠 active-run 唯一索引碰巧擋住。
    match super::run_state::transition(&app.db, &run.id, &["stopping"], "stopped", None).await {
        Ok(super::run_state::Moved::Applied) => {}
        // pane-exit 事件先把它收掉了（pane 沒了）：同上面 pane 不見的處理，不在別處重開。
        Ok(super::run_state::Moved::Lost) => {
            app.emit_bot_status(bot_id).await;
            return Err(LcError::Bad("這個子 agent 的 pane 已經被關掉了".into()));
        }
        // agent 已經退出 pane，舊 run 卻還是 `stopping`：不重啟。憑證隨 return 放掉（#129：沒開回來照舊撤），
        // 對帳照證據把舊 run 收成 exited 之後，排著的派工才當孤兒撤。
        Err(e) => {
            tracing::warn!(bot = %bot.name, run = %run.id, error = %e, "the child left its pane but its old run could not be recorded as stopped; not restarting");
            super::run_state::schedule_settle(app, &run.id, super::run_state::Settle::Reconcile { stuck: "stopping".into() });
            app.emit_bot_status(bot_id).await;
            return Err(LcError::uncommitted(
                "stop_state_uncommitted",
                &run.id,
                "子 agent 已經退出，但舊 run 的狀態寫不進 DB；沒有重啟，已排重試",
                e,
            ));
        }
    }
    // 測試在這裡插進不拿 bot 鎖的 sweeper。
    #[cfg(test)]
    {
        super::race_point::hit("child_restart_between_runs", bot_id).await;
    }

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
    drop(restarting);

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
    // 同 #145：agent 起來了，`running` 寫不進去就不回成功；pane-exit 事件先收掉的不拉回來。
    match super::run_state::transition(&app.db, &run_id, &["starting"], "running", None).await {
        Ok(super::run_state::Moved::Applied) => {}
        Ok(super::run_state::Moved::Lost) => {
            app.emit_bot_status(bot_id).await;
            return Err(LcError::Bad("這個子 agent 的 pane 已經被關掉了".into()));
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, run = %run_id, error = %e, "the child is back but its run could not be recorded as running");
            super::run_state::schedule_settle(app, &run_id, super::run_state::Settle::Reconcile { stuck: "starting".into() });
            // 排著的派工留給它（#129），但收成 running 的對帳不叫 flush：等它收斂再叫（#165）。
            super::start_send::flush_once_running(app, bot_id, &run_id);
            app.emit_bot_status(bot_id).await;
            return Err(LcError::uncommitted(
                "start_state_uncommitted",
                &run_id,
                "子 agent 已經在原 pane 裡起來了，但 run 的狀態寫不進 DB；已排重試，會照 herdr 的狀態收斂成 running",
                e,
            ));
        }
    }
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

    /// `resume_native` 沒要求一定要接（`resume_required=false`）、接不回時照舊退回開新對話——但這件
    /// 事以前只寫一行 log，聊天室裡看不出來，使用者會以為 bot 正常重啟了，其實脈絡已經斷了
    /// （issue #92）。現在要在對話裡留一則看得到的系統訊息。
    #[tokio::test]
    async fn a_silent_fallback_to_a_new_conversation_leaves_a_visible_note() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        // 沒有任何上一段原生對話：native_resume_plan 判定 no_session_id，退回開新對話。
        start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        assert!(!started_args(&e).pop().unwrap().contains(&"--resume".into()), "沒有上一段對話，不該帶 --resume");

        let conv = db::conversation_id(&e.app.db, &pm.id).await.unwrap();
        let note: String = sqlx::query_scalar(
            "SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&conv)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert!(note.contains("接不回"), "{note}");
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

    /// issue #95：換身分後 transcript 只搬本機，遠端主機的對話接不回來。這裡測遠端那條路：純函式
    /// 部分（組 script、解析輸出）直接驗內容；連線失敗時的 fail-closed 用一個刻意連不上的假 host
    /// 驗證（沒有可重用的 live-SSH 測試環境，這是能不碰任何真實遠端主機驗到的最大範圍）。
    mod remote_cross_identity_transcript_tests {
        use super::*;
        use crate::lifecycle::native_resume_plan;
        use crate::lifecycle::start::{parse_stage_output, remote_stage_script};
        use std::time::Duration;

        /// `remote_stage_script` 只是拼字串，真正的守衛（`[ -f ... ]`／`mkdir -p ... ||`／
        /// `cp ... ||`）活在產生出來的 shell 語法裡——只驗「文字裡有沒有出現某個標記」測不到
        /// 那些守衛是不是真的接對了地方（q4queue／is5859 今晚踩到同一類問題：trigger 測得到欄位
        /// 變化，測不到 WHERE 子句擋不擋得住）。這裡直接把腳本丟給本機 `/bin/sh` 跑：語法跟
        /// `ssh_exec` 遠端執行的是同一顆直譯器，用本機暫存目錄冒充「舊／新身分的 projects/」，
        /// 不連任何真實遠端主機。
        fn run_script_locally(script: &str) -> String {
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).output().expect("run script locally");
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        fn tmp() -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("am-remote-stage-{}", db::ulid()));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn symlinked_projects_dirs_report_same_and_copy_nothing() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("cwd-key")).unwrap();
            std::fs::write(old.join("cwd-key").join("sid.jsonl"), "orig").unwrap();
            let new = base.join("new-projects");
            std::os::unix::fs::symlink(&old, &new).unwrap();

            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "cwd-key", "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_SAME"), "{out}");

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn different_dirs_copy_the_transcript_and_its_companion() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("cwd-key")).unwrap();
            std::fs::write(old.join("cwd-key").join("sid.jsonl"), "hello").unwrap();
            std::fs::create_dir_all(old.join("cwd-key").join("sid")).unwrap();
            std::fs::write(old.join("cwd-key").join("sid").join("extra.txt"), "companion").unwrap();
            let new = base.join("new-projects"); // 還不存在，要靠 mkdir -p 建出來

            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "cwd-key", "sid.jsonl", Some("sid"));
            let out = run_script_locally(&script);
            assert!(out.contains("AM_STAGED"), "{out}");
            assert_eq!(std::fs::read_to_string(new.join("cwd-key").join("sid.jsonl")).unwrap(), "hello");
            assert_eq!(std::fs::read_to_string(new.join("cwd-key").join("sid").join("extra.txt")).unwrap(), "companion");

            let _ = std::fs::remove_dir_all(&base);
        }

        /// `[ -f "$SRC" ]` 那道守衛真的擋住了：來源檔不存在時不會往下跑 `mkdir`／`cp`，
        /// 只印 `AM_MISSING`，也不會建出目的目錄。
        #[test]
        fn a_missing_source_file_reports_missing_and_creates_nothing() {
            let base = tmp();
            let old = base.join("old-projects"); // 連目錄都沒建，模擬來源徹底不存在
            let new = base.join("new-projects");

            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "cwd-key", "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_MISSING"), "{out}");
            assert!(!new.exists(), "沒東西好搬，不該建出目的目錄：{}", new.display());

            let _ = std::fs::remove_dir_all(&base);
        }

        /// `mkdir -p ... ||` 那道守衛真的擋住了：目的路徑的上一層其實是一個檔案（不是目錄），
        /// `mkdir -p` 一定失敗，腳本要印 `AM_MKDIR_FAILED`，不能繼續往下跑 `cp` 假裝成功。
        #[test]
        fn an_unmakeable_destination_reports_mkdir_failed() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("cwd-key")).unwrap();
            std::fs::write(old.join("cwd-key").join("sid.jsonl"), "hello").unwrap();
            let blocker = base.join("blocker");
            std::fs::write(&blocker, "this is a file, not a directory").unwrap();
            let new = blocker.join("projects"); // 上一層是檔案，底下建不出任何東西

            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "cwd-key", "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_MKDIR_FAILED"), "{out}");

            let _ = std::fs::remove_dir_all(&base);
        }

        /// 目錄名帶著 shell 特殊字元（`$(...)`、單引號）不能被當成指令執行——每段路徑各自
        /// `sh_quote`，不是把值原樣黏進雙引號字串裡。用一個會在展開時建出檔案的 `$(...)` 當
        /// canary：quoting 對了就不會被展開，那個檔案就不會出現。
        #[test]
        fn shell_metacharacters_in_a_directory_name_are_not_executed() {
            let base = tmp();
            let canary = base.join("pwned-if-expanded");
            let weird_cwd = format!("it's-$(touch {})-cwd", canary.display());
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join(&weird_cwd)).unwrap();
            std::fs::write(old.join(&weird_cwd).join("sid.jsonl"), "hello").unwrap();
            let new = base.join("new-projects");

            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), &weird_cwd, "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_STAGED"), "{out}");
            assert_eq!(std::fs::read_to_string(new.join(&weird_cwd).join("sid.jsonl")).unwrap(), "hello");
            assert!(!canary.exists(), "$(...) 被當成指令展開執行了，quoting 沒有真的把它擋住");

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn parse_stage_output_only_trusts_the_success_markers() {
            assert_eq!(parse_stage_output("AM_SAME\n"), Ok(()));
            assert_eq!(parse_stage_output("AM_STAGED\n"), Ok(()));
            assert_eq!(parse_stage_output("AM_MISSING\n"), Err("transcript_missing"));
            assert_eq!(parse_stage_output("AM_MKDIR_FAILED\n"), Err("transcript_missing"));
            assert_eq!(parse_stage_output("AM_COPY_FAILED\n"), Err("transcript_missing"));
            assert_eq!(parse_stage_output(""), Err("transcript_missing"), "ssh 連上了但腳本什麼都沒印，不能當成功");
            assert_eq!(parse_stage_output("Permission denied (publickey)\n"), Err("transcript_missing"), "ssh 本身報錯的輸出，不能被誤判成任何一個 AM_* 標記");
        }

        /// 主機根本沒連線（`app.hosts` 裡沒有這個名字）：連 ssh 都沒機會跑，直接 fail closed。
        #[tokio::test]
        async fn an_unconfigured_host_fails_closed_without_touching_ssh() {
            let e = env().await;
            let pm = claude_bot(&e.app, &e.project_id, "pm").await;
            let transcript = e.dir.join("remote-pm.jsonl");
            std::fs::write(&transcript, "{}\n").unwrap();
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'stopped','idle','sid-remote-1',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
            )
            .bind(db::ulid())
            .bind(&pm.id)
            .bind(transcript.to_str().unwrap())
            .execute(&e.app.db)
            .await
            .unwrap();

            let bot = db::bot(&e.app.db, &pm.id).await.unwrap().unwrap();
            let plan = native_resume_plan(&e.app, &bot, "no-such-host", false).await.unwrap();
            assert_eq!(plan, Err("transcript_missing"));
        }

        /// 主機有設定，但連不上（沒有東西在聽那個 port，connection refused）：ssh_exec 真的失敗，
        /// 一樣要 fail closed，不能假裝已經搬過去——這是能不碰真實遠端主機驗到的最大範圍
        /// （issue #95 明講：沒有可重用的 live-SSH 測試環境時，用刻意連不上的假 host 驗證）。
        #[tokio::test]
        async fn an_unreachable_host_fails_closed_instead_of_pretending_to_stage() {
            let e = env().await;
            let pm = claude_bot(&e.app, &e.project_id, "pm").await;

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener); // 沒有人在聽這個 port 了：接下來連過去是 connection refused，快速失敗。

            let host_cfg = crate::config::HostCfg {
                name: "unreachable-box".into(),
                ssh: "127.0.0.1".into(),
                ssh_port: port,
                ssh_opts: vec!["-o".into(), "ConnectTimeout=2".into()],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
            };
            e.app.hosts.apply_config(&e.app, &[host_cfg]).await;

            let transcript = e.dir.join("remote-pm-2.jsonl");
            std::fs::write(&transcript, "{}\n").unwrap();
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'stopped','idle','sid-remote-2',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
            )
            .bind(db::ulid())
            .bind(&pm.id)
            .bind(transcript.to_str().unwrap())
            .execute(&e.app.db)
            .await
            .unwrap();

            let bot = db::bot(&e.app.db, &pm.id).await.unwrap().unwrap();
            let plan = tokio::time::timeout(Duration::from_secs(20), native_resume_plan(&e.app, &bot, "unreachable-box", false))
                .await
                .expect("連不上要快速失敗，不能卡住整個重啟流程")
                .unwrap();
            assert_eq!(plan, Err("transcript_missing"), "連不上遠端主機，不能假裝已經搬過去");

            e.app.hosts.remove(&e.app, "unreachable-box").await;
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

    /// 子 agent 原地重啟的「舊 run 停了、新 run 還沒寫進去」那一段也是重啟中（#106 換到這條路）：
    /// 不拿 bot 鎖的 orphan sweeper 剛好在這時候跑，不可以把 AGM 排著的派工當孤兒撤掉。
    #[tokio::test]
    async fn a_queued_dispatch_survives_a_sweep_in_the_middle_of_a_child_restart() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let parent = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let kid = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
             VALUES (?,?,'ui','claude','[]',0,0,'tok','child',?,?)",
        )
        .bind(&kid)
        .bind(&env.project_id)
        .bind(&parent.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,'proj-alfa-ui','test',1,?)",
        )
        .bind(db::ulid())
        .bind(&kid)
        .bind(&ws.workspace_id)
        .bind(&kid_pane.tab_id)
        .bind(&kid_pane.pane_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        // 有名字的 agent：mock 收到 ctrl+c 就讓它離開，`agent.start` 再把它放回來。
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "proj-alfa-ui", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id,
            "cwd": "/tmp/p"})];
        let conv = db::conversation_id(&app.db, &kid).await.unwrap();
        let queued = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&queued)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        let swept = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (app2, swept2) = (app.clone(), swept.clone());
        super::super::race_point::arm("child_restart_between_runs", &kid, move || async move {
            revoke_all_orphaned_queued_turns(&app2).await;
            swept2.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        restart_child_in_pane(&app, &kid).await.expect("子 agent 在原本的 pane 裡回來了");
        assert!(swept.load(std::sync::atomic::Ordering::SeqCst), "sweeper 真的落在停與起之間");
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&queued).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "queued", "重啟中的子 agent 不是孤兒：派工留給新的 run 送");
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
        // 工作目錄是經過符號連結進去的（macOS 的 `/tmp` 就是）；自己造一個，不靠這台機器的 tmp 剛好是不是連結
        // ——Linux 的 `/tmp` 不是，原本的 `!starts_with("/tmp/")` 在那邊必紅（#139，遠端編譯主機）。
        let repo = dir.join("real/repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let store = dir.join(".claude.json");
        std::fs::write(&store, "{\"numStartups\":7}").unwrap();

        let cwd = dir.join("link/repo").to_string_lossy().to_string();
        let wrote = crate::trust::mark_trusted("claude", &store, &[crate::trust::canonical(&cwd)]).unwrap();
        assert!(wrote, "a directory the CLI has not seen is recorded");

        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 7, "the CLI's own state survives the merge");
        let key = crate::trust::canonical(&cwd);
        assert_eq!(v["projects"][&key]["hasTrustDialogAccepted"], true);
        // Resolved path: the CLI compares its `getcwd()`, which never contains the symlink.
        assert!(!key.contains("/link/"), "the recorded path is canonical, got {key}");
        assert_eq!(key, std::fs::canonicalize(&repo).unwrap().to_string_lossy(), "resolved to the real directory");

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

#[cfg(test)]
mod run_state_commit_tests {
    //! #145：跨過 `agent.start` 之後，run 狀態寫不進去就不能回「啟動成功」，也不能留下一顆永遠卡住的 run。
    use super::super::run_state as rs;
    use super::*;
    use crate::testing as tt;

    fn starts(env: &tt::Env) -> usize {
        env.herdr.methods().iter().filter(|m| *m == "agent.start").count()
    }

    async fn live_runs(app: &Arc<App>, bot_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=? AND state IN ('starting','running','stopping')")
            .bind(bot_id)
            .fetch_one(&app.db)
            .await
            .unwrap()
    }

    /// 驗收 1、2：agent 真的起來了，`running` 卻寫不進去——不回一般的成功、不把活著的 agent 殺掉，
    /// DB 恢復後照 herdr 的證據收成 `running`，不再開第二顆。
    #[tokio::test]
    async fn a_start_whose_running_state_cannot_be_recorded_is_not_reported_as_started() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        rs::refuse_run_state(&app, "running").await;

        let err = start_bot(&app, &bot.id).await.expect_err("DB 沒記下 running，不能回啟動成功");
        // 先看錯誤是哪一種：沒走到 agent.start 就失敗（例如這台沒裝 claude）的話，訊息直接寫在這裡。
        let LcError::Uncommitted(body) = &err else { panic!("要 503 start_state_uncommitted，拿到 {err:?}") };
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("agent 在跑，run 不能被收成 exited");
        assert_eq!((body["error"].as_str(), body["run_id"].as_str()), (Some("start_state_uncommitted"), Some(run.id.as_str())));
        assert_eq!(run.state, "starting");
        let methods = env.herdr.methods();
        assert!(!methods.iter().any(|m| m == "pane.close"), "活著的 agent 不收：{methods:?}");
        assert_eq!(starts(&env), 1);
        assert_eq!(rs::scheduled(&run.id), vec![rs::Settle::Reconcile { stuck: "starting".into() }]);

        rs::accept_run_state(&app, "running").await;
        assert!(rs::settle_once(&app, &run.id, &rs::Settle::Reconcile { stuck: "starting".into() }).await);
        assert_eq!(db::run(&app.db, &run.id).await.unwrap().unwrap().state, "running", "照 herdr 的證據收斂");
        assert_eq!(starts(&env), 1, "沒有再開第二顆");
        assert_eq!(live_runs(&app, &bot.id).await, 1);
    }

    /// 同驗收 1，走重啟（換身分、`?resume=native`、一鍵重啟）而且 AGM 有一則排著的派工：重啟中不撤（#106），但對帳把新 run
    /// 收成 `running` 不會叫 flush（#152 在 start_send 補過同一個洞），它起來時的 idle 邊又早在 `starting` 就過了——要自己等它
    /// 收斂再叫，不然那一則要等 30 分鐘的排隊保險絲把它撤掉、交辦停在 blocked。
    #[tokio::test]
    async fn a_restart_whose_new_run_cannot_be_recorded_running_still_flushes_the_queue_once_it_settles() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        start_bot(&app, &bot.id).await.expect("先起來");
        let queued = rs::a_turn(&app, &bot.id, None, "queued").await;
        rs::refuse_run_state(&app, "running").await;

        let err = restart_bot(&app, &bot.id).await.expect_err("新 run 沒記下 running");
        let LcError::Uncommitted(body) = &err else { panic!("要 503 start_state_uncommitted，拿到 {err:?}") };
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("新 run 停在 starting");
        assert_eq!((body["run_id"].as_str(), run.state.as_str()), (Some(run.id.as_str()), "starting"));
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "重啟中：不當孤兒撤");
        assert!(super::super::start_send::watching_for_running(&run.id), "排了「收成 running 就叫 flush」");
    }

    /// 驗收 3：start 失敗、pane 收掉了，`exited` 卻也寫不進去——不能留一顆永遠擋住下一次 start 的 `starting`。
    #[tokio::test]
    async fn a_failed_start_that_could_not_record_the_exit_does_not_leave_a_zombie() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        sqlx::query(
            "CREATE TRIGGER fail_run_mapping BEFORE UPDATE OF workspace_id, pane_id, tab_id ON runs
             BEGIN SELECT RAISE(ABORT, 'forced run mapping failure'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        rs::refuse_run_state(&app, "exited").await;

        let err = start_bot(&app, &bot.id).await.expect_err("mapping 寫不進去");
        assert!(matches!(&err, LcError::Upstream(m) if m.contains("forced run mapping failure")), "要在 mapping 那一步失敗，拿到 {err:?}");
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("exited 寫不進去，DB 上還是 starting");
        assert!(env.herdr.methods().iter().any(|m| m == "pane.close"), "pane 照樣收掉");
        assert_eq!(rs::scheduled(&run.id), vec![rs::Settle::Reconcile { stuck: "starting".into() }], "排了重試，不是只吞掉");

        sqlx::query("DROP TRIGGER fail_run_mapping").execute(&app.db).await.unwrap();
        rs::accept_run_state(&app, "exited").await;
        assert!(rs::settle_once(&app, &run.id, &rs::Settle::Reconcile { stuck: "starting".into() }).await);
        assert_eq!(db::run(&app.db, &run.id).await.unwrap().unwrap().state, "exited");
        start_bot(&app, &bot.id).await.expect("zombie 清掉之後可以再啟動");
    }

    /// 驗收 4：agent 起來之後、`running` 記下之前，不拿 bot 鎖的 pane-exit 事件先把 run 收成 `exited`——
    /// 不能把一顆已經結束的 run 拉回 `running`，也不能回啟動成功。
    #[tokio::test]
    async fn a_start_does_not_bring_back_a_run_another_path_already_ended() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let (app2, bot2) = (app.clone(), bot.id.clone());
        super::super::race_point::arm("start_before_running", &bot.id, move || async move {
            let run = db::active_run(&app2.db, &bot2).await.unwrap().unwrap();
            mark_run_exited(&app2, &run.id, "pane exited").await;
        });

        let err = start_bot(&app, &bot.id).await.expect_err("run 已經結束了，不是啟動成功");
        assert!(matches!(&err, LcError::Upstream(m) if m.contains("ended while it was starting")), "要輸在 running 的 CAS，拿到 {err:?}");
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE bot_id=?").bind(&bot.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "exited", "終態不會被拉回 running");
    }
}
