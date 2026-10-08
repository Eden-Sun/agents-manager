//! Keystrokes, text and slash commands sent straight at a live pane.

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::state::App;
use super::*;

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
    let target = db::run_target(&run, &bot);
    client_for_run(app, &run).await?.agent_send_keys(&target, &keys).await.map_err(up)?;
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
    // 併送也是 daemon 直接打進 pane（#648）。沒先記下的話，下一則 prompt 仍走 agent.prompt，
    // 那條路會回 ok 但字沒進去。寫不進去就不打。
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
        // grok 1.0.46：`/effort` 立刻把 `[models] default_reasoning_effort` 寫進 `GROK_HOME` 的
        // config.toml（使用者自己開的 grok 也會沿用）。等級只走啟動參數 `--reasoning-effort`。
        ("grok", "effort") => None,
        ("claude", "effort") => Some(format!("/effort {}", value.to_ascii_lowercase())),
        ("grok", "model") => {
            let mut line = format!("/model {value}");
            if let Some(e) = effort.map(str::trim).filter(|s| !s.is_empty()) {
                line.push(' ');
                line.push_str(&e.to_ascii_lowercase());
            }
            Some(line)
        }
        ("claude", "model") => Some(format!("/model {value}")),
        _ => None,
    }
}

/// 不重啟就套用設定（SPEC §4.4a）。grok `model`、claude `model`/`effort` 走一行 slash
/// 指令；grok `effort` 不送 `/effort`（會寫進 config.toml）。codex 的 `/model` 是不吃參數的選單、`/fast` 是開關，走 [`crate::codex_live`]
/// 送鍵讀畫面再回讀狀態列（2026-09-09 實測）。副作用：claude（2.1.263 實測）與 codex 都會把
/// 選擇存成帳號之後新 session 的預設。
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
    // 呼叫端沒有「改設定前」的版本：不知道 run 原本的落差是不是只來自這次的欄位，所以只落 runtime，
    // 不寫 `live_rev` 豁免（SPEC §4.4a 第 4 點）。空字串不會等於任何 `launch_rev`。
    apply_live_setting_locked(app, bot_id, fields, "", &target_rev).await
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
    apply_live_setting_locked(app, bot_id, fields, baseline_rev, target_rev).await
}

async fn apply_live_setting_locked(
    app: &impl LcHost,
    bot_id: &str,
    fields: &[&str],
    baseline_rev: &str,
    target_rev: &str,
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
    let result = match apply_live_setting_inner(app, bot_id, fields, target_rev).await {
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
    app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess),
    bot_id: &str,
    fields: &[&str],
    target_rev: &str,
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
        Err(why) => return Err(format!("slash_gate: {}", why.reason())),
    };
    let Ok(client) = client_for_run(app, &run).await else {
        return Err("no_herdr_client".into());
    };

    if bot.kind == "codex" {
        // `/fast` 是開關：不知道現在狀態就不能按。
        let was_fast = run.runtime_fast.map(|v| v != 0);
        mark_pane_typed(app, &run.id).await?;
        let applied = if during_turn {
            crate::codex_live::apply_fast_during_turn(&client, &pane_id, &bot, was_fast).await
        } else {
            crate::codex_live::apply(&client, &pane_id, &bot, was_fast, fields).await
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
    // `/effort`、`/model` 都一定要帶值，清成「不指定」沒有 slash 指令。
    let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(format!("{field}_cleared_to_default"));
    };
    let Some(line) = live_slash_command(&bot.kind, field, value, bot.effort.as_deref()) else {
        return Err(format!("no_slash_command_for_{field}"));
    };
    // 不管套用成不成功，pane 都被直接打過字了。
    mark_pane_typed(app, &run.id).await?;
    if let Err(e) = send_slash_line(&client, &pane_id, &line).await {
        return Err(format!("slash_send_failed: {e:?}"));
    }
    // 還握著 bot 鎖：`prompt_grouped` 拿同一把鎖，所以下一則 prompt 一定排在 TUI 回到輸入列之後。
    let settled_screen = match wait_for_composer_settled(&client, &pane_id, &bot.kind).await {
        Ok(Some(screen)) => screen,
        Ok(None) => return Err("slash_composer_not_settled".into()),
        Err(e) => return Err(format!("slash_settle_read_failed: {e:?}")),
    };
    // Grok prints its active model and effort in the status frame. Use that read-back before
    // clearing the drift marker; a closed picker alone does not prove the requested value landed.
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
    let mut snapshot = RuntimeSnapshot {
        model: run.runtime_model.clone(),
        effort: run.runtime_effort.clone(),
        fast: run.runtime_fast,
    };
    match *field {
        "effort" => snapshot.effort = Some(value.to_string()),
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
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    client
        .pane_send_keys(pane_id, &["Enter"])
        .await
        .map_err(up)?;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
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
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
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
        tokio::time::sleep(std::time::Duration::from_millis(SLASH_SETTLE_POLL_MS)).await;
    }
}

