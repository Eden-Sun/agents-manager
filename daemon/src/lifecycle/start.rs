//! Starting and restarting a bot: pane acquisition, `agent.start`, and resume.

use super::*;

/// Identity CLI args on `host`. Discovered `ccN` identities carry none: alias flags are the user's shell habit.
async fn identity_args(app: &Arc<App>, bot: &db::Bot, host: &str) -> Vec<String> {
    let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) else { return vec![] };
    crate::tools::identity_for_host(app, host, idn).await.map(|i| i.args).unwrap_or_default()
}

/// `-c tui.show_tooltips=false`：codex 0.158 起回合超過 30 秒、以及第三回合起的回合結束後會插一行隨機 tip
/// （openai/codex#48352），畫面備援擷取可能把它當成回覆結尾（issue #728）。不依賴使用者的 config.toml，固定關掉。
fn codex_pane_guard_args(kind: &str) -> Vec<String> {
    if kind == "codex" {
        vec!["--no-daemon".into(), "--no-alt-screen".into(), "-c".into(), "tui.show_tooltips=false".into()]
    } else {
        Vec::new()
    }
}


/// `resume_native` continues the bot's last native session (batch update restart).
/// `fork_session`：從這個 native session 分出一個新 session（`POST /bots/:id/fork` 的第一次啟動，SPEC §6.10）。
/// `require_idle`：restart 在**拿到 bot 鎖之後**再確認一次閒置，不閒置回 409 `not_idle`、什麼都不動（一鍵重啟用）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StartOpts {
    pub resume_native: bool,
    /// 跟 `resume_native` 一起用：接不回原本的對話就**不啟動**，回 409 `resumed:false`＋原因，
    /// 由呼叫端決定要不要改成開新對話（`?resume=native`，herdr 升級 2026-09-17）。沒設的話照舊退回開新對話。
    pub resume_required: bool,
    pub fork_session: Option<String>,
    pub require_idle: bool,
    /// Bulk update restart must re-read the live pane under the bot lock before stopping it.
    pub refuse_background_jobs: bool,
    /// 跟 `resume_native` 一起用：不看 DB 記的 session，改接這一段（`?resume=native&session=<id>`）。
    /// 救援用：DB 記錯（例如 2026-09-22 被 codex 子行程的 thread-id 蓋掉）時，讓 AGM 指名接回真正的對話。
    pub resume_session: Option<String>,
}

/// 這顆 bot 現在為什麼不能被重啟；`None` ＝ 閒置。呼叫端持 bot 鎖。理由的代碼與 `bulk_restart::Skip` 一致。
///
/// 一鍵重啟在排到這顆時 recheck 過一次，但 recheck 到這裡拿到鎖之間仍有空檔：使用者剛好在那幾毫秒送出一則，
/// 那一回合會被 ctrl+c 砍掉（review2 quota #5）。鎖內再看一次才真的關掉。
async fn busy_reason_locked(app: &Arc<App>, bot_id: &str, refuse_background_jobs: bool) -> LcResult<Option<String>> {
    let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? else { return Ok(Some("not_running".into())) };
    let busy = if run.state != "running" {
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
    };
    if let Some(busy) = busy {
        return Ok(Some(busy.to_string()));
    }
    if refuse_background_jobs {
        if let Some(bot) = db::bot(&app.db, bot_id).await.map_err(up)? {
            crate::background_jobs::refresh(app, &run, &bot.kind).await;
            if let Some(n) = crate::background_jobs::known(app, &run.id).filter(|n| *n > 0) {
                return Ok(Some(format!("background_jobs:{n}")));
            }
        }
    }
    Ok(None)
}

fn not_idle(bot_id: &str, why: &str) -> LcError {
    LcError::conflict("not_idle", json!({"bot_id": bot_id, "busy": why}))
}

fn host_superseded(bot_id: &str, host: &str) -> LcError {
    LcError::conflict("host_superseded", json!({"bot_id": bot_id, "host": host}))
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
    start_bot_locked_with_host_fence(app, bot_id, opts, None).await
}

async fn start_bot_locked_with_host_fence(
    app: &Arc<App>,
    bot_id: &str,
    opts: StartOpts,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<String> {
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
    crate::handoff::refuse(&app.db, bot_id).await?;
    if let Some(existing) = db::active_run(&app.db, bot_id).await.map_err(up)? {
        return Err(LcError::conflict("active run already exists", json!({"run_id": existing.id})));
    }
    // 要求接回原對話：在任何副作用（run 列、pane、shim）之前就判斷，接不回就整個不啟動。
    if opts.resume_native && opts.resume_required {
        let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
        if let Err(why) = native_resume_plan(app, &bot, &host, false, opts.resume_session.as_deref()).await? {
            return Err(cannot_resume(bot_id, why));
        }
    }
    let project = db::project(&app.db, &bot.project_id)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let session = match fence {
        Some(fence) => app
            .session_for_bot_with_host_fence(&bot, &project.host, fence)
            .await
            .ok_or_else(|| host_superseded(bot_id, &project.host))?,
        None => app
            .session_for_bot(&bot, &project.host)
            .await
            .ok_or_else(|| LcError::Upstream(format!("host `{}` is not configured", project.host)))?,
    };
    if let Some(fence) = fence {
        if project.host != fence.conn().name || !app.hosts.is_current(fence).await {
            return Err(host_superseded(bot_id, &project.host));
        }
    }
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

    // 上一個 run 被外力收掉時還在忙：接回之後補一句續行提示（claude 2.1.281，`resume_nudge`）。要在新 run 寫進去之前讀。
    let nudge = super::resume_nudge::prior_run_ended_busy(app, &bot, &opts).await;
    // 1. INSERT Run before touching herdr (SPEC §6.2.1).
    // 身分跟著 run 走（issue #238）：pane 用這一刻的身分起來，之後 PATCH 改身分要重啟才生效，額度要記在這個身分上。
    let run_id = db::ulid();
    let started_identity = bot.identity.as_deref().map(str::trim).unwrap_or("").to_string();
    let ins = sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, runtime_identity, launch_rev, started_at) VALUES (?,?,'starting','unknown',?,?,?,?)",
    )
    .bind(&run_id)
    .bind(bot_id)
    .bind(&session)
    .bind(&started_identity)
    // 這個 run 載入的啟動設定版本（#353）：之後 config 改了、版本對不上＝需要重啟，不靠 PATCH 回應裡那個會遺失的布林。
    .bind(crate::launch_rev::of(&bot))
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

    match start_inner(app, &bot, &project, &run_id, &session, opts.clone(), fence).await {
        Ok(()) => {
            if let Some(busy) = nudge {
                super::resume_nudge::arm(app, &bot, &run_id, busy).await;
            }
            Ok(run_id)
        }
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
    override_session: Option<&str>,
) -> LcResult<Result<(String, Vec<String>), &'static str>> {
    let last = if let Some(sid) = override_session {
        // 指名的 session：transcript 路徑盡量從記過這段的 run 帶出來（換身分時要複製），沒有就讓 CLI 自己找。
        let transcript: Option<String> = sqlx::query_scalar(
            // 第二鍵用 `rowid`（寫入順序），不是 `id`：`started_at` 只到毫秒（`db::now`），
            // 同一顆 bot 快速重啟時兩個 run 會擠進同一毫秒，而 ULID 的亂數段在同一毫秒內不保證遞增
            // （issue #100／a4605b2，同 `supervisor/incidents.rs` 挑「最後一個 run」的寫法）。
            "SELECT transcript_path FROM runs WHERE bot_id = ? AND native_session_id = ? AND transcript_path IS NOT NULL
              ORDER BY started_at DESC, rowid DESC LIMIT 1",
        )
        .bind(&bot.id)
        .bind(sid)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?;
        Some((sid.to_string(), transcript))
    } else if include_active {
        sqlx::query_as::<_, (String, Option<String>)>(
            // 同上：挑錯的後果不是少接回一次，是 `--resume` 進**另一段對話**，
            // 之後這顆 bot 的訊息都落在那段裡（issue #461）。重啟正是最會擠同一毫秒的路徑。
            "SELECT native_session_id, transcript_path FROM runs
              WHERE bot_id = ? AND native_session_id IS NOT NULL ORDER BY started_at DESC, rowid DESC LIMIT 1",
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
    // 倒回過、之後沒有新回合：先把倒回點補進 transcript（換身分的話在複製之前），`--resume` 才不會接回舊分支（SPEC §6.13）。
    // 重啟前的檢查（`include_active`）那時 CLI 還開著，它結束時會蓋掉，不在那裡補。
    if !include_active && bot.kind == "claude" {
        crate::rewind::anchor::ensure(app, &session_id).await;
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
pub(crate) async fn identity_config_dir(app: &Arc<App>, host: &str, identity: Option<&str>) -> anyhow::Result<String> {
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    identity_config_dir_for_fence(app, host, identity, &fence).await
}

pub(crate) async fn identity_config_dir_for_fence(
    app: &Arc<App>,
    host: &str,
    identity: Option<&str>,
    fence: &crate::hosts::HostFence,
) -> anyhow::Result<String> {
    let home = crate::hosts::home_for_fence(fence).await?;
    if !app.hosts.is_current(fence).await {
        anyhow::bail!("host `{host}` changed while resolving HOME");
    }
    let dir = match identity.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => crate::tools::identity_for_host(app, host, name)
            .await
            .and_then(|i| i.env.get("CLAUDE_CONFIG_DIR").map(|v| crate::config::expand_home(v, &home))),
        None => None,
    };
    Ok(dir.unwrap_or_else(|| format!("{home}/.claude")))
}

/// 把 `transcript` 複製到目前身分的 `projects/<同一個 cwd 目錄名>/` 下，讓 `--resume` 在那個身分
/// 底下找得到。本機與遠端分開實作：遠端主機上舊/新 `projects/` 都在**那台機器**上，是同機複製，
/// 透過 ssh 執行一段 shell script，不是本機↔遠端搬檔（issue #95：以前只做本機這半，遠端完全跳過，
/// 換身分後 `--resume` 在遠端主機上一樣找不到檔案，只是要等 CLI 真的跑起來才會發現）。
async fn stage_cross_identity_transcript(app: &Arc<App>, bot: &db::Bot, host: &str, transcript: &str) -> Result<(), &'static str> {
    // 只有 claude 的 session 檔住在 `<CLAUDE_CONFIG_DIR>/projects/` 底下。codex／grok 的對話檔不歸這個目錄管：
    // 以前照樣複製，把 codex 的 rollout 整份丟進 `<claude 身分>/projects/<日>/`（資料安全審查）。
    if bot.kind != "claude" {
        return Ok(());
    }
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
    let new_dir = match identity_config_dir(app, LOCAL_HOST, bot.identity.as_deref()).await {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "identity switch：無法確認本機 HOME，改開新對話");
            return Err("transcript_missing");
        }
    };
    let new_projects = std::path::Path::new(&new_dir).join("projects");
    // transcript 可能有幾百 MB：複製放到 blocking pool，不占 tokio worker。
    let (src, cwd_dir, old_projects, fname, bot_name) =
        (src.to_path_buf(), cwd_dir.to_path_buf(), old_projects.to_path_buf(), fname.to_os_string(), bot.name.clone());
    tokio::task::spawn_blocking(move || stage_cross_identity_transcript_local_fs(&bot_name, &src, &cwd_dir, &old_projects, &fname, &new_projects))
        .await
        .unwrap_or(Err("transcript_missing"))
}

/// [`stage_cross_identity_transcript_local`] 的檔案那一半（同步）：呼叫端放進 `spawn_blocking`。
fn stage_cross_identity_transcript_local_fs(
    bot_name: &str,
    src: &std::path::Path,
    cwd_dir: &std::path::Path,
    old_projects: &std::path::Path,
    fname: &std::ffi::OsStr,
    new_projects: &std::path::Path,
) -> Result<(), &'static str> {
    let same = match (std::fs::canonicalize(old_projects), std::fs::canonicalize(new_projects)) {
        (Ok(a), Ok(b)) => a == b,
        _ => old_projects == new_projects,
    };
    if same {
        return Ok(());
    }
    // 要複製了：來源是 DB 記的字串，形狀不對（不是 projects/<key>/<sid>.jsonl）或是符號連結，一律不複製。
    if !super::transcript_stage::is_claude_transcript(src) {
        tracing::warn!(bot = %bot_name, from = %src.display(), "identity switch：來源不是 projects/<cwd>/<id>.jsonl 的一般檔，不複製，改開新對話");
        return Err("transcript_missing");
    }
    let Some(cwd_key) = cwd_dir.file_name() else { return Err("transcript_missing") };
    let dest_dir = new_projects.join(cwd_key);
    // 暫存檔＋rename、不蓋比來源長的目標、檔 0600／目錄 0700：規則在 `transcript_stage`。
    let staged = match super::transcript_stage::stage_file(src, &dest_dir, fname) {
        Ok(staged) => staged,
        Err(e) => {
            tracing::warn!(bot = %bot_name, from = %src.display(), to = %dest_dir.display(), error = %e, "identity switch：複製 session 檔到新身分失敗，改開新對話");
            return Err("transcript_missing");
        }
    };
    match &staged {
        super::transcript_stage::Staged::KeptLongerDestination => {
            tracing::info!(bot = %bot_name, dest = %dest_dir.join(fname).display(), "identity switch：新身分那邊的 session 檔已經比來源長，沿用它不覆蓋");
        }
        super::transcript_stage::Staged::Diverged { set_aside } => {
            tracing::warn!(bot = %bot_name, set_aside = %set_aside.display(), "identity switch：新身分那邊的 session 檔跟來源分岔，舊的改名留在旁邊");
        }
        _ => {}
    }
    // 檔名同名的附屬目錄（有些 CLI 版本會在 jsonl 旁邊放一份）一起搬，搬不動不影響主對話。
    if let Some(stem) = src.file_stem() {
        let companion_src = cwd_dir.join(stem);
        if companion_src.is_dir() {
            if let Err(e) = super::transcript_stage::copy_dir_private(&companion_src, &dest_dir.join(stem)) {
                tracing::warn!(bot = %bot_name, error = %e, "identity switch：session 附屬目錄複製失敗（不影響主對話檔）");
            }
        }
    }
    tracing::info!(bot = %bot_name, from = %src.display(), to = %dest_dir.join(fname).display(), "identity switch：session 檔已搬到新身分的 projects 目錄，可以接回對話");
    Ok(())
}

