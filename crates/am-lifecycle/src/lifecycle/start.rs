//! Starting and restarting a bot: pane acquisition, `agent.start`, and resume.

use super::*;

/// Session contract used by startup policy.
#[path = "../session_ports.rs"]
pub mod ports;
use self::ports::{HandoffSessionRepo, RestartIntentRepo, SessionProviderPort};
use self::ports::{PaneWatchPort, PreviewPort, RemoteCleanupPort, ShareSessionRepo, ShimInstallPort};
use super::s6_ports::{IdentityAccess, StartServices};
use crate::capabilities::{BootId, BotLocks, BotStatusEmit, Cfg, DataDir, Db, Emit, ExePath, ListenPort};
use crate::capabilities::HerdrRoutes;
use crate::hosts::{HostInstance, HostsAccess};

pub trait StartContext:
    StartServices + Db + Emit + BotLocks + BotStatusEmit + Cfg + DataDir + ExePath + ListenPort + BootId
    + HostsAccess + HostInstance + HerdrRoutes + IdentityAccess + SessionProviderPort
    + HandoffSessionRepo + RestartIntentRepo + ShareSessionRepo
    + ShimInstallPort + RemoteCleanupPort + PreviewPort + PaneWatchPort
    + crate::background_jobs::JobCounts + crate::kind_probe::KindProbeState + super::s6_ports::StopContext
    + super::s6_ports::QueueContext + super::s6_ports::ResumeNudgeContext + super::s6_ports::ScreenContext
    + Clone + Send + Sync + 'static
{}

impl<T> StartContext for T where
    T: StartServices + Db + Emit + BotLocks + BotStatusEmit + Cfg + DataDir + ExePath + ListenPort + BootId
        + HostsAccess + HostInstance + HerdrRoutes + IdentityAccess + SessionProviderPort
        + HandoffSessionRepo + RestartIntentRepo + ShareSessionRepo
        + ShimInstallPort + RemoteCleanupPort + PreviewPort + PaneWatchPort
        + crate::background_jobs::JobCounts + crate::kind_probe::KindProbeState + super::s6_ports::StopContext
        + super::s6_ports::QueueContext + super::s6_ports::ResumeNudgeContext + super::s6_ports::ScreenContext
        + Clone + Send + Sync + 'static
{}

/// Identity CLI args on `host`. Discovered `ccN` identities carry none: alias flags are the user's shell habit.
async fn identity_args(app: &impl IdentityAccess, bot: &db::Bot, host: &str) -> Vec<String> {
    let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) else { return vec![] };
    app.identity_for_host(host, idn).await.map(|i| i.args).unwrap_or_default()
}

/// `-c tui.show_tooltips=false`：codex 0.158 起回合超過 30 秒、以及第三回合起的回合結束後會插一行隨機 tip
/// （openai/codex#48352），畫面備援擷取可能把它當成回覆結尾（issue #728）。不依賴使用者的 config.toml，固定關掉。
///
/// `-c allow_login_shell=false`（issue #1675）：codex 的 shell 工具預設用 `$SHELL -lc` 跑指令，login shell 的 profile
/// 會重建 `PATH`（macOS `path_helper`、`brew shellenv`、`~/.local/bin`），把 pane env 裡擺在最前面的 herdr／cargo shim
/// 擠到真 herdr 後面。bot 打 `herdr agent start` 就繞過 shim：不補 `--no-daemon`、`-p`，codex 子 agent 連上版本較舊的共用
/// app-server，新模型被拒。關掉之後工具的 shell 直接繼承 pane 的 `PATH`。
fn codex_pane_guard_args(kind: &str) -> Vec<String> {
    if kind == "codex" {
        vec![
            "--no-daemon".into(),
            "--no-alt-screen".into(),
            "-c".into(),
            "tui.show_tooltips=false".into(),
            "-c".into(),
            "allow_login_shell=false".into(),
        ]
    } else {
        Vec::new()
    }
}

pub use crate::lc_error::StartOpts;

/// 這顆 bot 現在為什麼不能被重啟；`None` ＝ 閒置。呼叫端持 bot 鎖。理由的代碼與 `bulk_restart::Skip` 一致。
///
/// 一鍵重啟在排到這顆時 recheck 過一次，但 recheck 到這裡拿到鎖之間仍有空檔：使用者剛好在那幾毫秒送出一則，
/// 那一回合會被 ctrl+c 砍掉（review2 quota #5）。鎖內再看一次才真的關掉。
async fn busy_reason_locked(app: &impl StartContext, bot_id: &str, refuse_background_jobs: bool) -> LcResult<Option<String>> {
    let Some(run) = db::active_run(app.db(), bot_id).await.map_err(up)? else { return Ok(Some("not_running".into())) };
    let busy = if run.state != "running" {
        Some("not_running")
    } else if run.agent_status == "working" {
        Some("working")
    } else if run.agent_status == "blocked" {
        Some("blocked")
    } else if run.agent_status != "idle" {
        Some("unknown_status")
    } else if db::in_flight_turn(app.db(), &run.id).await.map_err(up)?.is_some() {
        Some("turn_in_flight")
    } else {
        None
    };
    if let Some(busy) = busy {
        return Ok(Some(busy.to_string()));
    }
    if refuse_background_jobs {
        if let Some(bot) = db::bot(app.db(), bot_id).await.map_err(up)? {
            app.refresh_background_jobs(&run, &bot.kind).await;
            if let Some(n) = crate::background_jobs::known(app, &run.id).filter(|n| *n > 0) {
                return Ok(Some(format!("background_jobs:{n}")));
            }
        }
    }
    Ok(None)
}

/// 專家（specialist）的範圍段：bot 專屬，接在 agent md 後面；不改共用的 `child_agent_rules`（改它會讓所有 bot 快取失效）。
/// 沒有 track 的 specialist 不會被投影進來（daemon 先擋 `track_required`），這裡回 `None`。
pub(crate) fn specialist_role_section(role: Option<&str>, track: Option<&str>) -> Option<String> {
    if role != Some("specialist") {
        return None;
    }
    let track = track.map(str::trim).filter(|t| !t.is_empty())?;
    Some(format!("\n\n## 專家範圍\n你是本專案 {track} 的專家，範圍限於 {track}；超出範圍的工作回報管理者，不要自己做。\n本專案的「專家可直接派工給執行者」開關開著時，你可以直接派工給本專案的執行者（`herdr agent prompt bot:<id> --expect-result …`，或群集 `bot:cluster:<名稱>`）並驗收結果；`agm peers` 會標 `dispatch`（`direct`＝可直接派、`via_manager`＝要經管理者）。開關關著就經管理者派工。這只涵蓋派工，不含啟動／重啟／增聘。"))
}

/// 管理者（manager）的短回合／派工段：bot 專屬，接在「## 專家模式角色」後面；不改共用的 `child_agent_rules`。
pub(crate) fn manager_role_section(role: Option<&str>) -> Option<String> {
    if role != Some("manager") {
        return None;
    }
    Some("你是管理者：收到使用者指令時，先自己判斷並找出問題——用 `codegraph_explore`、log、DB、`agm` 指令查出根因和修法，再把明確的工作（根因、要改哪裡、驗收條件）派給執行者。不要叫執行者去查根因，也不親自寫程式。查不出根因，或花了約 10 分鐘仍沒頭緒，就停止自己查，帶著已知線索問顧問（需要 repo 脈絡），或問該 track 的專家（純問答、審稿可問 claude-api 顧問）。派出去後就結束回合，結果會以通知送回，再由你驗收、整合；你在回合中時，外部請求與成員求助都只能排隊等你。不確定給誰時用 `bot:auto:cheap|standard|strong`（可加 `:<track>`），由 daemon 依閒置、額度、context 挑人；只有要指定某顆時才點名。同設定的執行者會組成群集，派工可直接用 `bot:cluster:<名稱>`（例 `herdr agent prompt bot:cluster:haiku --expect-result …`），由 daemon 挑閒置成員、全忙就排隊等第一個閒下來的；群集名稱看 `agm peers`。派工給停止的成員時會自動啟動。要調整成員的模型或強度用 `agm bot set <id> --model … --effort …`（改名用 `--name`）；改完要 `agm bot restart <id>` 才生效。專案設定的「專家可直接派工給執行者」開關（預設開）開著時，專家可直接派工給本專案執行者並驗收；同一顆執行者被同時派工照佇列排隊。關掉就只有你（或代班中的副管理者）能派工給執行者。claude-api 顧問（`agm peers` 標 billing=api）用 API 額度：純問答、審稿、草稿這類不需要讀 repo 的，優先 `herdr agent prompt bot:<id> --expect-result` 問它；需要 repo 脈絡才找其他顧問。其他 claude-api 成員不能派工。".to_string())
}

/// 執行者／顧問收到派工後的收尾規則：回合結束就等於回報，長檢查不可丟背景就結束回合。
pub(crate) const NO_BG_END_RULE: &str = "\n\n## 回合即回報\n收到派工（尤其 `--expect-result`）時，回合結束就等於回報：長時間的檢查、建置、測試不可以先回「跑完再回報」就結束回合。背景啟動時把結束碼寫進 log（例 `(scripts/check.sh changed > LOG 2>&1; echo CHECK_RC=$? >> LOG) &`），在同一回合用 `scripts/wait-log.py LOG --exit-prefix CHECK_RC= --timeout 540` 前景等，逾時就再等，直到有結果才寫最終回報並結束回合（shell 工具單次前景上限約 10 分鐘，check 常超過）。";

/// 顧問（advisor）的啟動指示：只給顧問本人，職責文字不含 bot 名稱或 id。
pub(crate) fn advisor_role_section(role: Option<&str>) -> Option<String> {
    if role != Some("advisor") {
        return None;
    }
    Some("\n\n你是本專案的顧問：不實作、不改檔、不 commit；接受任何成員的諮詢，負責回答問題、審查方案或 diff、比較設計取捨並給建議；需要動手的工作回報管理者派給執行者或專家。".to_string())
}

/// 副管理者設定後的專屬段（只給管理者與副管理者本人，其他 bot 不變）：管理者知道摘要會回來，副管理者知道自己的邊界。
pub(crate) fn deputy_role_section(bot_id: &str, role: Option<&str>, deputy_bot_id: Option<&str>) -> Option<String> {
    let deputy = deputy_bot_id?;
    if role == Some("manager") {
        Some("\n\n## 副管理者\n你設有副管理者：你在回合中太久時，排隊給你的請求會改送他處理；你閒下來後會收到他處理過哪些的摘要，需要你決策的自行接手。".to_string())
    } else if bot_id == deputy {
        Some("\n\n## 副管理者\n你是本專案的副管理者：只處理管理者忙不過來、改派給你的請求（訊息開頭會註明）。能回就直接回；需要管理者決策的明說「留給管理者」，不要自己拍板。代班期間可對成員派工與啟動／重啟，不能增聘；處理過的事會以摘要回報給管理者。".to_string())
    } else {
        None
    }
}

fn not_idle(bot_id: &str, why: &str) -> LcError {
    LcError::conflict("not_idle", json!({"bot_id": bot_id, "busy": why}))
}

fn host_superseded(bot_id: &str, host: &str) -> LcError {
    LcError::conflict("host_superseded", json!({"bot_id": bot_id, "host": host}))
}

pub async fn start_bot(app: &impl StartContext, bot_id: &str) -> LcResult<String> {
    start_bot_with(app, bot_id, StartOpts::default()).await
}

