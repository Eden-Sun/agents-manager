//! Keystrokes, text and slash commands sent straight at a live pane.

use super::*;

pub async fn send_keys(app: &Arc<App>, bot_id: &str, keys: Vec<String>, expect_run_id: Option<String>) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
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
pub async fn send_text(app: &Arc<App>, bot_id: &str, text: &str, enter: bool, expect_run_id: Option<String>) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
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
    if !text.is_empty() {
        client.pane_send_text(&pane_id, text).await.map_err(up)?;
    }
    if enter {
        client.pane_send_keys(&pane_id, &["enter"]).await.map_err(up)?;
    }
    Ok(())
}

/// TUI slash command for a live setting, or `None` (caller reports `needs_restart`).
fn live_slash_command(kind: &str, field: &str, value: &str, effort: Option<&str>) -> Option<String> {
    match (kind, field) {
        ("grok", "effort") | ("claude", "effort") => Some(format!("/effort {}", value.to_ascii_lowercase())),
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

/// 不重啟就套用設定（SPEC §4.4a）。grok `effort`/`model`、claude `model`/`effort` 走一行 slash
/// 指令；codex 的 `/model` 是不吃參數的選單、`/fast` 是開關，走 [`crate::codex_live`]
/// 送鍵讀畫面再回讀狀態列（2026-09-09 實測）。副作用：claude（2.1.263 實測）與 codex 都會把
/// 選擇存成帳號之後新 session 的預設。
/// 回傳 `None` = 已套用；`Some(理由)` = 退回重啟。理由一路帶回 `live_apply` 並寫 log——
/// 2026-09-13 使用者問 codex 改 effort 為何重啟，當時每個失敗出口都是靜默的。
pub async fn apply_live_setting(app: &Arc<App>, bot_id: &str, fields: &[&str]) -> Option<String> {
    let reason = apply_live_setting_inner(app, bot_id, fields).await;
    match &reason {
        Some(why) => tracing::info!(bot_id, ?fields, reason = %why, "設定沒能當場套用，改用重啟"),
        None => tracing::info!(bot_id, ?fields, "設定已當場套用，不需要重啟"),
    }
    reason
}

async fn apply_live_setting_inner(app: &Arc<App>, bot_id: &str, fields: &[&str]) -> Option<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let Ok(Some(bot)) = db::bot(&app.db, bot_id).await else { return Some("bot_missing".into()) };
    let Ok(Some(run)) = db::active_run(&app.db, bot_id).await else { return Some("no_active_run".into()) };
    let in_flight = !matches!(db::in_flight_turn(&app.db, &run.id).await, Ok(None));
    let pane_id = match slash_gate(&run, in_flight) {
        Ok(p) => p,
        Err(why) => return Some(format!("slash_gate: {}", why.reason())),
    };
    let Ok(client) = client_for_run(app, &run).await else { return Some("no_herdr_client".into()) };

    if bot.kind == "codex" {
        // `/fast` 是開關：不知道現在狀態就不能按。
        let was_fast = run.runtime_fast.map(|v| v != 0);
        if let Err(why) = mark_pane_typed(app, &run.id).await {
            return Some(why);
        }
        let seen = match crate::codex_live::apply(&client, &pane_id, &bot, was_fast, fields).await {
            Ok(seen) => seen,
            Err(why) => return Some(format!("codex: {why}")),
        };
        // 回讀的狀態列才是 runtime 的定義（SPEC §4.4a）。
        let _ = sqlx::query("UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ? WHERE id = ?")
            .bind(&seen.model)
            .bind(&seen.effort)
            .bind(i64::from(seen.fast))
            .bind(&run.id)
            .execute(&app.db)
            .await;
        app.emit_bot_status(bot_id).await;
        tracing::info!(bot_id, model = %seen.model, effort = ?seen.effort, fast = seen.fast, "codex applied live");
        return None;
    }

    let [field] = fields else { return Some("not_a_single_field".into()) };
    let value = match *field {
        "effort" => bot.effort.as_deref(),
        "model" => bot.model.as_deref(),
        _ => None,
    };
    // `/effort`、`/model` 都一定要帶值，清成「不指定」沒有 slash 指令。
    let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Some(format!("{field}_cleared_to_default"));
    };
    let Some(line) = live_slash_command(&bot.kind, field, value, bot.effort.as_deref()) else {
        return Some(format!("no_slash_command_for_{field}"));
    };
    // 不管套用成不成功，pane 都被直接打過字了。
    if let Err(why) = mark_pane_typed(app, &run.id).await {
        return Some(why);
    }
    if send_slash_line(&client, &pane_id, &line).await.is_err() {
        return Some("slash_send_failed".into());
    }
    // 還握著 bot 鎖：`prompt_grouped` 拿同一把鎖，所以下一則 prompt 一定排在 TUI 回到輸入列之後。
    if !wait_for_composer_settled(&client, &pane_id, &bot.kind).await {
        tracing::warn!(bot_id, line, "TUI did not settle back to an empty composer after the slash command");
    }
    // SPEC §4.4a: clear the drift marker only for the field sent (`/model` doesn't touch effort).
    let col = match *field {
        "effort" => "runtime_effort",
        _ => "runtime_model",
    };
    let _ = sqlx::query(&format!("UPDATE runs SET {col} = ? WHERE id = ?"))
        .bind(value)
        .bind(&run.id)
        .execute(&app.db)
        .await;
    if *field == "model" && bot.kind == "grok" {
        if let Some(e) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let _ = sqlx::query("UPDATE runs SET runtime_effort = ? WHERE id = ?")
                .bind(e.to_ascii_lowercase())
                .bind(&run.id)
                .execute(&app.db)
                .await;
        }
    }
    app.emit_bot_status(bot_id).await;
    tracing::info!(bot_id, line, "applied live via slash command");
    None
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