/// 跟 local 版做同一件事，但舊／新 `projects/` 都在**那台遠端主機**上：是同機複製，透過 ssh
/// 執行一段 shell script，不是本機↔遠端搬檔。主機沒連線／ssh 指令本身失敗都回 `transcript_missing`
/// ——連不上就假裝已經搬過去，比直接開新對話更糟（會讓 `--resume` 帶著錯的期待送出去）。
async fn stage_cross_identity_transcript_remote(app: &Arc<App>, bot: &db::Bot, host: &str, transcript: &str) -> Result<(), &'static str> {
    // 形狀不對（不是 projects/<key>/<id>.jsonl）就不碰 ssh：路徑是 DB 記的字串，不能拿來叫遠端複製任意檔案。
    if !super::transcript_stage::has_claude_transcript_shape(transcript) {
        tracing::warn!(bot = %bot.name, host, transcript, "identity switch：遠端來源不是 projects/<cwd>/<id>.jsonl，不複製，改開新對話");
        return Err("transcript_missing");
    }
    let src = std::path::Path::new(transcript);
    let (Some(cwd_dir), Some(fname)) = (src.parent(), src.file_name()) else { return Err("transcript_missing") };
    let Some(old_projects) = cwd_dir.parent() else { return Err("transcript_missing") };
    let Some(cwd_key) = cwd_dir.file_name() else { return Err("transcript_missing") };
    let Some(fence) = app.hosts.fence(host).await else {
        tracing::warn!(bot = %bot.name, host, "identity switch：主機沒連線，改開新對話");
        return Err("transcript_missing");
    };
    let new_dir = match identity_config_dir_for_fence(app, host, bot.identity.as_deref(), &fence).await {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(bot = %bot.name, host, error = %e, "identity switch：無法確認遠端身分目錄，改開新對話");
            return Err("transcript_missing");
        }
    };
    let new_projects = std::path::Path::new(&new_dir).join("projects");
    let script = remote_stage_script(
        &old_projects.to_string_lossy(),
        &new_projects.to_string_lossy(),
        &cwd_key.to_string_lossy(),
        &fname.to_string_lossy(),
        src.file_stem().map(|s| s.to_string_lossy().into_owned()).as_deref(),
    );
    let Some(result) = app.hosts.run_if_current(&fence, fence.conn().ssh_exec(&script)).await else {
        tracing::warn!(bot = %bot.name, host, "identity switch：搬檔前主機權威已變更，改開新對話");
        return Err("transcript_missing");
    };
    let out = match result {
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
    let companion = stem.map(remote_companion_script).unwrap_or_default();
    format!(
        r#"set -e
umask 077
OLD={old}
NEW={new}
K={key}
N={name}
T=
DT=
cleanup() {{ [ -z "$T" ] || rm -f "$T"; [ -z "$DT" ] || rm -f "$DT"; }}
trap cleanup EXIT HUP INT TERM
am_missing() {{ printf 'AM_MISSING\n'; exit 0; }}
am_mkdir_failed() {{ printf 'AM_MKDIR_FAILED\n'; exit 0; }}
am_copy_failed() {{ printf 'AM_COPY_FAILED\n'; exit 0; }}
# 第二個參數是 `/dev/fd/N`（已開啟的檔案）時，BSD（macOS）的 `stat` 回的是 devfs 的裝置編號、不是檔案所在的磁碟，
# 跟路徑那邊永遠對不上——2026-10-04 m4p 十顆 claude bot 更新 Claude 重啟時全部被當成「紀錄檔不見」、開了新對話。
# 所以 fd 一律用 `fstat` 讀（python3，跟下面附屬目錄複製同一個相依）；GNU 的 `/dev/fd/N` 是指向真檔的連結，`-ef` 照舊。
am_fd_is() {{
  python3 - "$@" <<'AM_FD_PY'
import os, sys
fd = int(sys.argv[2])
st = os.fstat(fd)
if sys.argv[1] == "nlink1":
    sys.exit(0 if st.st_nlink == 1 else 1)
p = os.stat(sys.argv[3])
sys.exit(0 if (p.st_dev, p.st_ino) == (st.st_dev, st.st_ino) else 1)
AM_FD_PY
}}
am_same() {{
  if stat -c %Y "$1" >/dev/null 2>&1; then [ "$1" -ef "$2" ]; return; fi
  case "$2" in /dev/fd/[0-9]*) am_fd_is same "${{2#/dev/fd/}}" "$1"; return;; esac
  a=$(stat -L -f '%d %i' "$1" 2>/dev/null) || return 1; b=$(stat -L -f '%d %i' "$2" 2>/dev/null) || return 1; [ "$a" = "$b" ]
}}
am_singlelink() {{
  if stat -L -c %h "$1" >/dev/null 2>&1; then l=$(stat -L -c %h "$1" 2>/dev/null) || return 1; [ "$l" = 1 ]; return; fi
  case "$1" in /dev/fd/[0-9]*) am_fd_is nlink1 "${{1#/dev/fd/}}"; return;; esac
  l=$(stat -L -f %l "$1" 2>/dev/null) || return 1
  [ "$l" = 1 ]
}}
am_size() {{
  if stat -L -c %s "$1" >/dev/null 2>&1; then stat -L -c %s "$1"
  else stat -L -f %z "$1"; fi
}}
case "$K" in ''|.|..|*/*|*[[:cntrl:]]*) am_missing;; esac
case "$N" in ''|.|..|*/*|*[[:cntrl:]]*) am_missing;; esac
case "$N" in *.jsonl) ;; *) am_missing;; esac
[ ! -L "$OLD" ] && [ -d "$OLD" ] || am_missing
cd -- "$OLD" 2>/dev/null || am_missing
[ ! -L "$OLD" ] && am_same "$OLD" . || am_missing
OLD_REAL=$(pwd -P)
[ ! -L "$K" ] && [ -d "$K" ] || am_missing
cd -- "$K" 2>/dev/null || am_missing
[ ! -L "$OLD/$K" ] && am_same "$OLD/$K" . || am_missing
[ ! -L "$N" ] && [ -f "$N" ] || am_missing
exec 3< "$N" || am_missing
am_same "$N" /dev/fd/3 && am_singlelink /dev/fd/3 || am_missing
SRC_DIR=$OLD/$K
SRC_NAME=$N
exec 5< . || am_missing
am_same "$SRC_DIR" /dev/fd/5 || am_missing
if [ -d "$NEW" ]; then
  NEW_REAL=$(cd -- "$NEW" 2>/dev/null && pwd -P) || am_missing
  [ "$NEW_REAL" = "$OLD_REAL" ] && {{ printf 'AM_SAME\n'; exit 0; }}
fi
mkdir -p "$NEW" 2>/dev/null || am_mkdir_failed
[ ! -L "$NEW" ] && [ -d "$NEW" ] || am_mkdir_failed
cd -- "$NEW" 2>/dev/null || am_mkdir_failed
if [ -L "$NEW" ]; then
  NEW_REAL=$(pwd -P)
  [ "$NEW_REAL" = "$OLD_REAL" ] && {{ printf 'AM_SAME\n'; exit 0; }}
  am_mkdir_failed
fi
am_same "$NEW" . || am_mkdir_failed
if [ -L "$K" ]; then am_mkdir_failed; fi
if [ ! -e "$K" ]; then mkdir -- "$K" 2>/dev/null || am_mkdir_failed; fi
[ -d "$K" ] && [ ! -L "$K" ] || am_mkdir_failed
cd -- "$K" 2>/dev/null || am_mkdir_failed
[ ! -L "$NEW/$K" ] && am_same "$NEW/$K" . || am_mkdir_failed
DEST=$N
exec 6< . || am_mkdir_failed
am_same "$NEW/$K" /dev/fd/6 || am_mkdir_failed
T=$(mktemp .stage-XXXXXX) || am_copy_failed
if ! cat <&3 > "$T"; then am_copy_failed; fi
chmod 600 "$T" || am_copy_failed
s=$(am_size "$T") || am_copy_failed
replace_old=
preserve_old=
if [ -e "$DEST" ] || [ -L "$DEST" ]; then
  [ ! -L "$DEST" ] && [ -f "$DEST" ] || am_missing
  exec 4< "$DEST" || am_missing
  am_same "$DEST" /dev/fd/4 && am_singlelink /dev/fd/4 || am_missing
  DT=$(mktemp .stage-dest-XXXXXX) || am_copy_failed
  if ! cat <&4 > "$DT"; then am_copy_failed; fi
  chmod 600 "$DT" || am_copy_failed
  d=$(am_size "$DT") || am_copy_failed
  if [ "$d" -ge "$s" ] && head -c "$s" "$DT" | cmp -s - "$T"; then printf 'AM_STAGED\n'; exit 0; fi
  replace_old=1
  if [ "$d" -lt "$s" ] && head -c "$d" "$T" | cmp -s - "$DT"; then preserve_old=; else preserve_old=1; fi
fi
BACKUP=
if [ -n "$replace_old" ]; then
  stamp=$(date +%s)
  BACKUP="$DEST.replaced-$stamp-$$"
  [ ! -e "$BACKUP" ] && [ ! -L "$BACKUP" ] || BACKUP="$DEST.replaced-$stamp-$$-1"
  mv "$DEST" "$BACKUP" || am_copy_failed
fi
if ! ln "$T" "$DEST"; then
  if [ -n "$BACKUP" ] && [ ! -e "$DEST" ] && [ ! -L "$DEST" ]; then mv "$BACKUP" "$DEST" || true; fi
  am_copy_failed
fi
rm -f "$T"
T=
if [ -n "$BACKUP" ] && [ -z "$preserve_old" ]; then rm -f "$BACKUP"; fi
{companion}printf 'AM_STAGED\n'
"#,
        old = sh_quote(old_projects),
        new = sh_quote(new_projects),
        key = sh_quote(cwd_key),
        name = sh_quote(fname),
        companion = companion,
    )
}

fn remote_companion_script(stem: &str) -> String {
    format!(
        r#"STEM={stem}
if [ ! -e "$STEM" ] && [ ! -L "$STEM" ]; then
  C=$(mktemp -d .companion-stage-XXXXXX) || C=
  if [ -n "$C" ] && python3 - "$STEM" "$C" <<'AM_COMPANION_PY'
import errno, os, secrets, stat, sys
stem, temp = sys.argv[1:]
srcbase, dstbase = 5, 6
src = os.open(stem, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0), dir_fd=srcbase)
dst = os.open(temp, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0), dir_fd=dstbase)
def copy_dir(source, target):
    for name in os.listdir(source):
        try:
            before = os.stat(name, dir_fd=source, follow_symlinks=False)
        except OSError:
            continue
        if stat.S_ISDIR(before.st_mode):
            try:
                os.mkdir(name, 0o700, dir_fd=target)
                s = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=source)
                d = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=target)
            except OSError:
                continue
            opened = os.fstat(s)
            if (opened.st_dev, opened.st_ino) == (before.st_dev, before.st_ino):
                copy_dir(s, d)
            os.close(s); os.close(d)
        elif stat.S_ISREG(before.st_mode) and before.st_nlink == 1:
            try:
                s = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=source)
            except OSError:
                continue
            opened = os.fstat(s)
            if not stat.S_ISREG(opened.st_mode) or opened.st_nlink != 1 or (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
                os.close(s); continue
            temp_name = ".am-" + secrets.token_hex(16)
            try:
                out = os.open(temp_name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=target)
                while True:
                    data = os.read(s, 1024 * 1024)
                    if not data: break
                    view = memoryview(data)
                    while view:
                        n = os.write(out, view)
                        view = view[n:]
                os.fsync(out); os.close(out)
                try: os.link(temp_name, name, src_dir_fd=target, dst_dir_fd=target, follow_symlinks=False)
                except FileExistsError: pass
            finally:
                try: os.unlink(temp_name, dir_fd=target)
                except OSError: pass
                os.close(s)
copy_dir(src, dst)
os.fsync(dst)
os.close(src); os.close(dst)
AM_COMPANION_PY
  then
    if python3 - "$C" "$STEM" <<'AM_COMPANION_RENAME_PY'
import os, sys
temp, name = sys.argv[1:]
try: os.stat(name, dir_fd=6, follow_symlinks=False)
except FileNotFoundError: os.rename(temp, name, src_dir_fd=6, dst_dir_fd=6)
else: raise SystemExit(1)
AM_COMPANION_RENAME_PY
    then C=; fi
  fi
  [ -z "$C" ] || rm -rf "$C"
fi
"#,
        stem = sh_quote(stem),
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
        &format!("{CONTEXT_LOST_PREFIX}（{reason}），已經開了新的對話——前面的脈絡沒有帶過來。"),
        "system",
        false,
        None,
    )
    .await;
    Ok(())
}