pub async fn start_bot_with(app: &impl StartContext, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    let kind = db::bot(app.db(), bot_id).await.map_err(|_| LcError::Upstream("bot_kind_unconfirmed".into()))?.ok_or_else(|| LcError::NotFound("bot".into()))?.kind;
    let host: Option<String> = sqlx::query_scalar("SELECT p.host FROM bots b JOIN projects p ON p.id=b.project_id WHERE b.id=?")
        .bind(bot_id)
        .fetch_optional(app.db())
        .await
        .ok()
        .flatten();
    if let Some(host) = host {
        let disabled: Option<i64> = sqlx::query_scalar("SELECT disabled FROM kind_prefs WHERE host=? AND kind=?")
            .bind(&host)
            .bind(&kind)
            .fetch_optional(app.db())
            .await
            .ok()
            .flatten();
        if disabled.unwrap_or(0) != 0 {
            return Err(LcError::conflict("kind_disabled", json!({"reason": "kind_disabled", "host": host, "kind": kind})));
        }
    }
    if crate::config::is_api_kind(&kind) {
        if opts.fork_session.is_some() || opts.fork_source_bot.is_some() || opts.resume_session.is_some() || opts.require_idle || opts.refuse_background_jobs {
            return Err(LcError::Upstream("api_native_start_unconfirmed".into()));
        }
        return crate::lifecycle::api_bot_port::ApiBotPort::api_native_start(app, bot_id, crate::lifecycle::api_bot_port::ApiNativeStartMode::Resume { required: opts.resume_required })
            .await.map(|result| result.run_id).map_err(|e| match e {
                crate::lifecycle::api_bot_port::ApiNativeStartError::Refused(reason) => LcError::conflict(reason, json!({"reason": reason})),
                crate::lifecycle::api_bot_port::ApiNativeStartError::ActiveRun { run_id } => LcError::conflict("api_run_already_active", json!({"reason": "api_run_already_active", "run_id": run_id})),
                crate::lifecycle::api_bot_port::ApiNativeStartError::ModelChanged { from, to } => LcError::conflict("api_model_changed", json!({
                    "reason": "api_model_changed",
                    "checkpoint_model": from,
                    "current_model": to,
                })),
                _ => LcError::Upstream("api_native_start_unconfirmed".into()),
            });
    }
    let locked_bot = bot_id.to_string();
    let _g = app.lock_bot_for_start(&locked_bot).await;
    start_bot_locked_with(app, bot_id, opts).await
}

pub async fn start_bot_locked_with(app: &impl StartContext, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    start_bot_locked_with_host_fence(app, bot_id, opts, None).await
}

/// #1465：debug 建置下 async fn 的 poll 框＝裡面每個 await 的暫存加總，呼叫端會把被呼叫的 future 整個值放進自己的框。
/// 這幾支（start／restart 鏈上的大框）在**被呼叫端**這側 `Box::pin`：呼叫端只留一個指標，future 本體在 heap。
fn start_bot_locked_with_host_fence<'a>(
    app: &'a impl StartContext,
    bot_id: &'a str,
    opts: StartOpts,
    fence: Option<&'a crate::hosts::HostFence>,
) -> impl std::future::Future<Output = LcResult<String>> + 'a {
    Box::pin(start_bot_locked_with_host_fence_body(app, bot_id, opts, fence))
}

async fn start_bot_locked_with_host_fence_body(
    app: &impl StartContext,
    bot_id: &str,
    opts: StartOpts,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<String> {
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    if crate::config::is_api_kind(&bot.kind) { return Err(LcError::Upstream("api_native_start_unconfirmed".into())); }
    if bot.managed_by == "child" {
        return Err(LcError::conflict(
            "a spawned child is started by its parent agent, not from here",
            json!({"parent_bot_id": bot.parent_bot_id}),
        ));
    }
    refuse_default_session(&bot)?;
    HandoffSessionRepo::refuse_handed_off(app, bot_id).await?;
    if let Some(existing) = db::active_run(app.db(), bot_id).await.map_err(up)? {
        return Err(LcError::conflict("active run already exists", json!({"run_id": existing.id})));
    }
    if opts.resume_native && opts.resume_required {
        let host = db::bot_host(app.db(), bot_id).await.map_err(up)?;
        if let Err(why) = native_resume_plan(app, &bot, &host, false, opts.resume_session.as_deref()).await? {
            if why.reason != "no_session_id" || unsettled_fork(app, bot_id).await.is_none() {
                return Err(cannot_resume(bot_id, why.reason));
            }
        }
    }
    let project = db::project(app.db(), &bot.project_id)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let expert_gate = db::expert_host_gate(&project.host);
    let _expert_guard = expert_gate.lock().await;
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
        if project.host != fence.conn().name || !app.hosts().is_current(fence).await {
            return Err(host_superseded(bot_id, &project.host));
        }
    }
    if let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if app.identity_for_host(&project.host, idn).await.is_none() {
            return Err(LcError::conflict(
                "identity is not known on this host",
                json!({"identity": idn, "host": project.host,
                       "hint": format!("主機 {} 沒有 `{idn}` 這個身份（config.toml 的 [[identities]] 或該機 zshrc 的 ccN alias）；先在那台建好，或到主機設定按「重新偵測」", project.host)}),
            ));
        }
        let mut not_logged_in = app.cached_identity_logged_out(&project.host, idn).await;
        if not_logged_in {
            if let Some(fresh) = app.recheck_identity_login(&project.host, idn).await {
                not_logged_in = !fresh;
            }
        }
        if not_logged_in {
            tracing::warn!(bot = %bot.name, identity = idn, host = %project.host, "explicit identity not logged in on host; refusing to start (fail closed)");
            if let Ok(conv) = db::conversation_id(app.db(), bot_id).await {
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
        if bot.kind == "claude" && project.host == crate::config::LOCAL_HOST {
            if let Some(i) = app.identity_for_host(&project.host, idn).await {
                let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                if !home.is_empty() {
                    let dir = claude_account_file_dir(&i.env, &home);
                    if crate::tools::ensure_claude_onboarded(std::path::Path::new(&dir)) {
                        tracing::info!(bot = %bot.name, identity = idn, dir, "marked claude onboarding complete so the TUI skips the login menu");
                    }
                }
            }
        }
    }

    let nudge = super::resume_nudge::prior_run_ended_busy(app, &bot, &opts).await;
    let run_id = db::ulid();
    let started_identity = bot.identity.as_deref().map(str::trim).unwrap_or("").to_string();
    let host_generation = if project.host == crate::config::LOCAL_HOST {
        None
    } else {
        app.hosts().fence(&project.host).await.and_then(|f| f.target_epoch())
    };
    let ins = sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, runtime_identity, runtime_role_policy, launch_rev, started_at, host, host_generation) VALUES (?,?,'starting','unknown',?,?,?,?,?,?,?)",
    )
    .bind(&run_id)
    .bind(bot_id)
    .bind(&session)
    .bind(&started_identity)
    .bind(format!("{}:{}:{}:{}", project.id, project.host, project.mode, bot.role.as_deref().unwrap_or("")))
    .bind(crate::launch_rev::of(&bot))
    .bind(db::now())
    .bind(&project.host)
    .bind(host_generation)
    .execute(app.db())
    .await;
    if let Err(e) = ins {
        if let Some(dbe) = e.as_database_error() {
            if dbe.is_unique_violation() {
                let existing = db::active_run(app.db(), bot_id).await.map_err(up)?.map(|r| r.id);
                return Err(LcError::conflict("active run already exists", json!({ "run_id": existing })));
            }
        }
        return Err(up(e));
    }
    app.bot_status_changed(bot_id).await;

    match start_inner(app, &bot, &project, &run_id, &session, opts.clone(), fence).await {
        Ok(()) => {
            if let Some(busy) = nudge {
                super::resume_nudge::arm(app, &bot, &run_id, busy).await;
            }
            Ok(run_id)
        }
        Err(e @ LcError::Uncommitted(_)) => {
            app.bot_status_changed(bot_id).await;
            Err(e)
        }
        Err(e) => {
            if let Err(db_err) = super::run_state::transition(app.db(), &run_id, &["starting"], "exited", None).await {
                tracing::warn!(bot = %bot.name, run = %run_id, error = %db_err, "failed start could not be recorded as exited");
                super::run_state::schedule_settle(app, &run_id, super::run_state::Settle::Reconcile { stuck: "starting".into() });
            }
            app.bot_status_changed(bot_id).await;
            Err(e)
        }
    }
}

/// Native-session continuation args. codex `resume` is a subcommand (must come first).
/// grok 1.0.34 起 `--resume <id>` 接得回（2026-09-17 herdr 0.9.0 隔離實測：server 重啟後記得先前的暗號）。
/// agy 是單一 argv `--conversation=<id>`（1.2.17 實測：`-p`／TUI、同 cwd／跨 cwd 都接得回）；**不用 `-c`**——
/// 它只認「這個 cwd 最近一段」（`cache/last_conversations.json` 一個 cwd 一格），同 cwd 的 bot 會互相搶，
/// 沒用過的 cwd 還會退到別處的舊對話。id 不存在時 agy 只警告、exit 0、悄悄開新對話，由 `resume_mismatch` 抓。
fn resume_args_by_kind(kind: &str, session_id: &str) -> Result<Vec<String>, &'static str> {
    if session_id.trim().is_empty() {
        return Err("no_session_id");
    }
    match kind {
        "claude" | "grok" => Ok(vec!["--resume".into(), session_id.into()]),
        "codex" => Ok(vec!["resume".into(), session_id.into()]),
        "agy" if super::agy_session::valid_session_id(session_id.trim()) => Ok(vec![format!("--conversation={}", session_id.trim())]),
        "agy" => Err("no_session_id"),
        _ => Err("unsupported_kind"),
    }
}

/// 這顆 kind＋session 接得回原對話嗎（閒置回收 §6.11 的前置）。跟 [`resume_args_by_kind`] 是同一份判斷，
/// 不另外維護名單（issue #1078：閒置回收的名單以前只有 claude／codex，grok／agy 永遠 `no_resume`）。
pub fn can_resume_native(kind: &str, session_id: &str) -> bool {
    resume_args_by_kind(kind, session_id).is_ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeResumeFailure {
    pub reason: &'static str,
    pub session_id: Option<String>,
}

impl NativeResumeFailure {
    fn without_session(reason: &'static str) -> Self {
        Self { reason, session_id: None }
    }

    fn for_session(reason: &'static str, session_id: &str) -> Self {
        Self { reason, session_id: Some(session_id.to_string()) }
    }
}

/// 這顆 bot 接得回哪一段原生對話：`Ok((session id, argv))`；接不回時回原因代碼與已知的目標 session
/// （`no_session_id` 沒有 session，其餘失敗保留實際嘗試的 id）。`include_active`：重啟前查——
/// 那時現在這一個 run 還沒結束，它的 session 才是要接的那段。
///
/// 換身份（`PATCH /bots/:id` 改 `identity`）後 `bot.identity` 已經是新的，但上一段對話的 jsonl
/// 還躺在**舊**身份的 `CLAUDE_CONFIG_DIR/projects/…` 下；兩個目錄不是同一份（symlink 除外）時
/// `--resume` 在新身份下找不到檔案。先把檔案複製過去再放行，複製不了就回退開新對話，不硬失敗
/// （2026-09-17 AGM 手動搶救的兩顆 bot：`resume_session_id` 一直是空的）。
pub async fn native_resume_plan(
    app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::ExePath + crate::hosts::HostInstance + crate::hosts::HostsAccess + IdentityAccess),
    bot: &db::Bot,
    host: &str,
    include_active: bool,
    override_session: Option<&str>,
) -> LcResult<Result<(String, Vec<String>), NativeResumeFailure>> {
    let last = if let Some(sid) = override_session {
        let transcript: Option<String> = sqlx::query_scalar(
            "SELECT transcript_path FROM runs WHERE bot_id = ? AND native_session_id = ? AND transcript_path IS NOT NULL
              ORDER BY started_at DESC, rowid DESC LIMIT 1",
        )
        .bind(&bot.id)
        .bind(sid)
        .fetch_optional(app.db())
        .await
        .map_err(up)?;
        Some((sid.to_string(), transcript))
    } else if include_active {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT native_session_id, transcript_path FROM runs
              WHERE bot_id = ? AND native_session_id IS NOT NULL ORDER BY started_at DESC, rowid DESC LIMIT 1",
        )
        .bind(&bot.id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?
    } else {
        db::last_native_session(app.db(), &bot.id).await.map_err(up)?
    };
    let Some((session_id, transcript)) = last else { return Ok(Err(NativeResumeFailure::without_session("no_session_id"))) };
    if session_id.trim().is_empty() {
        return Ok(Err(NativeResumeFailure::without_session("no_session_id")));
    }
    if !include_active && bot.kind == "claude" {
        crate::rewind::anchor::ensure(app, &session_id).await;
    }
    if let Some(transcript) = transcript.filter(|t| !t.trim().is_empty()) {
        if let Err(why) = stage_cross_identity_transcript(app, bot, host, &transcript).await {
            return Ok(Err(NativeResumeFailure::for_session(why, &session_id)));
        }
    }
    Ok(resume_args_by_kind(&bot.kind, &session_id)
        .map(|a| (session_id.clone(), a))
        .map_err(|why| NativeResumeFailure::for_session(why, &session_id)))
}