fn grok_model_from_screen(screen: &str) -> Option<String> {
    crate::models::grok_title_model_effort(&crate::models::grok_composer_fragment(screen)?).0
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
        "claude" | "codex" => Some("/compact"),
        _ => None,
    }
}

/// 對正在跑、閒著的 bot 送 `/compact`。跟 [`login`] 同一個 gate（沒在跑、忙著、回合在飛、找不到 pane 都不送，回 409 說理由）
/// 與打字節奏；daemon 不等壓縮做完——之後的 statusLine 會回報新的 context 用量。
///
/// `expected_run_id`（#872，主力熱壓）：計畫當時的 active run；鎖內 active run 已換掉或不再 idle 就回 409 `superseded_run`，一個鍵都不打。
pub async fn compact(app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), bot_id: &str, expected_run_id: Option<&str>) -> LcResult<LoginOut> {
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
    mark_pane_typed(app, &run.id).await.map_err(LcError::Upstream)?;
    send_slash_line(&client, &pane_id, line).await?;
    tracing::info!(bot_id, kind = %bot.kind, "sent /compact");
    Ok(LoginOut { run_id: run.id, kind: bot.kind, command: line.to_string() })
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod compact_tests {
    use super::compact_slash_command;

    #[test]
    fn compact_is_offered_for_claude_and_codex_only() {
        assert_eq!(compact_slash_command("claude"), Some("/compact"));
        assert_eq!(compact_slash_command("codex"), Some("/compact"));
        assert_eq!(compact_slash_command("grok"), None);
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod live_slash_tests {
    use super::{composer_settled, grok_model_from_screen, live_slash_command};
    use crate::testing as tt;

    /// #872：計畫綁 run A、鎖內 active 已是 B（或 A 不再 idle）→ `compact` 回 409 `superseded_run`，假 herdr 一個字、一個鍵都沒收到。
    #[tokio::test]
    async fn compact_with_a_stale_expected_run_types_nothing() {
        use crate::lifecycle::LcError;
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "compact-stale").await;
        let run_a = tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET state = 'exited', ended_at = ? WHERE id = ?").bind(crate::db::now()).bind(&run_a).execute(&env.app.db).await.unwrap();
        let run_b = tt::fake_run(&env.app, &bot.id).await;

        match super::compact(&env.app, &bot.id, Some(&run_a)).await.err().expect("superseded") {
            LcError::Conflict(v) => {
                assert_eq!(v["reason"], "superseded_run", "{v}");
                assert_eq!(v["run_id"], run_b.as_str());
            }
            other => panic!("{other:?}"),
        }
        assert!(env.herdr.calls_to("pane.send_text").is_empty() && env.herdr.calls_to("pane.send_keys").is_empty(), "一個字、一個鍵都沒打");

        // B 不再 idle：就算 expected 對得上也不送。
        sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?").bind(&run_b).execute(&env.app.db).await.unwrap();
        match super::compact(&env.app, &bot.id, Some(&run_b)).await.err().expect("not idle") {
            LcError::Conflict(v) => assert_eq!(v["reason"], "superseded_run", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(env.herdr.calls_to("pane.send_text").is_empty() && env.herdr.calls_to("pane.send_keys").is_empty());
    }

    const SWITCH_MODEL: &str = "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch to Claude Opus 5.5\n  2. No, go back\n";

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

    async fn model_apply_fixture(
        env: &tt::Env,
        name: &str,
        field: &str,
    ) -> (String, String, String) {
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let pane = format!("pane-{name}");
        let (setting_column, runtime_column, setting_value, old_value) = match field {
            "model" => (
                "model",
                "runtime_model",
                "claude-opus-5-5",
                "claude-sonnet-4-5",
            ),
            "effort" => ("effort", "runtime_effort", "max", "low"),
            _ => unreachable!(),
        };
        sqlx::query(&format!("UPDATE bots SET {setting_column}=? WHERE id=?"))
            .bind(setting_value)
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        sqlx::query(&format!(
            "UPDATE runs SET pane_id=?, {runtime_column}=? WHERE id=?"
        ))
        .bind(&pane)
        .bind(old_value)
        .bind(&run_id)
        .execute(&env.app.db)
        .await
        .unwrap();
        (bot.id, run_id, pane)
    }

    #[tokio::test]
    async fn unreadable_confirmation_reads_never_write_the_requested_runtime_value() {
        let env = tt::env().await;
        for (name, field, runtime_column, desired, command) in [
            (
                "model-read-error",
                "model",
                "runtime_model",
                "claude-opus-5-5",
                "/model claude-opus-5-5",
            ),
            (
                "effort-read-error",
                "effort",
                "runtime_effort",
                "max",
                "/effort max",
            ),
        ] {
            let (bot_id, run_id, pane) = model_apply_fixture(&env, name, field).await;
            env.herdr.set_screen(&pane, "Claude Code\n❯\n");
            let screens = env.herdr.screens.clone();
            let fail = env.herdr.fail_later();
            let pane_for_hook = pane.clone();
            super::super::race_point::arm(
                "slash_after_enter_before_read",
                &pane,
                move || async move {
                    screens
                        .lock()
                        .unwrap()
                        .insert(pane_for_hook, SWITCH_MODEL.into());
                    fail("pane.read", tt::Fault::Refuse);
                },
            );

            let reason = super::apply_live_setting(&env.app, &bot_id, &[field]).await;
            assert!(
                reason.is_some(),
                "a failed dialog read cannot be reported as a live apply"
            );
            assert!(
                reason
                    .as_deref()
                    .is_some_and(|why| why.starts_with("slash_send_failed")),
                "the failed authority read must be reported directly: {reason:?}"
            );
            let runtime: Option<String> =
                sqlx::query_scalar(&format!("SELECT {runtime_column} FROM runs WHERE id=?"))
                    .bind(&run_id)
                    .fetch_one(&env.app.db)
                    .await
                    .unwrap();
            assert_eq!(
                runtime.as_deref(),
                Some(if field == "model" {
                    "claude-sonnet-4-5"
                } else {
                    "low"
                })
            );
            assert_eq!(
                env.herdr
                    .calls_to("pane.send_text")
                    .last()
                    .and_then(|v| v["text"].as_str()),
                Some(command)
            );
            let keys = env.herdr.calls_to("pane.send_keys");
            assert_eq!(
                keys.last().map(|v| v["keys"].clone()),
                Some(serde_json::json!(["Enter"]))
            );
            assert!(!keys.iter().any(|v| v["keys"] == serde_json::json!(["1"])
                || v["keys"] == serde_json::json!(["Escape"])));
            assert!(desired != runtime.as_deref().unwrap_or_default());
        }
    }

    #[tokio::test]
    async fn unreadable_reread_before_answer_does_not_press_yes() {
        let env = tt::env().await;
        let pane = "pane-before-answer";
        env.herdr.set_screen(pane, SWITCH_MODEL);
        let fail = env.herdr.fail_later();
        super::super::race_point::arm("slash_confirm_before_answer", pane, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });

        assert!(
            super::send_slash_line(&env.app.herdr, pane, "/model claude-opus-5-5")
                .await
                .is_err()
        );
        assert_eq!(
            env.herdr
                .calls_to("pane.send_keys")
                .iter()
                .map(|v| v["keys"].clone())
                .collect::<Vec<_>>(),
            vec![serde_json::json!(["Enter"])]
        );
    }

    #[tokio::test]
    async fn unreadable_read_after_answer_is_unknown_and_is_not_cleaned_up_with_escape() {
        let env = tt::env().await;
        let pane = "pane-after-answer";
        env.herdr.set_screen(pane, SWITCH_MODEL);
        let fail = env.herdr.fail_later();
        super::super::race_point::arm("slash_after_answer_before_read", pane, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });

        assert!(
            super::send_slash_line(&env.app.herdr, pane, "/model claude-opus-5-5")
                .await
                .is_err()
        );
        assert_eq!(
            env.herdr
                .calls_to("pane.send_keys")
                .iter()
                .map(|v| v["keys"].clone())
                .collect::<Vec<_>>(),
            vec![serde_json::json!(["Enter"]), serde_json::json!(["1"]),]
        );
    }

    #[tokio::test]
    async fn an_unsettled_composer_does_not_write_runtime_model() {
        let env = tt::env().await;
        let (bot_id, run_id, pane) = model_apply_fixture(&env, "unsettled", "model").await;
        env.herdr
            .set_screen(&pane, "Claude Code\nworking on a task\n");

        let reason = super::apply_live_setting(&env.app, &bot_id, &["model"]).await;
        assert!(
            reason.is_some(),
            "settle timeout must fall back to restart/unknown"
        );
        let runtime: Option<String> =
            sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id=?")
                .bind(run_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        assert_eq!(runtime.as_deref(), Some("claude-sonnet-4-5"));
    }

    #[tokio::test]
    async fn an_unreadable_settle_read_does_not_write_runtime_model() {
        let env = tt::env().await;
        let (bot_id, run_id, pane) = model_apply_fixture(&env, "settle-read-error", "model").await;
        env.herdr.set_screen(&pane, "Claude Code\n❯\n");
        let fail = env.herdr.fail_later();
        super::super::race_point::arm("slash_before_settle_read", &pane, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });

        let reason = super::apply_live_setting(&env.app, &bot_id, &["model"]).await;
        assert!(
            reason.is_some(),
            "a failed settle read cannot be counted as a stable composer"
        );
        let runtime: Option<String> =
            sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id=?")
                .bind(run_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        assert_eq!(runtime.as_deref(), Some("claude-sonnet-4-5"));
    }

    #[tokio::test]
    async fn an_unreadable_grok_startup_effort_screen_does_not_send_or_mark_runtime() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "grok-startup-read-error").await;
        sqlx::query("UPDATE bots SET kind='grok', effort='high' WHERE id=?")
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let pane = "pane-grok-startup-read-error";
        sqlx::query("UPDATE runs SET pane_id=?, runtime_effort='low' WHERE id=?")
            .bind(pane)
            .bind(&run_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        env.herdr.set_screen(pane, "__READ_ERROR__");
        let bot = crate::db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();

        assert!(
            super::apply_grok_startup_effort(&env.app, &bot, &run_id, pane, &env.app.herdr)
                .await
                .is_ok()
        );
        assert!(env.herdr.calls_to("pane.send_text").is_empty());
        assert!(env.herdr.calls_to("pane.send_keys").is_empty());
        let runtime: Option<String> =
            sqlx::query_scalar("SELECT runtime_effort FROM runs WHERE id=?")
                .bind(run_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        assert_eq!(runtime.as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn a_readable_closed_confirmation_and_settled_composer_can_be_applied() {
        let env = tt::env().await;
        let (bot_id, run_id, pane) = model_apply_fixture(&env, "confirmed", "model").await;
        env.herdr.set_screen(&pane, SWITCH_MODEL);
        let screens = env.herdr.screens.clone();
        let pane_for_hook = pane.clone();
        super::super::race_point::arm(
            "slash_after_answer_before_read",
            &pane,
            move || async move {
                screens
                    .lock()
                    .unwrap()
                    .insert(pane_for_hook, "Claude Code\n❯\n".into());
            },
        );

        assert_eq!(
            super::apply_live_setting(&env.app, &bot_id, &["model"]).await,
            None
        );
        let runtime: Option<String> =
            sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id=?")
                .bind(run_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        assert_eq!(runtime.as_deref(), Some("claude-opus-5-5"));
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
        // 第一次讀到還沒有可比的上一張：不算穩。
        assert!(!composer_settled("claude", None, EFFORT_MAX_SETTLED));
        assert!(composer_settled("claude", Some(EFFORT_MAX_SETTLED), EFFORT_MAX_SETTLED));
        // 畫面還在變（回饋文字剛印出來、token 數在跳）：不算穩。
        let earlier = EFFORT_MAX_SETTLED.replace("615330 tokens", "615201 tokens");
        assert!(!composer_settled("claude", Some(&earlier), EFFORT_MAX_SETTLED));
        // 輸入列裡還有字：不是空的，不算穩。
        let typing = EFFORT_MAX_SETTLED.replace("\n❯\n", "\n❯ /effort max\n");
        assert!(!composer_settled("claude", Some(&typing), &typing));
    }

    #[tokio::test]
    async fn a_closed_model_switch_confirmation_does_not_get_a_stale_one() {
        let env = tt::env().await;
        let pane = "pane-switch-race";
        env.herdr.set_screen(pane, "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch\n  2. No, go back\n");
        let screens = env.herdr.screens.clone();
        crate::lifecycle::race_point::arm("slash_confirm_before_answer", pane, move || async move {
            screens.lock().unwrap().insert(pane.into(), "Claude Code\n❯\n".into());
        });

        super::send_slash_line(&env.app.herdr, pane, "/login").await.unwrap();
        let keys = env.herdr.calls_to("pane.send_keys");
        assert_eq!(keys.len(), 1, "only the initial Enter is sent");
        assert_eq!(keys[0]["keys"], serde_json::json!(["Enter"]));
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
        // codex has no slash for either (0.153.4).
        assert_eq!(live_slash_command("codex", "model", "gpt-5.5", None), None);
        assert_eq!(live_slash_command("codex", "effort", "high", None), None);
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod login_slash_tests {
    use super::{live_gate, login_slash_command, slash_gate, SlashBlocked};
    use crate::db;

    fn run(state: &str, agent_status: &str, pane_id: Option<&str>) -> db::Run {
        db::Run {
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
        // 「不知道」不是「在忙」：擋掉它只會讓按鈕在正常狀態下也按不動。
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
        // 空字串的 pane id 和沒有 pane 是同一件事。
        assert_eq!(slash_gate(&run("running", "idle", Some("  ")), false), Err(SlashBlocked::NoPane));
    }

    /// #712：codex 回合中只切 fast 直接送（0.157.1 `/fast` 回合中照樣生效）；其他組合照舊擋。
    #[test]
    fn only_a_codex_fast_only_change_goes_in_during_a_turn() {
        let working = run("running", "working", Some("w1:p1"));
        assert_eq!(live_gate(&working, true, "codex", &["fast"]), Ok(("w1:p1".into(), true)));
        assert_eq!(live_gate(&run("running", "idle", Some("w1:p1")), true, "codex", &["fast"]), Ok(("w1:p1".into(), true)), "回合在飛");
        // 閒著走一般路徑（可以按 Esc 關選單、改 model／effort）。
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
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod send_text_tests {
    use super::*;
    use crate::testing as tt;

    /// #648：併送直接打進 pane，必須先記下 pane_typed，之後的 prompt 才不會走 agent.prompt。
    #[tokio::test]
    async fn typing_text_into_the_pane_marks_it_so_later_prompts_are_typed() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "text-mark").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let transcript = tt::trusted_transcript(&env.app, &bot.id, &env.dir, "text-mark.jsonl").await;
        std::fs::write(&transcript, "").unwrap();
        sqlx::query("UPDATE runs SET native_session_id='sess-1', transcript_path=? WHERE id=?")
            .bind(transcript.to_str().unwrap())
            .bind(&run_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        env.herdr.live_pane(&pane, tt::LivePane::default());
        env.herdr.set_agent("agent", &pane, true);

        send_text(&env.app, &bot.id, "併送一句", true, None).await.unwrap();
        assert!(crate::db::pane_typed(&env.app.db, &run_id).await.unwrap());

        let run = crate::db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        let bot = crate::db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let client = super::super::client_for_run(&env.app, &run).await.unwrap();
        let planned = super::super::plan_delivery(&env.app, &client, &run, &bot, "下一則", false, false).await.unwrap();
        assert!(matches!(planned, Ok(Plan::Type { .. })), "併送之後不能再走 agent.prompt：{planned:?}");
    }

    /// 記號寫不進去就不打（跟 /login 同一條）。
    #[tokio::test]
    async fn text_is_not_typed_when_pane_typed_cannot_be_recorded() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "text-unwritable").await;
        tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("CREATE TRIGGER no_pane_typed BEFORE UPDATE OF pane_typed ON runs BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END")
            .execute(&env.app.db)
            .await
            .unwrap();

        let err = send_text(&env.app, &bot.id, "不要打", true, None).await.unwrap_err();
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        assert!(env.herdr.calls_to("pane.send_text").is_empty(), "寫不進去就不打");
        assert!(env.herdr.calls_to("pane.send_keys").is_empty());
    }

    async fn in_flight_turn(app: &Arc<App>, bot_id: &str, run_id: &str) -> String {
        let conv = crate::db::conversation_id(&app.db, bot_id).await.unwrap();
        let turn_id = crate::db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,?,'web','in_flight','ok','跑一次測試',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(run_id)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        turn_id
    }

    async fn user_rows(app: &Arc<App>, bot_id: &str) -> Vec<(Option<String>, String, Option<String>)> {
        let conv = crate::db::conversation_id(&app.db, bot_id).await.unwrap();
        sqlx::query_as("SELECT turn_id, content, sent_via FROM messages WHERE conversation_id=? AND role='user' ORDER BY created_at, rowid")
            .bind(conv)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 使用者 2026-09-28：補充也是發出的訊息，要在對話窗右邊。打字成功後記成進行中回合的使用者訊息並推 `message_added`。
    #[tokio::test]
    async fn a_recorded_supplement_is_a_user_message_on_the_in_flight_turn() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "supp-rec").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        env.herdr.live_pane(&format!("pane-{}", bot.id), tt::LivePane::default());
        let turn_id = in_flight_turn(&env.app, &bot.id, &run_id).await;
        let mut rx = env.app.subscribe();

        let m = send_text_recorded(&env.app, &bot.id, "順便看一下 log", true, None, true).await.unwrap().expect("有進行中的回合就記");
        assert_eq!((m.role.as_str(), m.source.as_str(), m.sent_via.as_deref()), ("user", "web", Some("supplement")));
        assert_eq!(user_rows(&env.app, &bot.id).await, vec![(Some(turn_id.clone()), "順便看一下 log".to_string(), Some("supplement".to_string()))]);
        assert_eq!(env.herdr.calls_to("pane.send_text").len(), 1, "照樣打進 pane");
        let mut pushed = false;
        while let Ok(ev) = rx.try_recv() {
            pushed |= ev.kind == "message_added" && ev.data["message"]["id"] == m.id && ev.data["bot_id"] == bot.id;
        }
        assert!(pushed, "推 message_added，網頁才看得到泡泡");

        // 不帶 record（BlockedChoices 那些打字）照舊不留訊息。
        assert!(send_text_recorded(&env.app, &bot.id, "1", true, None, false).await.unwrap().is_none());
        assert_eq!(user_rows(&env.app, &bot.id).await.len(), 1);
    }

    /// 沒有進行中的回合：這一句 CLI 會當成新的一輪，hook 開外部回合時自己記；這裡記了就會多一則。
    #[tokio::test]
    async fn a_supplement_without_an_in_flight_turn_is_not_recorded() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "supp-idle").await;
        tt::fake_run(&env.app, &bot.id).await;
        env.herdr.live_pane(&format!("pane-{}", bot.id), tt::LivePane::default());

        assert!(send_text_recorded(&env.app, &bot.id, "閒著", true, None, true).await.unwrap().is_none());
        assert!(user_rows(&env.app, &bot.id).await.is_empty());
    }

    /// 打字失敗（這裡是 pane_typed 寫不進去、一個字都沒打）：不留訊息，錯照回。
    #[tokio::test]
    async fn a_supplement_that_was_not_typed_is_not_recorded() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "supp-fail").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        in_flight_turn(&env.app, &bot.id, &run_id).await;
        sqlx::query("CREATE TRIGGER no_pane_typed BEFORE UPDATE OF pane_typed ON runs BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END")
            .execute(&env.app.db)
            .await
            .unwrap();

        assert!(send_text_recorded(&env.app, &bot.id, "沒打進去", true, None, true).await.is_err());
        assert!(user_rows(&env.app, &bot.id).await.is_empty(), "沒打進去就不能有泡泡");
    }
}