/// [`context_lost`] 那則通知的開頭；[`retire_context_lost`] 靠它認出要撤的訊息。
pub(crate) const CONTEXT_LOST_PREFIX: &str = "⚠️ 接不回原本的對話";

/// 之後真的接回了 `session_id` 那段對話（resume 驗證為 `verified`）：那段對話中斷後插的「接不回」通知已經不是事實，撤掉
/// （2026-10-04 使用者：「接回之後不需要再提示原本的失敗」——m4p 十顆 bot 修好後接回，聊天室還掛著前一晚的警告）。
/// 範圍是「最後一個用過這段 session 的**其他** run 結束之後」插的通知；從沒用過就不動。刪了就發 `resync`，前端重抓。
pub(crate) async fn retire_context_lost(app: &Arc<App>, bot_id: &str, current_run: &str, session_id: &str) {
    let ended: Option<String> = match sqlx::query_scalar(
        "SELECT ended_at FROM runs WHERE bot_id = ? AND id <> ? AND native_session_id = ? AND ended_at IS NOT NULL
          ORDER BY started_at DESC, rowid DESC LIMIT 1",
    )
    .bind(bot_id)
    .bind(current_run)
    .bind(session_id)
    .fetch_optional(&app.db)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = %e, "could not look up the resumed session's last run");
            return;
        }
    };
    let Some(ended) = ended else { return };
    let Ok(conv) = db::conversation_id(&app.db, bot_id).await else { return };
    let deleted = sqlx::query(
        "DELETE FROM messages WHERE conversation_id = ? AND role = 'system' AND source = 'system' AND created_at >= ?
            AND substr(content, 1, length(?)) = ?",
    )
    .bind(&conv)
    .bind(&ended)
    .bind(CONTEXT_LOST_PREFIX)
    .bind(CONTEXT_LOST_PREFIX)
    .execute(&app.db)
    .await;
    match deleted {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::info!(bot = %bot_id, session = %session_id, n = r.rows_affected(), "resumed the lost conversation; retired its context-lost notes");
            app.emit("resync", json!({"reason": "context_lost_retired", "bot_id": bot_id})).await;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(bot = %bot_id, error = %e, "could not retire context-lost notes"),
    }
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
    session: &str,
    opts: StartOpts,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<()> {
    let host = project.host.clone();
    let client = match fence {
        Some(fence) => app
            .herdr_for_host_fence(fence, session)
            .await
            .ok_or_else(|| host_superseded(&bot.id, &host))?,
        None => app
            .herdr_for_session(&host, session)
            .await
            .ok_or_else(|| LcError::Upstream(format!("Herdr session `{session}` for host `{host}` is not configured")))?,
    };
    let connected = match fence {
        Some(fence) => app.session_connected_with_host_fence(fence, session).await,
        None => app.session_connected(&host, session).await,
    };
    if !connected {
        return Err(LcError::Upstream(format!("host `{host}` is not connected")));
    }
    // 1b. preflight: a missing CLI would sit in `launch_pending` for the full 60 s silently.
    if let Err(reason) = ensure_kind_installed(app, &host, &bot.kind).await {
        let conv = db::conversation_id(&app.db, &bot.id).await.map_err(up)?;
        let _ = insert_message(app, &conv, None, "system", &reason, "system", false, None).await;
        return Err(LcError::Bad(reason));
    }
    let agent = crate::config::agent_name(&project.label, &bot.id);
    // 分享用的受限 bot（SPEC「分享 bot」）：沒有 shim（不能開子 agent）、env 收窄、不讀 agent md／CLAUDE.md、argv 換成籠子那一套。
    let restricted = crate::share::cage::prepare(app, bot, &host).await?;
    let shim_dir = if restricted.is_some() { None } else { install_shim(app, bot, project).await };
    let env = match fence {
        Some(fence) => pane_env_for_fence(app, bot, &host, run_id, &agent, shim_dir.as_deref(), fence).await,
        None => pane_env(app, bot, &host, run_id, &agent, shim_dir.as_deref()).await,
    }
    .map_err(up)?;
    let env = match &restricted {
        Some(_) => {
            let mut caged = env;
            crate::share::cage::cage_env(&mut caged, &crate::share::cage::identity_env(app, bot).await, &crate::share::cage::local_home());
            caged
        }
        None => env,
    };
    // §6.5i：agent md 讀一次，母 bot 的 persona 與子 agent 的檔用同一份。
    let agent_md = if restricted.is_some() { Default::default() } else { super::agent_md::load(app, project).await };
    if !agent_md.problems.is_empty() {
        let reason = format!("agent md 有問題，這次啟動沒帶到：{}", agent_md.problems.join("；"));
        tracing::warn!(bot = %bot.name, "{reason}");
        if let Ok(conv) = db::conversation_id(&app.db, &bot.id).await {
            let _ = insert_message(app, &conv, None, "system", &reason, "system", false, None).await;
        }
    }
    let mut env = env;
    if agent_md.configured {
        // 這個專案有 agent md：claude 一律不讀任何 CLAUDE.md（帳號層與 repo 都是）。不分 kind 都設——codex／grok bot
        // 開出來的 claude 子 agent 也繼承（shim 的保留清單帶下去）。沒設定的專案不動，CLI 照舊讀自己的檔。
        if let Some(map) = env.as_object_mut() {
            map.insert("CLAUDE_CODE_DISABLE_CLAUDE_MDS".into(), serde_json::json!("1"));
        }
    }
    // Grok's `--rules` accepts only a string, while Herdr trims every launch to 900 bytes to stay
    // below the PTY shell limit. Stage the full startup persona in a file and pass its short path.
    // Grok also needs the child-rules file when no custom [agents] document is configured.
    let child_md = super::agent_md::compose(
        &super::setup::child_agent_rules(&agent),
        agent_md.configured.then_some(agent_md.text.as_str()).unwrap_or(""),
    );
    let child_instructions_path = if agent_md.configured || bot.kind == "grok" {
        super::agent_md::install(app, bot, project, shim_dir.as_deref(), &child_md).await
    } else {
        None
    };
    let child_instructions_ready = child_instructions_path.is_some();
    if let Some(path) = child_instructions_path {
        if let Some(map) = env.as_object_mut() {
            map.insert("AM_INSTRUCTIONS_FILE".into(), serde_json::json!(path));
        }
    }
    let grok_rules_file = if bot.kind == "grok" {
        let full_persona = super::setup::persona_text(bot, &agent, agent_md.configured.then_some(agent_md.text.as_str()));
        super::agent_md::install_grok_rules(app, bot, project, shim_dir.as_deref(), &full_persona).await
    } else {
        None
    };
    if bot.kind == "grok" && (!child_instructions_ready || grok_rules_file.is_none()) {
        let reason = "無法把 Grok 的完整啟動指示寫進 bot 目錄；拒絕以遭截斷的 --rules 啟動";
        tracing::error!(bot = %bot.name, "{reason}");
        if let Ok(conv) = db::conversation_id(&app.db, &bot.id).await {
            let _ = insert_message(app, &conv, None, "system", reason, "system", false, None).await;
        }
        return Err(LcError::Upstream(reason.into()));
    }
    // SPEC §6.5c: claude learns herdr from a skill (the CLI's own doc), not the persona.
    if restricted.is_none() {
        install_herdr_skill(app, bot, project, &env, &agent).await;
    }

    // Remote hook injection may ssh-upload, so it must happen before workspace/tab creation.
    let injected = injected_args(app, bot, project, &env).await.map_err(up)?;
    let mut args = injected;
    if let Some(ws) = &restricted {
        // 受限 bot 不吃 bot／身分自訂的 args（那是加 `--dangerously-skip-permissions` 之類旗標的地方）。
        let prompt = crate::share::cage::install_prompt(app, bot, ws, &env).map_err(up)?;
        args.extend(crate::share::cage::launch_args(&env, &prompt));
        args.extend(model_args(&effort_checked(app, bot, &project.host).await));
    } else {
        args.extend(persona_args(bot, &agent, agent_md.configured.then_some(agent_md.text.as_str()), grok_rules_file.as_deref()));
        args.extend(model_args(&effort_checked(app, bot, &project.host).await));
        args.extend(identity_args(app, bot, &project.host).await);
        args.extend(bot.args());
    }

    // Reopen only: resolve the previous native session after preflight. The requested id is
    // persisted before `agent.start`; hookrecv uses it to detect a provider that ignored resume.
    let resume = if opts.resume_native {
        match native_resume_plan(app, bot, &host, false, opts.resume_session.as_deref()).await? {
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
        if bot.kind == "claude" {
            crate::rewind::anchor::ensure(app, from).await;
        }
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
    args.extend(codex_pane_guard_args(&bot.kind));

    let cwd = restricted.as_deref().unwrap_or_else(|| bot_cwd(bot, project));
    // A new dir opens on "trust this project?" with the cursor on *No*: claude quits, codex eats
    // the first message. Record trust first (local, only when not yet trusted).
    // 遠端也要（#407）：換身分＝換一個從沒信任過的設定目錄，不寫的話每次都停在提示上等人按。
    for w in crate::trust::pretrust_for_start(app, bot, &project.host, cwd).await {
        tracing::warn!(bot = %bot.name, cwd, warning = %w, "could not pre-trust the working directory");
    }

    // 2. workspace
    // `(root pane, 它開在哪個目錄)`：bot 有自己的 cwd 時不能沿用開在專案目錄的 root（見 `acquire_run_pane`）。
    let mut fresh_root: Option<(crate::herdr::PaneInfo, &str)> = None;
    // `projects.workspace_id` is the configured session's; an imported bot in `default` must not
    // overwrite it (the next reconcile would clear it).
    let workspace_id = match (session != "default", project.workspace_id.as_deref()) {
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
    // #742：全新對話起的 claude run 畫面上不會有舊的 `/model`、`/effort` 確認，第一輪巡邏前使用者真的切了也要採用。
    // 接回、分支（argv 帶 `--resume`／`--continue`）會把舊對話印回來，那種維持「第一次看到只當基準」。
    if bot.kind == "claude" && !args.iter().any(|a| matches!(a.as_str(), "--resume" | "-r" | "--continue" | "-c")) {
        crate::claude_live::start_fresh(run_id);
    }
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
    #[cfg(test)]
    let mut first_agent_start = true;
    for attempt in 0..10u32 {
        #[cfg(test)]
        if first_agent_start {
            super::race_point::hit("restart_before_agent_start", &bot.id).await;
        }
        #[cfg(test)]
        {
            first_agent_start = false;
        }
        if let Some(fence) = fence {
            if host != fence.conn().name || !app.hosts.is_current(fence).await {
                pane_guard.cleanup().await;
                return Err(host_superseded(&bot.id, &host));
            }
        }
        let launch = match fence {
            Some(fence) => match app.hosts.run_if_current(fence, client.agent_start(&agent, &bot.kind, &pane_id, &args, 60_000)).await {
                Some(result) => result,
                None => {
                    pane_guard.cleanup().await;
                    return Err(host_superseded(&bot.id, &host));
                }
            },
            None => client.agent_start(&agent, &bot.kind, &pane_id, &args, 60_000).await,
        };
        match launch {
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
    crate::events::watch_pane_on_session(app, &host, session, &pane_id).await;

    // 7. wait for readiness
    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    let status = match client.agent_wait_ready(&bot.kind, &agent, &until, 60_000).await {
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
    // 不補 `/effort`：grok 1.0.46 會把該 slash 寫進 config.toml 的 default_reasoning_effort。
    // 這一輪的等級只靠上面 argv 的 `--reasoning-effort`。
    let _ = super::apply_grok_startup_effort(app, &bot, run_id, &pane_id, &client).await;
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
        // could not probe → do not block the start
        crate::hosts::sh_local_stdout(&probe, Duration::from_secs(10), "kind preflight").await.ok().map(|o| o.trim().to_string())
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


#[cfg(test)]
pub async fn restart_bot(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    restart_bot_with(app, bot_id, StartOpts::default()).await
}

/// Stop + start under one hold of the bot's lock. Two holds let a reconcile adopt the just-stopped
/// agent in between (2026-09-10 23:02, `restart-idle`: AGM + three bots down 5.5 h).
/// If a run whose pane is gone still blocks the start, it is ended and the start retried once.
pub async fn restart_bot_with(app: &Arc<App>, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    restart_bot_with_authority(app, bot_id, opts, None).await
}

/// The scoped update restart must still belong to the host connection that installed the update.
/// Recheck after taking the bot lock so a queued restart cannot resolve a repointed host name.
pub(crate) async fn restart_bot_with_host_fence(
    app: &Arc<App>,
    bot_id: &str,
    opts: StartOpts,
    fence: &crate::hosts::HostFence,
) -> LcResult<String> {
    restart_bot_with_authority(app, bot_id, opts, Some(fence)).await
}

async fn restart_bot_with_authority(
    app: &Arc<App>,
    bot_id: &str,
    opts: StartOpts,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    if let Some(fence) = fence {
        ensure_current_host_fence(app, bot_id, fence).await?;
    }
    // Checked before the stop, or the user's agent gets ctrl+c for nothing.
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if let Some(fence) = fence {
        ensure_current_host_fence(app, bot_id, fence).await?;
    }
    refuse_default_session(&bot)?;
    // 子 agent 的 pane 是父 agent 開的，daemon 重建不了它的環境：這條 stop + start 會把 pane 關掉再開一個不一樣的（#188）。
    // 走哪條路由呼叫端先分類，分錯或誤呼不能靠約定——鎖裡讀到 child 就拒絕，什麼都還沒停。
    if bot.managed_by == "child" {
        return Err(LcError::conflict(
            "child_restarts_in_pane",
            json!({"bot_id": bot_id, "message": "子 agent 的 pane 是父 agent 開的，不能 stop + start：改走 restart_child_in_pane（原 pane 裡 exit + resume）。"}),
        ));
    }
    if opts.require_idle {
        if let Some(why) = busy_reason_locked(app, bot_id, opts.refuse_background_jobs).await? {
            return Err(not_idle(bot_id, &why));
        }
    }
    // 停之前就確定接得回，免得 ctrl+c 掉之後才發現只能開新對話。
    if opts.resume_native && opts.resume_required {
        let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
        if let Err(why) = native_resume_plan(app, &bot, &host, true, opts.resume_session.as_deref()).await? {
            return Err(cannot_resume(bot_id, why));
        }
    }
    let stopping = db::active_run(&app.db, bot_id).await.map_err(up)?.map(|r| r.id);
    let nudge = super::resume_nudge::busy_before_restart(app, &bot, &opts).await;
    // 持久 intent（#355 P2）：在第一個不可逆步驟（記 `stopping`）**之前**先 commit，daemon 在 stop 與 start 之間死掉的話，
    // 開機由 `restart_intents::recover_host` 往前補完。寫不進去＝什麼都還沒動，不能開始（fail closed）。
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    if let Some(fence) = fence {
        ensure_current_host_fence(app, bot_id, fence).await?;
    }
    let payload = json!({"opts": opts, "from_run_id": stopping, "bot_name": bot.name});
    let intent_id = crate::intents::prepare_restart(&app.db, bot_id, &host, &payload, RESTART_INTENT_TTL_SECS, &app.boot_id)
        .await
        .map_err(|e| LcError::Upstream(format!("cannot record the restart intent: {e:#}")))?;
    #[cfg(test)]
    super::race_point::hit("restart_after_intent", bot_id).await;
    let res = restart_stop_and_start(app, bot_id, opts, stopping, fence).await;
    settle_restart_intent(app, &intent_id, &res).await;
    // 不在這裡佔佇列。接回驗證且閒置滿 10 秒、尾巴仍是被砍的工具，才由 `resume_nudge` 送一次（#424）。
    if let (Ok(run_id), Some(busy)) = (&res, nudge) {
        super::resume_nudge::arm(app, &bot, run_id, busy).await;
    }
    res
}

async fn ensure_current_host_fence(app: &Arc<App>, bot_id: &str, fence: &crate::hosts::HostFence) -> LcResult<()> {
    let host = fence.conn().name.as_str();
    if !app.hosts.is_current(fence).await {
        return Err(LcError::conflict("host_superseded", json!({"bot_id": bot_id, "host": host})));
    }
    if db::bot_host(&app.db, bot_id).await.map_err(up)? != host || !app.hosts.is_current(fence).await {
        return Err(LcError::conflict("host_superseded", json!({"bot_id": bot_id, "host": host})));
    }
    Ok(())
}

/// 重啟的 intent 放置多久還沒補完就放棄（`failed`＋通知）。
const RESTART_INTENT_TTL_SECS: i64 = 15 * 60;

/// handler 還活著時的收尾：成功／bot 回來了＝`done`；什麼都沒動的拒絕＝`abandoned`；其他失敗＝`failed`（**不推 AGM**：呼叫端已經拿到錯誤）。
/// 標不成也不影響結果：intent 留著開機時會被驗證世界後收掉（bot 已經在跑就 `done`）。
async fn settle_restart_intent(app: &Arc<App>, intent_id: &str, res: &LcResult<String>) {
    let out = match res {
        Ok(_) => crate::intents::complete(&app.db, intent_id).await,
        // 新 agent 起來了、只是 `running` 還沒記下（#145）：bot 回來了。
        Err(LcError::Uncommitted(v)) if v.get("start_error").is_none() => crate::intents::complete(&app.db, intent_id).await,
        Err(LcError::Conflict(v)) if matches!(v.get("reason").and_then(|r| r.as_str()), Some("not_idle" | "no_longer_idle")) => {
            crate::intents::abandon(&app.db, intent_id, "refused before anything was stopped").await
        }
        Err(e) => crate::intents::fail(&app.db, intent_id, &format!("{e:?}")).await,
    };
    if let Err(e) = out {
        tracing::warn!(intent = intent_id, error = %e, "could not settle the restart intent; boot recovery will verify it");
    }
}

/// 停 → 起（原本 `restart_bot_with` 鎖裡的後半段，行為不變）。
async fn restart_stop_and_start(
    app: &Arc<App>,
    bot_id: &str,
    opts: StartOpts,
    stopping: Option<String>,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<String> {
    // 停到起之間沒有 active run，但 bot 馬上就回來：排著的派工不是孤兒（issue #106，`restart_hold`）。
    let restarting = super::restart_hold::begin(bot_id);
    if let Some(fence) = fence {
        ensure_current_host_fence(app, bot_id, fence).await?;
    }
    let stopped = match (opts.require_idle, fence) {
        (true, Some(fence)) => super::stop::stop_for_restart_if_idle_locked_with_host_fence(app, bot_id, opts.refuse_background_jobs, fence).await,
        (false, Some(fence)) => super::stop::stop_for_restart_locked_with_host_fence(app, bot_id, fence).await,
        (true, None) => super::stop::stop_for_restart_if_idle_locked(app, bot_id, opts.refuse_background_jobs).await,
        (false, None) => stop_for_restart_locked(app, bot_id).await,
    };
    match stopped {
        Ok(_) => {}
        // 鎖裡看過閒置之後、記 `stopping` 之前它開始忙了（#346）：什麼都沒動，照 `busy` 對回 `not_idle`。
        Err(LcError::Conflict(v)) if v.get("reason").and_then(|r| r.as_str()) == Some("no_longer_idle") => {
            let why = busy_reason_locked(app, bot_id, opts.refuse_background_jobs).await.ok().flatten().unwrap_or_else(|| "working".to_string());
            return Err(not_idle(bot_id, &why));
        }
        Err(e) => return Err(e),
    }
    #[cfg(test)]
    super::race_point::hit("restart_after_stop", bot_id).await;
    if let Some(fence) = fence {
        ensure_current_host_fence(app, bot_id, fence).await?;
    }
    let started = restart_start(app, bot_id, opts, fence).await;
    drop(restarting);
    match started {
        // 排著的交給新的 run：`--resume` 起的 claude 由 `resume_gate` 等驗證，其他照常送。
        Ok(run_id) => {
            schedule_flush_queued(app, bot_id);
            Ok(run_id)
        }
        // 新 agent 起來了，只是 `running` 還沒記下（#145）：bot 回來了，不是沒開回來。對帳重試收成 running 時不叫 flush，
        // 它起來時的 idle 邊又早在 `starting` 就過了：等它收斂再叫（#165，同 start_send 的 #152）。
        Err(LcError::Uncommitted(v)) => {
            if let Some(run_id) = v.get("run_id").and_then(|r| r.as_str()) {
                super::start_send::flush_once_running(app, bot_id, run_id);
            }
            Err(LcError::Uncommitted(v))
        }
        Err(e) => {
            let unlabelled = match stopping.as_deref() {
                Some(run_id) => left_down_by_restart(app, bot_id, run_id).await.err().map(|db_err| (run_id, db_err)),
                None => None,
            };
            // 沒開回來：這下排著的才真的沒有人會送。
            revoke_orphaned_queued_turns(app, bot_id, "重啟之後沒能把 bot 開回來").await;
            match unlabelled {
                // 舊 run 還記著 `stopped`（「使用者要它停」）：不能只回 start 的錯，那等於默認它是故意停的（#146 重開 B）。
                Some((run_id, db_err)) => {
                    let mut err = LcError::uncommitted(
                        "restart_state_uncommitted",
                        run_id,
                        "重啟停掉了 bot、沒能開回來；舊 run 改不成 exited（還記著使用者要它停），已排重試",
                        db_err,
                    );
                    if let LcError::Uncommitted(v) = &mut err {
                        v["start_error"] = json!(format!("{e:?}"));
                    }
                    Err(err)
                }
                None => Err(e),
            }
        }
    }
}

/// 重啟停掉了 bot、卻沒能把它開回來（start 在前置檢查就失敗，連新的 run 都沒建）：剛停掉的那個 run 改記
/// `exited`。`stopped` 的意思是「使用者要它停」——incident 探針靠它分辨故意停的 bot，留著的話一顆
/// `autostart=1` 的 bot 從此沒在跑、卻永遠不開 `bot_stopped`，health 一直是綠的（review 2026-09-16 c1 L3）。
///
/// 走 `run_state::relabel`（跟 stop 的改標同一支）：寫不進去回錯並排重試（#146 重開 B），不再只看 `Ok(rows>0)`。
pub(crate) async fn left_down_by_restart(app: &Arc<App>, bot_id: &str, run_id: &str) -> Result<(), sqlx::Error> {
    match super::run_state::relabel(&app.db, run_id, "stopped", "exited").await {
        Ok(super::run_state::Moved::Applied) => {
            tracing::warn!(bot = bot_id, run = run_id, "restart stopped the bot but could not start it again; recorded as exited, not as a user stop");
            app.emit_bot_status(bot_id).await;
            Ok(())
        }
        Ok(super::run_state::Moved::Lost) => Ok(()),
        Err(e) => {
            super::run_state::schedule_settle(app, run_id, super::run_state::Settle::Relabel { from: "stopped", to: "exited" });
            Err(e)
        }
    }
}

pub(crate) async fn restart_start(
    app: &Arc<App>,
    bot_id: &str,
    opts: StartOpts,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<String> {
    match start_bot_locked_with_host_fence(app, bot_id, opts.clone(), fence).await {
        Err(LcError::Conflict(v)) if v.get("reason").and_then(|r| r.as_str()) == Some("active run already exists") => {
            let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? else {
                return start_bot_locked_with_host_fence(app, bot_id, opts.clone(), fence).await;
            };
            let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
            if run_alive(app, &run, &bot).await {
                return Err(LcError::Conflict(v));
            }
            tracing::warn!(bot = %bot.name, run = %run.id, pane = ?run.pane_id, "restart found a run with no live pane in its way; ending it and starting again");
            mark_run_exited(app, &run.id, "its pane was gone when the bot restarted").await;
            start_bot_locked_with_host_fence(app, bot_id, opts, fence).await
        }
        other => other,
    }
}

/// herdr `agent.start` 回「名字被占」：原 pane 的舊 agent 還在（狀態 Done、名字沒釋放）。
#[cfg(test)]
fn agent_name_taken(e: &anyhow::Error) -> bool {
    if let Some(h) = e.downcast_ref::<crate::herdr::HerdrError>() {
        return h.code == "agent_name_taken";
    }
    e.to_string().contains("agent_name_taken")
}

/// 子 agent 原地重啟（SPEC §6.5a / §6.9）：在它自己的 pane 裡 `ctrl+c` 收掉、**不關 pane**，
/// 同名 `agent.start --resume <上一個 session>`。pane 是父 agent 開的，且 shell 裡的環境
/// （`CLAUDE_CONFIG_DIR`、shim）daemon 重建不了。沒注入 hook，回覆照舊走終端快照。
/// 過程中 pane 不見了就不重開。
#[cfg(test)]
pub async fn restart_child_in_pane(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    restart_child_in_pane_with(app, bot_id, false).await
}

/// 同上；`require_idle` 見 [`StartOpts::require_idle`]。
#[cfg(test)]
pub async fn restart_child_in_pane_with(app: &Arc<App>, bot_id: &str, require_idle: bool) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    if require_idle {
        if let Some(why) = busy_reason_locked(app, bot_id, false).await? {
            return Err(not_idle(bot_id, &why));
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

    // A concurrent reconcile can observe the gap after the old agent exits but before agent.start
    // replies. Guard that hand-off; agent_name_taken below upgrades the temporary guard permanently.
    crate::child_reconcile_safety::record_retirement_grace(&app.db, bot_id)
        .await
        .map_err(|e| LcError::Upstream(format!("cannot record child restart grace: {e:#}")))?;

    // 鎖裡看過閒置、還沒記 `stopping` 的那一瞬（測試在這裡讓使用者剛好開始打字）。
    #[cfg(test)]
    super::race_point::hit("child_restart_before_stopping", bot_id).await;
    if require_idle {
        if let Some(n) = crate::background_jobs::known(app, &run.id).filter(|n| *n > 0) {
            return Err(LcError::conflict(
                "not_idle",
                json!({"bot_id": bot_id, "busy": "background_jobs", "background_jobs": n}),
            ));
        }
    }
    // 舊 run 跟 stop 走同一套（#146 留言）：先記「正在停」，記不下來就一步都不做；被 pane-exit 事件先收掉就不重開。
    // `require_idle`（一鍵重啟）時這一步同時是「還是閒著才准停」的許可（#346，同 `stop_for_restart_if_idle_locked`）。
    let moved = if require_idle {
        super::stop::admit_idle_stop(&app.db, &run.id, bot_id, true).await.map_err(up)?
    } else {
        super::run_state::transition(&app.db, &run.id, super::run_state::LIVE, "stopping", None).await.map_err(up)?
    };
    match moved {
        super::run_state::Moved::Applied => {}
        super::run_state::Moved::Lost if require_idle => {
            app.emit_bot_status(bot_id).await;
            let why = busy_reason_locked(app, bot_id, false).await.ok().flatten().unwrap_or_else(|| "working".to_string());
            return Err(not_idle(bot_id, &why));
        }
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

    let mut args = super::setup::permission_args(&bot.kind, bot.auto_approve != 0);
    // 模型／強度用 `bots` 上 §4.4a 從子 agent argv 讀回的；讀不到就讓 CLI 用預設。
    args.extend(model_args(&effort_checked(app, &bot, &host).await));
    args.extend(bot.args());
    let resume = match db::last_native_session(&app.db, bot_id).await.map_err(up)? {
        Some((sid, _)) => resume_args_by_kind(&bot.kind, &sid).ok().map(|a| (sid, a)),
        None => None,
    };
    if let (Some((sid, _)), "claude") = (resume.as_ref(), bot.kind.as_str()) {
        crate::rewind::anchor::ensure(app, sid).await;
    }
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
    args.extend(codex_pane_guard_args(&bot.kind));

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
            Err(e) if agent_name_taken(&e) => {
                // 舊 agent 還掛在原 pane（Done、名字沒釋放）：agent 其實還在，新 run 當成收編、記 running；
                // 不能留成「沒有 active run」——那樣 reconcile 會把子 agent 退役軟刪（2026-09-22 rollout：pvd／rh）。
                crate::child_reconcile_safety::hold_after_name_taken(&app.db, bot_id, &agent)
                    .await
                    .map_err(|write| LcError::Upstream(format!("herdr returned agent_name_taken, and the child retirement reason could not be recorded: {write:#}")))?;
                let _ = super::run_state::transition(&app.db, &run_id, &["starting"], "running", None).await;
                app.emit_bot_status(bot_id).await;
                tracing::warn!(bot = %bot.name, run = %run_id, "子 agent 重啟：herdr 說名字還被原 pane 的舊 agent 占著；當作沒重啟、保留 bot");
                return Err(LcError::conflict("agent_name_taken", json!({"bot_id": bot_id, "run_id": run_id, "message": "舊 agent 還在原 pane，沒有重啟；由父 bot 用 herdr 重開"})));
            }
            Err(e) => {
                crate::child_reconcile_safety::clear_retirement_grace(&app.db, bot_id)
                    .await
                    .map_err(|write| LcError::Upstream(format!("child restart failed and its grace could not be cleared: {write:#}")))?;
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
    if let Err(e) = crate::child_reconcile_safety::clear_after_successful_restart(&app.db, bot_id).await {
        tracing::warn!(bot = bot_id, error = %e, "child restart succeeded but its prior agent_name_taken hold could not be cleared");
    }
    if let Err(e) = crate::child_reconcile_safety::clear_retirement_grace(&app.db, bot_id).await {
        tracing::warn!(bot = bot_id, error = %e, "child restart succeeded but its hand-off grace could not be cleared");
    }
    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    match client.agent_wait_ready(&bot.kind, &agent, &until, 60_000).await {
        // 新 run 以 `unknown` 起頭、`running` 時沒帶狀態；閒著的 codex（herdr 0.9.2+）不會再有狀態事件，
        // 不把 ready 的結果記下來就一直停在 `unknown`。只補還是 `unknown` 的，不蓋掉期間來的事件。
        Ok(info) => {
            let ready = info.agent_status.normalized();
            if ready != AgentStatus::Unknown {
                if let Err(e) = sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ? AND agent_status = 'unknown'")
                    .bind(ready.as_str())
                    .bind(&run_id)
                    .execute(&app.db)
                    .await
                {
                    tracing::warn!(bot = %bot.name, run = %run_id, error = %e, "could not record the restarted child's ready status");
                }
            }
        }
        Err(e) => tracing::warn!(bot = %bot.name, error = %e, "子 agent 重啟後沒等到 ready，run 留著讓對帳接手"),
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

    /// 指名的 session 優先於 DB 記的那段（DB 記錯時的救援路徑，2026-09-22）：`--resume <指名的>`，
    /// `resume_session_id` 也是它，之後 SessionStart 回報的就拿它來對。
    #[tokio::test]
    async fn an_explicit_session_overrides_the_recorded_one() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "rescued").await;
        let plan = super::native_resume_plan(&e.app, &bot, crate::config::LOCAL_HOST, false, Some("246fcf93-real")).await.unwrap().unwrap();
        assert_eq!(plan.0, "246fcf93-real");
        assert!(plan.1.iter().any(|a| a == "246fcf93-real") && plan.1.iter().any(|a| a == "--resume"), "{:?}", plan.1);
        // DB 記了另一段（錯的）也不影響。
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, started_at, ended_at, native_session_id)
                     VALUES (?,?,'exited','idle','ws-1','p-old',?,?, '01a0b8c6-wrong')")
            .bind(db::ulid()).bind(&bot.id).bind(db::now()).bind(db::now()).execute(&e.app.db).await.unwrap();
        assert_eq!(super::native_resume_plan(&e.app, &bot, crate::config::LOCAL_HOST, false, Some("246fcf93-real")).await.unwrap().unwrap().0, "246fcf93-real");
        assert_eq!(super::native_resume_plan(&e.app, &bot, crate::config::LOCAL_HOST, false, None).await.unwrap().unwrap().0, "01a0b8c6-wrong");
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

    #[tokio::test]
    async fn codex_start_uses_an_inline_tui_and_its_own_app_server() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "codex").await;
        sqlx::query("UPDATE bots SET kind='codex', identity=NULL, model='gpt-5.6-sol', auto_approve=1 WHERE id=?")
            .bind(&bot.id)
            .execute(&e.app.db)
            .await
            .unwrap();

        start_bot(&e.app, &bot.id).await.unwrap();

        let args = started_args(&e).pop().unwrap();
        assert!(args.contains(&"--no-daemon".into()), "Codex must not reuse a shared app server: {args:?}");
        assert!(args.contains(&"--no-alt-screen".into()), "Codex must keep its screen readable: {args:?}");
        assert!(args.contains(&"--yolo".into()), "existing auto-approve args must be preserved: {args:?}");
        assert!(args.windows(2).any(|w| w == ["-c", "tui.show_tooltips=false"]), "Codex turn tips must stay off: {args:?}");
        stop_bot(&e.app, &bot.id).await.unwrap();

        // resume：子命令排最前面，guard 參數一樣要帶。
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
             VALUES (?,?,'stopped','idle','codex-previous','2026-09-07T00:00:00Z','2026-09-07T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .execute(&e.app.db)
        .await
        .unwrap();
        start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        assert_eq!(&args[..2], ["resume", "codex-previous"], "{args:?}");
        assert!(args.contains(&"--no-daemon".into()), "resumed Codex must not reuse a shared app server: {args:?}");
        assert!(args.windows(2).any(|w| w == ["-c", "tui.show_tooltips=false"]), "resumed Codex turn tips must stay off: {args:?}");
    }

    #[tokio::test]
    async fn grok_start_passes_a_short_rules_file_pointer_without_truncating_the_rules() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "grok-rules").await;
        let persona = "Keep this configured persona intact: $(echo GROK-PERSONA)";
        sqlx::query("UPDATE bots SET kind='grok', persona=? WHERE id=?").bind(persona).bind(&bot.id).execute(&e.app.db).await.unwrap();

        start_bot(&e.app, &bot.id).await.unwrap();

        let args = started_args(&e).pop().unwrap();
        let at = args.iter().position(|a| a == "--rules").expect("Grok gets --rules");
        let pointer = &args[at + 1];
        assert!(pointer.contains("grok-rules.md"), "the argv carries a file pointer, not a truncated prompt: {pointer}");
        assert!(pointer.len() < 300, "the directive fits Herdr's command-line budget: {}", pointer.len());

        let rules_file = e.app.data_dir.join("bots").join(&bot.id).join("grok-rules.md");
        let full_rules = std::fs::read_to_string(&rules_file).expect("the full instructions are staged beside the bot");
        assert!(full_rules.contains("需要開子任務或平行工作時"), "base rules survive outside the shell argv");
        assert!(full_rules.contains(persona), "configured persona survives outside the shell argv");
    }

    /// codex 0.160.0 的 resume 會恢復上次存的權限，除非明確覆寫（issue #778）：auto_approve 0／1 ×
    /// resume／fork 四種組合都要把權限明講，關掉 auto_approve 的不能因為接回 Full Access 的對話變成 yolo。
    #[tokio::test]
    async fn codex_resume_and_fork_pin_permissions_to_auto_approve() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "codex").await;
        sqlx::query("UPDATE bots SET kind='codex', identity=NULL WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
             VALUES (?,?,'stopped','idle','codex-previous','2026-09-07T00:00:00Z','2026-09-07T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .execute(&e.app.db)
        .await
        .unwrap();
        let sandbox = ["-c", "sandbox_mode=\"workspace-write\""];
        // First resume with Full Access, then turn auto_approve off and resume that same saved session again.
        for auto_approve in [1, 0] {
            sqlx::query("UPDATE bots SET auto_approve=? WHERE id=?").bind(auto_approve).bind(&bot.id).execute(&e.app.db).await.unwrap();
            let resume = StartOpts { resume_native: true, ..Default::default() };
            let fork = StartOpts { fork_session: Some("codex-source".into()), ..Default::default() };
            for (opts, head) in [(resume, ["resume", "codex-previous"]), (fork, ["fork", "codex-source"])] {
                start_bot_with(&e.app, &bot.id, opts).await.unwrap();
                let args = started_args(&e).pop().unwrap();
                stop_bot(&e.app, &bot.id).await.unwrap();
                assert_eq!(&args[..2], head, "{args:?}");
                let yolo = args.iter().filter(|a| *a == "--yolo").count();
                let pinned = args.windows(2).filter(|w| *w == sandbox).count();
                if auto_approve == 0 {
                    assert_eq!((yolo, pinned), (0, 1), "auto_approve=0 must pin a non-yolo sandbox: {args:?}");
                } else {
                    assert_eq!((yolo, pinned), (1, 0), "auto_approve=1 must stay yolo: {args:?}");
                }
            }
        }
    }

    #[test]
    fn permission_args_follow_auto_approve() {
        use super::super::setup::permission_args;
        assert_eq!(permission_args("codex", true), vec!["--yolo"]);
        assert_eq!(permission_args("codex", false), vec!["-c", "sandbox_mode=\"workspace-write\""]);
        assert_eq!(permission_args("claude", true), vec!["--dangerously-skip-permissions"]);
        assert_eq!(permission_args("grok", true), vec!["--always-approve"]);
        // claude 的非 auto 權限由 settings 的 permissions.defaultMode 釘（#722）；grok 沒有 resume 恢復權限的問題。
        assert!(permission_args("claude", false).is_empty());
        assert!(permission_args("grok", false).is_empty());
        assert!(permission_args("gemini", true).is_empty());
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

    /// 這個 bot 自己身分的 `projects/` 底下的 transcript 路徑（檔還沒寫）：身分的 config dir 就是它所在的那一份，
    /// 所以「換身分複製」判成同一份、不碰測試機器真正的 `~/.claude`。
    async fn own_transcript(e: &crate::testing::Env, bot: &db::Bot, name: &str) -> std::path::PathBuf {
        let dir = e.dir.join("own-cfg");
        let d = dir.to_str().unwrap().to_string();
        e.app
            .cfg
            .update(|c| {
                if !c.identities.iter().any(|i| i.name == "own") {
                    c.identities.push(crate::config::IdentityCfg {
                        name: "own".into(),
                        kind: "claude".into(),
                        host: None,
                        env: [("CLAUDE_CONFIG_DIR".to_string(), d.clone())].into(),
                        args: vec![],
                    });
                }
                Ok(())
            })
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET identity='own' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        let path = dir.join("projects/-own").join(format!("{name}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    /// #742：全新對話起的 claude run 記空基準（第一輪巡邏前的切換要採用）；`--resume` 接回的不記。
    #[tokio::test]
    async fn only_a_fresh_conversation_gets_an_empty_live_switch_baseline() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        let fresh = start_bot_with(&e.app, &pm.id, StartOpts::default()).await.unwrap();
        assert!(crate::claude_live::is_fresh(&fresh), "全新對話：空基準");

        let transcript = own_transcript(&e, &pm, "pm-742").await;
        std::fs::write(&transcript, "{}\n").unwrap();
        sqlx::query("UPDATE runs SET native_session_id='sid-742', transcript_path=? WHERE id=?")
            .bind(transcript.to_str().unwrap())
            .bind(&fresh)
            .execute(&e.app.db)
            .await
            .unwrap();
        let resumed = restart_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        assert!(started_args(&e).pop().unwrap().windows(2).any(|w| w == ["--resume", "sid-742"]));
        assert!(!crate::claude_live::is_fresh(&resumed), "接回的 run 畫面上可能有上個 session 的確認，第一次看到只當基準");
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

        let transcript = own_transcript(&e, &pm, "pm").await;
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

        let written = own_transcript(&e, &pm, "written").await;
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
            let plan = native_resume_plan(&e.app, &bot, LOCAL_HOST, false, None).await.unwrap();
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
            let plan = native_resume_plan(&e.app, &bot, LOCAL_HOST, false, None).await.unwrap();
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
            let plan = native_resume_plan(&e.app, &bot, LOCAL_HOST, false, None).await.unwrap();
            assert_eq!(plan, Err("transcript_missing"));
        }

        // ---- 對抗式審查（資料安全）：以下測的都是「換身分複製 transcript」的失敗與邊界 ----

        use std::os::unix::fs::PermissionsExt;

        /// 把一段 run 記在 DB 裡，回新身分（`cc9`）的 config dir。
        async fn seed(e: &crate::testing::Env, bot: &db::Bot, kind: &str, sid: &str, transcript: &std::path::Path) -> std::path::PathBuf {
            let new_dir = e.dir.join(format!("new-{sid}"));
            set_identity_dir(&e.app, "cc9", &new_dir).await;
            sqlx::query("UPDATE bots SET identity='cc9', kind=? WHERE id=?").bind(kind).bind(&bot.id).execute(&e.app.db).await.unwrap();
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'stopped','idle',?,?,'2026-09-17T00:00:00Z','2026-09-17T00:01:00Z')",
            )
            .bind(db::ulid())
            .bind(&bot.id)
            .bind(sid)
            .bind(transcript.to_str().unwrap())
            .execute(&e.app.db)
            .await
            .unwrap();
            new_dir
        }

        async fn plan_of(e: &crate::testing::Env, bot: &db::Bot) -> Result<(String, Vec<String>), &'static str> {
            let bot = db::bot(&e.app.db, &bot.id).await.unwrap().unwrap();
            native_resume_plan(&e.app, &bot, LOCAL_HOST, false, None).await.unwrap()
        }

        fn files_under(dir: &std::path::Path) -> Vec<String> {
            fn walk(d: &std::path::Path, out: &mut Vec<String>) {
                let Ok(rd) = std::fs::read_dir(d) else { return };
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() { walk(&p, out) } else { out.push(p.to_string_lossy().into_owned()) }
                }
            }
            let mut out = vec![];
            walk(dir, &mut out);
            out.sort();
            out
        }

        /// codex／grok 的對話檔不在 claude 的 `projects/` 底下，也不歸 `CLAUDE_CONFIG_DIR` 管：以前照樣複製進
        /// `<claude 身分>/projects/<日>/rollout-….jsonl`，把整段 codex 對話複製到 claude 的目錄裡當垃圾專案。
        #[tokio::test]
        async fn a_codex_rollout_is_never_copied_into_a_claude_projects_dir() {
            let e = env().await;
            let bot = claude_bot(&e.app, &e.project_id, "cx").await;
            let rollout = e.dir.join("codex-home/sessions/2026/10/02/rollout-2026-10-02T00-00-00-sid-cx.jsonl");
            std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
            std::fs::write(&rollout, "{\"secret\":\"conversation\"}\n").unwrap();
            let new_dir = seed(&e, &bot, "codex", "sid-cx", &rollout).await;
            let _ = plan_of(&e, &bot).await;
            assert!(files_under(&new_dir).is_empty(), "codex 的對話被複製進 claude 的目錄：{:?}", files_under(&new_dir));
        }

        /// 來源路徑來自 hook payload 記下的 `transcript_path`：不是 `…/projects/<key>/<sid>.jsonl` 的形狀、或是符號連結
        /// （指到別的檔），都不能被「換身分」複製到另一個身分的目錄。
        #[tokio::test]
        async fn only_a_regular_jsonl_inside_a_projects_dir_is_ever_staged() {
            let e = env().await;
            let bot = claude_bot(&e.app, &e.project_id, "shape").await;
            let secret = e.dir.join("fake-home/.codex/auth.json");
            std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
            std::fs::write(&secret, "TOKEN").unwrap();

            // 1. 形狀不對（不是 projects/<key>/<sid>.jsonl）。
            let new_dir = seed(&e, &bot, "claude", "sid-shape", &secret).await;
            assert_eq!(plan_of(&e, &bot).await, Err("transcript_missing"));
            assert!(files_under(&new_dir).is_empty(), "憑證被複製了：{:?}", files_under(&new_dir));

            // 2. 形狀對，但檔案是指到別處的符號連結。
            let linked = e.dir.join("old-link/projects/-k/sid-link.jsonl");
            std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&secret, &linked).unwrap();
            sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at) VALUES (?,?,'stopped','idle','sid-link',?,'2026-09-18T00:00:00Z','2026-09-18T00:01:00Z')")
                .bind(db::ulid()).bind(&bot.id).bind(linked.to_str().unwrap()).execute(&e.app.db).await.unwrap();
            assert_eq!(plan_of(&e, &bot).await, Err("transcript_missing"));
            assert!(files_under(&new_dir).is_empty(), "符號連結被跟著複製：{:?}", files_under(&new_dir));
        }

        /// transcript 是整段對話內容：複製出去的檔 0600、這次新建的目錄 0700，不管來源檔當初是什麼權限（umask 022 的 0644 很常見）。
        #[tokio::test]
        async fn the_copy_and_its_new_directories_are_private() {
            let e = env().await;
            let bot = claude_bot(&e.app, &e.project_id, "priv").await;
            let old_dir = e.dir.join("old-priv");
            let t = write_jsonl(&old_dir, "-Users-x-priv", "sid-priv");
            std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
            let companion = old_dir.join("projects/-Users-x-priv/sid-priv/sub");
            std::fs::create_dir_all(&companion).unwrap();
            std::fs::write(companion.join("note.txt"), "x").unwrap();
            std::fs::set_permissions(companion.join("note.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
            let new_dir = seed(&e, &bot, "claude", "sid-priv", &t).await;
            plan_of(&e, &bot).await.expect("resumable");
            let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            let base = new_dir.join("projects/-Users-x-priv");
            assert_eq!(mode(base.join("sid-priv.jsonl")), 0o600, "複製出去的對話檔");
            assert_eq!(mode(base.clone()), 0o700, "新建的 cwd 目錄");
            assert_eq!(mode(new_dir.join("projects")), 0o700, "新建的 projects 目錄");
            assert_eq!(mode(base.join("sid-priv/sub/note.txt")), 0o600, "附屬目錄的檔");
            assert_eq!(mode(base.join("sid-priv/sub")), 0o700, "附屬目錄");
        }

        /// 目標已經有同名檔而且比來源**長**（來源是它的前綴）：那邊已經接著寫過了，不能被較短的來源蓋回去。
        #[tokio::test]
        async fn a_destination_that_already_continued_the_session_is_not_truncated() {
            let e = env().await;
            let bot = claude_bot(&e.app, &e.project_id, "cont").await;
            let old_dir = e.dir.join("old-cont");
            let t = write_jsonl(&old_dir, "-Users-x-cont", "sid-cont");
            std::fs::write(&t, "{\"a\":1}\n").unwrap();
            let new_dir = seed(&e, &bot, "claude", "sid-cont", &t).await;
            let dest = new_dir.join("projects/-Users-x-cont/sid-cont.jsonl");
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(&dest, "{\"a\":1}\n{\"b\":2}\n").unwrap();
            plan_of(&e, &bot).await.expect("resumable");
            assert_eq!(std::fs::read_to_string(&dest).unwrap(), "{\"a\":1}\n{\"b\":2}\n", "新身分那邊多出來的回合被蓋掉了");
        }

        /// 兩邊各自長出不同的內容（不是前綴關係）：照 run 記的來源為準，但被換掉的那份留在旁邊，不是直接消失。
        #[tokio::test]
        async fn a_diverged_destination_is_set_aside_not_destroyed() {
            let e = env().await;
            let bot = claude_bot(&e.app, &e.project_id, "div").await;
            let old_dir = e.dir.join("old-div");
            let t = write_jsonl(&old_dir, "-Users-x-div", "sid-div");
            std::fs::write(&t, "{\"a\":1}\n{\"src\":true}\n").unwrap();
            let new_dir = seed(&e, &bot, "claude", "sid-div", &t).await;
            let dest_dir = new_dir.join("projects/-Users-x-div");
            std::fs::create_dir_all(&dest_dir).unwrap();
            std::fs::write(dest_dir.join("sid-div.jsonl"), "{\"a\":1}\n{\"dest\":true}\n").unwrap();
            plan_of(&e, &bot).await.expect("resumable");
            assert_eq!(std::fs::read_to_string(dest_dir.join("sid-div.jsonl")).unwrap(), "{\"a\":1}\n{\"src\":true}\n");
            let aside: Vec<_> = files_under(&dest_dir).into_iter().filter(|f| !f.ends_with("/sid-div.jsonl")).collect();
            assert_eq!(aside.len(), 1, "被換掉的那份要留下來：{aside:?}");
            assert!(!aside[0].ends_with(".jsonl"), "備份不能也叫 .jsonl，不然 CLI 會把它當成另一段 session：{aside:?}");
            assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), "{\"a\":1}\n{\"dest\":true}\n");
        }

        /// 同一顆 bot 同時兩個（以上）接回請求：不會留下暫存檔、結果是完整的來源內容。
        #[tokio::test]
        async fn concurrent_resumes_leave_one_complete_copy_and_no_debris() {
            let e = env().await;
            let bot = claude_bot(&e.app, &e.project_id, "race").await;
            let old_dir = e.dir.join("old-race");
            let t = write_jsonl(&old_dir, "-Users-x-race", "sid-race");
            let body = "{\"turn\":\"x\"}\n".repeat(20_000);
            std::fs::write(&t, &body).unwrap();
            let new_dir = seed(&e, &bot, "claude", "sid-race", &t).await;
            let results = futures::future::join_all((0..6).map(|_| plan_of(&e, &bot))).await;
            assert!(results.iter().all(|r| r.is_ok()), "{results:?}");
            let files = files_under(&new_dir);
            assert_eq!(files.len(), 1, "只該有最後那一個檔，沒有 .tmp／備份：{files:?}");
            assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), body);
        }
    }

    /// issue #95：換身分後 transcript 只搬本機，遠端主機的對話接不回來。這裡測遠端那條路：純函式
    /// 部分（組 script、解析輸出）直接驗內容；連線失敗時的 fail-closed 用一個刻意連不上的假 host
    /// 驗證（沒有可重用的 live-SSH 測試環境，這是能不碰任何真實遠端主機驗到的最大範圍）。
    mod remote_cross_identity_transcript_tests {
        use super::*;
        use crate::lifecycle::native_resume_plan;
        use crate::lifecycle::start::{parse_stage_output, remote_stage_script, stage_cross_identity_transcript_remote};
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
            let dir = crate::testing::track(std::env::temp_dir().join(format!("am-remote-stage-{}", db::ulid())));
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

        // ---- 對抗式審查（資料安全）：遠端 script 的暫存檔／權限／覆蓋規則要跟本機版一致 ----

        fn mode(p: &std::path::Path) -> u32 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).unwrap().permissions().mode() & 0o777
        }

        fn names(dir: &std::path::Path) -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        }

        /// 形狀不對的遠端來源（例如指到 `auth.json`）：連 ssh 都不碰就退回開新對話；host 刻意是連不上的假名，
        /// 要是有去連會是另一種失敗訊息，但一樣是 `transcript_missing`，所以另外確認 script 根本沒被組出來。
        #[tokio::test]
        async fn a_remote_source_of_the_wrong_shape_is_refused_before_any_ssh() {
            assert!(!crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.codex/auth.json"));
            assert!(!crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.claude/projects/k/sid.txt"));
            assert!(!crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.claude/other/k/sid.jsonl"));
            assert!(!crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.claude/projects/../auth.jsonl"), "remote staging must reject a traversal cwd component");
            assert!(crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.claude-cc1/projects/-Users-x/sid.jsonl"));
        }

        #[test]
        fn the_remote_copy_is_private_and_leaves_no_temp_file() {
            use std::os::unix::fs::PermissionsExt;
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("k/sid/sub")).unwrap();
            std::fs::write(old.join("k/sid.jsonl"), "hello").unwrap();
            std::fs::write(old.join("k/sid/sub/n.txt"), "c").unwrap();
            for f in ["k/sid.jsonl", "k/sid/sub/n.txt"] {
                std::fs::set_permissions(old.join(f), std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            let new = base.join("new-projects");
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", Some("sid"));
            assert!(run_script_locally(&script).contains("AM_STAGED"));
            assert_eq!(mode(&new.join("k/sid.jsonl")), 0o600);
            assert_eq!(mode(&new.join("k")), 0o700);
            assert_eq!(mode(&new.join("k/sid/sub/n.txt")), 0o600);
            assert_eq!(names(&new.join("k")), vec!["sid".to_string(), "sid.jsonl".to_string()], "不能留下暫存檔");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn the_remote_copy_never_truncates_a_destination_that_already_continued() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("k")).unwrap();
            std::fs::write(old.join("k/sid.jsonl"), "{\"a\":1}\n").unwrap();
            let new = base.join("new-projects");
            std::fs::create_dir_all(new.join("k")).unwrap();
            std::fs::write(new.join("k/sid.jsonl"), "{\"a\":1}\n{\"b\":2}\n").unwrap();
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            assert!(run_script_locally(&script).contains("AM_STAGED"));
            assert_eq!(std::fs::read_to_string(new.join("k/sid.jsonl")).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_diverged_remote_destination_is_set_aside_not_destroyed() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("k")).unwrap();
            std::fs::write(old.join("k/sid.jsonl"), "{\"a\":1}\n{\"src\":1}\n").unwrap();
            let new = base.join("new-projects");
            std::fs::create_dir_all(new.join("k")).unwrap();
            std::fs::write(new.join("k/sid.jsonl"), "{\"a\":1}\n{\"dest\":1}\n").unwrap();
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            assert!(run_script_locally(&script).contains("AM_STAGED"));
            assert_eq!(std::fs::read_to_string(new.join("k/sid.jsonl")).unwrap(), "{\"a\":1}\n{\"src\":1}\n");
            let aside: Vec<String> = names(&new.join("k")).into_iter().filter(|n| n != "sid.jsonl").collect();
            assert_eq!(aside.len(), 1, "{aside:?}");
            assert!(!aside[0].ends_with(".jsonl"), "{aside:?}");
            assert_eq!(std::fs::read_to_string(new.join("k").join(&aside[0])).unwrap(), "{\"a\":1}\n{\"dest\":1}\n");
            let _ = std::fs::remove_dir_all(&base);
        }

        /// 假的 BSD `stat`：不認 `-c`（逼腳本走 macOS 那條路），`-f` 照 macOS 的行為——對 `/dev/fd/N` 回 devfs 的裝置編號
        /// （2026-10-04 m4p 實測：檔案 dev 16777234、`/dev/fd/3` dev 1318745794，inode 相同）。
        fn bsd_stat_dir() -> std::path::PathBuf {
            let dir = tmp().join("bsd-bin");
            std::fs::create_dir_all(&dir).unwrap();
            let shim = dir.join("stat");
            std::fs::write(
                &shim,
                r#"#!/bin/sh
fmt=; path=
while [ $# -gt 0 ]; do
  case "$1" in -c) exit 1;; -L) ;; -f) fmt=$2; shift;; *) path=$1;; esac
  shift
done
gfmt=$(printf '%s' "$fmt" | sed -e 's/%l/%h/g' -e 's/%z/%s/g')
case "$path" in
  /dev/fd/*) out=$(/usr/bin/stat -L -c "$gfmt" "$path") || exit 1; printf '%s\n' "$out" | sed 's/^[0-9]* /1318745794 /' ;;
  *) /usr/bin/stat -L -c "$gfmt" "$path" ;;
esac
"#,
            )
            .unwrap();
            std::fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            dir
        }

        /// 2026-10-04：macOS 上 `/dev/fd/N` 的裝置編號是 devfs 的，以前比對永遠失敗 → `AM_MISSING` → 每次重啟都開新對話。
        #[test]
        fn a_bsd_host_stages_the_transcript_instead_of_calling_it_missing() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("k")).unwrap();
            std::fs::write(old.join("k/sid.jsonl"), "{\"a\":1}\n").unwrap();
            let new = base.join("new-projects");
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            let path = format!("{}:{}", bsd_stat_dir().display(), std::env::var("PATH").unwrap_or_default());
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(&script).env("PATH", path).output().unwrap();
            let out = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(out.contains("AM_STAGED"), "BSD stat 的 /dev/fd 裝置編號不同也要搬得過去：{out}");
            assert_eq!(std::fs::read_to_string(new.join("k/sid.jsonl")).unwrap(), "{\"a\":1}\n");
            // 同一個 projects（console-rpa 的情形：身分沒換）回 AM_SAME。
            let same = remote_stage_script(&old.to_string_lossy(), &old.to_string_lossy(), "k", "sid.jsonl", None);
            let path = format!("{}:{}", bsd_stat_dir().display(), std::env::var("PATH").unwrap_or_default());
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(&same).env("PATH", path).output().unwrap();
            assert!(String::from_utf8_lossy(&out.stdout).contains("AM_SAME"), "{}", String::from_utf8_lossy(&out.stdout));
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_symlinked_remote_source_is_reported_missing_and_copies_nothing() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("k")).unwrap();
            let secret = base.join("secret");
            std::fs::write(&secret, "TOKEN").unwrap();
            std::os::unix::fs::symlink(&secret, old.join("k/sid.jsonl")).unwrap();
            let new = base.join("new-projects");
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_MISSING"), "{out}");
            assert!(!new.exists(), "符號連結被跟著複製");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_remote_projects_symlink_cannot_import_a_transcript_from_another_tree() {
            let base = tmp();
            let real = base.join("private-projects");
            std::fs::create_dir_all(real.join("k")).unwrap();
            std::fs::write(real.join("k/sid.jsonl"), "private transcript").unwrap();
            let old = base.join("old-projects");
            std::os::unix::fs::symlink(&real, &old).unwrap();
            let new = base.join("new-projects");
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_MISSING"), "{out}");
            assert!(!new.exists(), "transcript outside the owned projects tree was copied");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_remote_destination_cwd_symlink_cannot_write_into_another_tree() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("k")).unwrap();
            std::fs::write(old.join("k/sid.jsonl"), "private transcript").unwrap();
            let outside = base.join("outside");
            std::fs::create_dir(&outside).unwrap();
            let new = base.join("new-projects");
            std::fs::create_dir(&new).unwrap();
            std::os::unix::fs::symlink(&outside, new.join("k")).unwrap();
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_MKDIR_FAILED") || out.contains("AM_MISSING"), "{out}");
            assert!(!outside.join("sid.jsonl").exists(), "destination cwd symlink was followed");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_remote_hard_linked_source_is_not_treated_as_a_transcript() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("k")).unwrap();
            let secret = base.join("credential");
            std::fs::write(&secret, "sensitive value").unwrap();
            std::fs::hard_link(&secret, old.join("k/sid.jsonl")).unwrap();
            let new = base.join("new-projects");
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_MISSING"), "{out}");
            assert!(!new.exists(), "hard-linked credential was copied as a transcript");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_source_swapped_to_a_symlink_at_copy_time_is_not_read() {
            let base = tmp();
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join("k")).unwrap();
            let src = old.join("k/sid.jsonl");
            std::fs::write(&src, "ordinary transcript").unwrap();
            let secret = base.join("credential");
            std::fs::write(&secret, "sensitive value").unwrap();
            let new = base.join("new-projects");
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            let injected = format!(
                "cat() {{ rm -f {src}; ln -s {secret} {src}; command cat \"$@\"; }}\n{script}",
                src = crate::hosts::sh_quote(&src.to_string_lossy()),
                secret = crate::hosts::sh_quote(&secret.to_string_lossy()),
            );
            let out = run_script_locally(&injected);
            assert!(out.contains("AM_STAGED"), "{out}");
            assert_eq!(std::fs::read_to_string(new.join("k/sid.jsonl")).unwrap(), "ordinary transcript", "TOCTOU swap must keep reading the opened fd");
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

        /// Claude 的 projects 目錄名是 `-Users-…`。`cd "$K"` 會把前導 `-` 當成選項，搬檔被判成不存在。
        #[test]
        fn a_project_dir_whose_name_starts_with_a_dash_is_still_staged() {
            let base = tmp();
            let cwd = "-Users-m4p-project";
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join(cwd)).unwrap();
            std::fs::write(old.join(cwd).join("sid.jsonl"), "hello").unwrap();
            let new = base.join("new-projects");
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), cwd, "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_STAGED"), "{out}");
            assert_eq!(std::fs::read_to_string(new.join(cwd).join("sid.jsonl")).unwrap(), "hello");
            let _ = std::fs::remove_dir_all(&base);
        }

        /// 目錄名帶著 shell 特殊字元（`$(...)`、單引號）不能被當成指令執行——每段路徑各自
        /// `sh_quote`，不是把值原樣黏進雙引號字串裡。用一個會在展開時建出檔案的 `$(...)` 當
        /// canary：quoting 對了就不會被展開，那個檔案就不會出現。
        #[test]
        fn shell_metacharacters_in_a_directory_name_are_not_executed() {
            let base = tmp();
            let weird_cwd = "it's-$(printf pwned)-cwd";
            let old = base.join("old-projects");
            std::fs::create_dir_all(old.join(&weird_cwd)).unwrap();
            std::fs::write(old.join(&weird_cwd).join("sid.jsonl"), "hello").unwrap();
            let new = base.join("new-projects");

            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), &weird_cwd, "sid.jsonl", None);
            let out = run_script_locally(&script);
            assert!(out.contains("AM_STAGED"), "{out}");
            assert_eq!(std::fs::read_to_string(new.join(&weird_cwd).join("sid.jsonl")).unwrap(), "hello");

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
            let plan = native_resume_plan(&e.app, &bot, "no-such-host", false, None).await.unwrap();
            assert_eq!(plan, Err("transcript_missing"));
        }

        #[tokio::test]
        async fn remote_transcript_staging_skips_unreadable_home_and_retries_with_remote_home() {
            let e = env().await;
            let pm = claude_bot(&e.app, &e.project_id, "pm").await;
            let host = format!("resume-home-616-{}", db::ulid().to_ascii_lowercase());
            let conn = e.app.hosts.insert_remote_for_test(crate::config::HostCfg {
                shared_session: false,
                name: host.clone(),
                ssh: "unused".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
            }).await;
            let transcript = e.dir.join("projects/-k/remote-home-resume.jsonl");
            std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            std::fs::write(&transcript, "{}\n").unwrap();
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let calls2 = calls.clone();
            crate::hosts::set_ssh_fake(&host, move |script| {
                calls2.lock().unwrap().push(script.to_string());
                Err(anyhow::anyhow!("injected remote HOME read failure"))
            });

            assert_eq!(stage_cross_identity_transcript_remote(&e.app, &pm, &host, transcript.to_str().unwrap()).await, Err("transcript_missing"));
            assert_eq!(calls.lock().unwrap().len(), 1, "do not run a transcript-copy script with unknown identity HOME");

            *conn.remote_home.lock().await = Some("/home/remote-resume".into());
            let calls2 = calls.clone();
            crate::hosts::set_ssh_fake(&host, move |script| {
                calls2.lock().unwrap().push(script.to_string());
                Ok("AM_SAME\n".into())
            });
            assert_eq!(stage_cross_identity_transcript_remote(&e.app, &pm, &host, transcript.to_str().unwrap()).await, Ok(()));
            assert!(calls.lock().unwrap().last().unwrap().contains("/home/remote-resume/.claude/projects"), "retry uses the recovered remote HOME");
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
                shared_session: false,
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
            let plan = tokio::time::timeout(Duration::from_secs(20), native_resume_plan(&e.app, &bot, "unreachable-box", false, None))
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
                Err(e) => panic!("restart {i} was refused while a reconcile was running: {e:?}"),
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
        crate::testing::write_exec(&script, format!("#!/bin/sh\nprintf '{{\"loggedIn\": {fresh_logged_in}}}'\n"));
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
                utc_offset_secs: None, herdr_cli: None, checked_at: db::now(),
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

    #[tokio::test]
    async fn a_codex_child_restart_keeps_its_process_and_screen_guards() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let parent = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let kid = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, model, args_json, autostart, inject_hooks, hook_token, auto_approve, managed_by, parent_bot_id, created_at)
             VALUES (?,?,'ui','codex','gpt-5.6-sol','[]',0,0,'tok',1,'child',?,?)",
        )
        .bind(&kid)
        .bind(&env.project_id)
        .bind(&parent.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let agent = "proj-alfa-ui";
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',1,?)",
        )
        .bind(db::ulid())
        .bind(&kid)
        .bind(&ws.workspace_id)
        .bind(&kid_pane.tab_id)
        .bind(&kid_pane.pane_id)
        .bind(agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": agent, "agent": "codex", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id,
            "cwd": "/tmp/p"})];

        restart_child_in_pane(&app, &kid).await.unwrap();

        let calls = env.herdr.calls_to("agent.start");
        let args = calls.last().unwrap()["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|arg| arg.as_str())
            .collect::<Vec<_>>();
        assert!(args.contains(&"--no-daemon"), "Codex must not reuse a shared app server: {args:?}");
        assert!(args.contains(&"--no-alt-screen"), "Codex must keep its screen readable: {args:?}");
        assert!(args.windows(2).any(|w| w == ["-c", "tui.show_tooltips=false"]), "Codex turn tips must stay off: {args:?}");
    }

    /// 稽核：herdr 0.9.2+ 的閒著 codex 永遠回 `unknown`、不會再有狀態事件。原地重啟新 run 以 `unknown` 起頭，
    /// 等到 ready 的結果（折成 idle）卻被丟掉，DB 就一直停在 `unknown`：一鍵重啟、閒置回收都把它當「狀態不明」跳過。
    #[tokio::test]
    async fn a_codex_child_restarted_in_its_pane_is_recorded_idle_once_ready() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let parent = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let kid = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, auto_approve, managed_by, parent_bot_id, created_at)
             VALUES (?,?,'ui','codex','[]',0,0,'tok',1,'child',?,?)",
        )
        .bind(&kid)
        .bind(&env.project_id)
        .bind(&parent.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let agent = "proj-alfa-ui";
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',1,?)",
        )
        .bind(db::ulid())
        .bind(&kid)
        .bind(&ws.workspace_id)
        .bind(&kid_pane.tab_id)
        .bind(&kid_pane.pane_id)
        .bind(agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": agent, "agent": "codex", "agent_status": "unknown", "launch_pending": false,
            "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id,
            "cwd": "/tmp/p"})];

        let run_id = restart_child_in_pane(&app, &kid).await.unwrap();

        let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "idle", "ready 之後要記下折過的狀態，不能留在 unknown");
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
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-trust-start-{}", crate::db::ulid())));
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

    /// #146 重開 B：重啟停掉了、start 在前置檢查就失敗，舊 run 改記 `exited` 卻寫不進去。以前只看 `Ok(rows>0)`，DB 錯誤
    /// 直接吞掉，留下 `stopped`（「使用者要它停」）：autostart 的 bot 從此沒在跑，incident 探針卻永遠不報。現在回 503
    /// `restart_state_uncommitted`（帶著 start 的錯）、排 `Relabel` 重試；DB 恢復後成 `exited`，探針報 `bot_stopped`。
    #[tokio::test]
    async fn a_restart_that_cannot_relabel_its_old_run_says_so_and_retries() {
        use super::super::run_state as rs;
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id) = bot_with_run(&env, "user", "idle").await;
        sqlx::query("UPDATE bots SET identity='nope-not-on-this-host', autostart=1 WHERE id=?").bind(&bot_id).execute(&app.db).await.unwrap();
        rs::refuse_run_state(&app, "exited").await;
        match restart_bot(&app, &bot_id).await {
            Err(LcError::Uncommitted(v)) => {
                assert_eq!((v["error"].as_str(), v["run_id"].as_str()), (Some("restart_state_uncommitted"), Some(run_id.as_str())));
                assert!(v["start_error"].as_str().is_some_and(|s| s.contains("Conflict")), "start 的錯一起帶回：{v}");
            }
            other => panic!("舊 run 還記著使用者要它停，不能只回 start 的錯：{other:?}"),
        }
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "stopped");
        assert_eq!(rs::scheduled(&run_id), vec![rs::Settle::Relabel { from: "stopped", to: "exited" }], "排了改標的重試");
        assert!(!rs::bot_stopped_reported(&app, &bot_id).await, "前提：留著 stopped，探針以為是故意停的");

        rs::accept_run_state(&app, "exited").await;
        assert!(rs::settle_once(&app, &run_id, &rs::Settle::Relabel { from: "stopped", to: "exited" }).await);
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "exited", "重啟沒開回來，不是使用者停的");
        assert!(rs::bot_stopped_reported(&app, &bot_id).await, "autostart 的 bot 沒在跑：探針報出來");
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

    /// 子 agent 的原地重啟（一鍵重啟也會走）同一個競態：鎖裡看過閒置之後才開始工作，不能送 ctrl+c。
    #[tokio::test]
    async fn a_child_that_starts_working_right_before_the_in_pane_restart_is_not_interrupted() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, run_id) = bot_with_run(&env, "child", "idle").await;
        let (a, r) = (app.clone(), run_id.clone());
        crate::lifecycle::race_point::arm("child_restart_before_stopping", &kid, move || async move {
            sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&r).execute(&a.db).await.unwrap();
        });
        let err = restart_child_in_pane_with(&app, &kid, true).await.expect_err("剛開始工作的子 agent 不能被重啟");
        assert_eq!(reason(err).0, "not_idle");
        assert_eq!(db::active_run(&app.db, &kid).await.unwrap().map(|r| (r.id, r.state)), Some((run_id, "running".into())), "run 原封不動");
        assert!(!env.herdr.methods().iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "沒有 ctrl+c");
    }

    /// The in-pane restart has its own stop path, so a late hook report must be checked after its
    /// idle inspection just like a bulk restart through `stop_locked`.
    #[tokio::test]
    async fn a_child_background_hook_report_before_stopping_blocks_the_in_pane_restart() {
        let env = tt::env().await;
        let app = env.app.clone();
        let kid = crate::lifecycle::restart_kind_tests::live_child(&env, "late-hook").await;
        let run = db::run(&app.db, &kid.run_id).await.unwrap().unwrap();
        let hook_app = app.clone();
        crate::lifecycle::race_point::arm("child_restart_before_stopping", &kid.id, move || async move {
            crate::background_hook::on_stop(
                &hook_app,
                &run,
                &json!({"background_tasks": [{"id": "agent-1", "type": "subagent", "status": "running", "description": "review"}]}),
            )
            .await;
        });

        let err = restart_child_in_pane_with(&app, &kid.id, true).await.expect_err("new background work blocks the in-pane restart");
        assert_eq!(reason(err).1, "background_jobs");
        assert_eq!(db::active_run(&app.db, &kid.id).await.unwrap().map(|r| (r.id, r.state)), Some((kid.run_id, "running".into())));
        assert!(!env.herdr.methods().iter().any(|m| m == "agent.send_keys" || m == "agent.start"), "background work must not be interrupted");
    }

    /// 一鍵重啟鎖裡看過閒置、還沒記 `stopping` 的那一瞬，使用者直接在 pane 裡打字：`events::handle_status` 不拿 bot 鎖就寫
    /// `agent_status=working`。閒置回收（#144）對這一瞬有「還是 idle 才准停」的許可，重啟停機那一步以前沒有：照樣記 `stopping`、
    /// 送 ctrl+c，使用者剛開始的那一回合被砍。要跟那一句寫入互相排序：不閒了就不停，什麼都不動。
    #[tokio::test]
    async fn a_bot_that_starts_working_right_before_the_restart_stop_is_not_interrupted() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id) = bot_with_run(&env, "user", "idle").await;
        let (a, r) = (app.clone(), run_id.clone());
        crate::lifecycle::race_point::arm("stop_before_stopping", &bot_id, move || async move {
            sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&r).execute(&a.db).await.unwrap();
        });
        let opts = StartOpts { resume_native: true, require_idle: true, ..Default::default() };
        let err = restart_bot_with(&app, &bot_id, opts).await.expect_err("剛開始工作的 bot 不能被重啟");
        assert_eq!(reason(err).0, "not_idle");
        assert_eq!(db::active_run(&app.db, &bot_id).await.unwrap().map(|r| (r.id, r.state)), Some((run_id, "running".into())), "run 原封不動");
        let methods = env.herdr.methods();
        assert!(!methods.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "沒有 ctrl+c、沒有關 pane：{methods:?}");
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

/// grok 啟動不准送 `/effort`（會持久化 `default_reasoning_effort`）。等級只在 argv。
#[cfg(test)]
mod grok_startup_effort_tests {
    use super::start_bot;
    use crate::db;
    use crate::testing as tt;

    const GROK_HIGH: &str = "  ╭──────────────────────────────────────────╮\n  │ ❯                                        │\n  ╰──────────────── Grok 4.6 (high) · always-approve ─╯\n";
    const GROK_MEDIUM: &str = "  ╭──────────────────────────────────────────╮\n  │ ❯                                        │\n  ╰──────────────── Grok 4.6 (medium) · always-approve ─╯\n";

    async fn grok_bot(app: &std::sync::Arc<crate::state::App>, project_id: &str, effort: Option<&str>) -> db::Bot {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, model, effort, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,?,'grok','grok-4.6',?,'[]',0,1,'tok','user',?)",
        )
        .bind(&id)
        .bind(project_id)
        .bind("g-effort")
        .bind(effort)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        db::bot(&app.db, &id).await.unwrap().unwrap()
    }

    fn sent_effort_slash(e: &tt::Env) -> Vec<String> {
        e.herdr
            .calls_to("pane.send_text")
            .into_iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .filter(|t| t.contains("/effort "))
            .collect()
    }

    /// grok 1.0.46 拋棄式 `GROK_HOME`：`/effort` 會把 `[models] default_reasoning_effort` 寫進
    /// 使用者的 config.toml；`--reasoning-effort` 只改這一輪框底、不寫檔。框底還是 high 也不准補 slash。
    #[tokio::test]
    async fn a_grok_bot_set_to_medium_does_not_persist_effort_with_a_slash() {
        let e = tt::env().await;
        e.herdr.set_screen("*", GROK_HIGH);
        let bot = grok_bot(&e.app, &e.project_id, Some("medium")).await;
        start_bot(&e.app, &bot.id).await.unwrap();
        assert!(sent_effort_slash(&e).is_empty(), "不准送 /effort，那會寫進 ~/.grok/config.toml");
        let run = db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(run.runtime_effort.as_deref(), Some("medium"), "等級只來自 --reasoning-effort");
    }

    /// 畫面已經是設定的等級：不要再打字（pane_typed 會改 prompt 路徑）。
    #[tokio::test]
    async fn a_grok_bot_does_not_slash_when_the_tui_already_matches() {
        let e = tt::env().await;
        e.herdr.set_screen("*", GROK_MEDIUM);
        let bot = grok_bot(&e.app, &e.project_id, Some("medium")).await;
        start_bot(&e.app, &bot.id).await.unwrap();
        assert!(sent_effort_slash(&e).is_empty(), "已經 medium 就不要再 /effort");
    }

    /// 沒設 effort：不要自作主張改 TUI。
    #[tokio::test]
    async fn a_grok_bot_without_effort_is_left_on_the_tui_default() {
        let e = tt::env().await;
        e.herdr.set_screen("*", GROK_HIGH);
        let bot = grok_bot(&e.app, &e.project_id, None).await;
        start_bot(&e.app, &bot.id).await.unwrap();
        assert!(sent_effort_slash(&e).is_empty());
    }
}

/// Continue a credential-rotation restart while the caller holds the bot lock.
pub(crate) async fn resume_credential_rotation_locked(app: &Arc<App>, bot_id: &str, opts: StartOpts, from_run: &str) -> LcResult<String> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    refuse_default_session(&bot)?;
    restart_stop_and_start(app, bot_id, opts, Some(from_run.to_string()), None).await
}

/// 開機補完被打斷的重啟（#355 P2）：呼叫端持 bot 鎖，已經決定要往前補。stop 做到一半（`stopping`）先補完它；
/// 剛停掉的舊 run 由 `stopped`（使用者要它停）改標 `exited`；再照原本的選項 start（不再要求閒置——使用者要的是它回來）。
pub(crate) async fn resume_restart_locked(app: &Arc<App>, bot_id: &str, opts: StartOpts, from_run: Option<&str>) -> LcResult<String> {
    if let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? {
        if run.state == "stopping" {
            stop_for_restart_locked(app, bot_id).await?;
        }
    }
    if let Some(run_id) = from_run {
        left_down_by_restart(app, bot_id, run_id).await.map_err(up)?;
    }
    let restarting = super::restart_hold::begin(bot_id);
    let started = restart_start(app, bot_id, StartOpts { require_idle: false, ..opts }, None).await;
    drop(restarting);
    if started.is_ok() {
        schedule_flush_queued(app, bot_id);
    }
    started
}

/// 2026-10-04 使用者：「接回之後不需要再提示原本的失敗」。
#[cfg(test)]
mod retire_context_lost_tests {
    use super::*;

    async fn notes(app: &Arc<App>, conv: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'system' ORDER BY created_at, rowid")
            .bind(conv)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn resuming_the_lost_session_retires_only_the_notes_written_after_it_ended() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "rpa").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        // 很久以前另一次失敗（別段 session）：不能被這次接回撤掉。
        let _ = insert_message(&app, &conv, None, "system", "⚠️ 接不回原本的對話（舊的），已經開了新的對話——前面的脈絡沒有帶過來。", "system", false, None).await;
        sqlx::query("UPDATE messages SET created_at = '2026-10-01T00:00:00.000Z' WHERE conversation_id = ?").bind(&conv).execute(&app.db).await.unwrap();
        // 用過 S 的那個 run 在 10-04 00:19:32 結束，緊接著插了失敗通知與一則一般系統訊息。
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, ended_at, native_session_id)
             VALUES ('run-old', ?, 'exited', 'idle', 'ws', 'p', 'a', 't', '2026-10-03T02:27:05.470Z', '2026-10-04T00:19:32.795Z', 'S')",
        )
        .bind(&bot.id)
        .execute(&app.db)
        .await
        .unwrap();
        context_lost(&app, &bot, "transcript_missing").await.unwrap();
        let _ = insert_message(&app, &conv, None, "system", "Claude Code 已更新到 2.1.290", "system", false, None).await;
        let current = crate::testing::fake_run(&app, &bot.id).await;
        assert_eq!(notes(&app, &conv).await.len(), 3);

        // 接回別段 session：不動。
        retire_context_lost(&app, &bot.id, &current, "OTHER").await;
        assert_eq!(notes(&app, &conv).await.len(), 3);

        // 接回 S：只撤 S 結束之後那則「接不回」。
        retire_context_lost(&app, &bot.id, &current, "S").await;
        let left = notes(&app, &conv).await;
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(left[0].contains("（舊的）"), "更早、別段的失敗通知留著");
        assert!(left[1].contains("已更新"), "不是失敗通知的系統訊息不動");
    }
}