/// 這個身份實際用的 `CLAUDE_CONFIG_DIR`（沒設、或身份未知都算預設帳號 `~/.claude`）。
pub async fn identity_config_dir(app: &(impl crate::hosts::HostsAccess + IdentityAccess), host: &str, identity: Option<&str>) -> anyhow::Result<String> {
    let fence = app.hosts().fence(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    identity_config_dir_for_fence(app, host, identity, &fence).await
}

pub async fn identity_config_dir_for_fence(
    app: &(impl crate::hosts::HostsAccess + IdentityAccess),
    host: &str,
    identity: Option<&str>,
    fence: &crate::hosts::HostFence,
) -> anyhow::Result<String> {
    let home = crate::hosts::home_for_fence(fence).await?;
    if !app.hosts().is_current(fence).await {
        anyhow::bail!("host `{host}` changed while resolving HOME");
    }
    let dir = match identity.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => app.identity_for_host(host, name)
            .await
            .and_then(|i| i.env.get("CLAUDE_CONFIG_DIR").map(|v| crate::config::expand_home(v, &home))),
        None => None,
    };
    Ok(dir.unwrap_or_else(|| format!("{home}/.claude")))
}

/// 把 `transcript` 複製到目前身份的 `projects/<同一個 cwd 目錄名>/` 下，讓 `--resume` 在那個身份
/// 底下找得到。本機與遠端分開實作：遠端主機上舊/新 `projects/` 都在**那台機器**上，是同機複製，
/// 透過 ssh 執行一段 shell script，不是本機↔遠端搬檔（issue #95：以前只做本機這半，遠端完全跳過，
/// 換身份後 `--resume` 在遠端主機上一樣找不到檔案，只是要等 CLI 真的跑起來才會發現）。
async fn stage_cross_identity_transcript(app: &(impl crate::hosts::HostsAccess + IdentityAccess), bot: &db::Bot, host: &str, transcript: &str) -> Result<(), &'static str> {
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
async fn stage_cross_identity_transcript_local(app: &(impl crate::hosts::HostsAccess + IdentityAccess), bot: &db::Bot, transcript: &str) -> Result<(), &'static str> {
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
    if !super::transcript_stage::is_claude_transcript(src) {
        tracing::warn!(bot = %bot_name, from = %src.display(), "identity switch：來源不是 projects/<cwd>/<id>.jsonl 的一般檔，不複製，改開新對話");
        return Err("transcript_missing");
    }
    let Some(cwd_key) = cwd_dir.file_name() else { return Err("transcript_missing") };
    let dest_dir = new_projects.join(cwd_key);
    let staged = match super::transcript_stage::stage_file(src, &dest_dir, fname) {
        Ok(staged) => staged,
        Err(e) => {
            tracing::warn!(bot = %bot_name, from = %src.display(), to = %dest_dir.display(), error = %e, "identity switch：複製 session 檔到新身份失敗，改開新對話");
            return Err("transcript_missing");
        }
    };
    match &staged {
        super::transcript_stage::Staged::KeptLongerDestination => {
            tracing::info!(bot = %bot_name, dest = %dest_dir.join(fname).display(), "identity switch：新身份那邊的 session 檔已經比來源長，沿用它不覆蓋");
        }
        super::transcript_stage::Staged::Diverged { set_aside } => {
            tracing::warn!(bot = %bot_name, set_aside = %set_aside.display(), "identity switch：新身份那邊的 session 檔跟來源分岔，舊的改名留在旁邊");
        }
        _ => {}
    }
    if let Some(stem) = src.file_stem() {
        let companion_src = cwd_dir.join(stem);
        if companion_src.is_dir() {
            if let Err(e) = super::transcript_stage::copy_dir_private(&companion_src, &dest_dir.join(stem)) {
                tracing::warn!(bot = %bot_name, error = %e, "identity switch：session 附屬目錄複製失敗（不影響主對話檔）");
            }
        }
    }
    tracing::info!(bot = %bot_name, from = %src.display(), to = %dest_dir.join(fname).display(), "identity switch：session 檔已搬到新身份的 projects 目錄，可以接回對話");
    Ok(())
}

/// 跟 local 版做同一件事，但舊／新 `projects/` 都在**那台遠端主機**上：是同機複製，透過 ssh
/// 執行一段 shell script，不是本機↔遠端搬檔。主機沒連線／ssh 指令本身失敗都回 `transcript_missing`
/// ——連不上就假裝已經搬過去，比直接開新對話更糟（會讓 `--resume` 帶著錯的期待送出去）。
async fn stage_cross_identity_transcript_remote(app: &(impl crate::hosts::HostsAccess + IdentityAccess), bot: &db::Bot, host: &str, transcript: &str) -> Result<(), &'static str> {
    if !super::transcript_stage::has_claude_transcript_shape(transcript) {
        tracing::warn!(bot = %bot.name, host, transcript, "identity switch：遠端來源不是 projects/<cwd>/<id>.jsonl，不複製，改開新對話");
        return Err("transcript_missing");
    }
    let src = std::path::Path::new(transcript);
    let (Some(cwd_dir), Some(fname)) = (src.parent(), src.file_name()) else { return Err("transcript_missing") };
    let Some(old_projects) = cwd_dir.parent() else { return Err("transcript_missing") };
    let Some(cwd_key) = cwd_dir.file_name() else { return Err("transcript_missing") };
    let Some(fence) = app.hosts().fence(host).await else {
        tracing::warn!(bot = %bot.name, host, "identity switch：主機沒連線，改開新對話");
        return Err("transcript_missing");
    };
    let new_dir = match identity_config_dir_for_fence(app, host, bot.identity.as_deref(), &fence).await {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(bot = %bot.name, host, error = %e, "identity switch：無法確認遠端身份目錄，改開新對話");
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
    let Some(result) = app.hosts().run_if_current(&fence, fence.conn().ssh_exec(&script)).await else {
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
            tracing::info!(bot = %bot.name, host, "identity switch：遠端 session 檔已搬到新身份的 projects 目錄，可以接回對話");
            Ok(())
        }
        Err(why) => {
            tracing::warn!(bot = %bot.name, host, output = %out.trim(), "identity switch：遠端搬 session 檔失敗，改開新對話");
            Err(why)
        }
    }
}

/// 純函式：組出「在同一台遠端主機上把 session 檔從舊身份的 projects/ 搬到新身份」的 shell script。
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
# 跟路徑那邊永遠對不上——2026-10-04 mac_builder 十顆 claude bot 更新 Claude 重啟時全部被當成「紀錄檔不見」、開了新對話。
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
# 兩個身份共用同一個 projects（一邊是指向另一邊的 symlink；mac_builder 的 `~/.claude-cc1/projects` → `~/.claude/projects`）：
# 檔案本來就在新身份讀得到的位置，不用搬。2026-10-13 ops-web 從 cc1 換回 cc0，來源那邊是 symlink，被下面
# 「來源不能是 symlink」擋成 AM_MISSING、開了新對話；反方向（目的是 symlink）一直都回 AM_SAME。
# 只在兩邊解析後是同一個目錄時才放行，而且那個檔本身仍然不能是 symlink——沒有任何東西被複製。
if [ -d "$OLD" ] && [ -d "$NEW" ]; then
  o=$(cd -- "$OLD" 2>/dev/null && pwd -P) || o=
  n=$(cd -- "$NEW" 2>/dev/null && pwd -P) || n=
  if [ -n "$o" ] && [ "$o" = "$n" ] && [ -d "$o/$K" ] && [ ! -L "$o/$K" ] && [ -f "$o/$K/$N" ] && [ ! -L "$o/$K/$N" ]; then
    printf 'AM_SAME\n'; exit 0
  fi
fi
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

/// 這個身份的 `.claude.json` 所在目錄：設了 `CLAUDE_CONFIG_DIR` 就在裡面；沒設是 `~/.claude.json`（**不是**
/// `~/.claude/.claude.json`，同 `trust::store_path`）。#1125：以前沒設時算成 `~/.claude`，旗標永遠補不上。
fn claude_account_file_dir(identity_env: &std::collections::BTreeMap<String, String>, home: &str) -> String {
    identity_env
        .get("CLAUDE_CONFIG_DIR")
        .map(|d| d.trim())
        .filter(|d| !d.is_empty())
        .map(|d| crate::config::expand_home(d, home))
        .unwrap_or_else(|| home.to_string())
}

fn cannot_resume(bot_id: &str, why: &str) -> LcError {
    LcError::conflict("cannot_resume", json!({"bot_id": bot_id, "resumed": false, "resume_reason": why}))
}

/// 分出新 session 的參數。claude、grok 是 `--resume <id> --fork-session`；codex 的 `fork` 跟 `resume`
/// 一樣是子命令，要排最前面（三家 CLI 的 help 2026-09-14 實測）。
pub fn fork_args_by_kind(kind: &str, session_id: &str) -> Result<Vec<String>, &'static str> {
    if session_id.trim().is_empty() {
        return Err("no_session_id");
    }
    match kind {
        "claude" | "grok" => Ok(vec!["--resume".into(), session_id.into(), "--fork-session".into()]),
        "codex" => Ok(vec!["fork".into(), session_id.into()]),
        _ => Err("unsupported_kind"),
    }
}

/// 這顆 bot 之前從來沒有 run、對話裡也沒有任何 user／assistant 訊息（#1001）。`run_id` 是這次啟動剛插進去的那一列，不算。
/// #1761：這顆 bot 是 fork 出來的（`fork_ops` 有它），但任何一代都還沒記到自己的 native session——第一代還沒回報就被重啟
/// （剛 fork 就換身份；遠端 hook 經 spool 晚幾秒才到）。回傳 fork 的來源 session 與來源 bot，讓這次啟動再從來源分一次；
/// 第一代沒有回合（有回合就會有 session），重分一次的脈絡跟原本一樣。查不到（舊 DB 沒有這張表）當作不是。
async fn unsettled_fork(app: &impl Db, bot_id: &str) -> Option<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT session_id, source_bot_id FROM fork_ops
          WHERE target_bot_id = ? AND state IN ('created','started') AND session_id != ''
            AND NOT EXISTS (SELECT 1 FROM runs WHERE bot_id = ? AND native_session_id IS NOT NULL AND native_session_id != '')
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(bot_id)
    .bind(bot_id)
    .fetch_optional(app.db())
    .await
    .ok()
    .flatten()
}

/// 這顆 bot 還在等它的 fork（config 的 `fork_from`）：沒有自己的 session，且來源 codex bot 有 session → `(來源的 session, 來源 bot)`。
/// 有自己的 session 了就不再分岔（那是重啟，走 resume）。
test_visible_item! { pub(crate),
async fn pending_fork(app: &(impl Cfg + Db), bot: &db::Bot) -> anyhow::Result<Option<(String, String)>> {
    let cfg = app.cfg().get().await;
    let Some(source) = cfg
        .projects
        .iter()
        .flat_map(|p| p.bots.iter())
        .find(|b| b.id.as_deref() == Some(bot.id.as_str()))
        .and_then(|b| b.fork_from.clone())
    else {
        return Ok(None);
    };
    if db::latest_native_session_id(app.db(), &bot.id).await?.is_some() {
        return Ok(None);
    }
    match db::latest_native_session_id(app.db(), &source).await? {
        Some(sid) => Ok(Some((sid, source))),
        None => {
            tracing::warn!(bot = %bot.name, source = %source, "fork source has no session; starting without a fork");
            Ok(None)
        }
    }
}
}

