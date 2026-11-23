//! Keystrokes, text and slash commands sent straight at a live pane.

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::state::App;
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
struct LiveApplyWindow {
    started_at: std::time::Instant,
    finished_at: Option<std::time::Instant>,
    sent_line: Option<String>,
}

fn live_apply_windows() -> &'static Mutex<HashMap<String, LiveApplyWindow>> {
    static WINDOWS: OnceLock<Mutex<HashMap<String, LiveApplyWindow>>> = OnceLock::new();
    WINDOWS.get_or_init(Default::default)
}

pub struct LiveApplyGuard {
    run_id: String,
    started_at: std::time::Instant,
}

impl LiveApplyGuard {
    pub fn enter(run_id: &str, sent_line: Option<String>) -> Self {
        let now = std::time::Instant::now();
        if let Ok(mut map) = live_apply_windows().lock() {
            map.retain(|_, win| {
                win.finished_at.map_or(true, |fin| fin.elapsed() < Duration::from_secs(60))
            });
            map.insert(
                run_id.to_string(),
                LiveApplyWindow {
                    started_at: now,
                    finished_at: None,
                    sent_line,
                },
            );
        }
        Self {
            run_id: run_id.to_string(),
            started_at: now,
        }
    }
}

impl Drop for LiveApplyGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = live_apply_windows().lock() {
            if let Some(win) = map.get_mut(&self.run_id) {
                if win.started_at == self.started_at {
                    win.finished_at = Some(std::time::Instant::now());
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveApplyMatch {
    None,
    InProgressOrOverlapped,
    RecentTail { sent_line: Option<String> },
}

pub fn live_apply_window_match(run_id: &str, edge_at: std::time::Instant) -> LiveApplyMatch {
    let map = match live_apply_windows().lock() {
        Ok(m) => m,
        Err(_) => return LiveApplyMatch::None,
    };
    let Some(window) = map.get(run_id) else {
        return LiveApplyMatch::None;
    };
    let not_ancient_before_start = edge_at >= window.started_at
        || window.started_at.saturating_duration_since(edge_at) <= Duration::from_secs(2);
    match window.finished_at {
        None => {
            if not_ancient_before_start {
                LiveApplyMatch::InProgressOrOverlapped
            } else {
                LiveApplyMatch::None
            }
        }
        Some(finished) => {
            if not_ancient_before_start && edge_at <= finished {
                LiveApplyMatch::InProgressOrOverlapped
            } else if edge_at > finished && edge_at.saturating_duration_since(finished) <= Duration::from_secs(2) {
                LiveApplyMatch::RecentTail {
                    sent_line: window.sent_line.clone(),
                }
            } else {
                LiveApplyMatch::None
            }
        }
    }
}

pub async fn send_keys(app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), bot_id: &str, keys: Vec<String>, expect_run_id: Option<String>) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    if let Some(exp) = expect_run_id {
        if exp != run.id {
            return Err(LcError::conflict("run mismatch", json!({"run_id": run.id})));
        }
    }
    let client = client_for_run(app, &run).await?;
    let target = super::dead_panes::execution_target(&client, &run, &bot).await?;
    client.agent_send_keys(&target, &keys).await.map_err(up)?;
    Ok(())
}

/// `POST /api/bots/:id/text` — 把（多行）文字打進 pane，選擇性按 Enter。`\n` 不是鍵名，所以不走
/// `send_keys`；Enter 另用 `pane.send_keys`（`send_text` 裡的 `\n` 是貼上換行）。不擋
/// `agent_status`：用途就是回合中「併送」。
#[cfg(all(test, feature = "daemon-test-harness"))]
pub async fn send_text(app: &Arc<App>, bot_id: &str, text: &str, enter: bool, expect_run_id: Option<String>) -> LcResult<()> {
    send_text_recorded(app, bot_id, text, enter, expect_run_id, false).await.map(|_| ())
}

/// [`send_text`]，`record` 時打字成功後在同一把 bot 鎖裡把這句記成進行中回合的使用者訊息（`sent_via =
/// 'supplement'`，網頁的「補充」）。沒有進行中的回合就不記：那一句 CLI 會當成新的一輪，hook 開外部回合時自己記。
/// 打字失敗一個字都不記；打完了才寫不進去只記 log、回 `None`——字已經進 pane，回錯會讓使用者再補一次。
pub async fn send_text_recorded(
    app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess),
    bot_id: &str,
    text: &str,
    enter: bool,
    expect_run_id: Option<String>,
    record: bool,
) -> LcResult<Option<db::Message>> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    if let Some(exp) = expect_run_id {
        if exp != run.id {
            return Err(LcError::conflict("run mismatch", json!({"run_id": run.id})));
        }
    }
    let pane_id = run
        .pane_id
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| LcError::NotFound("pane".into()))?
        .to_string();
    let client = client_for_run(app, &run).await?;
    if super::dead_panes::pane_target_stale(&client, &run).await {
        return Err(LcError::conflict(
            "pane_reused",
            json!({"run_id": run.id, "pane_id": pane_id, "sent": false}),
        ));
    }
    if !text.is_empty() || enter {
        mark_pane_typed(app, &run.id).await.map_err(LcError::Upstream)?;
    }
    if !text.is_empty() {
        client.pane_send_text(&pane_id, text).await.map_err(up)?;
    }
    if enter {
        client.pane_send_keys(&pane_id, &["enter"]).await.map_err(up)?;
    }
    if !record || text.trim().is_empty() {
        return Ok(None);
    }
    match record_supplement(app, bot_id, &run.id, text).await {
        Ok(m) => Ok(m),
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = %e, "補充的字已經打進 pane，但記成訊息失敗");
            Ok(None)
        }
    }
}

async fn record_supplement(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), bot_id: &str, run_id: &str, text: &str) -> anyhow::Result<Option<db::Message>> {
    let Some(turn) = db::in_flight_turn(app.db(), run_id).await? else { return Ok(None) };
    insert_supplement(app, bot_id, &turn, text).await.map(Some)
}

/// 把 `text` 記成 `turn` 的補充（`sent_via = 'supplement'`）並推 `message_added`。codex steer（#748）也用這一支。
pub(super) async fn insert_supplement(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), bot_id: &str, turn: &db::Turn, text: &str) -> anyhow::Result<db::Message> {
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, sent_via, created_at)
         VALUES (?,?,?,'user',?,'web','supplement',?)",
    )
    .bind(&id)
    .bind(&turn.conversation_id)
    .bind(&turn.id)
    .bind(text)
    .bind(db::now())
    .execute(app.db())
    .await?;
    let m = sqlx::query_as::<_, db::Message>("SELECT *, rowid AS seq FROM messages WHERE id = ?").bind(&id).fetch_one(app.db()).await?;
    emit_message_added(app, bot_id, m.clone()).await;
    Ok(m)
}

/// TUI slash command for a live setting, or `None` (caller reports `needs_restart`).
fn live_slash_command(kind: &str, field: &str, value: &str, effort: Option<&str>) -> Option<String> {
    match (kind, field) {
        ("grok", "effort") => None,
        ("claude" | "agy", "effort") => Some(format!("/effort {}", value.to_ascii_lowercase())),
        ("grok", "model") => {
            let mut line = format!("/model {value}");
            if let Some(e) = effort.map(str::trim).filter(|s| !s.is_empty()) {
                line.push(' ');
                line.push_str(&e.to_ascii_lowercase());
            }
            Some(line)
        }
        ("claude" | "agy", "model") => Some(format!("/model {value}")),
        _ => None,
    }
}