/// 打一行 slash 指令並送出：先打字、等輸入列畫好、再 Enter（`"/login\n"` 會被當多行貼上）。
/// 送出不等於套用（2026-09-11 AGM）：claude 有對話時 `/model` 會跳「Switch model?」框，留著會吃掉
/// 下一則 prompt、Enter 替人按 Yes、回合 stall。所以回頭看畫面：是那個框 → 按 `1`（使用者已在
/// AG Man 選過）；還在 → Esc 並回 `Err`。絕不把框留在畫面上。
async fn send_slash_line(client: &HerdrClient, pane_id: &str, line: &str) -> LcResult<()> {
    client.pane_send_text(pane_id, line).await.map_err(up)?;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    client.pane_send_keys(pane_id, &["Enter"]).await.map_err(up)?;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let screen = client.pane_read(pane_id, "visible", 60).await.map(|r| r.text).unwrap_or_default();
    if !crate::tui_prompts::is_switch_model_dialog(&screen) {
        return Ok(());
    }
    tracing::info!(pane_id, line, "claude asked to confirm the model switch; answering Yes");
    client.pane_send_keys(pane_id, &["1"]).await.map_err(up)?;
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    let screen = client.pane_read(pane_id, "visible", 60).await.map(|r| r.text).unwrap_or_default();
    if !crate::tui_prompts::is_switch_model_dialog(&screen) {
        return Ok(());
    }
    let _ = client.pane_send_keys(pane_id, &["Escape"]).await;
    tracing::warn!(pane_id, line, "model-switch confirmation would not close; backed out with Esc");
    Err(up(anyhow::anyhow!("claude 的換模型確認框沒有關掉，已按 Esc 退出")))
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
async fn mark_pane_typed(app: &Arc<App>, run_id: &str) -> Result<(), String> {
    crate::lifecycle::remember_pane_typed(run_id);
    db::set_pane_typed(&app.db, run_id).await.map_err(|e| {
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
async fn wait_for_composer_settled(client: &HerdrClient, pane_id: &str, kind: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(SLASH_SETTLE_MAX_MS);
    let mut prev: Option<String> = None;
    loop {
        let screen = client.pane_read(pane_id, "visible", 60).await.map(|r| r.text).unwrap_or_default();
        if composer_settled(kind, prev.as_deref(), &screen) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        prev = Some(screen);
        tokio::time::sleep(std::time::Duration::from_millis(SLASH_SETTLE_POLL_MS)).await;
    }
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
pub async fn login(app: &Arc<App>, bot_id: &str) -> LcResult<LoginOut> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let Some(line) = login_slash_command(&bot.kind) else {
        return Err(LcError::BadValue(json!({
            "error": "login_unsupported",
            "kind": bot.kind,
            "message": format!("{} has no in-session login command", bot.kind),
        })));
    };
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| {
        LcError::conflict(SlashBlocked::NotRunning.reason(), json!({ "bot_id": bot_id }))
    })?;
    let in_flight = db::in_flight_turn(&app.db, &run.id).await.map_err(up)?.is_some();
    let pane_id = slash_gate(&run, in_flight)
        .map_err(|b| LcError::conflict(b.reason(), json!({"bot_id": bot_id, "run_id": run.id})))?;
    let client = client_for_run(app, &run).await?;
    mark_pane_typed(app, &run.id).await.map_err(|why| LcError::Upstream(why))?;
    send_slash_line(&client, &pane_id, line).await?;
    tracing::info!(bot_id, kind = %bot.kind, line, "sent login slash command");
    Ok(LoginOut { run_id: run.id, kind: bot.kind, command: line.to_string() })
}

#[cfg(test)]
mod live_slash_tests {
    use super::{composer_settled, live_slash_command};

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

    #[test]
    fn grok_effort_and_model() {
        assert_eq!(
            live_slash_command("grok", "effort", "HIGH", None).as_deref(),
            Some("/effort high")
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

#[cfg(test)]
mod login_slash_tests {
    use super::{login_slash_command, slash_gate, SlashBlocked};
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
            update_notice: None,
            turn_error: None,
            native_session_id: None,
            transcript_path: None,
            last_read_revision: None,
            last_read_tail_hash: None,
            started_at: "2026-01-01T00:00:00Z".into(),
            ended_at: None,
            resume_session_id: None,
            agent_status_since: None,
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