async fn is_first_contact(app: &impl Db, bot: &db::Bot, run_id: &str) -> LcResult<bool> {
    let conv = db::conversation_id(app.db(), &bot.id).await.map_err(up)?;
    let seen: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM runs WHERE bot_id = ? AND id <> ?)
              + (SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND role IN ('user','assistant'))",
    )
    .bind(&bot.id)
    .bind(run_id)
    .bind(&conv)
    .fetch_one(app.db())
    .await
    .map_err(up)?;
    Ok(seen == 0)
}

/// The last native session could not be continued, so this start opens a new conversation.
///
/// 這件事一定要在聊天室裡看得見：以前只寫一行 `tracing::info!`，使用者（或 AGM）看到的是
/// 一顆重啟成功、狀態正常的 bot，實際上脈絡已經悄悄斷了，要等到回覆內容不對勁才會發現
/// （issue #92：換身份接不回原對話那次，靠人工翻 log 才查到；`resume_mismatch`——hook 回報的
/// session 跟預期的不是同一個——更隱蔽，CLI 自己開了新對話卻沒有任何錯誤）。同一交易寫入系統訊息與
/// 對應的 session metadata，前端仍直接顯示一般系統訊息。
pub async fn context_lost(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit + StartServices), bot: &db::Bot, why: &str, failed_session: Option<&str>) -> LcResult<()> {
    tracing::warn!(bot = %bot.name, why, failed_session = ?failed_session, "native session continuation unavailable; starting a new conversation");
    let reason = match why {
        "no_session_id" => "沒有記到上一段對話的 session id",
        "transcript_missing" => "上一段對話的紀錄檔不見了（換身份時可能沒搬過去，或檔案被清掉）",
        "resume_mismatch" => "接回去之後，實際回報的對話跟原本要接的不是同一個",
        "unsupported_kind" => "這個 CLI 種類不支援接續原生對話",
        other => other,
    };
    let conv = db::conversation_id(app.db(), &bot.id).await.map_err(up)?;
    let failed_session = failed_session.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let mut tx = db::begin_write(app.db()).await.map_err(up)?;
    let msg = insert_message_tx(
        &mut tx,
        &conv,
        None,
        "system",
        &format!("{CONTEXT_LOST_PREFIX}（{reason}），已經開了新的對話——前面的脈絡沒有帶過來。"),
        "system",
        false,
        None,
    )
    .await
    .map_err(up)?;
    let notice_id = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO context_lost_notices (id, bot_id, native_session_id, message_id, created_at, retired_at)
         VALUES (?, ?, ?, ?, ?, NULL)",
    )
    .bind(&notice_id)
    .bind(&bot.id)
    .bind(&failed_session)
    .bind(&msg.id)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    app.emit_lifecycle_event("message_added", Some(&bot.id), json!({ "bot_id": bot.id, "message": msg })).await;

    Ok(())
}

/// [`context_lost`] 給使用者看的通知開頭。
pub const CONTEXT_LOST_PREFIX: &str = "⚠️ 接不回原本的對話";

/// 之後真的接回了 `session_id` 那段對話（resume 驗證為 `verified`）：那段對話中斷後插的「接不回」通知已經不是事實，撤掉
/// （issue #852：依 session id 精確撤銷，避免撤掉其他 session 仍有效的 context-loss 警告）。
pub async fn retire_context_lost(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit + StartServices), bot_id: &str, session_id: &str) {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return;
    }
    let mut tx = match db::begin_write(app.db()).await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = %e, "could not begin tx to retire context-lost notes");
            return;
        }
    };

    let notes: Vec<(String, String)> = match sqlx::query_as(
        "SELECT id, message_id FROM context_lost_notices
          WHERE bot_id = ? AND native_session_id = ? AND retired_at IS NULL",
    )
    .bind(bot_id)
    .bind(session_id)
    .fetch_all(&mut *tx)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.rollback().await;
            tracing::warn!(bot = %bot_id, error = %e, "could not query context-lost notices");
            return;
        }
    };

    if notes.is_empty() {
        let _ = tx.rollback().await;
        return;
    }

    let now = db::now();
    let mut deleted = 0;
    for (notice_id, message_id) in &notes {
        if let Err(e) = sqlx::query("UPDATE context_lost_notices SET retired_at = ? WHERE id = ? AND retired_at IS NULL")
            .bind(&now)
            .bind(notice_id)
            .execute(&mut *tx)
            .await
        {
            let _ = tx.rollback().await;
            tracing::warn!(bot = %bot_id, error = %e, "could not mark context-lost notice as retired");
            return;
        }
        match sqlx::query("DELETE FROM messages WHERE id = ?")
            .bind(message_id)
            .execute(&mut *tx)
            .await
        {
            Ok(result) => deleted += result.rows_affected(),
            Err(e) => {
                let _ = tx.rollback().await;
                tracing::warn!(bot = %bot_id, error = %e, "could not remove context-lost message");
                return;
            }
        }
    }

    if let Err(e) = tx.commit().await {
        tracing::warn!(bot = %bot_id, error = %e, "could not commit retirement of context-lost notes");
        return;
    }

    tracing::info!(bot = %bot_id, session = %session_id, n = notes.len(), deleted, "resumed the lost conversation; retired its context-lost notes");
    if deleted > 0 {
        app.emit_lifecycle_event("resync", None, json!({"reason": "context_lost_retired", "bot_id": bot_id})).await;
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
pub async fn close_tab_if_empty(client: &crate::herdr::HerdrClient, workspace_id: &str, tab_id: &str) {
    let tabs = match client.tab_list(workspace_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(workspace_id, tab_id, error = %e, "tab.list failed; leaving the tab alone");
            return;
        }
    };
    match tabs.iter().find(|t| t.tab_id == tab_id) {
        None => {}
        Some(t) if t.pane_count == 0 => {
            if let Err(e) = client.tab_close(tab_id).await {
                tracing::debug!(tab_id, error = %e, "tab.close failed");
            }
        }
        Some(_) => {}
    }
}

/// Close a run's pane and its tab if now empty; `tab_id` is `None` for pre one-bot-one-tab runs.
pub async fn close_pane_and_tab(
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

/// 母 bot 的完整啟動指示放在哪裡。herdr 把整條指令壓在 900 bytes 內，整份放進 argv 會被砍成「…（後略）」，
/// 所以 claude／grok 走檔案、codex 走 profile（SPEC §6.5i）。
enum StagedPersona {
    /// 受限 bot（cage 自己有檔）或沒有 persona argv 的 kind（agy）。
    NotNeeded,
    /// claude／grok＝檔案路徑，codex＝profile 名。
    Staged(String),
    /// codex bot 自己的參數已有 `-p`／`--profile`：不覆蓋它，指示照舊放 argv（會被截斷），訊息說明原因。
    KeptInline(String),
    /// 寫不進去：呼叫端 fail closed。
    Failed,
    /// Codex profile 的固定安全診斷，讓使用者修復原檔而不洩漏內容。
    FailedWithReason(String),
}

impl StagedPersona {
    fn reference(&self) -> Option<&str> {
        match self {
            StagedPersona::Staged(s) => Some(s),
            _ => None,
        }
    }
}

/// args 裡有沒有 codex 的 `-p <name>`／`--profile <name>`／`--profile=<name>`。
fn has_profile_arg<'a>(args: impl IntoIterator<Item = &'a String>) -> bool {
    args.into_iter().any(|a| a == "-p" || a == "--profile" || a.starts_with("--profile="))
}

/// codex 母 bot 那台主機的 CODEX_HOME：pane env 已經合併過身份與 bot 自己的 env（`~` 展開成那台主機的 HOME），有就用；
/// 都沒有才退回 `<那台的 HOME>/.codex`。遠端 HOME 讀不到就回 None（不借用 daemon 這台的 HOME）。
async fn codex_home_dir(app: &impl HostsAccess, project: &db::Project, env: &Value) -> Option<String> {
    if let Some(v) = env.get("CODEX_HOME").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        return Some(v.to_string());
    }
    let home = if project.host == crate::config::LOCAL_HOST {
        crate::home::dir()?.to_string_lossy().into_owned()
    } else {
        app.hosts().get(&project.host).await?.home().await.ok()?
    };
    Some(format!("{}/.codex", home.trim_end_matches('/')))
}

/// Build the fork-specific suffix from the durable fork operation and current source children.
async fn fork_identity_note(pool: &sqlx::SqlitePool, bot_id: &str) -> Result<Option<String>, sqlx::Error> {
    let source: Option<(String, String, String)> = sqlx::query_as(
        "SELECT b.id, b.name, p.label FROM fork_ops f JOIN bots b ON b.id=f.source_bot_id JOIN projects p ON p.id=b.project_id WHERE f.target_bot_id=? ORDER BY f.created_at DESC LIMIT 1",
    ).bind(bot_id).fetch_optional(pool).await?;
    let Some((source_id, source_name, label)) = source else { return Ok(None) };
    let source_agent: Option<String> = sqlx::query_scalar("SELECT agent_name FROM runs WHERE bot_id=? AND agent_name IS NOT NULL ORDER BY rowid DESC LIMIT 1")
        .bind(&source_id).fetch_optional(pool).await?;
    let source_agent = source_agent.unwrap_or_else(|| crate::config::agent_name(&label, &source_id));
    let children: Vec<String> = sqlx::query_scalar(
        "SELECT r.agent_name FROM bots b JOIN runs r ON r.rowid=(SELECT r2.rowid FROM runs r2 WHERE r2.bot_id=b.id AND r2.agent_name IS NOT NULL ORDER BY r2.rowid DESC LIMIT 1) WHERE b.parent_bot_id=? AND b.deleted_at IS NULL ORDER BY r.agent_name",
    ).bind(&source_id).fetch_all(pool).await?;
    let children = if children.is_empty() { "（目前沒有已知 child）".to_string() } else { children.join("、") };
    Ok(Some(format!("你是從 {source_name}（agent 名 {source_agent}）fork 出來的新 bot。來源對話裡的 child {children} 屬於來源 bot，不屬於你。派工或操作 child 前，先確認 `$AM_AGENT_NAME`，並用 `herdr agent list` 確認 child 屬於自己。")))
}

/// 啟動用的模型：agy 先確認設定的模型在這個身份的清單裡（[`agy_model_guard`]），再照 effort 校正（[`effort_checked`]）。
async fn launch_model_bot(app: &impl StartContext, bot: &db::Bot, host: &str) -> db::Bot {
    let bot = agy_model_guard(app, bot, host).await;
    effort_checked(app, &bot, host).await
}

/// agy 的模型是 slug，換帳號（身份＝換 `HOME`）可用的不一樣：設定的模型不在「這台＋這個身份」剛抓到的清單裡，就不靜靜帶進去
/// （agy 的互動模式遇到不認得的 `--model` 只警告、退回它自己的預設，畫面仍顯示設定的值，2026-11-20）。
/// 改用該身份清單的預設啟動，並在對話裡留一則系統訊息；`bots.model` 不動，不一致由既有的「設定與實際不同」標記顯示。
/// 只在新鮮、本機抓到的清單（`agy-cli`）上判斷；借來的、寫死的清單不拿來說這個帳號不支援。
pub(super) async fn agy_model_unlisted(app: &impl StartContext, bot: &db::Bot, host: &str, model: &str) -> Option<String> {
    if bot.kind != "agy" {
        return None;
    }
    let list = app.models_list(host, "agy", bot.identity.as_deref(), false).await.ok()?;
    if list["source"] != "agy-cli" || list["stale"].as_bool() == Some(true) {
        return None;
    }
    let models = list["models"].as_array()?;
    crate::models::agy_model_fallback(models, model)
}

async fn agy_model_guard(app: &impl StartContext, bot: &db::Bot, host: &str) -> db::Bot {
    let Some(model) = bot.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) else { return bot.clone() };
    let Some(fallback) = agy_model_unlisted(app, bot, host, model).await else { return bot.clone() };
    let who = match bot.identity.as_deref().filter(|s| !s.is_empty()) {
        Some(name) => format!("身份 {name}"),
        None => "預設帳號".to_string(),
    };
    let notice = format!("設定的 {model} 在此帳號（{who}）不支援，已改用 {fallback} 啟動。要換模型請到設定頁選這個帳號可用的模型。");
    tracing::warn!(bot = %bot.name, model, fallback = %fallback, "agy model is not in this identity's list; starting with its default");
    match db::conversation_id(app.db(), &bot.id).await {
        Ok(conv) => {
            if let Err(e) = crate::lifecycle::messages::insert_message(app, &conv, None, "system", &notice, "system", false, None).await {
                tracing::warn!(bot = %bot.name, error = %e, "could not record the agy model fallback notice");
            }
        }
        Err(e) => tracing::warn!(bot = %bot.name, error = %e, "could not find the conversation for the agy model fallback notice"),
    }
    let mut out = bot.clone();
    out.model = Some(fallback);
    out
}