/// 不重啟就套用設定（SPEC §4.4a）。grok `model`、claude `model`/`effort` 走一行 slash
/// 指令；grok `effort` 不送 `/effort`（會寫進 config.toml）。codex >= 0.164.0 一行 `/model <id> --reasoning=… --speed=…`（只作用於這個 session）；更舊的版本 `/model` 是不吃參數的選單、`/fast` 是開關，
/// 都走 [`crate::codex_live`]，送出後讀畫面再回讀狀態列（2026-09-09、2026-10-23 實測）。副作用：claude（2.1.263 實測）與 codex 的選單路徑會把
/// 選擇存成帳號之後新 session 的預設；codex 的一行路徑不會。
/// 回傳 `None` = 已套用；`Some(理由)` = 退回重啟。理由一路帶回 `live_apply` 並寫 log——
/// 2026-09-13 使用者問 codex 改 effort 為何重啟，當時每個失敗出口都是靜默的。
pub async fn apply_live_setting(app: &impl LcHost, bot_id: &str, fields: &[&str]) -> Option<String> {
    match apply_live_setting_with_run(app, bot_id, fields).await {
        LiveApplyOutcome::Applied { .. } => None,
        LiveApplyOutcome::BookkeepingPending { reason, .. } => Some(reason),
        LiveApplyOutcome::Failed(why) => Some(why),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveApplyOutcome {
    Applied { run_id: String },
    BookkeepingPending { run_id: String, reason: String },
    Failed(String),
}

struct LiveApplyReceipt {
    run_id: String,
    bot_id: String,
    snapshot: RuntimeSnapshot,
}

#[derive(Default)]
struct RuntimeSnapshot {
    model: Option<String>,
    effort: Option<String>,
    fast: Option<i64>,
}

/// 帶回實際執行 slash/picker 的 run；呼叫端的 PATCH 快照可能已經過時。
async fn apply_live_setting_with_run(
    app: &impl LcHost,
    bot_id: &str,
    fields: &[&str],
) -> LiveApplyOutcome {
    let lock = app.bot_lock(bot_id).await;
    let _guard = lock.lock().await;
    let bot = match db::bot(app.db(), bot_id).await {
        Ok(Some(bot)) => bot,
        Ok(None) => return LiveApplyOutcome::Failed("bot_missing".into()),
        Err(error) => return LiveApplyOutcome::Failed(format!("bot_read_failed: {error}")),
    };
    let target_rev = crate::launch_rev::of(&bot);
    apply_live_setting_locked(app, bot_id, fields, "", &target_rev, None).await
}

pub async fn apply_live_setting_with_revision(
    app: &impl LcHost,
    bot_id: &str,
    fields: &[&str],
    baseline_rev: &str,
    target_rev: &str,
) -> LiveApplyOutcome {
    let lock = app.bot_lock(bot_id).await;
    let _guard = lock.lock().await;
    apply_live_setting_locked(app, bot_id, fields, baseline_rev, target_rev, None).await
}

/// [`apply_live_setting_with_revision`]，回合中改 codex 的 model／effort 時允許打斷（#1681，只有 PATCH 這條路用）：
/// 先按 Esc 停下目前回合、套用、再送 `continue` 讓它接著做（見 [`interrupt_and_apply`]）。打斷不了照舊回忙的理由、排到回合結束。
/// 第二個值＝這次真的按了 Esc（不論之後套用成不成功，回合都被停過、並已嘗試接著跑）。
pub async fn apply_live_setting_interrupting(
    app: &impl LcHost,
    bot_id: &str,
    fields: &[&str],
    baseline_rev: &str,
    target_rev: &str,
) -> (LiveApplyOutcome, bool) {
    let interrupted = std::sync::atomic::AtomicBool::new(false);
    let lock = app.bot_lock(bot_id).await;
    let _guard = lock.lock().await;
    let outcome = apply_live_setting_locked(app, bot_id, fields, baseline_rev, target_rev, Some(&interrupted)).await;
    (outcome, interrupted.load(std::sync::atomic::Ordering::SeqCst))
}

async fn apply_live_setting_locked(
    app: &impl LcHost,
    bot_id: &str,
    fields: &[&str],
    baseline_rev: &str,
    target_rev: &str,
    interrupt: Option<&std::sync::atomic::AtomicBool>,
) -> LiveApplyOutcome {
    match db::active_run(app.db(), bot_id).await {
        Ok(Some(run)) => {
            if let Err(error) = super::live_apply_debt::retry_once(app, &run.id).await {
                return LiveApplyOutcome::BookkeepingPending {
                    run_id: run.id,
                    reason: format!("live_bookkeeping_still_pending: {error}"),
                };
            }
        }
        Ok(None) => {}
        Err(error) => {
            return LiveApplyOutcome::Failed(format!("active_run_read_failed: {error}"));
        }
    }
    let result = match apply_live_setting_inner(app, bot_id, fields, target_rev, interrupt).await {
        Err(why) => LiveApplyOutcome::Failed(why),
        Ok(receipt) => {
            let debt = super::live_apply_debt::RuntimeDebt {
                run_id: receipt.run_id.clone(),
                bot_id: receipt.bot_id,
                baseline_rev: baseline_rev.to_string(),
                target_rev: target_rev.to_string(),
                runtime_model: receipt.snapshot.model,
                runtime_effort: receipt.snapshot.effort,
                runtime_fast: receipt.snapshot.fast,
                created_at: db::now(),
            };
            match super::live_apply_debt::persist_and_commit(app, debt).await {
                Ok(()) => {
                    app.emit_bot_status(bot_id).await;
                    LiveApplyOutcome::Applied {
                        run_id: receipt.run_id,
                    }
                }
                Err(reason) => LiveApplyOutcome::BookkeepingPending {
                    run_id: receipt.run_id,
                    reason,
                },
            }
        }
    };
    match &result {
        LiveApplyOutcome::Failed(why) => {
            tracing::info!(bot_id, ?fields, reason = %why, "設定沒能當場套用，改用重啟")
        }
        LiveApplyOutcome::BookkeepingPending { run_id, reason } => tracing::warn!(
            bot_id,
            ?fields,
            run_id,
            reason,
            "live apply readback succeeded but runtime bookkeeping is pending"
        ),
        LiveApplyOutcome::Applied { run_id, .. } => {
            tracing::info!(bot_id, ?fields, run_id, "設定已當場套用，不需要重啟")
        }
    }
    result
}

async fn apply_live_setting_inner(
    app: &impl LcHost,
    bot_id: &str,
    fields: &[&str],
    target_rev: &str,
    interrupt: Option<&std::sync::atomic::AtomicBool>,
) -> Result<LiveApplyReceipt, String> {
    let Ok(Some(bot)) = db::bot(app.db(), bot_id).await else {
        return Err("bot_missing".into());
    };
    if crate::launch_rev::of(&bot) != target_rev {
        return Err("live_target_revision_changed".into());
    }
    let Ok(Some(run)) = db::active_run(app.db(), bot_id).await else {
        return Err("no_active_run".into());
    };
    let in_flight = !matches!(db::in_flight_turn(app.db(), &run.id).await, Ok(None));
    let (pane_id, during_turn) = match live_gate(&run, in_flight, &bot.kind, fields) {
        Ok(gate) => gate,
        Err(why @ (SlashBlocked::AgentBusy | SlashBlocked::TurnInFlight)) => {
            let interruptible = bot.kind == "codex"
                && run.agent_status != "blocked"
                && fields.iter().any(|f| matches!(*f, "model" | "effort"));
            match interrupt {
                Some(flag) if interruptible => return interrupt_and_apply(app, &bot, &run, fields, flag).await,
                _ => return Err(format!("slash_gate: {}", why.reason())),
            }
        }
        Err(why) => return Err(format!("slash_gate: {}", why.reason())),
    };
    let Ok(client) = client_for_run(app, &run).await else {
        return Err("no_herdr_client".into());
    };

    if bot.kind == "codex" {
        let was_fast = run.runtime_fast.map(|v| v != 0);
        let running = crate::codex_update::running_version_or(&run.id, run.cli_version.as_deref());
        let default_effort = crate::codex_live::model_default_effort(app, &bot, run.runtime_model.as_deref()).await;
        let _live_guard = (!during_turn).then(|| LiveApplyGuard::enter(&run.id, None));
        mark_pane_typed(app, &run.id).await?;
        let applied = if during_turn {
            crate::codex_live::apply_fast_during_turn(&client, &pane_id, &bot, was_fast).await
        } else {
            crate::codex_live::apply(&client, &pane_id, &bot, was_fast, fields, running.as_deref(), default_effort.as_deref()).await
        };
        let seen = match applied {
            Ok(seen) => seen,
            Err(why) => return Err(format!("codex: {why}")),
        };
        tracing::info!(bot_id, model = %seen.model, effort = ?seen.effort, fast = seen.fast, "codex applied live");
        return Ok(LiveApplyReceipt {
            run_id: run.id,
            bot_id: bot_id.to_string(),
            snapshot: RuntimeSnapshot {
                model: Some(seen.model),
                effort: seen.effort,
                fast: Some(i64::from(seen.fast)),
            },
        });
    }

    let [field] = fields else {
        return Err("not_a_single_field".into());
    };
    let value = match *field {
        "effort" => bot.effort.as_deref(),
        "model" => bot.model.as_deref(),
        _ => None,
    };
    let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(format!("{field}_cleared_to_default"));
    };
    if bot.kind == "agy" && *field == "model" {
        if !crate::models::agy_model_allowed(value) {
            return Err("agy_model_disallowed".into());
        }
        let host = db::bot_host(app.db(), &bot.id)
            .await
            .unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
        if super::start::agy_model_unlisted(app, &bot, &host, value).await.is_some() {
            return Err("agy_model_not_in_account_list".into());
        }
    }
    let Some(line) = live_slash_command(&bot.kind, field, value, bot.effort.as_deref()) else {
        return Err(format!("no_slash_command_for_{field}"));
    };
    let _live_guard = LiveApplyGuard::enter(&run.id, Some(line.clone()));
    mark_pane_typed(app, &run.id).await?;
    if let Err(e) = send_slash_line(&client, &pane_id, &line).await {
        return Err(format!("slash_send_failed: {e:?}"));
    }
    let settled_screen = match wait_for_composer_settled(&client, &pane_id, &bot.kind).await {
        Ok(Some(screen)) => screen,
        Ok(None) => return Err("slash_composer_not_settled".into()),
        Err(e) => return Err(format!("slash_settle_read_failed: {e:?}")),
    };
    if bot.kind == "grok" {
        let expected_model = bot
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if *field == "model" {
            let expected = expected_model.map(|m| crate::models::canonical_model("grok", m));
            if expected.is_some_and(|want| {
                grok_model_from_screen(&settled_screen).as_deref() != Some(want)
            }) {
                return Err("grok_model_readback_mismatch".into());
            }
        }
        let expected_effort = if *field == "effort" {
            bot.effort
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        } else if *field == "model" {
            bot.effort
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        if expected_effort.is_some_and(|want| {
            crate::models::grok_effort_from_screen(&settled_screen).as_deref()
                != Some(want.to_ascii_lowercase().as_str())
        }) {
            return Err("grok_effort_readback_mismatch".into());
        }
    }
    let mut agy_effort_model = None;
    if bot.kind == "agy" {
        if *field == "model" {
            let want = crate::models::canonical_model("agy", value);
            if !agy_model_matches_screen(&settled_screen, want) {
                return Err("agy_model_readback_mismatch".into());
            }
        } else if *field == "effort" {
            let want = value.to_ascii_lowercase();
            let Some(seen) = agy_effort_matches_screen(&settled_screen, &want) else {
                return Err("agy_effort_readback_mismatch".into());
            };
            agy_effort_model = Some(seen);
        }
    }
    let mut snapshot = RuntimeSnapshot {
        model: run.runtime_model.clone(),
        effort: run.runtime_effort.clone(),
        fast: run.runtime_fast,
    };
    match *field {
        "effort" => {
            snapshot.effort = Some(value.to_string());
            if let Some(seen) = agy_effort_model {
                if snapshot.model.is_some() {
                    snapshot.model = Some(seen);
                }
            }
        }
        "model" => snapshot.model = Some(value.to_string()),
        _ => return Err("not_a_single_field".into()),
    }
    if *field == "model" && bot.kind == "grok" {
        if let Some(e) = bot
            .effort
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            snapshot.effort = Some(e.to_ascii_lowercase());
        }
    }
    tracing::info!(bot_id, line, "applied live via slash command");
    Ok(LiveApplyReceipt {
        run_id: run.id,
        bot_id: bot_id.to_string(),
        snapshot,
    })
}

/// 回合中改 codex 的 model／effort（#1681，SPEC §4.4a）：Esc 把目前回合停下來 → 閒著時的 `codex_live::apply` → 送 `continue`
/// 讓它接著做，立即生效。呼叫端握著 bot 鎖，整段都在鎖裡：hook 的收尾、§4.3 備援、排隊的 flush 都要同一把鎖，
/// 不會在打斷到接著跑之間把這個回合收掉、或讓排著的 prompt 插進去變成亂序。
///
/// 回合的記帳：**不收這個回合**。Esc 在這裡不是使用者中止（不走 `interruption`、不記接管、不發中斷通知），回合照舊
/// in_flight；`continue` 記成它的補充（`sent_via = 'supplement'`，hook 的 `input-messages` 才對得上），codex 接著跑完的
/// Stop 把同一個回合收成 completed——`continue` 是原回合的延續，不是新回合。
///
/// 一個鍵都還沒按之前碰到任何一道閘都回忙的理由，照舊排到回合結束：回合不是真的在飛（送達結果沒定案、欠著打斷的收尾）、
/// 維護窗口握著、畫面不是「回合進行中、輸入框空、沒選單」。Esc 之後確認不了回到可輸入也一樣不盲打。
/// Esc 進去之後 bot 不能卡在打斷的狀態：套用失敗照樣送 `continue`（`Err` 帶回原因、留下需重啟）；
/// `continue` 送不出去就把回合收成 failed、對話裡寫明原因，使用者到終端接手。
async fn interrupt_and_apply(
    app: &impl LcHost,
    bot: &db::Bot,
    run: &db::Run,
    fields: &[&str],
    interrupted: &std::sync::atomic::AtomicBool,
) -> Result<LiveApplyReceipt, String> {
    let busy = || format!("codex: {}", crate::codex_live::BUSY_NOT_READY);
    let Some(pane_id) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(str::to_string) else {
        return Err("slash_gate: no_pane".into());
    };
    let turn = match db::in_flight_turn(app.db(), &run.id).await {
        Ok(Some(t)) => t,
        _ => return Err(busy()),
    };
    if turn.delivery != "ok" || super::interruption::owes(&bot.id, &turn.id) {
        return Err(busy());
    }
    match crate::lifecycle::send_now::ports::MaintenancePort::window_held(app).await {
        Ok(None) => {}
        _ => return Err(busy()),
    }
    let Ok(client) = client_for_run(app, run).await else {
        return Err("no_herdr_client".into());
    };
    mark_pane_typed(app, &run.id).await?;
    if let Err(why) = crate::codex_live::interrupt_for_apply(&client, &pane_id).await {
        return Err(format!("codex: {why}"));
    }
    interrupted.store(true, std::sync::atomic::Ordering::SeqCst);

    let was_fast = run.runtime_fast.map(|v| v != 0);
    let running = crate::codex_update::running_version_or(&run.id, run.cli_version.as_deref());
    let default_effort = crate::codex_live::model_default_effort(app, bot, run.runtime_model.as_deref()).await;
    let applied = crate::codex_live::apply(&client, &pane_id, bot, was_fast, fields, running.as_deref(), default_effort.as_deref()).await;
    let resumed = crate::codex_live::send_resume(&client, &pane_id).await;
    tracing::info!(bot_id = %bot.id, ?fields, applied = ?applied.as_ref().err(), resumed = ?resumed.as_ref().err(), "codex live apply interrupted the turn");

    let what = fields
        .iter()
        .filter_map(|f| match *f {
            "model" => Some("模型"),
            "effort" => Some("強度"),
            "fast" => Some("fast"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("、");
    match (&applied, &resumed) {
        (_, Err(why)) => {
            let note = format!(
                "改{what}時已按 Esc 停下這個回合，但接著跑的 `{}` 沒能送出（{why}）；回合到此為止，請到終端輸入 continue 或重新下指令。",
                crate::codex_live::RESUME_TEXT
            );
            if let Err(e) = super::interruption::close_turn(app, &bot.id, &run.id, &turn.id, &note).await {
                tracing::warn!(bot_id = %bot.id, turn = %turn.id, error = %e, "could not close the turn after a failed resume");
            }
        }
        (applied, Ok(())) => {
            let note = match applied {
                Ok(_) => format!("改{what}：已按 Esc 停下這個回合、套用新設定，並送出 `{}` 接著跑。", crate::codex_live::RESUME_TEXT),
                Err(why) => format!(
                    "改{what}：已按 Esc 停下這個回合，但新設定沒套上（{why}），已送出 `{}` 接著跑；設定需重啟才會生效。",
                    crate::codex_live::RESUME_TEXT
                ),
            };
            if let Err(e) = record_supplement(app, &bot.id, &run.id, crate::codex_live::RESUME_TEXT).await {
                tracing::warn!(bot_id = %bot.id, error = %e, "the resume text was typed but could not be recorded as a supplement");
            }
            if let Err(e) = super::messages::insert_message(app, &turn.conversation_id, Some(&turn.id), "system", &note, "system", false, None).await {
                tracing::warn!(bot_id = %bot.id, error = %e, "could not write the interrupted-apply note");
            }
        }
    }

    let seen = applied.map_err(|why| format!("codex: {why}"))?;
    Ok(LiveApplyReceipt {
        run_id: run.id.clone(),
        bot_id: bot.id.clone(),
        snapshot: RuntimeSnapshot { model: Some(seen.model), effort: seen.effort, fast: Some(i64::from(seen.fast)) },
    })
}

/// grok 的等級只靠啟動參數 `--reasoning-effort`（grok 1.0.46 實測會改這一輪框底，且不寫 config.toml）。
/// 不再補 `/effort`：那個 slash 會把 `[models] default_reasoning_effort` 寫進使用者的 `GROK_HOME`。
/// 參數留著是因為 start 仍會呼叫；送 slash 的舊路徑（#215）已停用。
pub async fn apply_grok_startup_effort(
    _app: &impl LcHost,
    _bot: &db::Bot,
    _run_id: &str,
    _pane_id: &str,
    _client: &HerdrClient,
) -> Result<(), String> {
    Ok(())
}

/// 不能送 slash 指令的理由。`apply_live_setting` 只需要「不行」，但使用者按「登入」時靜默失敗是 bug。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashBlocked {
    NotRunning,
    /// `working` / `blocked`：打的字會被吃掉，或掉進權限提示。
    AgentBusy,
    /// 這一行會變成在飛 prompt 的一部分。
    TurnInFlight,
    NoPane,
}

impl SlashBlocked {
    /// 機器可讀理由；文案由前端翻。
    pub fn reason(self) -> &'static str {
        match self {
            SlashBlocked::NotRunning => "not_running",
            SlashBlocked::AgentBusy => "agent_busy",
            SlashBlocked::TurnInFlight => "turn_in_flight",
            SlashBlocked::NoPane => "no_pane",
        }
    }
}

/// 能打字就回 pane id。純函式：`apply_live_setting` 與 `login` 共用，每條擋下的理由都測得到。
fn slash_gate(run: &db::Run, turn_in_flight: bool) -> Result<String, SlashBlocked> {
    if run.state != "running" {
        return Err(SlashBlocked::NotRunning);
    }
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Err(SlashBlocked::AgentBusy);
    }
    if turn_in_flight {
        return Err(SlashBlocked::TurnInFlight);
    }
    run.pane_id
        .clone()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .ok_or(SlashBlocked::NoPane)
}

/// [`slash_gate`]，外加 codex 回合中只切 fast 的例外（#712）：`/fast` 在 codex 0.157.1 回合中照樣生效、不打斷回合，
/// 所以 working／回合在飛也放行，回 `(pane, true)` 走 `codex_live::apply_fast_during_turn`（不按 Esc、輸入框有字就不碰）。
/// `blocked`（權限／選擇畫面）照舊擋：打的字會掉進那個畫面。model／effort 要開選單，回合中不碰，照舊排到回合結束。
fn live_gate(run: &db::Run, turn_in_flight: bool, kind: &str, fields: &[&str]) -> Result<(String, bool), SlashBlocked> {
    match slash_gate(run, turn_in_flight) {
        Ok(pane) => Ok((pane, false)),
        Err(SlashBlocked::AgentBusy | SlashBlocked::TurnInFlight)
            if kind == "codex" && fields == ["fast"] && run.agent_status != "blocked" =>
        {
            run.pane_id
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| (p.to_string(), true))
                .ok_or(SlashBlocked::NoPane)
        }
        Err(why) => Err(why),
    }
}