#[allow(clippy::too_many_arguments)]
async fn stage_persona(
    app: &impl HostsAccess,
    bot: &db::Bot,
    project: &db::Project,
    shim_dir: Option<&str>,
    env: &Value,
    agent: &str,
    agent_md: Option<&str>,
    identity_args: &[String],
    fork_note: Option<&str>,
) -> StagedPersona {
    let staged = |name: Option<String>| name.map(StagedPersona::Staged).unwrap_or(StagedPersona::Failed);
    let full_persona = || {
        let mut full = super::setup::persona_text(bot, agent, agent_md);
        if let Some(note) = fork_note { full.push_str("\n\n"); full.push_str(note); }
        full
    };
    match bot.kind.as_str() {
        "claude" | "grok" => {
            let full = full_persona();
            let path = if bot.kind == "grok" {
                super::agent_md::install_grok_rules(app, bot, project, shim_dir, &full).await
            } else {
                super::agent_md::install_persona(app, bot, project, shim_dir, &full).await
            };
            staged(path)
        }
        "codex" => {
            let own = bot.args();
            if has_profile_arg(identity_args.iter().chain(own.iter())) {
                if fork_note.is_some() { return StagedPersona::FailedWithReason("fork 的 Codex 自訂 profile 無法注入完整身分提醒；請移除 -p／--profile".into()); }
                return StagedPersona::KeptInline(
                    "這顆 codex bot 的參數已指定 -p／--profile，完整啟動指示無法走 profile；改以 -c developer_instructions= 帶入，\
                     會被 herdr 的 900 bytes 指令上限截斷（只剩 AG Man 規則的前段）。要完整指示請拿掉 -p／--profile"
                        .to_string(),
                );
            }
            let Some(home) = codex_home_dir(app, project, env).await else { return StagedPersona::Failed };
            let full = full_persona();
            match super::agent_md::install_codex_profile(app, bot, project, &home, &full).await {
                Ok(name) => StagedPersona::Staged(name),
                Err(error) => StagedPersona::FailedWithReason(error.to_string()),
            }
        }
        _ => StagedPersona::NotNeeded,
    }
}

/// issue #748：codex 啟動時要不要帶 `-c features.instant_interrupt=true`。`[codex] instant_interrupt` 開著才帶；
/// 受限的分享 bot 不帶（它的參數由籠子決定）。回傳空＝沒帶，`runs.codex_instant_interrupt` 照這個結果記 0／1。
pub(crate) fn claude_exclude_dynamic_args(args: &[String]) -> Vec<String> {
    if args.iter().any(|a| a == "--exclude-dynamic-system-prompt-sections") {
        Vec::new()
    } else {
        vec!["--exclude-dynamic-system-prompt-sections".into()]
    }
}

pub(crate) fn claude_fresh_session_id(args: &[String]) -> Option<String> {
    const CONFLICTS: [&str; 7] = ["--resume", "-r", "--continue", "-c", "--session-id", "--from-pr", "--fork-session"];
    if args.iter().any(|a| CONFLICTS.contains(&a.as_str()) || CONFLICTS.iter().any(|c| a.starts_with(&format!("{c}=")))) {
        return None;
    }
    let mut bytes = ulid::Ulid::new().0.to_be_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// issue #748：啟動 codex 時在執行的那台主機跑一次 `codex --version`（登入 shell 解析同 [`ensure_kind_installed`]，限時 5 秒）。
/// 版本寫進 `runs.cli_version` 並記進記憶體：`--resume` 起的 run 畫面沒有 banner、daemon 重啟後記憶體是空的，兩者都靠這一筆。
/// 探不到就略過、不擋啟動（閘門照舊回版本未知）；開機畫面讀到的版本之後會覆寫它。
async fn record_codex_cli_version(app: &impl StartContext, host: &str, run_id: &str) {
    let script = crate::codex_live::version_probe_command();
    let out = if host == LOCAL_HOST {
        crate::hosts::sh_local_stdout(&script, Duration::from_secs(5), "codex --version").await.ok()
    } else {
        match app.hosts().get(host).await {
            Some(conn) => conn.ssh_exec_path_timeout(&script, Duration::from_secs(5)).await.ok(),
            None => None,
        }
    };
    let Some(version) = out.as_deref().and_then(am_base::changelog::cli_version_string) else {
        tracing::debug!(run = run_id, host, "codex --version gave no version; leaving it unknown");
        return;
    };
    crate::codex_update::remember_cli_version(run_id, &version);
    if let Err(e) = sqlx::query("UPDATE runs SET cli_version = ? WHERE id = ?").bind(&version).bind(run_id).execute(app.db()).await {
        tracing::warn!(run = run_id, error = %e, "could not record the codex cli version");
    }
}

pub(crate) fn codex_instant_interrupt_args(flag_on: bool, restricted: bool) -> Vec<String> {
    if flag_on && !restricted {
        vec!["-c".into(), "features.instant_interrupt=true".into()]
    } else {
        Vec::new()
    }
}

pub(super) async fn record_daemon_url_injected(pool: &sqlx::SqlitePool, run_id: &str, env: &Value) -> LcResult<()> {
    let injected = env.get("AM_DAEMON_URL").and_then(|value| value.as_str()).is_some_and(|value| !value.is_empty());
    sqlx::query("UPDATE runs SET daemon_url_injected=? WHERE id=?").bind(i64::from(injected)).bind(run_id).execute(pool).await.map_err(up)?;
    Ok(())
}

fn start_inner<'a>(
    app: &'a impl StartContext,
    bot: &'a db::Bot,
    project: &'a db::Project,
    run_id: &'a str,
    session: &'a str,
    opts: StartOpts,
    fence: Option<&'a crate::hosts::HostFence>,
) -> impl std::future::Future<Output = LcResult<()>> + 'a {
    Box::pin(start_inner_body(app, bot, project, run_id, session, opts, fence))
}