/// 打一行 slash 指令並送出：先打字、等輸入列畫好、再 Enter（`"/login\n"` 會被當多行貼上）。
/// 送出不等於套用（2026-09-11 AGM）：claude 有對話時 `/model` 會跳「Switch model?」框，留著會吃掉
/// 下一則 prompt、Enter 替人按 Yes、回合 stall。所以回頭看畫面：是那個框 → 按 `1`（使用者已在
/// AG Man 選過）；還在 → Esc 並回 `Err`。絕不把框留在畫面上。
async fn send_slash_line(client: &HerdrClient, pane_id: &str, line: &str) -> LcResult<()> {
    client.pane_send_text(pane_id, line).await.map_err(up)?;
    tokio::time::sleep(pane_settle(std::time::Duration::from_millis(800))).await;
    client
        .pane_send_keys(pane_id, &["Enter"])
        .await
        .map_err(up)?;
    tokio::time::sleep(pane_settle(std::time::Duration::from_millis(1200))).await;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("slash_after_enter_before_read", pane_id).await;
    let screen = client
        .pane_read(pane_id, "visible", 60)
        .await
        .map_err(up)?
        .text;
    if !crate::tui_prompts::is_switch_model_dialog(&screen) {
        return Ok(());
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("slash_confirm_before_answer", pane_id).await;
    let latest = client
        .pane_read(pane_id, "visible", 60)
        .await
        .map_err(up)?
        .text;
    if !crate::tui_prompts::is_switch_model_dialog(&latest) {
        return Ok(());
    }
    tracing::info!(
        pane_id,
        line,
        "claude asked to confirm the model switch; answering Yes"
    );
    client.pane_send_keys(pane_id, &["1"]).await.map_err(up)?;
    tokio::time::sleep(pane_settle(std::time::Duration::from_millis(1000))).await;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("slash_after_answer_before_read", pane_id).await;
    let screen = client
        .pane_read(pane_id, "visible", 60)
        .await
        .map_err(up)?
        .text;
    if !crate::tui_prompts::is_switch_model_dialog(&screen) {
        return Ok(());
    }
    let _ = client.pane_send_keys(pane_id, &["Escape"]).await;
    tracing::warn!(
        pane_id,
        line,
        "model-switch confirmation would not close; backed out with Esc"
    );
    Err(up(anyhow::anyhow!(
        "claude 的換模型確認框沒有關掉，已按 Esc 退出"
    )))
}

/// 記下「daemon 直接對這個 run 的 pane 打過字」（當場套用設定、codex 選單、`/login`）。
///
/// 2026-09-14 wits-c1-op-xh 兩次（w1HJ:pH 14:24、w1HJ:pM 15:33）：daemon 對 pane 打 `/effort` 之後，
/// 經 herdr `agent.prompt` 送的 prompt 回 ok 卻沒進 pane——第二次隔了兩分鐘，所以不是回穩時間的問題；
/// 同一個 pane 用 `pane.send_text`＋Enter 直接打字每次都成功。之後這個 run 一律走「打字進 pane 再看
/// 畫面確認」（`prompt::deliver_prompt`）。**寫進 `runs.pane_typed`**：daemon 重啟後不能忘記，否則
/// 第一則又走回已知會失效的那條路（sol review 2026-09-14 #3）。
/// 打字之前先把記號寫進 DB；寫不進去就**不要打**——打完卻沒記住，下一則與重啟後又會走回會吞訊息的
/// `agent.prompt`（sol review 第三輪 #2）。行程內的備份記號同時記上，這次啟動內不會忘。
pub async fn mark_pane_typed(app: &impl crate::capabilities::Db, run_id: &str) -> Result<(), String> {
    crate::lifecycle::remember_pane_typed(run_id);
    db::set_pane_typed(app.db(), run_id).await.map_err(|e| {
        tracing::warn!(run = run_id, error = %e, "could not record that the daemon types into this pane; not typing");
        format!("pane_typed_not_persisted: {e}")
    })
}

/// slash 指令送出後，TUI 要多久內回到「空的輸入列、畫面不再變」。
const SLASH_SETTLE_MAX_MS: u64 = 6_000;
const SLASH_SETTLE_POLL_MS: u64 = 500;

/// 等 TUI 回到空輸入列，且連續兩次讀到的畫面一模一樣才算穩（2026-09-14 w1HJ:pH：`/effort max`
/// 當場套用後緊接的 prompt 沒進 pane，12 秒後 stall）。握著 bot 鎖等，下一則 prompt 自然排在後面。
/// 等不到回 `false`，呼叫端只記 log——套用本身已經成功，送達的保險在 stall watchdog 的自動重送。
async fn wait_for_composer_settled(
    client: &HerdrClient,
    pane_id: &str,
    kind: &str,
) -> LcResult<Option<String>> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(SLASH_SETTLE_MAX_MS);
    let mut prev: Option<String> = None;
    loop {
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("slash_before_settle_read", pane_id).await;
        let screen = client
            .pane_read(pane_id, "visible", 60)
            .await
            .map_err(up)?
            .text;
        if composer_settled(kind, prev.as_deref(), &screen) {
            return Ok(Some(screen));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        prev = Some(screen);
        tokio::time::sleep(pane_settle(std::time::Duration::from_millis(SLASH_SETTLE_POLL_MS))).await;
    }
}

fn grok_model_from_screen(screen: &str) -> Option<String> {
    crate::models::grok_title_model_effort(&crate::models::grok_composer_fragment(screen)?).0
}

fn agy_last_slash_output(screen: &str) -> Option<&str> {
    let lines: Vec<&str> = screen.lines().collect();
    let end = lines.iter().rposition(|l| !l.trim().is_empty())? + 1;
    let bottom_rule = lines[..end].iter().rposition(|l| {
        let t = l.trim();
        t.chars().count() >= 10 && t.chars().all(|c| c == '─')
    })?;
    let top_rule = lines[..bottom_rule].iter().rposition(|l| {
        let t = l.trim();
        t.chars().count() >= 10 && t.chars().all(|c| c == '─')
    })?;
    let above = &lines[..top_rule];
    let cmd_at = above.iter().rposition(|l| {
        let t = l.trim();
        t.starts_with("> /model") || t.starts_with("> /effort")
    })?;
    above[cmd_at + 1..]
        .iter()
        .find_map(|l| l.split_once('⎿').map(|(_, rest)| rest.trim()))
}

fn agy_last_slash_failed(screen: &str) -> bool {
    agy_last_slash_output(screen).is_some_and(|out| {
        out.starts_with("Invalid model ")
            || out.starts_with("Invalid effort ")
            || out.starts_with("Effort isn't adjustable")
            || out.contains(" has no \"")
    })
}

fn agy_model_matches_screen(screen: &str, want: &str) -> bool {
    if agy_last_slash_failed(screen) {
        return false;
    }
    if crate::models::agy_is_orchestrator(want) {
        if !crate::models::agy_footer_is_orchestrator(screen) {
            return false;
        }
        if let Some(display) = agy_last_slash_output(screen).and_then(|o| o.strip_prefix("Model set to ")) {
            return crate::models::agy_display_name_to_slug(display).as_deref() == Some(want);
        }
        return true;
    }
    crate::models::agy_footer_model(screen).as_deref() == Some(want)
}

fn agy_effort_matches_screen(screen: &str, want_effort: &str) -> Option<String> {
    if agy_last_slash_failed(screen) {
        return None;
    }
    let suffix = format!("-{want_effort}");
    crate::models::agy_footer_model(screen).filter(|m| m.ends_with(&suffix))
}

/// 純函式：這一次讀到的畫面是空輸入列，而且跟上一次讀到的一樣。
fn composer_settled(kind: &str, prev: Option<&str>, screen: &str) -> bool {
    pane_awaits_input(kind, screen) && prev == Some(screen)
}

/// 登入／換帳號的 slash 指令（拋棄式 herdr session 實測）：claude 2.1.263 與 grok 1.0.13 是
/// `/login`；codex 0.153.4 沒有（只有 `/logout`），送進去會被當一般 prompt 丟給模型，所以 `None`。
pub fn login_slash_command(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" | "grok" => Some("/login"),
        _ => None,
    }
}

#[derive(serde::Serialize)]
pub struct LoginOut {
    pub run_id: String,
    pub kind: String,
    pub command: String,
}

/// 對正在跑的 bot 送登入指令。同 `apply_live_setting` 的 gate 與打字節奏，但使用者明確按了按鈕，
/// 送不出去要說明理由。daemon 不等登入完成；由 `POST /hosts/:name/tools/refresh` 重新偵測。
pub async fn login(app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), bot_id: &str) -> LcResult<LoginOut> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let Some(line) = login_slash_command(&bot.kind) else {
        return Err(LcError::BadValue(json!({
            "error": "login_unsupported",
            "kind": bot.kind,
            "message": format!("{} has no in-session login command", bot.kind),
        })));
    };
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| {
        LcError::conflict(SlashBlocked::NotRunning.reason(), json!({ "bot_id": bot_id }))
    })?;
    let in_flight = db::in_flight_turn(app.db(), &run.id).await.map_err(up)?.is_some();
    let pane_id = slash_gate(&run, in_flight)
        .map_err(|b| LcError::conflict(b.reason(), json!({"bot_id": bot_id, "run_id": run.id})))?;
    let client = client_for_run(app, &run).await?;
    mark_pane_typed(app, &run.id).await.map_err(LcError::Upstream)?;
    send_slash_line(&client, &pane_id, line).await?;
    tracing::info!(bot_id, kind = %bot.kind, line, "sent login slash command");
    Ok(LoginOut { run_id: run.id, kind: bot.kind, command: line.to_string() })
}