async fn start_inner_body(
    app: &impl StartContext,
    bot: &db::Bot,
    project: &db::Project,
    run_id: &str,
    session: &str,
    opts: StartOpts,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<()> {
    let host = project.host.clone();
    let ready_fence = app.hosts().fence(&host).await;
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
    if let Err(reason) = ensure_kind_installed(app, &host, &bot.kind).await {
        let conv = db::conversation_id(app.db(), &bot.id).await.map_err(up)?;
        let _ = insert_message(app, &conv, None, "system", &reason, "system", false, None).await;
        return Err(LcError::Bad(reason));
    }
    if let Some(warning) = app.claude_hook_preflight(bot, &host).await {
        tracing::warn!(bot = %bot.name, host = %host, "{warning}");
        if let Ok(conv) = db::conversation_id(app.db(), &bot.id).await {
            let _ = insert_message(app, &conv, None, "system", &warning, "system", false, None).await;
        }
    }
    let agent = crate::config::agent_name(&project.label, &bot.id);
    let restricted = app.prepare_restricted_bot(bot, &host).await?;
    let shim_dir = if restricted.is_some() { None } else { install_shim(app, bot, project).await };
    let env = app.pane_env(bot, &host, run_id, &agent, shim_dir.as_deref(), fence).await.map_err(up)?;
    let env = match &restricted {
        Some(_) => {
            let mut caged = env;
            app.cage_environment(&mut caged, bot, &host).await;
            caged
        }
        None => env,
    };
    let mut agent_md = if restricted.is_some() { Default::default() } else { super::agent_md::load(app, project).await };
    let child_base_text = agent_md.text.clone();
    if restricted.is_none() && project.mode == "expert" {
        let role = bot.role.as_deref().unwrap_or("未分配");
        agent_md.text.push_str(&format!("\n\n## 專家模式角色\n你是 bot ID {}，角色：{}。專家模式沒有 child：任何角色都不能開 child。管理者把工作派給同專案的頂層執行者、向顧問請教；執行者與顧問需要人手時向管理者請求；管理者可用 `agm hire <名稱> --kind codex|claude` 增聘執行者、`agm bot start|stop <id>` 啟動或收掉執行者，顧問由使用者建立。用 `agm peers` 查同專案同事；要對方給結果時用 `herdr agent prompt bot:<id> --expect-result \"…\"`；結果會以「請求結果通知」送回；查進度用 `agm requests --open`。查程式碼先用 `codegraph_explore`，再用 grep／Read 補細節。", bot.id, role));
        if let Some(extra) = manager_role_section(bot.role.as_deref()) {
            agent_md.text.push_str(&extra);
        }
        if let Some(extra) = advisor_role_section(bot.role.as_deref()) {
            agent_md.text.push_str(&extra);
        }
        if matches!(bot.role.as_deref(), Some("executor" | "advisor")) {
            agent_md.text.push_str(NO_BG_END_RULE);
        }
        if let Some(extra) = deputy_role_section(&bot.id, bot.role.as_deref(), project.deputy_bot_id.as_deref()) {
            agent_md.text.push_str(&extra);
        }
    }
    if restricted.is_none() {
        if let Some(section) = specialist_role_section(bot.role.as_deref(), bot.track.as_deref()) {
            agent_md.text.push_str(&section);
        }
    }
    if !agent_md.problems.is_empty() {
        let mut reason = format!("agent md 有問題，這次啟動沒帶到：{}", agent_md.problems.join("；"));
        if !agent_md.unreadable.is_empty() {
            reason.push_str("。因為有指示檔讀不到，這次不關 CLI 自己的指示檔（CLAUDE.md／AGENTS.md 照常讀），免得 bot 兩邊都沒有規則");
        }
        tracing::warn!(bot = %bot.name, "{reason}");
        if let Ok(conv) = db::conversation_id(app.db(), &bot.id).await {
            let _ = insert_message(app, &conv, None, "system", &reason, "system", false, None).await;
        }
    }
    if !agent_md.unreadable.is_empty() {
        super::agent_md::push_unreadable_alert(app, bot, &agent_md.unreadable).await;
    }
    let mut env = env;
    record_daemon_url_injected(app.db(), run_id, &env).await?;
    if agent_md.cli_docs_disabled() {
        if let Some(map) = env.as_object_mut() {
            map.insert("CLAUDE_CODE_DISABLE_CLAUDE_MDS".into(), serde_json::json!("1"));
        }
    } else if agent_md.configured {
        if let Some(map) = env.as_object_mut() {
            map.insert("AM_KEEP_CLI_DOCS".into(), serde_json::json!("1"));
        }
    }
    let child_md = super::setup::child_instructions(&child_base_text);
    let child_instructions_path = if agent_md.configured || !agent_md.text.is_empty() || bot.kind == "grok" {
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
    let identity_args_v = if restricted.is_some() { Vec::new() } else { identity_args(app, bot, &project.host).await };
    let fork_note = if restricted.is_some() { None } else { fork_identity_note(app.db(), &bot.id).await.map_err(up)? };
    let staged_persona = if restricted.is_some() {
        StagedPersona::NotNeeded
    } else {
        stage_persona(app, bot, project, shim_dir.as_deref(), &env, &agent, (agent_md.configured || !agent_md.text.is_empty()).then_some(agent_md.text.as_str()), &identity_args_v, fork_note.as_deref())
            .await
    };
    let refuse = match (&staged_persona, bot.kind.as_str()) {
        (StagedPersona::FailedWithReason(reason), _) => Some(reason.clone()),
        (StagedPersona::Failed, kind) => Some(format!("無法把 {kind} 的完整啟動指示寫進 bot 目錄；拒絕以遭截斷的系統提示啟動")),
        (_, "grok") if !child_instructions_ready => Some("無法把 Grok 的完整啟動指示寫進 bot 目錄；拒絕以遭截斷的 --rules 啟動".to_string()),
        _ => None,
    };
    if let Some(reason) = refuse {
        tracing::error!(bot = %bot.name, "{reason}");
        if let Ok(conv) = db::conversation_id(app.db(), &bot.id).await {
            let _ = insert_message(app, &conv, None, "system", &reason, "system", false, None).await;
        }
        return Err(LcError::Upstream(reason));
    }
    if let StagedPersona::KeptInline(note) = &staged_persona {
        tracing::warn!(bot = %bot.name, "{note}");
        if let Ok(conv) = db::conversation_id(app.db(), &bot.id).await {
            let _ = insert_message(app, &conv, None, "system", note, "system", false, None).await;
        }
    }
    if restricted.is_none() {
        install_herdr_skill(app, bot, project, &env, &agent).await;
    }

    let injected = injected_args(app, bot, project, &env).await.map_err(up)?;
    let mut args = injected;
    if let Some(ws) = &restricted {
        let prompt = app.install_restricted_prompt(bot, &host, ws, &env).await.map_err(up)?;
        args.extend(app.restricted_launch_args(&env, std::path::Path::new(&prompt)));
        let overlay = super::runtime_overlay::overlay_for(app, bot).await.map_err(up)?;
        let mut model_bot = launch_model_bot(app, bot, &project.host).await;
        if overlay.model.is_some() {
            model_bot.model = overlay.model.clone();
        }
        args.extend(model_args(&model_bot));
        if let Err(e) = sqlx::query("UPDATE runs SET policy_rev = ? WHERE id = ?")
            .bind(super::runtime_overlay::policy_rev_of(&overlay.eff))
            .bind(run_id)
            .execute(app.db())
            .await
        {
            tracing::warn!(bot = %bot.name, error = %e, "could not record the runtime policy revision for this run");
        }
    } else {
        args.extend(persona_args_cli_docs(bot, &agent, (agent_md.configured || !agent_md.text.is_empty()).then_some(agent_md.text.as_str()), staged_persona.reference(), agent_md.cli_docs_disabled()));
        let overlay = super::runtime_overlay::overlay_for(app, bot).await.map_err(up)?;
        let mut model_bot = launch_model_bot(app, bot, &project.host).await;
        if overlay.model.is_some() {
            model_bot.model = overlay.model.clone();
        }
        args.extend(model_args(&model_bot));
        args.extend(identity_args_v);
        args.extend(bot.args());
        args.extend(overlay.args.clone());
        if let Err(e) = sqlx::query("UPDATE runs SET policy_rev = ? WHERE id = ?")
            .bind(super::runtime_overlay::policy_rev_of(&overlay.eff))
            .bind(run_id)
            .execute(app.db())
            .await
        {
            tracing::warn!(bot = %bot.name, error = %e, "could not record the runtime policy revision for this run");
        }
    }
    if bot.kind == "codex" {
        let ii = codex_instant_interrupt_args(app.cfg().get().await.codex.instant_interrupt, restricted.is_some());
        if let Err(e) = sqlx::query("UPDATE runs SET codex_instant_interrupt = ? WHERE id = ?").bind(i64::from(!ii.is_empty())).bind(run_id).execute(app.db()).await {
            tracing::warn!(bot = %bot.name, error = %e, "could not record whether codex was launched with instant_interrupt");
        }
        args.extend(ii);
        record_codex_cli_version(app, &host, run_id).await;
    }

    let mut opts = opts;
    if opts.fork_session.is_none() {
        if let Some((sid, source)) = pending_fork(app, bot).await.map_err(up)? {
            opts.fork_session = Some(sid);
            opts.fork_source_bot = Some(source);
        }
    }
    let resume = if opts.resume_native {
        match native_resume_plan(app, bot, &host, false, opts.resume_session.as_deref()).await? {
            Ok(plan) => Some(plan),
            Err(failure) if failure.reason == "no_session_id" && opts.fork_session.is_none() && unsettled_fork(app, &bot.id).await.is_some() => {
                let (from, source) = unsettled_fork(app, &bot.id).await.unwrap_or_default();
                tracing::info!(bot = %bot.name, from = %from, "fork has no session of its own yet; forking again from its source");
                opts.fork_session = Some(from);
                opts.fork_source_bot = Some(source);
                None
            }
            Err(failure) => {
                if opts.resume_required {
                    return Err(cannot_resume(&bot.id, failure.reason));
                }
                if failure.reason == "no_session_id" && is_first_contact(app, bot, run_id).await? {
                    tracing::info!(bot = %bot.name, "first start of a new bot; no earlier conversation to resume");
                } else {
                    context_lost(app, bot, failure.reason, failure.session_id.as_deref()).await?;
                }
                None
            }
        }
    } else {
        None
    };
    if let Some(from) = opts.fork_session.as_deref() {
        let fork = fork_args_by_kind(&bot.kind, from).map_err(|why| LcError::Bad(format!("cannot fork: {why}")))?;
        if bot.kind == "claude" {
            crate::rewind::anchor::ensure(app, from).await;
            let source = opts.fork_source_bot.as_deref().ok_or_else(|| LcError::Bad("cannot fork: 沒有來源 bot".into()))?;
            let transcript: Option<String> = sqlx::query_scalar(
                "SELECT transcript_path FROM runs WHERE bot_id = ? AND native_session_id = ? AND transcript_path IS NOT NULL AND transcript_path != ''
                  ORDER BY started_at DESC, rowid DESC LIMIT 1",
            )
            .bind(source)
            .bind(from)
            .fetch_optional(app.db())
            .await
            .map_err(up)?;
            let Some(t) = transcript.filter(|t| !t.trim().is_empty()) else {
                return Err(LcError::conflict("no_source_transcript", json!({"session_id": from, "message": "來源對話的檔案位置沒有記下來，無法確認新身份底下找得到這段對話，不啟動。"})));
            };
            if let Err(why) = stage_cross_identity_transcript(app, bot, &host, &t).await {
                return Err(LcError::conflict("fork_transcript_unavailable", json!({"session_id": from, "cause": why})));
            }
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
            .execute(app.db())
            .await
            .map_err(up)?;
        if bot.kind == "claude" {
            super::resume_context::restore(app.db(), &bot.id, run_id, &session_id, crate::models::model_effort_from_argv(&bot.kind, &args).0.as_deref()).await.map_err(up)?;
        }
    }
    args.extend(codex_pane_guard_args(&bot.kind));
    if bot.kind == "claude" {
        if app.cfg().get().await.claude.exclude_dynamic_system_prompt {
            args.extend(claude_exclude_dynamic_args(&args));
        }
        if let Some(sid) = claude_fresh_session_id(&args) {
            sqlx::query("UPDATE runs SET native_session_id = ? WHERE id = ?").bind(&sid).bind(run_id).execute(app.db()).await.map_err(up)?;
            args.extend(["--session-id".to_string(), sid]);
        }
    }

    let cwd = restricted.as_deref().unwrap_or_else(|| bot_cwd(bot, project));
    for w in app.pretrust_for_start(bot, &project.host, cwd).await {
        tracing::warn!(bot = %bot.name, cwd, warning = %w, "could not pre-trust the working directory");
    }
    if bot.kind == "claude" {
        for dir in add_dir_args(&args) {
            for w in app.pretrust_for_start(bot, &project.host, dir).await {
                tracing::warn!(bot = %bot.name, dir, warning = %w, "could not pre-trust an --add-dir directory");
            }
        }
    }

    let mut fresh_root: Option<(crate::herdr::PaneInfo, &str)> = None;
    let workspace_id = match (session != "default", project.workspace_id.as_deref()) {
        (true, Some(ws)) if client.workspace_get(ws).await.map_err(up)?.is_some() => ws.to_string(),
        _ => {
            let (ws, root) = client.workspace_create(&project.path, &project.label, env.clone()).await.map_err(up)?;
            if session != "default" {
                sqlx::query("UPDATE projects SET workspace_id = ? WHERE id = ?")
                    .bind(&ws.workspace_id)
                    .bind(&project.id)
                    .execute(app.db())
                    .await
                    .map_err(up)?;
            }
            fresh_root = Some((root, project.path.as_str()));
            ws.workspace_id
        }
    };

    let root = acquire_run_pane(&client, &workspace_id, cwd, &tab_label(bot), &env, fresh_root).await.map_err(up)?;
    let pane_id = root.pane_id;
    let tab_id = root.tab_id;
    let mut pane_guard = StartPaneGuard::new(&client, &workspace_id, &tab_id, &pane_id);

    pane_guard
        .protect(
            sqlx::query("UPDATE runs SET workspace_id = ?, pane_id = ?, tab_id = ? WHERE id = ?")
                .bind(&workspace_id)
                .bind(&pane_id)
                .bind(&tab_id)
                .bind(run_id)
                .execute(app.db())
                .await
                .map_err(up),
        )
        .await?;

    pane_guard
        .protect(
            sqlx::query("UPDATE runs SET agent_name = ? WHERE id = ?")
                .bind(&agent)
                .bind(run_id)
                .execute(app.db())
                .await
                .map_err(up),
        )
        .await?;
    let (rt_model, rt_effort) = crate::models::model_effort_from_argv(&bot.kind, &args);
    let rt_fast = i64::from(args.iter().any(|a| a.contains("service_tier=\"priority\"")));
    pane_guard
        .protect(
            sqlx::query("UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ? WHERE id = ?")
                .bind(&rt_model)
                .bind(&rt_effort)
                .bind(rt_fast)
                .bind(run_id)
                .execute(app.db())
                .await
                .map_err(up),
        )
        .await?;
    if bot.kind == "claude" && !args.iter().any(|a| matches!(a.as_str(), "--resume" | "-r" | "--continue" | "-c")) {
        app.claude_live_start_fresh(run_id);
    }
    if let Some(dir) = shim_dir.as_deref() {
        let line = crate::shim_path::pane_export_line(&sh_quote(dir));
        if let Err(e) = client.pane_send_text(&pane_id, &line).await {
            tracing::warn!(bot = %bot.name, pane = %pane_id, error = %e, "could not prepend the herdr shim to the pane PATH");
        }
    }
    let mut started = false;
    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    let mut first_agent_start = true;
    for attempt in 0..10u32 {
        #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
        if first_agent_start {
            super::race_point::hit("restart_before_agent_start", &bot.id).await;
        }
        #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
        {
            first_agent_start = false;
        }
        if let Some(fence) = fence {
            if host != fence.conn().name || !app.hosts().is_current(fence).await {
                pane_guard.cleanup().await;
                return Err(host_superseded(&bot.id, &host));
            }
        }
        let launch = match fence {
            Some(fence) => match app.hosts().run_if_current(fence, client.agent_start(&agent, &bot.kind, &pane_id, &args, 60_000)).await {
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

    app.watch_pane_on_session(&host, session, &pane_id).await;

    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    let (status, ready_session) = match client.agent_wait_ready(&bot.kind, &pane_id, &until, 60_000).await {
        Ok(info) => (info.agent_status.normalized(), super::dead_panes::herdr_session_id(&info)),
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "agent.wait did not settle");
            match client.agent_get(&pane_id).await {
                Ok(Some(info)) => (info.agent_status.normalized(), super::dead_panes::herdr_session_id(&info)),
                _ => {
                    close_pane_and_tab(&client, Some(&workspace_id), Some(&tab_id), &pane_id).await;
                    return Err(up(e));
                }
            }
        }
    };
    if bot.kind == "codex" && opts.fork_session.is_some() {
        if let Some(sid) = ready_session.as_deref().filter(|s| Some(*s) != opts.fork_session.as_deref()) {
            let _ = sqlx::query("UPDATE runs SET native_session_id = COALESCE(native_session_id, ?) WHERE id = ?")
                .bind(sid)
                .bind(run_id)
                .execute(app.db())
                .await;
        }
    }
    let status = super::claude_resume_menu::after_start(app, &bot.kind, run_id, status, fence.or(ready_fence.as_ref())).await;
    crate::codex_live::spawn_boot_version_read(app.db(), &client, fence.or(ready_fence.as_ref()), &bot.kind, &bot.id, run_id, &pane_id);
    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    {
        super::race_point::hit("start_before_running", &bot.id).await;
    }
    match super::run_state::transition(app.db(), run_id, &["starting"], "running", Some(status.as_str())).await {
        Ok(super::run_state::Moved::Applied) => {}
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
    if bot.kind == "codex" {
        schedule_codex_notice_capture(app, &bot.id, run_id);
    }
    let _ = app.apply_grok_startup_effort(&bot, run_id, &pane_id, &client).await;
    app.bot_status_changed(&bot.id).await;
    Ok(())
}

/// Find the kind's executable via the user's login shell (`$SHELL -lic`), else PATH. Returns a
/// user-facing reason when missing; a lookup that itself fails passes. 「怎麼去問」測試可以換掉
/// （`App.kind_probe`，見 `kind_probe.rs`）；指令與判讀是同一份。
async fn ensure_kind_installed(app: &(impl crate::hosts::HostsAccess + crate::kind_probe::KindProbeState), host: &str, kind: &str) -> Result<(), String> {
    if !crate::config::valid_kind(kind) {
        return Err(format!("未知的 bot kind `{kind}`"));
    }
    let probe = crate::kind_probe::probe_command(kind);
    let found: Option<String> = if let Some(run) = app.kind_probe().get() {
        run(host, kind, &probe)
    } else if host == LOCAL_HOST {
        crate::hosts::sh_local_stdout(&probe, Duration::from_secs(10), "kind preflight").await.ok().map(|o| o.trim().to_string())
    } else {
        match app.hosts().get(host).await {
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

#[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
pub async fn restart_bot(app: &impl StartContext, bot_id: &str) -> LcResult<String> {
    restart_bot_with(app, bot_id, StartOpts::default()).await
}

/// Stop + start under one hold of the bot's lock. Two holds let a reconcile adopt the just-stopped
/// agent in between (2026-09-10 23:02, `restart-idle`: AGM + three bots down 5.5 h).
/// If a run whose pane is gone still blocks the start, it is ended and the start retried once.
pub async fn restart_bot_with(app: &impl StartContext, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    restart_bot_with_authority(app, bot_id, opts, None).await
}

/// The scoped update restart must still belong to the host connection that installed the update.
/// Recheck after taking the bot lock so a queued restart cannot resolve a repointed host name.
pub async fn restart_bot_with_host_fence(
    app: &impl StartContext,
    bot_id: &str,
    opts: StartOpts,
    fence: &crate::hosts::HostFence,
) -> LcResult<String> {
    restart_bot_with_authority(app, bot_id, opts, Some(fence)).await
}

fn restart_bot_with_authority<'a>(
    app: &'a impl StartContext,
    bot_id: &'a str,
    opts: StartOpts,
    fence: Option<&'a crate::hosts::HostFence>,
) -> impl std::future::Future<Output = LcResult<String>> + 'a {
    Box::pin(restart_bot_with_authority_body(app, bot_id, opts, fence))
}

async fn restart_bot_with_authority_body(
    app: &impl StartContext,
    bot_id: &str,
    opts: StartOpts,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<String> {
    let locked_bot = bot_id.to_string();
    let _g = app.lock_bot_for_start(&locked_bot).await;
    if let Some(fence) = fence {
        ensure_current_host_fence(app, bot_id, fence).await?;
    }
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if crate::config::is_api_kind(&bot.kind) { return Err(LcError::conflict("api_bot_restart_unsupported", json!({"bot_id": bot_id}))); }
    if let Some(fence) = fence {
        ensure_current_host_fence(app, bot_id, fence).await?;
    }
    refuse_default_session(&bot)?;
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
    if opts.resume_native && opts.resume_required {
        let host = db::bot_host(app.db(), bot_id).await.map_err(up)?;
        if let Err(why) = native_resume_plan(app, &bot, &host, true, opts.resume_session.as_deref()).await? {
            if why.reason != "no_session_id" || unsettled_fork(app, bot_id).await.is_none() {
                return Err(cannot_resume(bot_id, why.reason));
            }
        }
    }
    let stopping = db::active_run(app.db(), bot_id).await.map_err(up)?.map(|r| r.id);
    let nudge = super::resume_nudge::busy_before_restart(app, &bot, &opts).await;
    let host = db::bot_host(app.db(), bot_id).await.map_err(up)?;
    if let Some(fence) = fence {
        ensure_current_host_fence(app, bot_id, fence).await?;
    }
    let payload = json!({"opts": opts, "from_run_id": stopping, "bot_name": bot.name});
    let intent_id = app.prepare_restart_intent(bot_id, &host, &payload, RESTART_INTENT_TTL_SECS, app.boot_id())
        .await
        .map_err(|e| LcError::Upstream(format!("cannot record the restart intent: {e:#}")))?;
    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    super::race_point::hit("restart_after_intent", bot_id).await;
    let res = restart_stop_and_start(app, bot_id, opts, stopping, fence).await;
    settle_restart_intent(app, &intent_id, &res).await;
    if let (Ok(run_id), Some(busy)) = (&res, nudge) {
        super::resume_nudge::arm(app, &bot, run_id, busy).await;
    }
    res
}

async fn ensure_current_host_fence(app: &(impl crate::capabilities::Db + crate::hosts::HostsAccess), bot_id: &str, fence: &crate::hosts::HostFence) -> LcResult<()> {
    let host = fence.conn().name.as_str();
    if !app.hosts().is_current(fence).await {
        return Err(LcError::conflict("host_superseded", json!({"bot_id": bot_id, "host": host})));
    }
    if db::bot_host(app.db(), bot_id).await.map_err(up)? != host || !app.hosts().is_current(fence).await {
        return Err(LcError::conflict("host_superseded", json!({"bot_id": bot_id, "host": host})));
    }
    Ok(())
}

/// 重啟的 intent 放置多久還沒補完就放棄（`failed`＋通知）。
const RESTART_INTENT_TTL_SECS: i64 = 15 * 60;

/// handler 還活著時的收尾：成功／bot 回來了＝`done`；什麼都沒動的拒絕＝`abandoned`；其他失敗＝`failed`（**不推 AGM**：呼叫端已經拿到錯誤）。
/// 標不成也不影響結果：intent 留著開機時會被驗證世界後收掉（bot 已經在跑就 `done`）。
async fn settle_restart_intent(app: &(impl crate::capabilities::Db + crate::lifecycle::start::ports::RestartIntentRepo), intent_id: &str, res: &LcResult<String>) {
    let out = match res {
        Ok(_) => app.complete_intent(intent_id).await,
        Err(LcError::Uncommitted(v)) if v.get("start_error").is_none() => app.complete_intent(intent_id).await,
        Err(LcError::Conflict(v)) if matches!(v.get("reason").and_then(|r| r.as_str()), Some("not_idle" | "no_longer_idle")) => {
            app.abandon_intent(intent_id, "refused before anything was stopped").await
        }
        Err(e) => app.fail_intent(intent_id, &format!("{e:?}")).await,
    };
    if let Err(e) = out {
        tracing::warn!(intent = intent_id, error = %e, "could not settle the restart intent; boot recovery will verify it");
    }
}

/// 停 → 起（原本 `restart_bot_with` 鎖裡的後半段，行為不變）。
fn restart_stop_and_start<'a>(
    app: &'a impl StartContext,
    bot_id: &'a str,
    opts: StartOpts,
    stopping: Option<String>,
    fence: Option<&'a crate::hosts::HostFence>,
) -> impl std::future::Future<Output = LcResult<String>> + 'a {
    Box::pin(restart_stop_and_start_body(app, bot_id, opts, stopping, fence))
}

async fn restart_stop_and_start_body(
    app: &impl StartContext,
    bot_id: &str,
    opts: StartOpts,
    stopping: Option<String>,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<String> {
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
        Err(LcError::Conflict(v)) if v.get("reason").and_then(|r| r.as_str()) == Some("no_longer_idle") => {
            let why = busy_reason_locked(app, bot_id, opts.refuse_background_jobs).await.ok().flatten().unwrap_or_else(|| "working".to_string());
            return Err(not_idle(bot_id, &why));
        }
        Err(e) => return Err(e),
    }
    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    super::race_point::hit("restart_after_stop", bot_id).await;
    if let Some(fence) = fence {
        ensure_current_host_fence(app, bot_id, fence).await?;
    }
    let started = restart_start(app, bot_id, opts, fence).await;
    drop(restarting);
    match started {
        Ok(run_id) => {
            schedule_flush_queued(app, bot_id);
            Ok(run_id)
        }
        Err(LcError::Uncommitted(v)) => {
            if let Some(run_id) = v.get("run_id").and_then(|r| r.as_str()) {
                app.flush_once_running(bot_id, run_id);
            }
            Err(LcError::Uncommitted(v))
        }
        Err(e) => {
            let unlabelled = match stopping.as_deref() {
                Some(run_id) => left_down_by_restart(app, bot_id, run_id).await.err().map(|db_err| (run_id, db_err)),
                None => None,
            };
            revoke_orphaned_queued_turns(app, bot_id, "重啟之後沒能把 bot 開回來").await;
            match unlabelled {
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
pub async fn left_down_by_restart(app: &impl StartContext, bot_id: &str, run_id: &str) -> Result<(), sqlx::Error> {
    match super::run_state::relabel(app.db(), run_id, "stopped", "exited").await {
        Ok(super::run_state::Moved::Applied) => {
            tracing::warn!(bot = bot_id, run = run_id, "restart stopped the bot but could not start it again; recorded as exited, not as a user stop");
            app.bot_status_changed(bot_id).await;
            Ok(())
        }
        Ok(super::run_state::Moved::Lost) => Ok(()),
        Err(e) => {
            super::run_state::schedule_settle(app, run_id, super::run_state::Settle::Relabel { from: "stopped", to: "exited" });
            Err(e)
        }
    }
}

pub fn restart_start<'a>(
    app: &'a impl StartContext,
    bot_id: &'a str,
    opts: StartOpts,
    fence: Option<&'a crate::hosts::HostFence>,
) -> impl std::future::Future<Output = LcResult<String>> + 'a {
    Box::pin(restart_start_body(app, bot_id, opts, fence))
}

async fn restart_start_body(
    app: &impl StartContext,
    bot_id: &str,
    opts: StartOpts,
    fence: Option<&crate::hosts::HostFence>,
) -> LcResult<String> {
    match start_bot_locked_with_host_fence(app, bot_id, opts.clone(), fence).await {
        Err(LcError::Conflict(v)) if v.get("reason").and_then(|r| r.as_str()) == Some("active run already exists") => {
            let Some(run) = db::active_run(app.db(), bot_id).await.map_err(up)? else {
                return start_bot_locked_with_host_fence(app, bot_id, opts.clone(), fence).await;
            };
            let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
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
#[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
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
#[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
pub async fn restart_child_in_pane(app: &impl StartContext, bot_id: &str) -> LcResult<String> {
    restart_child_in_pane_with(app, bot_id, false).await
}

/// 同上；`require_idle` 見 [`StartOpts::require_idle`]。
#[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
pub async fn restart_child_in_pane_with(app: &impl StartContext, bot_id: &str, require_idle: bool) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    if require_idle {
        if let Some(why) = busy_reason_locked(app, bot_id, false).await? {
            return Err(not_idle(bot_id, &why));
        }
    }
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.managed_by != "child" {
        return Err(LcError::Bad("這不是 agent spawn 出來的子 agent".into()));
    }
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let Some(pane_id) = run.pane_id.clone() else {
        return Err(LcError::Bad("這個子 agent 的 run 沒有記到 pane".into()));
    };
    let host = db::bot_host(app.db(), bot_id).await.map_err(up)?;
    let client = client_for_run(app, &run).await?;
    let target = super::dead_panes::execution_target(&client, &run, &bot).await?;
    let agent = run.agent_name.clone().unwrap_or_else(|| bot.name.clone());

    crate::child_reconcile_safety::record_retirement_grace(app.db(), bot_id)
        .await
        .map_err(|e| LcError::Upstream(format!("cannot record child restart grace: {e:#}")))?;

    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    super::race_point::hit("child_restart_before_stopping", bot_id).await;
    if require_idle {
        if let Some(n) = crate::background_jobs::known(app, &run.id).filter(|n| *n > 0) {
            return Err(LcError::conflict(
                "not_idle",
                json!({"bot_id": bot_id, "busy": "background_jobs", "background_jobs": n}),
            ));
        }
    }
    let moved = if require_idle {
        super::stop::admit_idle_stop(app.db(), &run.id, bot_id, true).await.map_err(up)?
    } else {
        super::run_state::transition(app.db(), &run.id, super::run_state::LIVE, "stopping", None).await.map_err(up)?
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
    if let Err(e) = fail_in_flight(app, &run.id, "restarted to apply the CLI update").await {
        return Err(super::turn_unwritable(app, bot_id, &run.id, "重啟", e).await);
    }

    for _ in 0..2 {
        let _ = client.agent_send_keys(&target, &["ctrl+c".to_string()]).await;
        tokio::time::sleep(pane_settle(Duration::from_millis(500))).await;
    }
    let mut empty = false;
    for _ in 0..20 {
        match client.pane_get(&pane_id).await {
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
        tokio::time::sleep(pane_settle(Duration::from_millis(500))).await;
    }
    if !empty {
        back_to_running(app, &run.id).await;
        app.emit_bot_status(bot_id).await;
        return Err(LcError::Upstream("子 agent 十秒內沒有退出，沒有動它的 pane".into()));
    }
    let restarting = super::restart_hold::begin(bot_id);
    match super::run_state::transition(app.db(), &run.id, &["stopping"], "stopped", None).await {
        Ok(super::run_state::Moved::Applied) => {}
        Ok(super::run_state::Moved::Lost) => {
            app.emit_bot_status(bot_id).await;
            return Err(LcError::Bad("這個子 agent 的 pane 已經被關掉了".into()));
        }
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
    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    {
        super::race_point::hit("child_restart_between_runs", bot_id).await;
    }

    let mut args = super::setup::permission_args(&bot.kind, bot.auto_approve != 0);
    let overlay = super::runtime_overlay::overlay_for(app, &bot).await.map_err(up)?;
    let mut model_bot = launch_model_bot(app, &bot, &host).await;
    if overlay.model.is_some() {
        model_bot.model = overlay.model.clone();
    }
    args.extend(model_args(&model_bot));
    args.extend(bot.args());
    args.extend(overlay.args.clone());
    let resume = match db::last_native_session(app.db(), bot_id).await.map_err(up)? {
        Some((sid, _)) => match resume_args_by_kind(&bot.kind, &sid) {
            Ok(a) => Some((sid, a)),
            Err(why) => {
                context_lost(app, &bot, why, Some(sid.as_str()).filter(|s| !s.trim().is_empty())).await?;
                None
            }
        },
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
    .execute(app.db())
    .await
    .map_err(up)?;
    if bot.kind == "claude" {
        if let Some((sid, _)) = &resume {
            super::resume_context::restore(app.db(), bot_id, &run_id, sid, crate::models::model_effort_from_argv(&bot.kind, &args).0.as_deref()).await.map_err(up)?;
        }
    }
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
                crate::child_reconcile_safety::hold_after_name_taken(app.db(), bot_id, &agent)
                    .await
                    .map_err(|write| LcError::Upstream(format!("herdr returned agent_name_taken, and the child retirement reason could not be recorded: {write:#}")))?;
                let _ = super::run_state::transition(app.db(), &run_id, &["starting"], "running", None).await;
                app.emit_bot_status(bot_id).await;
                tracing::warn!(bot = %bot.name, run = %run_id, "子 agent 重啟：herdr 說名字還被原 pane 的舊 agent 占著；當作沒重啟、保留 bot");
                return Err(LcError::conflict("agent_name_taken", json!({"bot_id": bot_id, "run_id": run_id, "message": "舊 agent 還在原 pane，沒有重啟；由父 bot 用 herdr 重開"})));
            }
            Err(e) => {
                crate::child_reconcile_safety::clear_retirement_grace(app.db(), bot_id)
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
    match super::run_state::transition(app.db(), &run_id, &["starting"], "running", None).await {
        Ok(super::run_state::Moved::Applied) => {}
        Ok(super::run_state::Moved::Lost) => {
            app.emit_bot_status(bot_id).await;
            return Err(LcError::Bad("這個子 agent 的 pane 已經被關掉了".into()));
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, run = %run_id, error = %e, "the child is back but its run could not be recorded as running");
            super::run_state::schedule_settle(app, &run_id, super::run_state::Settle::Reconcile { stuck: "starting".into() });
            app.flush_once_running(bot_id, &run_id);
            app.emit_bot_status(bot_id).await;
            return Err(LcError::uncommitted(
                "start_state_uncommitted",
                &run_id,
                "子 agent 已經在原 pane 裡起來了，但 run 的狀態寫不進 DB；已排重試，會照 herdr 的狀態收斂成 running",
                e,
            ));
        }
    }
    if let Err(e) = crate::child_reconcile_safety::clear_after_successful_restart(app.db(), bot_id).await {
        tracing::warn!(bot = bot_id, error = %e, "child restart succeeded but its prior agent_name_taken hold could not be cleared");
    }
    if let Err(e) = crate::child_reconcile_safety::clear_retirement_grace(app.db(), bot_id).await {
        tracing::warn!(bot = bot_id, error = %e, "child restart succeeded but its hand-off grace could not be cleared");
    }
    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    match client.agent_wait_ready(&bot.kind, &pane_id, &until, 60_000).await {
        Ok(info) => {
            let ready = super::claude_resume_menu::after_start(app, &bot.kind, &run_id, info.agent_status.normalized(), Some(&client.fence)).await;
            crate::codex_live::spawn_boot_version_read(app.db(), &client, Some(&client.fence), &bot.kind, bot_id, &run_id, &pane_id);
            if ready != AgentStatus::Unknown {
                if let Err(e) = sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ? AND agent_status = 'unknown'")
                    .bind(ready.as_str())
                    .bind(&run_id)
                    .execute(app.db())
                    .await
                {
                    tracing::warn!(bot = %bot.name, run = %run_id, error = %e, "could not record the restarted child's ready status");
                }
            }
        }
        Err(e) => tracing::warn!(bot = %bot.name, error = %e, "子 agent 重啟後沒等到 ready，run 留著讓對帳接手"),
    }
    app.spawn_adopted_capture(&run_id, bot_id);
    app.emit_bot_status(bot_id).await;
    app.emit("bot_changed", json!({"bot_id": bot_id})).await;
    tracing::info!(bot = %bot.name, pane = %pane_id, run = %run_id, "子 agent 在原本的 pane 裡重啟完成");
    Ok(run_id)
}

/// Continue a credential-rotation restart while the caller holds the bot lock.
pub async fn resume_credential_rotation_locked(app: &impl StartContext, bot_id: &str, opts: StartOpts, from_run: &str) -> LcResult<String> {
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    refuse_default_session(&bot)?;
    restart_stop_and_start(app, bot_id, opts, Some(from_run.to_string()), None).await
}

/// 開機補完被打斷的重啟（#355 P2）：呼叫端持 bot 鎖，已經決定要往前補。stop 做到一半（`stopping`）先補完它；
/// 剛停掉的舊 run 由 `stopped`（使用者要它停）改標 `exited`；再照原本的選項 start（不再要求閒置——使用者要的是它回來）。
pub async fn resume_restart_locked(app: &impl StartContext, bot_id: &str, opts: StartOpts, from_run: Option<&str>) -> LcResult<String> {
    if let Some(run) = db::active_run(app.db(), bot_id).await.map_err(up)? {
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

#[cfg(all(test, not(feature = "daemon-test-harness")))]
#[path = "start_tests/account_file_tests.rs"]
mod account_file_tests;

/// 專家（specialist）的範圍段只給 specialist、且一定帶 track（bot 專屬段，不進共用的 child_agent_rules）。
#[cfg(test)]
#[path = "start_tests/specialist_section_tests.rs"]
mod specialist_section_tests;

/// argv 裡每個 `--add-dir <dir>`／`--add-dir=<dir>` 的目錄（claude 2.1.326 起每個都要信任，見 `start_inner`）。
fn add_dir_args(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--add-dir" {
            if let Some(d) = it.next().map(String::as_str).filter(|d| !d.is_empty() && !d.starts_with("--")) {
                out.push(d);
            }
        } else if let Some(d) = a.strip_prefix("--add-dir=").filter(|d| !d.is_empty()) {
            out.push(d);
        }
    }
    out
}

#[cfg(test)]
mod add_dir_args_tests {
    #[test]
    fn collects_every_add_dir_value() {
        let args: Vec<String> = ["--restricted", "--add-dir", "/o/B1", "--add-dir=/x", "--add-dir", "--model", "opus"].map(String::from).to_vec();
        assert_eq!(super::add_dir_args(&args), vec!["/o/B1", "/x"]);
        assert!(super::add_dir_args(&["--model".to_string(), "opus".to_string()]).is_empty());
    }
}

#[cfg(any(test, feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
#[doc(hidden)]
pub mod test_seams {
    //! #1862 L2：daemon 測試要碰的私有項，窄轉呼叫（只在測試建置存在）。
    use super::*;
    pub async fn record_daemon_url_injected(pool: &sqlx::SqlitePool, run_id: &str, env: &Value) -> LcResult<()> { super::record_daemon_url_injected(pool, run_id, env).await }
    pub fn resume_args_by_kind(kind: &str, session_id: &str) -> Result<Vec<String>, &'static str> { super::resume_args_by_kind(kind, session_id) }
    pub async fn stage_cross_identity_transcript_remote(app: &(impl crate::hosts::HostsAccess + IdentityAccess), bot: &db::Bot, host: &str, transcript: &str) -> Result<(), &'static str> { super::stage_cross_identity_transcript_remote(app, bot, host, transcript).await }
    pub fn remote_stage_script(old_projects: &str, new_projects: &str, cwd_key: &str, fname: &str, stem: Option<&str>) -> String { super::remote_stage_script(old_projects, new_projects, cwd_key, fname, stem) }
    pub fn parse_stage_output(out: &str) -> Result<(), &'static str> { super::parse_stage_output(out) }
    pub async fn unsettled_fork(app: &impl Db, bot_id: &str) -> Option<(String, String)> { super::unsettled_fork(app, bot_id).await }
    pub async fn acquire_run_pane(client: &crate::herdr::HerdrClient, workspace_id: &str, cwd: &str, label: &str, env: &Value, fresh_root: Option<(crate::herdr::PaneInfo, &str)>) -> anyhow::Result<crate::herdr::PaneInfo> { super::acquire_run_pane(client, workspace_id, cwd, label, env, fresh_root).await }
    pub fn has_profile_arg<'a>(args: impl IntoIterator<Item = &'a String>) -> bool { super::has_profile_arg(args) }
    pub async fn codex_home_dir(app: &impl HostsAccess, project: &db::Project, env: &Value) -> Option<String> { super::codex_home_dir(app, project, env).await }
    pub async fn fork_identity_note(pool: &sqlx::SqlitePool, bot_id: &str) -> Result<Option<String>, sqlx::Error> { super::fork_identity_note(pool, bot_id).await }
    pub fn claude_fresh_session_id(args: &[String]) -> Option<String> { super::claude_fresh_session_id(args) }
    pub fn codex_instant_interrupt_args(flag_on: bool, restricted: bool) -> Vec<String> { super::codex_instant_interrupt_args(flag_on, restricted) }
    pub enum StagedPersona { NotNeeded, Staged(String), KeptInline(String), Failed, FailedWithReason(String) }
    impl From<super::StagedPersona> for StagedPersona {
        fn from(p: super::StagedPersona) -> Self {
            match p {
                super::StagedPersona::NotNeeded => Self::NotNeeded,
                super::StagedPersona::Staged(s) => Self::Staged(s),
                super::StagedPersona::KeptInline(s) => Self::KeptInline(s),
                super::StagedPersona::Failed => Self::Failed,
                super::StagedPersona::FailedWithReason(s) => Self::FailedWithReason(s),
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn stage_persona(app: &impl HostsAccess, bot: &db::Bot, project: &db::Project, shim_dir: Option<&str>, env: &Value, agent: &str, agent_md: Option<&str>, identity_args: &[String], fork_note: Option<&str>) -> StagedPersona { super::stage_persona(app, bot, project, shim_dir, env, agent, agent_md, identity_args, fork_note).await.into() }
}