/// 手動 compact（2026-10-04 使用者：「可以對某個 bot 下 compact，按鈕做在 context 旁邊」）：claude 與 codex 都有 `/compact`；
/// grok 沒有同名指令，不猜。
pub fn compact_slash_command(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" | "codex" | "agy" | crate::config::CLAUDE_API_KIND => Some("/compact"),
        _ => None,
    }
}

/// 對正在跑、閒著的 bot 送 `/compact`。跟 [`login`] 同一個 gate（沒在跑、忙著、回合在飛、找不到 pane 都不送，回 409 說理由）
/// 與打字節奏；daemon 不等壓縮做完——之後的 statusLine 會回報新的 context 用量。
///
/// `expected_run_id`（#872，主力熱壓）：計畫當時的 active run；鎖內 active run 已換掉或不再 idle 就回 409 `superseded_run`，一個鍵都不打。
pub async fn compact(app: &impl LcHost, bot_id: &str, expected_run_id: Option<&str>) -> LcResult<LoginOut> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let Some(line) = compact_slash_command(&bot.kind) else {
        return Err(LcError::BadValue(json!({
            "error": "compact_unsupported",
            "kind": bot.kind,
            "message": format!("{} has no /compact command", bot.kind),
        })));
    };
    if crate::config::is_api_kind(&bot.kind) {
        let run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| {
            LcError::conflict(SlashBlocked::NotRunning.reason(), json!({ "bot_id": bot_id }))
        })?;
        if let Some(want) = expected_run_id {
            if run.id != want || run.agent_status != "idle" {
                return Err(LcError::conflict(
                    "superseded_run",
                    json!({"expected_run_id": want, "run_id": run.id, "agent_status": run.agent_status}),
                ));
            }
        } else if run.agent_status != "idle" {
            return Err(LcError::conflict(
                "busy",
                json!({"bot_id": bot_id, "run_id": run.id, "agent_status": run.agent_status}),
            ));
        }
        let in_flight = db::in_flight_turn(app.db(), &run.id).await.map_err(up)?.is_some();
        if in_flight {
            return Err(LcError::conflict("turn_in_flight", json!({"bot_id": bot_id, "run_id": run.id})));
        }
        let crid = am_base::db::ulid();
        app.api_native_prompt(bot_id, line, &crid).await.map_err(|e| {
            LcError::conflict(
                "prompt_failed",
                json!({"bot_id": bot_id, "error": format!("{e:?}")}),
            )
        })?;
        tracing::info!(bot_id, kind = %bot.kind, "sent /compact via api_native_prompt");
        return Ok(LoginOut { run_id: run.id, kind: bot.kind, command: line.to_string() });
    }
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| {
        LcError::conflict(SlashBlocked::NotRunning.reason(), json!({ "bot_id": bot_id }))
    })?;
    if let Some(want) = expected_run_id {
        if run.id != want || run.agent_status != "idle" {
            return Err(LcError::conflict(
                "superseded_run",
                json!({"expected_run_id": want, "run_id": run.id, "agent_status": run.agent_status}),
            ));
        }
    }
    let in_flight = db::in_flight_turn(app.db(), &run.id).await.map_err(up)?.is_some();
    let pane_id = slash_gate(&run, in_flight)
        .map_err(|b| LcError::conflict(b.reason(), json!({"bot_id": bot_id, "run_id": run.id})))?;
    let client = client_for_run(app, &run).await?;
    if bot.kind == "agy" {
        let screen = super::delivery::read_composer(&client, &pane_id).await.map_err(up)?;
        if let Some(dialog) = crate::agy_screen::blocking_dialog(&screen) {
            return Err(LcError::conflict(dialog.reason(), json!({"bot_id": bot_id, "run_id": run.id})));
        }
        if crate::agy_screen::is_busy(&screen) {
            return Err(LcError::conflict(SlashBlocked::AgentBusy.reason(), json!({"bot_id": bot_id, "run_id": run.id})));
        }
        match super::delivery::box_state("agy", &screen) {
            super::delivery::BoxState::Empty => {}
            super::delivery::BoxState::NonEmpty => {
                return Err(LcError::conflict("composer_busy", json!({"bot_id": bot_id, "run_id": run.id})));
            }
            super::delivery::BoxState::Unready => {
                return Err(LcError::conflict("composer_unreadable", json!({"bot_id": bot_id, "run_id": run.id})));
            }
        }
    }
    mark_pane_typed(app, &run.id).await.map_err(LcError::Upstream)?;
    send_slash_line(&client, &pane_id, line).await?;
    if bot.kind == "codex" {
        let conv = db::conversation_id(app.db(), bot_id).await.map_err(up)?;
        let turn = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at)
             VALUES (?,?,?,'external','in_flight','ok',?,?)",
        ).bind(&turn).bind(&conv).bind(&run.id).bind(line).bind(db::now()).execute(app.db()).await.map_err(up)?;
        emit_turn(app, &turn).await;
        arm_progress(app, &run.id, bot_id, &turn).await;
    }
    tracing::info!(bot_id, kind = %bot.kind, "sent /compact");
    Ok(LoginOut { run_id: run.id, kind: bot.kind, command: line.to_string() })
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod compact_tests {
    use super::compact_slash_command;
    #[test]
    fn compact_is_offered_for_claude_codex_and_agy() {
        assert_eq!(compact_slash_command("claude"), Some("/compact"));
        assert_eq!(compact_slash_command("codex"), Some("/compact"));
        assert_eq!(compact_slash_command("agy"), Some("/compact"));
        assert_eq!(compact_slash_command("claude-api"), Some("/compact"));
        assert_eq!(compact_slash_command("openai-api"), None);
        assert_eq!(compact_slash_command("grok"), None);
    }
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod live_slash_tests {
    use super::{agy_effort_matches_screen, agy_model_matches_screen, composer_settled, grok_model_from_screen, live_slash_command};
    /// 窄 pane 把現在的框底拆成兩行時，不能退回對話裡引用的完整舊框。
    #[test]
    fn grok_model_readback_uses_the_wrapped_composer_not_a_quoted_footer() {
        let screen = "\
  ╰────────────── Grok 4.6 (low) · always-approve ─╯

  ╭────────────────────────────────────────────────╮
  │ ❯                                              │
  ╰────────────── Grok
  4.7 (high) · always-approve ─╯
";
        assert_eq!(grok_model_from_screen(screen).as_deref(), Some("grok-4.7"));
    }
    /// 2026-09-14 w1HJ:pH 在 `/effort max` 之後的真實畫面（使用者名稱換掉）。
    const EFFORT_MAX_SETTLED: &str = "✻ Sautéed for 15m 9s · done 2:18 PM

❯ /effort max
  ⎿  Set effort level to max (this session only): Maximum capability with deepest reasoning.
     May use excessive tokens resulting in long response times or overthinking. Use sparingly
     for the hardest tasks.
                                                                              615330 tokens
─────────────────────────────────────────────────────────────────────────────────────────────
❯
─────────────────────────────────────────────────────────────────────────────────────────────
  user. | web | OP5 61% | 5h:53%(rst 2h 35m) | 7d:95%(rst 6d 21h) | F5:100%
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";
    #[test]
    fn a_slash_counts_as_settled_only_on_two_identical_empty_composer_reads() {
        assert!(!composer_settled("claude", None, EFFORT_MAX_SETTLED));
        assert!(composer_settled("claude", Some(EFFORT_MAX_SETTLED), EFFORT_MAX_SETTLED));
        let earlier = EFFORT_MAX_SETTLED.replace("615330 tokens", "615201 tokens");
        assert!(!composer_settled("claude", Some(&earlier), EFFORT_MAX_SETTLED));
        let typing = EFFORT_MAX_SETTLED.replace("\n❯\n", "\n❯ /effort max\n");
        assert!(!composer_settled("claude", Some(&typing), &typing));
    }
    #[test]
    fn grok_effort_and_model() {
        assert_eq!(
            live_slash_command("grok", "effort", "HIGH", None).as_deref(),
            None,
            "/effort 會把 default_reasoning_effort 寫進 GROK_HOME 的 config.toml"
        );
        assert_eq!(
            live_slash_command("grok", "model", "grok-4.6", Some("high")).as_deref(),
            Some("/model grok-4.6 high")
        );
        assert_eq!(
            live_slash_command("grok", "model", "grok-4.5", None).as_deref(),
            Some("/model grok-4.5")
        );
        assert_eq!(
            live_slash_command("grok", "model", "grok-4.5", Some("  ")).as_deref(),
            Some("/model grok-4.5")
        );
    }
    /// claude takes both, but its `/model` has no second parameter the way grok's does.
    #[test]
    fn claude_model_and_effort() {
        assert_eq!(
            live_slash_command("claude", "model", "opus", Some("high")).as_deref(),
            Some("/model opus")
        );
        assert_eq!(
            live_slash_command("claude", "effort", "MAX", None).as_deref(),
            Some("/effort max")
        );
        assert_eq!(
            live_slash_command("agy", "model", "gemini-3.8-flash-high", None).as_deref(),
            Some("/model gemini-3.8-flash-high")
        );
        assert_eq!(
            live_slash_command("agy", "effort", "LOW", None).as_deref(),
            Some("/effort low")
        );
        assert_eq!(live_slash_command("codex", "model", "gpt-5.5", None), None);
        assert_eq!(live_slash_command("codex", "effort", "high", None), None);
    }
    #[test]
    fn agy_screen_readback_handles_standard_and_orchestrator_footers() {
        let rule = "───────────────────────────────────────────────────────────────";
        let flash = format!("> /model\n  ⎿  Model set to Gemini 3.8 Flash (High)\n{rule}\n>\n{rule}\n? for shortcuts      Gemini 3.8 Flash · high\n");
        assert!(agy_model_matches_screen(&flash, "gemini-3.8-flash-high"));
        assert!(!agy_model_matches_screen(&flash, "gemini-3.8-flash-low"));
        assert_eq!(agy_effort_matches_screen(&flash, "high").as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(agy_effort_matches_screen(&flash, "low"), None);

        let orch = format!("> /model\n  ⎿  Model set to Orchestrator (Argon)\n{rule}\n>\n{rule}\n  auto · Orchestrator · Gemini 4 Neon · xhigh\n");
        assert!(agy_model_matches_screen(&orch, "gemini-pro-orchestrator-g1-pro"));
        assert!(!agy_model_matches_screen(&orch, "gemini-pro-orchestrator-neon"));
        assert!(!agy_model_matches_screen(&orch, "gemini-4-neon-xhigh"));
        assert_eq!(agy_effort_matches_screen(&orch, "xhigh"), None);

        let invalid = format!("> /model\n  ⎿  Invalid model \"gemini-pro-orchestrator-bogus\" (available: gemini-3.8-flash-high)\n{rule}\n>\n{rule}\n  auto · Orchestrator · Gemini 4 Neon · xhigh\n");
        assert!(!agy_model_matches_screen(&invalid, "gemini-pro-orchestrator-neon"));
    }
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod login_slash_tests {
    use super::{live_gate, login_slash_command, slash_gate, LiveApplyGuard, LiveApplyMatch, live_apply_window_match, SlashBlocked};
    use crate::db;
    use std::time::Duration;
    fn run(state: &str, agent_status: &str, pane_id: Option<&str>) -> db::Run {
        db::Run { daemon_url_injected: 0, runtime_role_policy: None,
            id: "r1".into(),
            bot_id: "b1".into(),
            state: state.into(),
            agent_status: agent_status.into(),
            workspace_id: Some("w1".into()),
            pane_id: pane_id.map(str::to_string),
            tab_id: Some("w1:t1".into()),
            adopted: 0,
            agent_name: None,
            herdr_session: None,
            agent_title: None,
            status_line: None,
            status_json: None,
            runtime_model: None,
            runtime_effort: None,
            runtime_fast: None,
            runtime_identity: None,
            update_notice: None,
            turn_error: None,
            terminal_id: None,
            native_session_id: None,
            transcript_path: None,
            last_read_revision: None,
            last_read_tail_hash: None,
            started_at: "2026-01-01T00:00:00Z".into(),
            ended_at: None,
            resume_session_id: None,
            resume_outcome: None,
            agent_status_since: None,
            subagent_json: None,
            launch_rev: None,
            live_rev: None,
            codex_instant_interrupt: None,
            cli_version: None,
        }
    }
    /// codex 沒有 `/login`（只有 `/logout`），回「不支援」好過送不存在的指令。
    #[test]
    fn only_claude_and_grok_can_log_in_from_the_tui() {
        assert_eq!(login_slash_command("claude"), Some("/login"));
        assert_eq!(login_slash_command("grok"), Some("/login"));
        assert_eq!(login_slash_command("codex"), None);
        assert_eq!(login_slash_command(""), None);
        assert_eq!(login_slash_command("Claude"), None);
    }
    #[test]
    fn an_idle_running_pane_is_typeable() {
        assert_eq!(slash_gate(&run("running", "idle", Some("w1:p1")), false), Ok("w1:p1".into()));
        assert_eq!(slash_gate(&run("running", "unknown", Some("w1:p1")), false), Ok("w1:p1".into()));
    }
    #[test]
    fn each_reason_is_reported_separately() {
        assert_eq!(slash_gate(&run("stopped", "idle", Some("w1:p1")), false), Err(SlashBlocked::NotRunning));
        assert_eq!(slash_gate(&run("exited", "idle", Some("w1:p1")), false), Err(SlashBlocked::NotRunning));
        assert_eq!(slash_gate(&run("running", "working", Some("w1:p1")), false), Err(SlashBlocked::AgentBusy));
        assert_eq!(slash_gate(&run("running", "blocked", Some("w1:p1")), false), Err(SlashBlocked::AgentBusy));
        assert_eq!(slash_gate(&run("running", "idle", Some("w1:p1")), true), Err(SlashBlocked::TurnInFlight));
        assert_eq!(slash_gate(&run("running", "idle", None), false), Err(SlashBlocked::NoPane));
        assert_eq!(slash_gate(&run("running", "idle", Some("  ")), false), Err(SlashBlocked::NoPane));
    }
    /// #712：codex 回合中只切 fast 直接送（0.157.1 `/fast` 回合中照樣生效）；其他組合照舊擋。
    #[test]
    fn only_a_codex_fast_only_change_goes_in_during_a_turn() {
        let working = run("running", "working", Some("w1:p1"));
        assert_eq!(live_gate(&working, true, "codex", &["fast"]), Ok(("w1:p1".into(), true)));
        assert_eq!(live_gate(&run("running", "idle", Some("w1:p1")), true, "codex", &["fast"]), Ok(("w1:p1".into(), true)), "回合在飛");
        assert_eq!(live_gate(&run("running", "idle", Some("w1:p1")), false, "codex", &["fast"]), Ok(("w1:p1".into(), false)));
        assert_eq!(live_gate(&working, true, "codex", &["model", "fast"]), Err(SlashBlocked::AgentBusy), "model 要開選單，回合中不碰");
        assert_eq!(live_gate(&working, true, "codex", &["effort"]), Err(SlashBlocked::AgentBusy));
        assert_eq!(live_gate(&working, true, "claude", &["fast"]), Err(SlashBlocked::AgentBusy));
        assert_eq!(live_gate(&working, true, "grok", &["effort"]), Err(SlashBlocked::AgentBusy));
        assert_eq!(live_gate(&run("running", "blocked", Some("w1:p1")), false, "codex", &["fast"]), Err(SlashBlocked::AgentBusy), "blocked：字會掉進權限框");
        assert_eq!(live_gate(&run("running", "working", None), true, "codex", &["fast"]), Err(SlashBlocked::NoPane));
        assert_eq!(live_gate(&run("stopped", "working", Some("w1:p1")), true, "codex", &["fast"]), Err(SlashBlocked::NotRunning));
    }
    /// 停掉的 run 就算同時在忙也先報「沒在跑」：那是使用者要先處理的那一件事。
    #[test]
    fn the_reasons_are_checked_in_the_order_the_user_would_fix_them() {
        assert_eq!(slash_gate(&run("stopped", "working", None), true), Err(SlashBlocked::NotRunning));
        assert_eq!(slash_gate(&run("running", "working", None), true), Err(SlashBlocked::AgentBusy));
        assert_eq!(slash_gate(&run("running", "idle", None), true), Err(SlashBlocked::TurnInFlight));
    }
    #[test]
    fn reasons_are_stable_wire_keys() {
        assert_eq!(SlashBlocked::NotRunning.reason(), "not_running");
        assert_eq!(SlashBlocked::AgentBusy.reason(), "agent_busy");
        assert_eq!(SlashBlocked::TurnInFlight.reason(), "turn_in_flight");
        assert_eq!(SlashBlocked::NoPane.reason(), "no_pane");
    }

    #[test]
    fn live_apply_window_matching_lifecycle() {
        let run_id = "test-run-window-lifecycle";
        assert_eq!(live_apply_window_match(run_id, std::time::Instant::now()), LiveApplyMatch::None);

        let guard = LiveApplyGuard::enter(run_id, Some("/effort xhigh".to_string()));
        let t_during = std::time::Instant::now();
        assert_eq!(live_apply_window_match(run_id, t_during), LiveApplyMatch::InProgressOrOverlapped);

        drop(guard);

        let t_after_drop = std::time::Instant::now();
        assert_eq!(live_apply_window_match(run_id, t_during), LiveApplyMatch::InProgressOrOverlapped);
        assert_eq!(
            live_apply_window_match(run_id, t_after_drop),
            LiveApplyMatch::RecentTail {
                sent_line: Some("/effort xhigh".to_string()),
            }
        );

        let t_future_expired = t_after_drop + Duration::from_secs(3);
        assert_eq!(live_apply_window_match(run_id, t_future_expired), LiveApplyMatch::None);
    }

    #[test]
    fn live_apply_guard_early_drop_sets_finished_at() {
        let run_id = "test-run-early-drop";
        {
            let _guard = LiveApplyGuard::enter(run_id, None);
            assert_eq!(
                live_apply_window_match(run_id, std::time::Instant::now()),
                LiveApplyMatch::InProgressOrOverlapped
            );
        }
        let t_tail = std::time::Instant::now();
        assert_eq!(
            live_apply_window_match(run_id, t_tail),
            LiveApplyMatch::RecentTail { sent_line: None }
        );
        let t_expired = t_tail + Duration::from_secs(3);
        assert_eq!(live_apply_window_match(run_id, t_expired), LiveApplyMatch::None);
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    //! #1862 L2：daemon 測試要碰的私有項，窄轉呼叫（只在測試建置存在）。
    use super::*;
    pub async fn send_slash_line(client: &HerdrClient, pane_id: &str, line: &str) -> LcResult<()> { super::send_slash_line(client, pane_id, line).await }
    pub use super::{LiveApplyGuard, LiveApplyMatch, live_apply_window_match};
}
