//! Changing a **running** codex's model / effort / fast tier (SPEC §4.4a).
//!
//! * codex >= 0.164.0 (measured 2026-10-23 in a scratch `CODEX_HOME`): one line does it all,
//!   `/model [<id>] [--reasoning=<level>] [--speed=<tier>]` ([`oneshot_model_command`]). The answer is
//!   `• Model changed to <id> <effort> [fast] for this session only` or a `■ … unavailable` /
//!   `■ Unexpected argument …` line ([`model_responses`]). It changes this session only: unlike the
//!   menus below it does NOT write the choice into `config.toml`. Older codex (or a run whose version we
//!   have not seen) takes the menu path below: there `/model x --reasoning=y` would be sent to the model
//!   as an ordinary prompt, so the version gate ([`oneshot_supported`]) is strict.
//! * codex 0.153.4 (verified 2026-09-09) has no one-line form:
//!   `/model` takes **no arguments** (`/model x high` becomes a prompt); bare `/model` opens two
//!   numbered pickers (model, then reasoning level), so even an effort-only change picks a model.
//! * `/fast` is a plain **toggle** (2026-09-22, 0.154.0, measured in a scratch `CODEX_HOME`):
//!   bare `/fast` answers `Service tier set to priority` / `… default`, but `/fast on` and
//!   `/fast off` are NOT slash forms — the TUI submits them as an ordinary prompt and the model
//!   goes off to read the docs. So the target tier is reached by reading the status line first
//!   ([`fast_plan`]) and toggling only when it is wrong; when the tier cannot be read, toggle
//!   once, read back, and toggle back if it landed the wrong way round. Either way the read-back
//!   still proves the result, and nothing is refused for an unknown starting tier.
//!
//! Picker contents/order and `(default)`/`(current)` markers vary by account, so every step reads
//! the pane back and matches on text. The menus make codex save the choice as the account default in
//! `~/.codex/config.toml` (CLI behaviour, not ours); the one-line form does not.

use crate::db;
use crate::herdr::HerdrClient;
use std::time::Duration;

/// `runs.runtime_effort` 裡表示「TUI 明確顯示 CLI 預設強度」的值。SQL NULL 仍是「未知」（沒讀到／收編）。
pub const CLI_DEFAULT_EFFORT: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRuntime {
    pub model: String,
    /// `None`＝狀態列沒講強度（未知）；`Some("default")`＝狀態列明確寫 `default`（CLI 預設，SPEC §4.4a）。
    pub effort: Option<String>,
    pub fast: bool,
}

fn effort_menu_label(effort: &str) -> &'static str {
    match effort {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra high",
        "max" => "Max",
        "ultra" => "Ultra",
        _ => "",
    }
}

/// `max` and `ultra` live behind the `More reasoning…` entry of the first level menu.
fn is_nested_effort(effort: &str) -> bool {
    matches!(effort, "max" | "ultra")
}

/// 視窗底部才是現在的狀態列。再往上同一形狀的行是啟動 banner，或回覆裡引用的舊列。
const STATUS_TAIL_LINES: usize = 8;

fn context_percent(line: &str) -> bool {
    let Some(rest) = line.split("Context").nth(1) else { return false };
    rest.trim_start().starts_with(|c: char| c.is_ascii_digit())
}

/// `<model> [<effort>] [fast] · <cwd> · <topic|Context …|quota>` — the only place codex states
/// all three, so it is the read-back proving a live change landed (SPEC §4.4a).
pub fn parse_status_line(screen: &str) -> Option<CodexRuntime> {
    // Last match in the viewport tail wins: the startup banner and a quoted line higher up share
    // the shape. A narrow pane wraps the line; join a continuation line.
    let lines: Vec<&str> = screen.lines().collect();
    let tail = &lines[lines.len().saturating_sub(STATUS_TAIL_LINES)..];
    let mut out = None;
    let mut i = 0;
    while i < tail.len() {
        let mut line = tail[i].trim().to_string();
        if line.contains('·') {
            if let Some(next) = tail.get(i + 1) {
                let next = next.trim();
                let is_nav_or_prompt = next.starts_with('?')
                    || next.starts_with('←')
                    || next.starts_with('›')
                    || next.starts_with('•')
                    || next.contains("for shortcuts")
                    || next.contains("to interrupt");
                let is_continuation = !is_nav_or_prompt
                    && (line.trim_end().ends_with('·')
                        || next.starts_with('·')
                        || context_percent(next)
                        || next.contains("% left"));
                if is_continuation {
                    line.push(' ');
                    line.push_str(next);
                    i += 1;
                }
            }
        }
        i += 1;
        if !line.contains('·') {
            continue;
        }
        // 未開始回合的冷啟動 footer 只有兩段（`<model> <default> · <cwd>`），還沒有 Context／額度／主題，
        // 尚未有 active session，不當成 runtime（保留 argv 預設值，SPEC §4.3）。
        if line.matches('·').count() < 2 && !context_percent(&line) && !line.contains("% left") {
            continue;
        }
        let Some((head, _)) = line.split_once('·') else { continue };
        let mut parts = head.split_whitespace();
        // 這一行的開頭沒有東西（`· Context …`）只是別的行：略過，不能整個函式回 None（下面才有真的狀態列）。
        // 0.157 印顯示名（`GPT-6-Luna`）而不是 id（`gpt-6-luna`）：一律轉小寫，runtime 才跟設定比得起來（#712）。
        let Some(model) = parts.next().map(str::to_ascii_lowercase) else { continue };
        if !model.contains('-') || !model.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_') {
            continue;
        }
        let rest: Vec<&str> = parts.collect();
        // head 裡在 model 之後只能是 fast、default 或已知的 codex effort（避免把普通文字行誤認成狀態列）。
        if !rest.iter().all(|w| {
            let lower = w.to_ascii_lowercase();
            lower == "fast"
                || lower == "default"
                || crate::config::efforts_for_kind("codex").contains(&lower.as_str())
        }) {
            continue;
        }
        let fast = rest.iter().any(|w| w.eq_ignore_ascii_case("fast"));
        let effort = rest
            .iter()
            .find(|w| crate::config::efforts_for_kind("codex").contains(&w.to_ascii_lowercase().as_str()))
            .map(|w| w.to_ascii_lowercase())
            // 明確的 `default` 字樣＝CLI 預設（不是未知）。沒有這個字也沒有強度字，才是未知。
            .or_else(|| rest.iter().any(|w| w.eq_ignore_ascii_case("default")).then(|| CLI_DEFAULT_EFFORT.to_string()));
        out = Some(CodexRuntime { model, effort, fast });
    }
    out
}

/// 寫進 `runs.runtime_effort` 的值：狀態列講了就用它；沒講（未知）就沿用上一輪已知的值（SPEC §4.4a：只讀到其中一欄，其餘沿用）。
fn effective_effort(seen: &CodexRuntime, run: &db::Run) -> Option<String> {
    seen.effort.clone().or_else(|| run.runtime_effort.clone())
}

/// 畫面讀到的強度要怎麼動子 agent 的設定（SPEC §4.4a）。`prev` 是 run 上一輪記的值（`None`＝未知）。
/// * 具體值且跟上一輪不同 → 設定跟著改（既有行為）。
/// * 明確 `default`：只有上一輪是**已知的具體值**才清成 CLI 預設；未知→default 與 default→default 都不動設定，
///   避免把「還沒讀到」當成「已經是預設」而蓋掉使用者設定的 high（那樣 ⟳ 也會跟著消失）。
/// * 沒讀到強度（`None`）永遠不動設定。
fn effort_follow<'a>(seen: Option<&'a str>, prev: Option<&str>) -> crate::child_runtime::EffortFollow<'a> {
    use crate::child_runtime::EffortFollow;
    match (seen, prev) {
        (Some(CLI_DEFAULT_EFFORT), Some(p)) if p != CLI_DEFAULT_EFFORT => EffortFollow::Clear,
        (Some(CLI_DEFAULT_EFFORT), _) => EffortFollow::Keep,
        (Some(now), prev) if prev != Some(now) => EffortFollow::Set(now),
        _ => EffortFollow::Keep,
    }
}

// A failed/cancelled edge is still an attempt: replaying it could erase a newer UI intent.
const FAST_FOLLOW_LIMIT: usize = 1024;
/// 共用清理 task 的巡查間隔、每批 `IN (…)` 帶幾個 run id、表滿 WARN 的最短間隔（同一個 process）。
const SWEEP_EVERY: Duration = Duration::from_secs(30);
const SWEEP_BATCH: usize = 500;
const FULL_WARN_EVERY: Duration = Duration::from_secs(3600);
struct FastFollowEdge {
    attempted: bool,
    /// DB runtime at the last observation/commit, distinct from an uncommitted attempt.
    runtime: bool,
    warned: bool,
}
/// `attempt` 的結果：是否跟隨、是否由這次呼叫負責起共用清理 task、是否因為表滿被擋下。
#[derive(Debug, Default, PartialEq, Eq)]
struct Outcome { follow: bool, start_sweeper: bool, full: bool }
struct FastFollowEdges {
    map: std::collections::HashMap<String, FastFollowEdge>,
    limit: usize,
    /// 共用清理 task 是否已經（或正要）在跑。只在這把鎖裡改：插入 edge 時「false → true 並起 task」、
    /// task 結束時「表是空的 → false」，兩邊在同一把鎖內，所以表非空時一定有 task 在跑。
    sweeper: bool,
    last_full_warn: Option<std::time::Instant>,
}
impl Default for FastFollowEdges {
    fn default() -> Self { Self { map: Default::default(), limit: FAST_FOLLOW_LIMIT, sweeper: false, last_full_warn: None } }
}
impl FastFollowEdges {
    fn observe_runtime(&mut self, run: &str, runtime: bool, matches_screen: bool) {
        if let Some(edge) = self.map.get_mut(run) {
            // A live apply can advance runtime without this observer. A matching screen also
            // establishes the current edge; an unchanged DB after a failed attempt does not.
            if matches_screen || edge.runtime != runtime {
                edge.attempted = runtime;
                edge.runtime = runtime;
            }
        }
    }
    fn attempt(&mut self, run: &str, fast: bool, runtime: bool) -> Outcome {
        self.observe_runtime(run, runtime, false);
        if let Some(edge) = self.map.get_mut(run) {
            if edge.attempted == fast { return Outcome::default(); }
            edge.attempted = fast;
            return Outcome { follow: true, ..Outcome::default() };
        }
        // No eviction of active edges: at capacity preserve settings rather than replay them.
        // The caller sweeps ended runs (without holding this lock) and tries once more.
        if self.map.len() >= self.limit { return Outcome { full: true, ..Outcome::default() }; }
        self.map.insert(run.to_string(), FastFollowEdge { attempted: fast, runtime, warned: false });
        Outcome { follow: true, start_sweeper: !std::mem::replace(&mut self.sweeper, true), full: false }
    }
    /// 清理 task 巡完一輪後呼叫：表空了就把旗標清掉（回 `true`＝task 該結束）；不空就繼續。
    fn finish_sweeper_if_empty(&mut self) -> bool {
        if !self.map.is_empty() { return false; }
        self.sweeper = false;
        true
    }
    /// 表滿而且清完還是滿：同一個 process 每小時最多 WARN 一次。
    fn should_warn_full(&mut self, now: std::time::Instant) -> bool {
        if self.last_full_warn.is_some_and(|at| now.saturating_duration_since(at) < FULL_WARN_EVERY) { return false; }
        self.last_full_warn = Some(now);
        true
    }
}
type SharedEdges = &'static std::sync::Mutex<FastFollowEdges>;
#[cfg(not(test))]
fn fast_follow_edges() -> SharedEdges {
    static EDGES: std::sync::OnceLock<std::sync::Mutex<FastFollowEdges>> = std::sync::OnceLock::new();
    EDGES.get_or_init(Default::default)
}
/// 測試同一個 process 裡每個測試各有自己的 DB（和 current-thread runtime）：共用一張表的話，一個測試的清理會把別的測試
/// 的 edge 當成「run 不存在」清掉。所以測試時每條執行緒一張表。
#[cfg(test)]
fn fast_follow_edges() -> SharedEdges {
    thread_local! { static EDGES: SharedEdges = Box::leak(Box::default()); }
    EDGES.with(|edges| *edges)
}
fn lock_edges(edges: SharedEdges) -> std::sync::MutexGuard<'static, FastFollowEdges> {
    edges.lock().unwrap_or_else(|e| e.into_inner())
}
/// 把已結束（或不存在）的 run 的 edge 移掉，回傳移掉幾筆。查詢分批（[`SWEEP_BATCH`]），每批 await 前後各拿一次鎖，
/// 不會拿著 std Mutex 過 await；查詢失敗的那批保留（不確定就不清）。
async fn sweep_finished_fast_edges(edges: SharedEdges, pool: &sqlx::SqlitePool) -> usize {
    let ids: Vec<String> = lock_edges(edges).map.keys().cloned().collect();
    let mut removed = 0;
    for chunk in ids.chunks(SWEEP_BATCH) {
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT id FROM runs WHERE state IN ('starting','running','stopping') AND id IN (");
        {
            let mut list = query.separated(", ");
            for id in chunk { list.push_bind(id.as_str()); }
            list.push_unseparated(")");
        }
        let Ok(active) = query.build_query_scalar::<String>().fetch_all(pool).await else { continue };
        let active: std::collections::HashSet<String> = active.into_iter().collect();
        let mut guard = lock_edges(edges);
        for id in chunk {
            if !active.contains(id) && guard.map.remove(id).is_some() { removed += 1; }
        }
    }
    removed
}
/// 共用清理 task 被丟掉（runtime 關閉）而沒有正常結束時，把旗標還回去，下一個 edge 才起得了新的。
struct SweeperGuard { edges: SharedEdges, done: bool }
impl Drop for SweeperGuard {
    fn drop(&mut self) {
        if !self.done { lock_edges(self.edges).sweeper = false; }
    }
}
/// 整個 process 只有一個：每 [`SWEEP_EVERY`] 清一次已結束的 run，表空了才結束（旗標的清除與「表是空的」同一把鎖）。
fn spawn_sweeper(edges: SharedEdges, pool: sqlx::SqlitePool) {
    tokio::spawn(Box::pin(async move {
        let mut guard = SweeperGuard { edges, done: false };
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            sweep_finished_fast_edges(edges, &pool).await;
            if lock_edges(edges).finish_sweeper_if_empty() {
                guard.done = true;
                break;
            }
        }
    }));
}
struct FastFollowAttempt { edges: SharedEdges, run: String, committed: bool }
impl Drop for FastFollowAttempt {
    fn drop(&mut self) {
        if self.committed { return; }
        let mut edges = lock_edges(self.edges);
        if let Some(edge) = edges.map.get_mut(&self.run) {
            if !edge.warned {
                edge.warned = true;
                tracing::warn!(run = %self.run, "child fast settings were not synchronized; this edge will not be replayed; check the UI before the next start");
            }
        }
    }
}
async fn attempt_fast_follow(pool: &sqlx::SqlitePool, run: &str, fast: bool, runtime: bool) -> Option<FastFollowAttempt> {
    attempt_fast_follow_in(fast_follow_edges(), pool, run, fast, runtime).await
}
async fn attempt_fast_follow_in(edges: SharedEdges, pool: &sqlx::SqlitePool, run: &str, fast: bool, runtime: bool) -> Option<FastFollowAttempt> {
    let mut outcome = lock_edges(edges).attempt(run, fast, runtime);
    if outcome.full {
        // 表滿：先清掉已結束的 run 再試一次（鎖在 await 之前放掉）；仍然滿才放棄，WARN 節流。
        sweep_finished_fast_edges(edges, pool).await;
        let warn;
        {
            let mut guard = lock_edges(edges);
            outcome = guard.attempt(run, fast, runtime);
            warn = outcome.full && guard.should_warn_full(std::time::Instant::now());
        }
        if warn {
            tracing::warn!(limit = FAST_FOLLOW_LIMIT, run, "fast follow edge table is full of active runs; child fast settings are not synchronized for new runs");
        }
    }
    if outcome.start_sweeper { spawn_sweeper(edges, pool.clone()); }
    outcome.follow.then(|| FastFollowAttempt { edges, run: run.to_string(), committed: false })
}

/// 以畫面上的狀態列校正 `runs.runtime_*`（SPEC §4.4a）。回傳 `true` = 有改動。
/// * 讀不到狀態列（選單開著、畫面被清、CLI 剛啟動）什麼都不動：讀不到不是 fast=false。
/// * 讀得到就一律以它為準：狀態列有 `fast` 字樣 = 開，**整行讀得到卻沒有 = 關**（tier 關掉時 codex 省略那個字）。
///   使用者在 TUI 手打 `/fast`、`/model`，或當場套用中途失敗，都會讓啟動時記下的值過期。
pub async fn correct_runtime_from_screen(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::Emit), run_id: &str, screen: &str) -> bool {
    let Some(seen) = parse_status_line(screen) else { return false };
    let Ok(Some(run)) = db::run(app.db(), run_id).await else { return false };
    // 狀態列沒講強度（沒有強度字、也沒有 default）＝未知：沿用上一輪已知的值，不清成 NULL；model／fast 照常校正。
    let effort = effective_effort(&seen, &run);
    if let Some(runtime) = run.runtime_fast {
        fast_follow_edges().lock().unwrap_or_else(|e| e.into_inner())
            .observe_runtime(run_id, runtime != 0, runtime == i64::from(seen.fast));
    }
    let same = run.runtime_model.as_deref() == Some(seen.model.as_str())
        && run.runtime_effort == effort
        && run.runtime_fast == Some(i64::from(seen.fast));
    if same {
        return false;
    }
    // Re-read the current user's settings on every attempt, including after daemon restart.
    let bot: db::Bot = match sqlx::query_as("SELECT * FROM bots WHERE id=?").bind(&run.bot_id).fetch_one(app.db()).await {
        Ok(bot) => bot,
        Err(_) => return false,
    };
    let fast_edge = bot.managed_by == "child" && run.runtime_fast.is_some_and(|prev| prev != i64::from(seen.fast));
    let mut attempt = if fast_edge { attempt_fast_follow(app.db(), run_id, seen.fast, run.runtime_fast.unwrap_or(0) != 0).await } else { None };
    // 設定與 runtime 原子提交；失敗或取消一起 rollback，但嘗試記錄保留。
    let Ok(mut tx) = app.db().begin().await else { return false };
    let model = Some(seen.model.as_str()).filter(|_| run.runtime_model.as_deref() != Some(seen.model.as_str()));
    let effort_follow = effort_follow(seen.effort.as_deref(), run.runtime_effort.as_deref());
    let (effort_value, clear) = match effort_follow {
        crate::child_runtime::EffortFollow::Keep => (None, false),
        crate::child_runtime::EffortFollow::Set(value) => (Some(value), false),
        crate::child_runtime::EffortFollow::Clear => (None, true),
    };
    // 被去重（或表滿）的 fast edge 只是不重播 fast；同一輪的 model／effort 照常跟隨（#1906）。
    // `attempt` 只在 fast_edge 時才可能是 Some。
    let fast = attempt.is_some().then_some(i64::from(seen.fast));
    let changed = if bot.managed_by == "child" && (model.is_some() || effort_value.is_some() || clear || fast.is_some()) {
        match sqlx::query(
            "UPDATE bots SET model=COALESCE(?, model), effort=CASE WHEN ? THEN NULL ELSE COALESCE(?, effort) END, fast=COALESCE(?, fast)
             WHERE id=? AND managed_by='child'
               AND (COALESCE(?, model) IS NOT model OR (CASE WHEN ? THEN NULL ELSE COALESCE(?, effort) END) IS NOT effort OR COALESCE(?, fast) IS NOT fast)",
        ).bind(model).bind(clear).bind(effort_value).bind(fast).bind(&run.bot_id)
            .bind(model).bind(clear).bind(effort_value).bind(fast).execute(&mut *tx).await {
            Ok(result) => result.rows_affected() > 0,
            Err(e) => {
                tracing::debug!(run = %run_id, error = %e, "codex child follow transaction failed");
                return false;
            }
        }
    } else { false };
    #[cfg(test)]
    crate::race_point::hit("codex_follow_before_runtime_write", run_id).await;
    let wrote = sqlx::query("UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ? WHERE id = ?")
        .bind(&seen.model)
        .bind(&effort)
        .bind(i64::from(seen.fast))
        .bind(run_id)
        .execute(&mut *tx)
        .await;
    if !matches!(wrote, Ok(ref result) if result.rows_affected() == 1) || tx.commit().await.is_err() {
        return false;
    }
    fast_follow_edges().lock().unwrap_or_else(|e| e.into_inner()).observe_runtime(run_id, seen.fast, true);
    if let Some(attempt) = &mut attempt { attempt.committed = true; }
    if changed {
        app.emit("bot_changed", serde_json::json!({"bot_id": run.bot_id})).await;
    }
    app.emit_bot_status(&run.bot_id).await;
    tracing::info!(run = %run_id, model = %seen.model, effort = ?effort, fast = seen.fast,
                   "codex runtime corrected from the status line");
    true
}

/// 把畫面底部的模型列寫進 `runs.status_line`／`status_json`（網頁狀態列與 context 用量）。回傳 `true` = 有改動。
pub async fn record_footer_from_screen(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::Emit), run_id: &str, screen: &str) -> bool {
    // 讀不到模型列（壓縮中、選單開著）不清掉舊值。
    let Some(footer) = am_base::codex_status::parse_footer(screen) else { return false };
    let Ok(Some(run)) = db::run(app.db(), run_id).await else { return false };
    // 鍵名照 claude statusLine，網頁 `toStatusInfo` 讀 `context_window.used_percentage`。
    let json = footer.context_used_pct.map(|p| serde_json::json!({"context_window": {"used_percentage": p}}).to_string());
    if run.status_line.as_deref() == Some(footer.line.as_str()) && run.status_json == json {
        return false;
    }
    let wrote = sqlx::query("UPDATE runs SET status_line = ?, status_json = ? WHERE id = ?")
        .bind(&footer.line)
        .bind(&json)
        .bind(run_id)
        .execute(app.db())
        .await
        .is_ok();
    if wrote {
        app.emit_bot_status(&run.bot_id).await;
    }
    wrote
}

pub async fn hint_moves_runtime(app: &impl crate::capabilities::Db, run_id: &str, screen: &str) -> bool {
    let Some(seen) = parse_status_line(screen) else { return false };
    let Ok(Some(run)) = db::run(app.db(), run_id).await else { return true };
    run.runtime_model.as_deref() != Some(seen.model.as_str())
        || run.runtime_effort != effective_effort(&seen, &run)
        || run.runtime_fast != Some(i64::from(seen.fast))
}

/// 開機畫面重試次數（第一次讀之後，最多再讀這麼多次）。
const BOOT_VERSION_RETRIES: u32 = 2;

/// run 啟動當下，背景從開機畫面（`>_ OpenAI Codex (vX)`）記一次版本（#1924）。巡邏要等到第一輪才會記，
/// 這段空窗內 `/model` 會被當成版本未知而退回選單、寫進帳號預設。只對 codex 讀；讀不到就維持未知，不擋啟動。
pub fn spawn_boot_version_read(
    pool: &sqlx::SqlitePool,
    client: &HerdrClient,
    fence: Option<&crate::hosts::HostFence>,
    kind: &str,
    bot_id: &str,
    run_id: &str,
    pane_id: &str,
) {
    let (pool, client, fence) = (pool.clone(), client.clone(), fence.cloned());
    let (kind, bot_id, run_id, pane_id) = (kind.to_string(), bot_id.to_string(), run_id.to_string(), pane_id.to_string());
    tokio::spawn(Box::pin(async move {
        let read = move || {
            let (client, pane_id) = (client.clone(), pane_id.clone());
            async move { read(&client, &pane_id).await.ok() }
        };
        remember_boot_version(&pool, fence.as_ref(), &kind, &bot_id, &run_id, read, boot_version_retry_delay()).await;
    }));
}

fn boot_version_retry_delay() -> Duration {
    Duration::from_millis(if cfg!(test) { 1 } else { 1000 })
}

/// 讀到開機畫面就記（`codex_update::remember_running`）。第一次讀可能還沒畫出來，最多再讀 [`BOOT_VERSION_RETRIES`] 次。
/// 寫入前 run 必須還是這個 bot 的當前 run、host fence 也還是當前的，否則不記。
async fn remember_boot_version<F, Fut>(
    pool: &sqlx::SqlitePool,
    fence: Option<&crate::hosts::HostFence>,
    kind: &str,
    bot_id: &str,
    run_id: &str,
    mut read: F,
    delay: Duration,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    if kind != "codex" {
        return;
    }
    for attempt in 0..=BOOT_VERSION_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(delay).await;
        }
        let Some(screen) = read().await else { continue };
        if crate::codex_update::parse_running_version(&screen).is_none() {
            continue;
        }
        let current = matches!(db::active_run(pool, bot_id).await, Ok(Some(r)) if r.id == run_id)
            && fence.is_none_or(|f| f.is_current_now());
        if current {
            crate::codex_update::remember_running(run_id, &screen, None);
        }
        return;
    }
}

pub use am_base::codex_status::{parse_status_quota, CodexStatusQuota};

/// Digit for the one row whose **label** exactly matches `needle`. Descriptions are excluded;
/// current/default markers are matched separately, and duplicate labels are ambiguous.
pub fn picker_number(screen: &str, needle: &str) -> Option<u32> {
    if needle.is_empty() {
        return None;
    }
    let mut found = None;
    for raw in screen.lines() {
        let line = raw.trim_start().trim_start_matches('›').trim_start();
        let Some((num, rest)) = line.split_once('.') else { continue };
        let Ok(n) = num.trim().parse::<u32>() else { continue };
        let label = rest.trim_start().split("  ").next().unwrap_or("").trim();
        let (plain_label, marker) = if let Some(label) = label.strip_suffix(" (current)") {
            (label, Some("(current)"))
        } else if let Some(label) = label.strip_suffix(" (default)") {
            (label, Some("(default)"))
        } else {
            (label, None)
        };
        let matches = match needle {
            "(current)" | "(default)" => marker == Some(needle),
            // 0.162 的選單印顯示名（`GPT-6.1-Sol`），設定裡是 id（`gpt-6.1-sol`）：不分大小寫比（#1681 實測）。
            _ => plain_label.eq_ignore_ascii_case(needle),
        };
        if matches {
            if found.is_some() {
                return None;
            }
            found = Some(n);
        }
    }
    found
}

async fn read(client: &HerdrClient, pane_id: &str) -> Result<String, ()> {
    client
        .pane_read(pane_id, "visible", 60)
        .await
        .map(|r| r.text)
        .map_err(|_| ())
}

async fn key(client: &HerdrClient, pane_id: &str, k: &str) -> bool {
    client.pane_send_keys(pane_id, &[k]).await.is_ok()
}

async fn text(client: &HerdrClient, pane_id: &str, t: &str) -> bool {
    client.pane_send_text(pane_id, t).await.is_ok()
}

/// Is a Codex selection screen on screen? Footer or heading (a short pane can scroll either away).
/// Must be checked before typing into codex: a user message sent into a picker was eaten and its
/// Enter switched the model. Migration and rate-limit prompts are user decisions; never dismiss
/// them from this helper.
pub fn picker_open(screen: &str) -> bool {
    crate::tui_prompts::is_codex_model_migration_prompt(screen)
        || rate_limit_switch_prompt_open(screen)
        // 啟動時的更新選單（預設選 Update now）：`/model`、`/fast` 打進去、Enter 就是替使用者按下「現在更新」。
        || crate::codex_update::update_menu_open(screen)
        || model_menu_open(screen)
}

/// `/model` 的兩層選單是畫面最底下的彈出框，取代輸入框。對話裡印出來的同一串字（agent 讀別的 pane、引用選單）不算：
/// 以前整個畫面找字，xxdzvt 2026-10-14 畫面上留著一行 `Select Reasoning Level for GPT-6-Luna`，送 prompt 前每次都被當成
/// 選單連按 Esc，codex 被按進對話紀錄瀏覽，排著的通知 composer_unreadable 一小時（#1766）。
/// 選單字樣要在最後一個輸入框列（`› ` 開頭、不是 `› 4.` 這種選單列）之後，而且落在畫面尾端。
fn model_menu_open(screen: &str) -> bool {
    const MENU_TAIL_LINES: usize = 24;
    let lines: Vec<String> = screen.lines().filter(|l| !l.trim().is_empty()).map(|l| l.trim().to_lowercase()).collect();
    let tail_from = lines.len().saturating_sub(MENU_TAIL_LINES);
    let marker = |l: &str| {
        l.contains("press enter to confirm or esc to go back") || l.contains("select model and effort") || l.contains("select reasoning level")
    };
    let composer = |l: &str| l.strip_prefix('›').is_some_and(|rest| !rest.trim_start().starts_with(|c: char| c.is_ascii_digit()));
    let Some(menu_at) = lines.iter().rposition(|l| marker(l)) else { return false };
    let composer_at = lines.iter().rposition(|l| composer(l));
    menu_at >= tail_from && composer_at.is_none_or(|c| c < menu_at)
}

/// Codex 0.157.0's quota warning offers a model switch using the bottom selection popup.
/// Match its structure and footer so a transcript merely mentioning the recommendation is ignored.
pub fn rate_limit_switch_prompt_open(screen: &str) -> bool {
    const TAIL_LINES: usize = 20;
    let raw: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail_raw = &raw[raw.len().saturating_sub(TAIL_LINES)..];
    let lines: Vec<String> = tail_raw
        .iter()
        .map(|line| {
            let spaced: String = line
                .chars()
                .map(|c| if c.is_whitespace() || "│┌┐└┘─├┤┬┴┼╭╮╯╰▎▔".contains(c) { ' ' } else { c.to_ascii_lowercase() })
                .collect();
            spaced
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .trim_start_matches(|c| "❯›»>*●•⏺⎿✻-— ".contains(c))
                .trim()
                .to_string()
        })
        .collect();
    lines.iter().any(|l| l.starts_with("approaching rate limits"))
        && lines.iter().any(|l| l.starts_with("switch to ") && l.contains("for lower credit usage?"))
        && lines.iter().any(|l| l.starts_with("1. switch to "))
        && lines.iter().any(|l| l.starts_with("2. keep current model"))
        && lines.iter().any(|l| l.starts_with("3. keep current model (never show again)"))
        && rate_limit_footer_open(&lines)
}

/// 腳註整行是 `enter select · esc back`。窄 pane 會在 `·` 後面折成兩行，最後一行就不再同時含這兩段。
fn rate_limit_footer_open(lines: &[String]) -> bool {
    if lines.last().is_some_and(|l| l.starts_with("enter select") && l.contains("esc back")) {
        return true;
    }
    let Some(last) = lines.last() else { return false };
    let Some(prev) = lines.get(lines.len().saturating_sub(2)) else { return false };
    prev.starts_with("enter select") && !prev.contains("esc back") && last == "esc back"
}

/// The exact row we selected is still in a recognized picker on the latest pane read.
fn picker_choice_matches(screen: &str, needle: &str, n: u32) -> bool {
    picker_open(screen) && picker_number(screen, needle) == Some(n)
}

/// Escapes until no picker is left. One Escape is not enough: from the level menu it only goes
/// back to the model menu, and anything typed next would land in it.
async fn close_picker_checked(client: &HerdrClient, pane_id: &str) -> Result<bool, ()> {
    for _ in 0..PICKER_ESCAPES {
        let screen = read(client, pane_id).await?;
        if crate::tui_prompts::is_codex_model_migration_prompt(&screen) || rate_limit_switch_prompt_open(&screen) || crate::codex_update::update_menu_open(&screen) {
            return Ok(false);
        }
        if !picker_open(&screen) && !transcript_browser_open(&screen) {
            return Ok(true);
        }
        if !key(client, pane_id, "Escape").await {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::race_point::hit("codex_picker_before_final_read", pane_id).await;
    let last = read(client, pane_id).await?;
    Ok(!picker_open(&last) && !transcript_browser_open(&last))
}

/// codex 的對話紀錄瀏覽（輸入框空著時連按兩下 Esc 就會進去，底列 `Browsing transcript · … · esc back`）：
/// 輸入框不在畫面上，送 prompt 一直是 `composer_unreadable`、排著的通知重試到放棄（xxdzvt 2026-10-14 15:00–16:00，#1766）。
/// 只認最後一行的底列；Esc 回輸入框，不動對話。
pub fn transcript_browser_open(screen: &str) -> bool {
    screen
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .is_some_and(|l| l.to_lowercase().starts_with("browsing transcript") && l.to_lowercase().contains("esc back"))
}

pub async fn close_picker(client: &HerdrClient, pane_id: &str) -> bool {
    matches!(close_picker_checked(client, pane_id).await, Ok(true))
}

/// Two levels + nested `More reasoning…` + one spare.
const PICKER_ESCAPES: u32 = 4;

/// A failed send leaves the menu open, so back out before reporting failure.
async fn press_number(client: &HerdrClient, pane_id: &str, n: u32, needle: &str) -> Result<bool, ()> {
    // A picker can close or change while the caller is deciding which row to choose. Re-read and
    // verify both the picker and the intended row immediately before typing the one-digit choice.
    let screen = read(client, pane_id).await?;
    if !picker_choice_matches(&screen, needle, n) {
        let _ = close_picker_checked(client, pane_id).await;
        return Ok(false);
    }
    if !text(client, pane_id, &n.to_string()).await {
        let _ = close_picker_checked(client, pane_id).await;
        return Ok(false);
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    Ok(true)
}

/// `model: None` → `(current)` row; `effort: None` → `(default)` row. Any unexpected menu returns
/// `false` and the caller falls back to "needs restart" (always safe).
async fn apply_model_and_effort(
    client: &HerdrClient,
    pane_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<bool, ()> {
    if !text(client, pane_id, "/model").await {
        let _ = close_picker_checked(client, pane_id).await;
        return Ok(false);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    if !key(client, pane_id, "Enter").await {
        let _ = close_picker_checked(client, pane_id).await;
        return Ok(false);
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;

    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::race_point::hit("codex_apply_before_model_picker_read", pane_id).await;
    let screen = read(client, pane_id).await?;
    if !screen.contains("Select Model") {
        let _ = close_picker_checked(client, pane_id).await;
        return Ok(false);
    }
    let needle = model.map(str::to_string).unwrap_or_else(|| "(current)".to_string());
    let Some(n) = picker_number(&screen, &needle) else {
        let _ = close_picker_checked(client, pane_id).await;
        return Ok(false);
    };
    if !press_number(client, pane_id, n, &needle).await? {
        return Ok(false);
    }

    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::race_point::hit("codex_apply_before_effort_picker_read", pane_id).await;
    let screen = read(client, pane_id).await?;
    if !screen.contains("Select Reasoning Level") {
        let _ = close_picker_checked(client, pane_id).await;
        return Ok(false);
    }
    let want = effort.unwrap_or("");
    let needle = if want.is_empty() { "(default)".to_string() } else { effort_menu_label(want).to_string() };
    if needle.is_empty() {
        let _ = close_picker_checked(client, pane_id).await;
        return Ok(false);
    }
    let screen = if is_nested_effort(want) && picker_number(&screen, &needle).is_none() {
        let Some(more) = picker_number(&screen, "More reasoning…") else {
            let _ = close_picker_checked(client, pane_id).await;
            return Ok(false);
        };
        if !press_number(client, pane_id, more, "More reasoning…").await? {
            return Ok(false);
        }
        read(client, pane_id).await?
    } else {
        screen
    };
    let Some(n) = picker_number(&screen, &needle) else {
        let _ = close_picker_checked(client, pane_id).await;
        return Ok(false);
    };
    if !press_number(client, pane_id, n, &needle).await? {
        return Ok(false);
    }
    // Should be closed now; if not, the next prompt would be typed into it.
    close_picker_checked(client, pane_id).await
}

async fn toggle_fast(client: &HerdrClient, pane_id: &str) -> bool {
    if !text(client, pane_id, "/fast").await {
        return false;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    if !key(client, pane_id, "Enter").await {
        return false;
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::race_point::hit("codex_after_fast_toggle", pane_id).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    true
}

/// What to do to reach `want` given what we know about the current tier (`fast` on the status line,
/// else `runs.runtime_fast`). `/fast on|off` is not a slash form (see the module docs), so the
/// only tool is the toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastPlan {
    /// Already there: send nothing.
    Keep,
    /// Known wrong: one toggle.
    Toggle,
    /// Unknown start: one toggle, read back, toggle again only if it landed on the wrong side.
    ToggleThenCheck,
}

pub fn fast_plan(now: Option<bool>, want: bool) -> FastPlan {
    match now {
        Some(n) if n == want => FastPlan::Keep,
        Some(_) => FastPlan::Toggle,
        None => FastPlan::ToggleThenCheck,
    }
}

/// After the first toggle of [`FastPlan::ToggleThenCheck`]: is a second one needed?
pub fn needs_second_toggle(seen: Option<bool>, want: bool) -> bool {
    matches!(seen, Some(n) if n != want)
}

/// On `Err` the caller keeps `needs_restart: true`. `running_version` is `codex_update::running_version_of(run.id)`
/// (the version of the process in the pane, never the one installed on disk): `None` or older than
/// [`ONESHOT_MIN_VERSION`] takes the menu path.
pub async fn apply(
    client: &HerdrClient,
    pane_id: &str,
    bot: &db::Bot,
    was_fast: Option<bool>,
    fields: &[&str],
    running_version: Option<&str>,
    model_default_effort: Option<&str>,
) -> Result<CodexRuntime, &'static str> {
    // The startup migration and rate-limit suggestion both require a user choice. Do not type into
    // either screen or dismiss it with Escape while applying a setting.
    match close_picker_checked(client, pane_id).await {
        Ok(true) => {}
        Ok(false) => return Err("picker_open"),
        Err(()) => return Err("pane_read_failed"),
    }
    // 輸入框裡有使用者的草稿：`/model`、`/fast` 會接在後面，Enter 把整段當 prompt 送出（回合中那條 #712 早有這道檢查）。
    // herdr 不給帶樣式的讀法時分不出灰色佔位字與草稿，不擋（維持原行為）。
    if let Ok(r) = crate::composer_parse::read_styled_snapshot(client, pane_id, "visible", 60).await {
        if r.format == "ansi" && crate::composer_parse::box_state("codex", &r.text) == crate::composer_parse::BoxState::NonEmpty {
            return Err("composer_not_empty");
        }
    }
    if oneshot_supported(running_version) && fields.iter().any(|f| matches!(*f, "model" | "effort" | "fast")) {
        match oneshot_model_command(bot, fields, model_default_effort) {
            Ok((line, want_effort)) => match apply_oneshot(client, pane_id, bot, fields, &line, want_effort.as_deref()).await {
                Err(ONESHOT_NOT_SUPPORTED) => {
                    // 送出後畫面完全沒變、狀態列也沒變：這個 codex 不認得該語法（版本讀錯）。退回選單**一次**——
                    // 但先重看輸入框：不是空的就不能再打字，不然會接成 `/model … /model` 再按 Enter。
                    #[cfg(all(test, feature = "daemon-test-harness"))]
                    crate::race_point::hit("codex_oneshot_before_fallback", pane_id).await;
                    match composer_is_empty(client, pane_id).await {
                        Some(true) => {}
                        Some(false) => {
                            tracing::warn!(pane_id, bot_id = %bot.id, ?running_version, "codex oneshot got no answer and the composer is not empty; not falling back to the menus");
                            return Err(ONESHOT_RESIDUE);
                        }
                        None => {
                            tracing::warn!(pane_id, bot_id = %bot.id, ?running_version, "codex oneshot got no answer and the composer cannot be read; not falling back to the menus");
                            return Err(ONESHOT_UNVERIFIED);
                        }
                    }
                    tracing::warn!(pane_id, bot_id = %bot.id, ?running_version, "codex oneshot /model got no answer; falling back to the menus once");
                }
                done => return done,
            },
            // 沒設強度又查不到預設強度：不送，走選單的 `(default)` 那一列。
            Err(ONESHOT_NO_DEFAULT_EFFORT) => {}
            Err(why) => return Err(why),
        }
    }
    if fields.iter().any(|f| *f == "model" || *f == "effort") {
        let model = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty());
        match apply_model_and_effort(client, pane_id, model, configured_effort(bot)).await {
            Ok(true) => {}
            Ok(false) => return Err("picker_failed"),
            Err(()) => return Err("pane_read_failed"),
        }
    }
    if fields.contains(&"fast") {
        reach_fast(client, pane_id, bot.fast != 0, was_fast).await?;
    }
    readback(client, pane_id, bot, fields).await
}

/// 第一個認得 `/model <id> --reasoning=… --speed=…` 的 codex（2026-10-23 在 0.164.0 實測）。
pub const ONESHOT_MIN_VERSION: &str = "0.164.0";

/// 一次性指令被拒（`■ … unavailable`／`■ Unexpected argument`）：這次 apply 失敗，**不**退回選單重試。
pub const ONESHOT_REJECTED: &str = "oneshot_rejected";
/// 送出後畫面完全沒變、狀態列也沒變：這個 codex 不認得該語法，呼叫端可退回選單一次。
const ONESHOT_NOT_SUPPORTED: &str = "oneshot_not_supported";
/// 送出的那行變成了 prompt（畫面進入「回合進行中」）：絕不再往同一個畫面打字。
const ONESHOT_BECAME_PROMPT: &str = "oneshot_became_prompt";
/// 3 秒內讀不到可驗證的狀態列（max／ultra 的動畫會蓋住它）：回讀沒完成，不算套用成功。
const ONESHOT_UNVERIFIED: &str = "oneshot_unverified";
/// 失敗之後輸入框裡還留著東西（送出的那行沒被收走）：不再往裡面打字，也不退回選單。
pub const ONESHOT_RESIDUE: &str = "oneshot_residue";
/// 沒設強度、又查不到這個模型的預設強度：一行指令無從明確指定，走選單的 `(default)` 那一列。
const ONESHOT_NO_DEFAULT_EFFORT: &str = "oneshot_no_default_effort";

/// 等回應文字、等狀態列回來各自最多這麼久，不無限等。
fn oneshot_wait() -> Duration {
    Duration::from_millis(if cfg!(test) { 200 } else { 3000 })
}

fn oneshot_poll() -> Duration {
    Duration::from_millis(if cfg!(test) { 10 } else { 250 })
}

/// 這個 run 跑的 codex（`codex_update::running_version_of`）能不能吃一次性 `/model`。版本沒看過、讀不懂（含 `-alpha`
/// 之類的後綴）一律當不能：舊版會把那一行當 prompt 送給模型。
pub fn oneshot_supported(running_version: Option<&str>) -> bool {
    let parse = |v: &str| crate::changelog::version_string(v).and_then(|v| crate::changelog::parse_version(&v));
    match (running_version.and_then(parse), parse(ONESHOT_MIN_VERSION)) {
        (Some(running), Some(min)) => running >= min,
        _ => false,
    }
}

/// 模型 id 只收 `[A-Za-z0-9._-]`：多出來的空白或 `--` 會變成別的參數。
fn oneshot_token_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

/// 模型清單快取（`GET /api/models` 的同一份，10 分鐘內）裡這個模型的預設強度。`model` 沒設就用 run 現在的模型
/// （一行指令不帶模型時，codex 維持現在的）。查不到（快取過期、模型不在清單）回 `None`。
pub async fn model_default_effort(app: &(impl crate::capabilities::Db + crate::models::ModelsCache), bot: &db::Bot, runtime_model: Option<&str>) -> Option<String> {
    let model = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty()).or(runtime_model.map(str::trim).filter(|s| !s.is_empty()))?;
    let model = crate::models::canonical_model("codex", model);
    let host = db::bot_host(app.db(), &bot.id).await.ok()?;
    let list = crate::models::cached(app, &host, "codex", bot.identity.as_deref()).await?;
    list.get("models")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()).is_some_and(|id| id.eq_ignore_ascii_case(model)))?
        .get("default_effort")?
        .as_str()
        .map(str::to_ascii_lowercase)
        .filter(|e| !effort_menu_label(e).is_empty())
}

/// 一次性指令那一行，與它要求的強度（回讀要對上，`None`＝沒帶）。
/// * `model`／`effort` 在 `fields` 裡才帶模型與強度，跟選單路徑一樣：沒設模型＝不帶（維持現在的）。
/// * 沒設強度（空或 `default`）＝選單的 `(default)` 那一列＝這個模型的預設強度。0.164.0 實測：**同一個模型不帶 `--reasoning`
///   會把強度重設成該模型的預設**（`--reasoning=high` 之後 `/model gpt-6.1-sol --speed=standard` 變 `low`；不帶模型才保留），
///   所以不能靠「不帶」，要把 `default_effort`（模型清單的預設強度）明確帶上；沒有就回 [`ONESHOT_NO_DEFAULT_EFFORT`]，呼叫端走選單。
///   （`--reasoning=default` 會被拒。）
/// * 速度**一律明確帶**，不靠開關：fast 帶 `--speed=priority`（0.164.0 實測 `fast` 與 `priority` 都接受，用 `priority`
///   是為了跟 `-c service_tier="priority"` 對齊），非 fast 帶 `--speed=standard`。只換模型時 codex 會保留原本的速度，所以不帶就不確定。
pub fn oneshot_model_command(bot: &db::Bot, fields: &[&str], default_effort: Option<&str>) -> Result<(String, Option<String>), &'static str> {
    let mut line = String::from("/model");
    let mut effort = None;
    if fields.iter().any(|f| *f == "model" || *f == "effort") {
        if let Some(m) = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if !oneshot_token_ok(m) {
                return Err("oneshot_bad_model");
            }
            line.push(' ');
            line.push_str(m);
        }
        let e = match configured_effort(bot) {
            Some(e) => e.to_string(),
            None => default_effort.map(str::to_ascii_lowercase).ok_or(ONESHOT_NO_DEFAULT_EFFORT)?,
        };
        if effort_menu_label(&e).is_empty() {
            return Err(if configured_effort(bot).is_some() { "oneshot_bad_effort" } else { ONESHOT_NO_DEFAULT_EFFORT });
        }
        line.push_str(" --reasoning=");
        line.push_str(&e);
        effort = Some(e);
    }
    line.push_str(if bot.fast != 0 { " --speed=priority" } else { " --speed=standard" });
    Ok((line, effort))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelResponse {
    Changed,
    Rejected,
}

/// 畫面上所有 `/model` 一次性指令的回應行，由舊到新：`• Model changed to … for this session only`（成功）、
/// `■ … is unavailable`／`■ Unexpected argument …`（被拒）。
pub fn model_responses(screen: &str) -> Vec<(ModelResponse, String)> {
    screen
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with("• Model changed to ") && l.ends_with("for this session only") {
                Some((ModelResponse::Changed, l.to_string()))
            } else if l.starts_with('■') && (l.contains("is unavailable") || l.contains("Unexpected argument")) {
                Some((ModelResponse::Rejected, l.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// 送出之後多出來的那一則回應（同樣的字可能出現不只一次，所以比筆數也比最後一則）。
fn new_model_response(before: &[(ModelResponse, String)], after: &[(ModelResponse, String)]) -> Option<ModelResponse> {
    let grew = after.len() != before.len() || after.last() != before.last();
    grew.then(|| after.last().map(|(kind, _)| *kind)).flatten()
}

/// `after` 裡有沒有 `before` 沒有的非空行（逐行計次比對，同樣的字多出來一行也算）。沒有＝送出之後畫面上什麼都沒多。
fn has_new_nonblank_line(before: &str, after: &str) -> bool {
    let mut seen: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for l in before.lines().map(str::trim).filter(|l| !l.is_empty()) {
        *seen.entry(l).or_default() += 1;
    }
    after.lines().map(str::trim).filter(|l| !l.is_empty()).any(|l| {
        let n = seen.entry(l).or_default();
        *n -= 1;
        *n < 0
    })
}

/// 輸入框現在是空的嗎？`Some(false)`＝有東西（草稿、或送出的那行還留著）。讀取失敗、或 herdr 沒有帶樣式的讀法（分不出灰色佔位字與草稿，
/// 跟 [`apply`] 的前置檢查一樣不下判斷）回 `None`。
async fn composer_is_empty(client: &HerdrClient, pane_id: &str) -> Option<bool> {
    let snap = crate::composer_parse::read_styled_snapshot(client, pane_id, "visible", 60).await.ok()?;
    (snap.format == "ansi").then(|| crate::composer_parse::box_state("codex", &snap.text) == crate::composer_parse::BoxState::Empty)
}

/// 一行指令送出之後的失敗出口：先看輸入框，不是空的就回 [`ONESHOT_RESIDUE`]（記 WARN），否則原樣回 `why`。
async fn oneshot_fail(client: &HerdrClient, pane_id: &str, bot: &db::Bot, why: &'static str) -> &'static str {
    if composer_is_empty(client, pane_id).await == Some(false) {
        tracing::warn!(pane_id, bot_id = %bot.id, why, "codex oneshot /model failed and the composer is not empty; leaving it alone");
        return ONESHOT_RESIDUE;
    }
    why
}

/// codex >= 0.164.0 的一行套用（SPEC §4.4a）。先照 [`apply`] 的前置檢查（沒有選單、輸入框沒有草稿）才會進來。
/// 回讀沿用 [`verify`]（再加上明確帶的強度要對上），只看狀態列；`thread_settings_applied` 要到下一個回合才寫進 rollout，不當回讀。
async fn apply_oneshot(
    client: &HerdrClient,
    pane_id: &str,
    bot: &db::Bot,
    fields: &[&str],
    line: &str,
    want_effort: Option<&str>,
) -> Result<CodexRuntime, &'static str> {
    let before = read(client, pane_id).await.map_err(|_| "pane_read_failed")?;
    let before_runtime = parse_status_line(&before);
    let before_responses = model_responses(&before);
    // 送出前就已經是目標：回應再慢也不能當成「不認得」而重送（退回選單會接成 `/model … /model`）。
    let already_there = before_runtime.as_ref().is_some_and(|rt| verify(rt, bot, fields).is_ok() && want_effort.is_none_or(|e| rt.effort.as_deref() == Some(e)));
    if !text(client, pane_id, line).await {
        return Err(oneshot_fail(client, pane_id, bot, "oneshot_send_failed").await);
    }
    tokio::time::sleep(Duration::from_millis(if cfg!(test) { 20 } else { 600 })).await;
    if !key(client, pane_id, "Enter").await {
        return Err(oneshot_fail(client, pane_id, bot, "oneshot_send_failed").await);
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::race_point::hit("codex_oneshot_after_enter", pane_id).await;

    // 先等回應文字。
    let deadline = tokio::time::Instant::now() + oneshot_wait();
    loop {
        tokio::time::sleep(oneshot_poll()).await;
        let screen = read(client, pane_id).await;
        if let Ok(screen) = &screen {
            match new_model_response(&before_responses, &model_responses(screen)) {
                Some(ModelResponse::Rejected) => {
                    tracing::warn!(pane_id, bot_id = %bot.id, command = %line, "codex oneshot /model rejected by the TUI; not retrying through the menus");
                    return Err(ONESHOT_REJECTED);
                }
                Some(ModelResponse::Changed) => break,
                None => {}
            }
            if crate::lifecycle::poller::pane_still_busy(screen) {
                return Err(ONESHOT_BECAME_PROMPT);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let Ok(screen) = screen else { return Err(oneshot_fail(client, pane_id, bot, "pane_read_failed").await) };
            return match parse_status_line(&screen) {
                // 連送出的那一行都完全沒讓畫面多出任何東西、狀態列也沒動，而且送出前不是已經在目標上：才當作不認得。
                // 其餘（畫面有新的字、狀態列動了但沒有回應行、本來就在目標上）都不能重送。
                Some(now) if Some(&now) == before_runtime.as_ref() && !has_new_nonblank_line(&before, &screen) && !already_there => {
                    Err(ONESHOT_NOT_SUPPORTED)
                }
                Some(now) if Some(&now) != before_runtime.as_ref() => break,
                _ => Err(oneshot_fail(client, pane_id, bot, ONESHOT_UNVERIFIED).await),
            };
        }
    }

    // 再等狀態列回來並對上設定（max／ultra 的動畫會暫時蓋住它）。
    let deadline = tokio::time::Instant::now() + oneshot_wait();
    let mut last = ONESHOT_UNVERIFIED;
    loop {
        if let Ok(screen) = read(client, pane_id).await {
            if let Some(seen) = parse_status_line(&screen) {
                match verify(&seen, bot, fields) {
                    Ok(()) if want_effort.is_some_and(|e| seen.effort.as_deref() != Some(e)) => last = "readback_effort_mismatch",
                    Ok(()) => return Ok(seen),
                    Err(why) => last = why,
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(oneshot_fail(client, pane_id, bot, last).await);
        }
        tokio::time::sleep(oneshot_poll()).await;
    }
}

/// `/fast` until the status line says `want`: read the tier first, toggle only when wrong; an unknown tier is
/// toggled once, read back, and toggled back if it landed the wrong way round (module docs).
async fn reach_fast(client: &HerdrClient, pane_id: &str, want: bool, was_fast: Option<bool>) -> Result<(), &'static str> {
    // The screen is the truth about the tier right now; `runs.runtime_fast` may be stale.
    let screen = read(client, pane_id).await.map_err(|_| "pane_read_failed")?;
    let now = parse_status_line(&screen).map(|r| r.fast).or(was_fast);
    match fast_plan(now, want) {
        FastPlan::Keep => {}
        FastPlan::Toggle => {
            if !toggle_fast(client, pane_id).await {
                return Err("fast_toggle_failed");
            }
        }
        FastPlan::ToggleThenCheck => {
            if !toggle_fast(client, pane_id).await {
                return Err("fast_toggle_failed");
            }
            tokio::time::sleep(Duration::from_millis(900)).await;
            let screen = read(client, pane_id).await.map_err(|_| "pane_read_failed")?;
            let seen = parse_status_line(&screen).map(|r| r.fast);
            if needs_second_toggle(seen, want) && !toggle_fast(client, pane_id).await {
                return Err("fast_toggle_failed");
            }
        }
    }
    Ok(())
}

/// Read-back (SPEC §4.4a).
async fn readback(client: &HerdrClient, pane_id: &str, bot: &db::Bot, fields: &[&str]) -> Result<CodexRuntime, &'static str> {
    tokio::time::sleep(Duration::from_millis(900)).await;
    let screen = read(client, pane_id).await.map_err(|_| "pane_read_failed")?;
    let Some(seen) = parse_status_line(&screen) else { return Err("no_status_line") };
    verify(&seen, bot, fields)?;
    Ok(seen)
}

/// 設定的強度要送進 `/model` 選單的值。`default`（或空）＝CLI 預設，選單走 `(default)` 那一列；不把 `default` 當成具體等級傳進去。
fn configured_effort(bot: &db::Bot) -> Option<&str> {
    bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty() && *s != CLI_DEFAULT_EFFORT)
}

/// Does the status line show what the bot is configured for? The model is compared without case: 0.157 prints
/// the display name (`GPT-6-Luna`) for the id `gpt-6-luna`, and a case-only mismatch used to fail every live
/// apply — a fast-only change included — back to "needs restart" (#712).
pub fn verify(seen: &CodexRuntime, bot: &db::Bot, fields: &[&str]) -> Result<(), &'static str> {
    if let Some(m) = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if !seen.model.eq_ignore_ascii_case(m) {
            return Err("readback_model_mismatch");
        }
    }
    if let Some(e) = configured_effort(bot) {
        if seen.effort.as_deref() != Some(e) {
            return Err("readback_effort_mismatch");
        }
    }
    if fields.contains(&"fast") && seen.fast != (bot.fast != 0) {
        return Err("readback_fast_mismatch");
    }
    Ok(())
}

/// 回合中不能碰這個畫面（[`apply_fast_during_turn`]）：選單／選擇畫面開著，或輸入框不是空的（使用者的草稿——打 `/fast`
/// 會接在它後面，Enter 就把整段送出去）。輸入框要用**帶樣式**的讀法判：純文字分不出 codex 的灰色佔位字與打的字。
pub fn busy_fast_blocked(plain: &str, styled: &str) -> bool {
    picker_open(plain) || crate::composer_parse::box_state("codex", styled) != crate::composer_parse::BoxState::Empty
}

/// 回合中只切 fast（#712）。codex 0.157.1 的 service tier 指令（`/fast`）是 `available_during_task`：回合跑著照樣
/// 當場生效（`• Service tier set to priority`，狀態列立刻多出 `fast`），回合不中斷、照常跑完（2026-09-28 隔離 herdr
/// session 實測，兩個方向都試過）。跟閒著時的 [`apply`] 不同：這裡**絕不按 Esc**（回合中 Esc＝中斷回合），
/// 畫面不能碰就回 `busy_not_ready`，由呼叫端排到回合結束再走 [`apply`]。
pub async fn apply_fast_during_turn(
    client: &HerdrClient,
    pane_id: &str,
    bot: &db::Bot,
    was_fast: Option<bool>,
) -> Result<CodexRuntime, &'static str> {
    let plain = read(client, pane_id).await.map_err(|_| "pane_read_failed")?;
    let styled = crate::composer_parse::read_styled(client, pane_id, "visible", 60)
        .await
        .map_err(|_| "pane_read_failed")?;
    if busy_fast_blocked(&plain, &styled) {
        return Err(BUSY_NOT_READY);
    }
    reach_fast(client, pane_id, bot.fast != 0, was_fast).await?;
    readback(client, pane_id, bot, &["fast"]).await
}

/// [`apply_fast_during_turn`] 碰不得畫面時的理由：跟「agent 忙」一樣排到回合結束（`deferred_live::is_busy_reason`）。
pub const BUSY_NOT_READY: &str = "busy_not_ready";

/// 回合中改 model／effort 打斷後接著跑送的那一句（#1681）。
pub const RESUME_TEXT: &str = "continue";

/// 打斷之後等畫面回到可輸入的讀取次數與間隔（連續兩次讀到同一個「沒在跑、輸入框空」才算穩）。
const INTERRUPT_SETTLE_READS: u32 = 10;

fn interrupt_settle_poll() -> Duration {
    Duration::from_millis(if cfg!(test) { 20 } else { 400 })
}

/// 回合中改 model／effort（#1681）：model／effort 要開選單，回合跑著不能碰，所以先按 Esc 把回合停下來，
/// 讓呼叫端接著走閒著時的 [`apply`]、再用 [`send_resume`] 讓它接著做。
///
/// 只有畫面確定是「回合進行中、輸入框空的、沒有選單」才按（跟 [`apply_fast_during_turn`] 同一道閘：輸入框有使用者的草稿
/// 就不碰）。按完讀畫面直到它真的回到可輸入——連續兩次讀到一樣的「沒在跑、輸入框空、沒選單」。
/// `Err(`[`BUSY_NOT_READY`]`)`：沒按 Esc，或按了但畫面確認不了回到可輸入——**不盲打**，呼叫端照舊排到回合結束。
/// `Err("pane_read_failed")`：開頭讀不到畫面，什麼都沒按。
pub async fn interrupt_for_apply(client: &HerdrClient, pane_id: &str) -> Result<(), &'static str> {
    // 一律讀帶樣式的那份、自己剝掉樣式再判斷「在跑／有選單」：herdr 沒有帶樣式的讀法時 `read_styled` 退回純文字，
    // 輸入框就讀成 NonEmpty（佔位字當草稿），這裡照 [`busy_fast_blocked`] 的保守作法不碰。
    let styled = crate::composer_parse::read_styled(client, pane_id, "visible", 60)
        .await
        .map_err(|_| "pane_read_failed")?;
    let plain = unstyled(&styled);
    if !crate::lifecycle::poller::pane_still_busy(&plain) || busy_fast_blocked(&plain, &styled) {
        return Err(BUSY_NOT_READY);
    }
    if !key(client, pane_id, "Escape").await {
        return Err(BUSY_NOT_READY);
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::race_point::hit("codex_interrupt_after_esc", pane_id).await;
    let mut prev: Option<String> = None;
    for _ in 0..INTERRUPT_SETTLE_READS {
        tokio::time::sleep(interrupt_settle_poll()).await;
        let Ok(styled) = crate::composer_parse::read_styled(client, pane_id, "visible", 60).await else {
            prev = None;
            continue;
        };
        let plain = unstyled(&styled);
        let ready = !crate::lifecycle::poller::pane_still_busy(&plain)
            && !picker_open(&plain)
            && crate::composer_parse::box_state("codex", &styled) == crate::composer_parse::BoxState::Empty;
        if ready && prev.as_deref() == Some(plain.as_str()) {
            return Ok(());
        }
        prev = ready.then_some(plain);
    }
    tracing::warn!(pane_id, "codex: Esc sent for a live apply but the pane never settled back to an empty composer");
    Err(BUSY_NOT_READY)
}

/// 帶樣式讀法的文字剝掉樣式（一列一列）。
fn unstyled(styled: &str) -> String {
    styled.lines().map(crate::composer_parse::strip_ansi).collect::<Vec<_>>().join("\n")
}

/// 打斷＋套用之後讓它接著做（#1681）：把 [`RESUME_TEXT`] 打進輸入框、Enter。畫面上選單開著、輸入框有字就不打；
/// Enter 之後輸入框還留著那句字就再按一次（最多兩次），仍然留著回 `Err("resume_not_submitted")`。
/// 打得進去但畫面讀不到也算送出（hook 會照常收尾這個回合），不為了讀不到把已經送出的 continue 報成失敗。
pub async fn send_resume(client: &HerdrClient, pane_id: &str) -> Result<(), &'static str> {
    let snap = crate::composer_parse::read_styled_snapshot(client, pane_id, "visible", 60)
        .await
        .map_err(|_| "pane_read_failed")?;
    if picker_open(&unstyled(&snap.text)) {
        return Err("picker_open");
    }
    let styled = snap.format == "ansi";
    if styled && crate::composer_parse::box_state("codex", &snap.text) == crate::composer_parse::BoxState::NonEmpty {
        return Err("composer_not_empty");
    }
    if !text(client, pane_id, RESUME_TEXT).await {
        return Err("resume_send_failed");
    }
    tokio::time::sleep(Duration::from_millis(if cfg!(test) { 20 } else { 600 })).await;
    for press in 0..2 {
        if !key(client, pane_id, "Enter").await {
            return Err("resume_send_failed");
        }
        tokio::time::sleep(Duration::from_millis(if cfg!(test) { 20 } else { 900 })).await;
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::race_point::hit("codex_resume_after_enter", pane_id).await;
        let Ok(snap) = crate::composer_parse::read_styled_snapshot(client, pane_id, "visible", 60).await else {
            return Ok(());
        };
        let still_typed = snap.format == "ansi"
            && !crate::lifecycle::poller::pane_still_busy(&unstyled(&snap.text))
            && crate::composer_parse::box_state("codex", &snap.text) == crate::composer_parse::BoxState::NonEmpty;
        if !still_typed {
            return Ok(());
        }
        if press == 1 {
            return Err("resume_not_submitted");
        }
    }
    Err("resume_not_submitted")
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::{fast_plan, needs_second_toggle, FastPlan};

    /// #393：`/fast on|off` 不是 slash 形式（0.154.0 實測會被當一般 prompt 送給模型），所以只能靠開關＋讀畫面。
    /// 已知現況就只在不對時按一下；未知現況以前直接拒絕（unknown_fast_tier），現在先按一下、讀回、方向錯才再按。
    /// 窄 pane 把 `enter select · esc back` 折成兩行時，選單仍開著，不能把 prompt 打進去。
    #[test]
    fn a_wrapped_rate_limit_footer_is_still_the_open_prompt() {
        let screen = "\
  Approaching rate limits
  Switch to gpt-6-luna for lower credit usage?

› 1. Switch to gpt-6-luna
  2. Keep current model
  3. Keep current model (never show again)

  enter select ·
  esc back
";
        assert!(rate_limit_switch_prompt_open(screen));
    }

    #[test]
    fn the_plan_toggles_only_when_the_tier_is_known_to_be_wrong() {
        assert_eq!(fast_plan(Some(true), true), FastPlan::Keep);
        assert_eq!(fast_plan(Some(false), false), FastPlan::Keep);
        assert_eq!(fast_plan(Some(false), true), FastPlan::Toggle);
        assert_eq!(fast_plan(Some(true), false), FastPlan::Toggle);
        assert_eq!(fast_plan(None, true), FastPlan::ToggleThenCheck, "不知道現況也不拒絕");
    }

    #[test]
    fn an_unknown_start_toggles_back_only_when_the_first_toggle_went_the_wrong_way() {
        assert!(!needs_second_toggle(Some(true), true), "第一下就到了");
        assert!(needs_second_toggle(Some(false), true), "本來就是開的，按一下變關了：要按回來");
        assert!(!needs_second_toggle(None, true), "讀不到就不再亂按，讓讀回驗證報錯");
    }

    /// 2026-09-14 實況：fork 起來的 codex 狀態列沒有 `Context` 那段，照樣要讀得到剩餘額度。
    /// #321：畫面上方有一行開頭是 `·`、又含 `Context` 的字（bot 印的、或別的 chrome），以前 `parts.next()?` 直接讓整個函式
    /// 回 None，下面真正的狀態列讀不到——改模型／強度的讀回驗證就誤判成「沒落地」。
    #[test]
    fn a_quoted_status_line_above_the_viewport_tail_is_not_the_runtime() {
        let mut screen = "  gpt-6-luna max · /tmp · Context 90% used\n".to_string();
        for _ in 0..STATUS_TAIL_LINES {
            screen.push_str("  the model mentioned Context once\n");
        }
        screen.push_str("  gpt-6.1-sol high · /tmp · Context 3% used\n");
        let rt = parse_status_line(&screen).expect("底部那行才是現況");
        assert_eq!((rt.model.as_str(), rt.effort.as_deref()), ("gpt-6.1-sol", Some("high")));
    }

    #[test]
    fn a_wrapped_status_line_still_reads_the_model_and_effort() {
        let screen = "  gpt-6-luna max · /tmp ·\n  Context 3% used · 5h 90% left\n";
        let rt = parse_status_line(screen).expect("斷行的狀態列仍是同一列");
        assert_eq!((rt.model.as_str(), rt.effort.as_deref(), rt.fast), ("gpt-6-luna", Some("max"), false));
    }

    #[test]
    fn a_stray_leading_dot_row_does_not_hide_the_real_status_line() {
        let screen = "  · Context notes: see docs\n\n  gpt-5.6-sol high · /tmp · Context 3% used\n";
        let rt = parse_status_line(screen).expect("下面那行才是狀態列");
        assert_eq!((rt.model.as_str(), rt.effort.as_deref()), ("gpt-5.6-sol", Some("high")));
    }

    #[test]
    fn a_status_line_without_context_still_yields_the_quota() {
        let q = parse_status_quota("  gpt-5.6-sol medium · ~/project/agents-manager · 5h 82% left · weekly 97% left\n").unwrap();
        assert_eq!(q.five_hour_left, Some(82.0));
        assert_eq!(q.weekly_left, Some(97.0));
    }

    use super::*;

    #[tokio::test]
    async fn close_picker_leaves_model_migration_rate_limit_and_update_choices_for_the_user() {
        let env = crate::testing::env().await;
        for (name, pane, screen) in [
            (
                "migration",
                "pane-codex-migration",
                include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt"),
            ),
            (
                "rate-limit",
                "pane-codex-rate-limit",
                include_str!("lifecycle/fixtures/codex-0.157-rate-limit-switch.txt"),
            ),
            (
                "update-menu",
                "pane-codex-update-menu",
                include_str!("lifecycle/fixtures/codex-0.155-update-menu.txt"),
            ),
        ] {
            let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
            sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
            let run_id = crate::testing::fake_run(&env.app, &bot.id).await;
            sqlx::query("UPDATE runs SET pane_id=? WHERE id=?").bind(pane).bind(&run_id).execute(&env.app.db).await.unwrap();
            env.herdr.set_screen(pane, screen);
            let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
            let client = env.app.herdr_for_run(&run).await.unwrap();
            assert!(!close_picker(&client, pane).await, "{name} requires the user's choice");
        }
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "Escape must not dismiss either choice");
    }

    #[tokio::test]
    async fn an_unreadable_migration_or_rate_limit_preflight_does_not_touch_a_user_choice_screen() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "unreadable-picker").await;
        sqlx::query("UPDATE bots SET kind='codex', model='gpt-6-astra', effort='high' WHERE id=?")
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        for (pane, screen) in [
            ("pane-codex-unreadable-migration", include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt")),
            ("pane-codex-unreadable-rate-limit", include_str!("lifecycle/fixtures/codex-0.157-rate-limit-switch.txt")),
        ] {
            env.herdr.set_screen(pane, screen);
            env.herdr.fail_next("pane.read", crate::testing::Fault::Refuse);

            assert!(apply(&env.app.herdr, pane, &bot, Some(false), &["model"], None, None).await.is_err(), "{pane}");
            assert!(env.herdr.calls_to("pane.send_text").is_empty(), "an unreadable preflight cannot authorize /model: {pane}");
            assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "an unreadable preflight cannot authorize Enter or Escape: {pane}");
        }
    }

    #[tokio::test]
    async fn an_unreadable_ordinary_model_picker_does_not_choose_a_setting() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "unreadable-model-menu").await;
        sqlx::query("UPDATE bots SET kind='codex', model='gpt-6-astra', effort='high' WHERE id=?")
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let pane = "pane-codex-unreadable-model-menu";
        env.herdr.set_screen(pane, EMPTY_BOX_ANSI);
        let screens = env.herdr.screens.clone();
        let fail = env.herdr.fail_later();
        crate::race_point::arm("codex_apply_before_model_picker_read", pane, move || async move {
            screens.lock().unwrap().insert(pane.into(), MODEL_MENU.into());
            fail("pane.read", crate::testing::Fault::Refuse);
        });

        assert_eq!(apply(&env.app.herdr, pane, &bot, Some(false), &["model"], None, None).await, Err("pane_read_failed"));
        assert_eq!(
            env.herdr.calls_to("pane.send_text").iter().map(|v| v["text"].as_str()).collect::<Vec<_>>(),
            vec![Some("/model")],
            "a readable initial composer may open the ordinary picker, but unreadable picker state cannot authorize a setting choice"
        );
        assert_eq!(
            env.herdr.calls_to("pane.send_keys").iter().map(|v| v["keys"].clone()).collect::<Vec<_>>(),
            vec![serde_json::json!(["Enter"])],
            "no numeric choice or cleanup key is safe until the picker can be read"
        );
    }

    #[tokio::test]
    async fn an_unreadable_final_picker_check_is_not_treated_as_closed() {
        let env = crate::testing::env().await;
        let pane = "pane-codex-final-picker-read";
        env.herdr.set_screen(pane, MODEL_MENU);
        let fail = env.herdr.fail_later();
        crate::race_point::arm("codex_picker_before_final_read", pane, move || async move {
            fail("pane.read", crate::testing::Fault::Refuse);
        });

        assert!(close_picker_checked(&env.app.herdr, pane).await.is_err(), "a failed final read is not proof the picker closed");
        assert_eq!(
            env.herdr.calls_to("pane.send_keys").len(),
            4,
            "the read failure after the Escape budget cannot grant a safe-to-type result"
        );
    }

    #[tokio::test]
    async fn a_readable_empty_composer_allows_a_verified_model_and_effort_apply() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "readable-model-menu").await;
        sqlx::query("UPDATE bots SET kind='codex', model='gpt-6-astra', effort='high' WHERE id=?")
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let pane = "pane-codex-readable-model-menu";
        let screens = env.herdr.screens.clone();
        env.herdr.set_screen(pane, EMPTY_BOX_ANSI);
        let model_screens = screens.clone();
        crate::race_point::arm("codex_apply_before_model_picker_read", pane, move || async move {
            model_screens.lock().unwrap().insert(pane.into(), MODEL_MENU.into());
        });
        let effort_screens = screens.clone();
        crate::race_point::arm("codex_apply_before_effort_picker_read", pane, move || async move {
            effort_screens.lock().unwrap().insert(pane.into(), EFFORT_MENU.into());
        });
        crate::race_point::arm("codex_picker_before_final_read", pane, move || async move {
            screens.lock().unwrap().insert(pane.into(), COMPOSER.into());
        });

        let seen = apply(&env.app.herdr, pane, &bot, Some(false), &["model"], None, None).await.unwrap();
        assert_eq!(seen, CodexRuntime { model: "gpt-6-astra".into(), effort: Some("high".into()), fast: false });
        assert_eq!(
            env.herdr.calls_to("pane.send_text").iter().map(|v| v["text"].as_str()).collect::<Vec<_>>(),
            vec![Some("/model"), Some("1"), Some("3")],
            "a readable composer opens the picker and the verified requested rows are selected"
        );
    }

    // ---- codex >= 0.164.0 一行 `/model`（2026-10-23 在 0.164.0 的一次性 CODEX_HOME 實測，回應文字照原樣）----

    #[test]
    fn only_a_seen_codex_at_or_above_0_164_0_takes_the_one_line_form() {
        for (v, ok) in [
            (None, false),
            (Some(""), false),
            (Some("0.163.9"), false),
            (Some("0.164"), false),
            (Some("0.164.0-alpha.1"), false),
            (Some("0.164.0"), true),
            (Some("0.164.1"), true),
            (Some("0.165.0"), true),
            (Some("1.0.0"), true),
        ] {
            assert_eq!(oneshot_supported(v), ok, "{v:?}");
        }
    }

    async fn codex_bot(env: &crate::testing::Env, name: &str, model: Option<&str>, effort: Option<&str>, fast: i64) -> db::Bot {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET kind='codex', model=?, effort=?, fast=? WHERE id=?")
            .bind(model)
            .bind(effort)
            .bind(fast)
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        db::bot(&env.app.db, &bot.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn the_one_line_command_always_names_the_speed_and_a_concrete_effort() {
        let env = crate::testing::env().await;
        let all = ["model", "effort", "fast"];
        let fast = codex_bot(&env, "oneshot-fast", Some("gpt-6-astra"), Some("high"), 1).await;
        let cmd = |bot: &db::Bot, fields: &[&str], default: Option<&str>| oneshot_model_command(bot, fields, default);
        assert_eq!(cmd(&fast, &all, None).unwrap(), ("/model gpt-6-astra --reasoning=high --speed=priority".into(), Some("high".into())));
        let slow = codex_bot(&env, "oneshot-slow", Some("gpt-6-astra"), Some("xhigh"), 0).await;
        assert_eq!(cmd(&slow, &all, Some("low")).unwrap().0, "/model gpt-6-astra --reasoning=xhigh --speed=standard", "設了強度就不看預設");
        // 沒設強度（空、`default`）＝這個模型的預設強度，明確帶上：0.164.0 實測同一個模型不帶 `--reasoning` 會把強度重設成預設，
        // 而 `--reasoning=default` 會被拒，所以要從模型清單查出具體等級。
        for (i, effort) in [None, Some(""), Some("default")].into_iter().enumerate() {
            let bot = codex_bot(&env, &format!("oneshot-no-effort-{i}"), Some("gpt-6.1-sol"), effort, 0).await;
            assert_eq!(
                cmd(&bot, &["effort"], Some("LOW")).unwrap(),
                ("/model gpt-6.1-sol --reasoning=low --speed=standard".into(), Some("low".into())),
                "{effort:?}"
            );
            assert_eq!(cmd(&bot, &["effort"], None), Err(ONESHOT_NO_DEFAULT_EFFORT), "查不到預設強度＝不送，走選單：{effort:?}");
            assert_eq!(cmd(&bot, &["effort"], Some("turbo")), Err(ONESHOT_NO_DEFAULT_EFFORT), "清單裡的值不是已知等級也不送：{effort:?}");
        }
        // 沒設模型＝選單的 `(current)`：不帶模型，維持現在的。
        let no_model = codex_bot(&env, "oneshot-no-model", None, Some("max"), 1).await;
        assert_eq!(cmd(&no_model, &["effort"], None).unwrap().0, "/model --reasoning=max --speed=priority");
        // 只改 fast：不碰模型與強度（跟選單路徑只按 `/fast` 一樣），也就不需要預設強度。
        assert_eq!(cmd(&fast, &["fast"], None).unwrap(), ("/model --speed=priority".into(), None));
        let unset = codex_bot(&env, "oneshot-fast-only", Some("gpt-6.1-sol"), None, 0).await;
        assert_eq!(cmd(&unset, &["fast"], None).unwrap().0, "/model --speed=standard");
        // 多出來的空白或旗標會變成別的參數：不送。
        let bad = codex_bot(&env, "oneshot-bad-model", Some("gpt-6-luna --speed=flex"), None, 0).await;
        assert_eq!(cmd(&bad, &["model"], Some("low")), Err("oneshot_bad_model"));
        let bad = codex_bot(&env, "oneshot-bad-effort", Some("gpt-6-luna"), Some("turbo"), 0).await;
        assert_eq!(cmd(&bad, &["effort"], Some("low")), Err("oneshot_bad_effort"));
    }

    #[tokio::test]
    async fn the_default_effort_comes_from_the_fresh_model_list_of_the_bots_host_and_identity() {
        let env = crate::testing::env().await;
        let bot = codex_bot(&env, "oneshot-default-effort", Some("GPT-6.1-Sol"), None, 0).await;
        let list = serde_json::json!({"models": [
            {"id": "gpt-6.1-sol", "default_effort": "Low"},
            {"id": "gpt-6-luna", "default_effort": "medium"},
            {"id": "gpt-6-astra", "default_effort": null},
            {"id": "gpt-7-test", "default_effort": "bogus"},
        ]});
        assert_eq!(model_default_effort(&env.app, &bot, None).await, None, "清單還沒進快取：查不到");
        env.app.models_cache.lock().await.insert("local/codex/".into(), (std::time::Instant::now(), list.clone()));
        assert_eq!(model_default_effort(&env.app, &bot, None).await.as_deref(), Some("low"), "不分大小寫比 id");
        let unset = codex_bot(&env, "oneshot-default-effort-unset", None, None, 0).await;
        assert_eq!(model_default_effort(&env.app, &unset, Some("gpt-6-luna")).await.as_deref(), Some("medium"), "沒設模型就用 run 現在的");
        assert_eq!(model_default_effort(&env.app, &unset, None).await, None);
        for model in ["gpt-6-astra", "gpt-7-test", "gpt-9"] {
            assert_eq!(model_default_effort(&env.app, &unset, Some(model)).await, None, "{model}：沒有、或不是已知等級");
        }
        // 過期的快取（10 分鐘）不算。
        let old = std::time::Instant::now().checked_sub(crate::models::CACHE_TTL + std::time::Duration::from_secs(1)).unwrap();
        env.app.models_cache.lock().await.insert("local/codex/".into(), (old, list));
        assert_eq!(model_default_effort(&env.app, &bot, None).await, None);
    }

    /// 回應文字照 0.164.0 實測原文。
    const CHANGED: &str = "• Model changed to gpt-6-astra high fast for this session only";
    const UNAVAILABLE: &str = "■ Model 'nosuchmodel' is unavailable. Use /model to see available models.";
    const BAD_LEVEL: &str = "■ Reasoning level 'bogus' is unavailable for gpt-6-astra. Available: low, medium, high, xhigh, max, ultra.";
    const BAD_SPEED: &str = "■ Speed 'turbo' is unavailable for gpt-6-astra. Available: standard, flex, priority.";
    const BAD_ARG: &str = "■ Unexpected argument 'high'. Use '--reasoning=<level>' or '--speed=<tier>'.";

    #[test]
    fn the_answers_of_the_one_line_form_are_told_apart() {
        for (line, kind) in [
            (CHANGED, ModelResponse::Changed),
            ("• Model changed to gpt-6-astra medium flex for this session only", ModelResponse::Changed),
            (UNAVAILABLE, ModelResponse::Rejected),
            (BAD_LEVEL, ModelResponse::Rejected),
            (BAD_SPEED, ModelResponse::Rejected),
            (BAD_ARG, ModelResponse::Rejected),
        ] {
            assert_eq!(model_responses(&format!("  {line}\n\n› Ask Codex to do anything\n")), vec![(kind, line.to_string())], "{line}");
        }
        // 舊版選單的結果（沒有 `for this session only`）、一般文字、引用的 `■` 行都不是這個回應。
        assert!(model_responses("• Model changed to gpt-6-astra high\n■ something else broke\n  > ■ Unexpected argument").is_empty());
        let before = model_responses(&format!("{CHANGED}\n"));
        assert_eq!(new_model_response(&before, &before), None, "畫面沒變＝沒有新的回應");
        let after = model_responses(&format!("{CHANGED}\n{CHANGED}\n"));
        assert_eq!(new_model_response(&before, &after), Some(ModelResponse::Changed), "同樣的字再出現一次也算新的");
        let after = model_responses(&format!("{CHANGED}\n{UNAVAILABLE}\n"));
        assert_eq!(new_model_response(&before, &after), Some(ModelResponse::Rejected));
    }

    fn with_footer(fixture: &str, footer: &str) -> String {
        let mut lines: Vec<&str> = fixture.lines().collect();
        lines.pop();
        format!("{}\n{footer}\n", lines.join("\n"))
    }

    fn idle_with_footer(footer: &str) -> String {
        with_footer(EMPTY_BOX_ANSI, footer)
    }

    /// 輸入框裡留著字（使用者的草稿、或送出的那行沒被收走）的畫面。
    fn residue_with_footer(footer: &str) -> String {
        with_footer(include_str!("lifecycle/fixtures/codex-0.155-draft.ansi"), footer)
    }

    /// 一行套用的 apply：送出前是輸入框空的畫面，Enter 之後畫面換成 `after`；`at_fallback` 是退回選單前一刻的畫面。
    #[derive(Default)]
    struct Run<'a> {
        name: &'a str,
        bot: (Option<&'a str>, Option<&'a str>, i64),
        fields: &'a [&'a str],
        version: Option<&'a str>,
        default_effort: Option<&'a str>,
        before_footer: &'a str,
        after: String,
        at_fallback: Option<String>,
    }

    struct Done {
        result: Result<CodexRuntime, &'static str>,
        typed: Vec<String>,
        keys: Vec<serde_json::Value>,
    }

    async fn oneshot_apply(r: Run<'static>) -> Done {
        let env = crate::testing::env().await;
        let bot = codex_bot(&env, r.name, r.bot.0, r.bot.1, r.bot.2).await;
        let pane: &'static str = Box::leak(format!("pane-codex-oneshot-{}", r.name).into_boxed_str());
        env.herdr.set_screen(pane, &idle_with_footer(r.before_footer));
        let screens = env.herdr.screens.clone();
        let after = r.after;
        let on_enter = screens.clone();
        crate::race_point::arm("codex_oneshot_after_enter", pane, move || async move {
            on_enter.lock().unwrap().insert(pane.into(), after);
        });
        if let Some(screen) = r.at_fallback {
            crate::race_point::arm("codex_oneshot_before_fallback", pane, move || async move {
                screens.lock().unwrap().insert(pane.into(), screen);
            });
        }
        let result = apply(&env.app.herdr, pane, &bot, Some(false), r.fields, r.version, r.default_effort).await;
        let typed = env.herdr.calls_to("pane.send_text").iter().filter_map(|v| v["text"].as_str().map(String::from)).collect();
        let keys = env.herdr.calls_to("pane.send_keys").iter().map(|v| v["keys"].clone()).collect();
        Done { result, typed, keys }
    }

    const BEFORE: &str = "  gpt-5.6-sol low · ~/project/agents-manager · Context 3% used · 5h 10% left";
    const V164: Option<&str> = Some("0.164.0");

    #[tokio::test]
    async fn a_supported_codex_applies_model_effort_and_speed_in_one_line_and_reads_the_status_line_back() {
        let after = format!("  {CHANGED}\n{}", idle_with_footer("  GPT-6-Astra high fast · ~/project/agents-manager · Context 3% used · 5h 10% left"));
        let d = oneshot_apply(Run {
            name: "ok", bot: (Some("gpt-6-astra"), Some("high"), 1), fields: &["model", "effort", "fast"], version: V164, before_footer: BEFORE, after, ..Default::default()
        }).await;
        assert_eq!(d.result, Ok(CodexRuntime { model: "gpt-6-astra".into(), effort: Some("high".into()), fast: true }));
        assert_eq!(d.typed, vec!["/model gpt-6-astra --reasoning=high --speed=priority"], "一行就完成，沒有第二次打字、沒有選單");
        assert_eq!(d.keys, vec![serde_json::json!(["Enter"])]);
    }

    /// 第 5 點：同一個模型、沒設強度。0.164.0 實測 `/model gpt-6.1-sol --reasoning=high` 之後 `/model gpt-6.1-sol --speed=standard` 會變 `low`（重設成模型預設），
    /// 所以把清單查到的預設強度明確帶上，並且回讀要對上。
    #[tokio::test]
    async fn the_same_model_with_an_unset_effort_names_the_default_effort_and_the_readback_must_match_it() {
        let same = "  gpt-6.1-sol high · ~/project/agents-manager · Context 3% used · 5h 10% left";
        let ok = format!("  {}\n{}", CHANGED.replace("astra high fast", "sol low"), idle_with_footer("  GPT-6.1-Sol low · ~/project/agents-manager · Context 3% used · 5h 10% left"));
        let d = oneshot_apply(Run {
            name: "same-model", bot: (Some("gpt-6.1-sol"), None, 0), fields: &["effort"], version: V164, default_effort: Some("low"), before_footer: same, after: ok, ..Default::default()
        }).await;
        assert_eq!(d.result, Ok(CodexRuntime { model: "gpt-6.1-sol".into(), effort: Some("low".into()), fast: false }));
        assert_eq!(d.typed, vec!["/model gpt-6.1-sol --reasoning=low --speed=standard"]);
        // 畫面上強度不是我們帶的那個：不算套用成功。
        let still_high = format!("  {}\n{}", CHANGED.replace("astra high fast", "sol high"), idle_with_footer(same));
        let d = oneshot_apply(Run {
            name: "same-model-high", bot: (Some("gpt-6.1-sol"), None, 0), fields: &["effort"], version: V164, default_effort: Some("low"), before_footer: same, after: still_high, ..Default::default()
        }).await;
        assert_eq!(d.result, Err("readback_effort_mismatch"));
    }

    #[tokio::test]
    async fn an_unset_effort_without_a_known_default_uses_the_menu_default_row() {
        let d = oneshot_apply(Run {
            name: "no-default", bot: (Some("gpt-6.1-sol"), None, 0), fields: &["model", "effort"], version: V164, default_effort: None, before_footer: BEFORE, after: idle_with_footer(BEFORE), ..Default::default()
        }).await;
        assert_eq!(d.typed, vec!["/model"], "一行指令沒東西可帶：直接走選單，不送帶參數的行");
        assert_eq!(d.result, Err("picker_failed"));
    }

    #[tokio::test]
    async fn a_rejected_one_line_command_fails_the_apply_and_never_retries_through_the_menus() {
        for rejection in [UNAVAILABLE, BAD_LEVEL, BAD_SPEED, BAD_ARG] {
            let after = format!("  {rejection}\n{}", idle_with_footer(BEFORE));
            let d = oneshot_apply(Run {
                name: "rejected", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, default_effort: Some("low"), before_footer: BEFORE, after, ..Default::default()
            }).await;
            assert_eq!(d.result, Err(ONESHOT_REJECTED), "{rejection}");
            assert_eq!(d.typed.len(), 1, "被拒就停：不退回 `/model` 選單 ({rejection})");
            assert_eq!(d.keys, vec![serde_json::json!(["Enter"])], "{rejection}");
        }
    }

    #[tokio::test]
    async fn a_screen_that_did_not_change_at_all_falls_back_to_the_menus_exactly_once() {
        let d = oneshot_apply(Run {
            name: "silent", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, before_footer: BEFORE, after: idle_with_footer(BEFORE), ..Default::default()
        }).await;
        // 退回的選單路徑在這個假畫面上開不出選單，所以失敗；重點是順序：先一行、再恰好一次裸 `/model`。
        assert_eq!(d.result, Err("picker_failed"));
        assert_eq!(d.typed, vec!["/model gpt-6-astra --reasoning=high --speed=standard".to_string(), "/model".to_string()]);
    }

    /// 第 1 點：退回選單前輸入框已經不是空的（送出的那行還留著）：再打 `/model` 會接成 `/model … /model` 再按 Enter。
    #[tokio::test]
    async fn the_fallback_never_types_into_a_composer_that_is_not_empty() {
        let d = oneshot_apply(Run {
            name: "fallback-residue", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, before_footer: BEFORE,
            after: idle_with_footer(BEFORE), at_fallback: Some(residue_with_footer(BEFORE)), ..Default::default()
        }).await;
        assert_eq!(d.result, Err(ONESHOT_RESIDUE));
        assert_eq!(d.typed.len(), 1, "只有一行指令那一次打字，沒有選單的 `/model`");
        assert_eq!(d.keys, vec![serde_json::json!(["Enter"])], "沒有第二次 Enter");
    }

    /// 第 2 點：每個失敗出口都看一次輸入框，有東西就是 `oneshot_residue`。
    #[tokio::test]
    async fn every_failure_exit_after_sending_reports_a_composer_that_is_not_empty() {
        let hidden_footer = |composer: String| format!("  {CHANGED}\n{composer}\n  M A X\n");
        let screens: [(&str, String, &str); 4] = [
            // 回應來了，但狀態列一直被蓋住（逾時）
            ("hidden", hidden_footer(with_footer(include_str!("lifecycle/fixtures/codex-0.155-draft.ansi"), "")), ONESHOT_RESIDUE),
            // 回應來了，狀態列對不上設定
            ("mismatch", format!("  {CHANGED}\n{}", residue_with_footer("  GPT-6-Astra low · ~/project/agents-manager · Context 3% used · 5h 10% left")), ONESHOT_RESIDUE),
            // 沒有回應文字、畫面多了字（輸入框留著送出的行）
            ("no-answer", residue_with_footer(BEFORE), ONESHOT_RESIDUE),
            // 同樣三種情形、輸入框是空的：照原因回
            ("hidden-empty", hidden_footer(with_footer(EMPTY_BOX_ANSI, "")), ONESHOT_UNVERIFIED),
        ];
        for (name, after, want) in screens {
            let d = oneshot_apply(Run {
                name, bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model", "effort"], version: V164, before_footer: BEFORE, after, ..Default::default()
            }).await;
            assert_eq!(d.result, Err(want), "{name}");
            assert_eq!(d.typed.len(), 1, "{name}: 不再打字");
        }
        let empty_mismatch = format!("  {CHANGED}\n{}", idle_with_footer("  GPT-6-Astra low · ~/project/agents-manager · Context 3% used · 5h 10% left"));
        let d = oneshot_apply(Run {
            name: "mismatch-empty", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model", "effort"], version: V164, before_footer: BEFORE, after: empty_mismatch, ..Default::default()
        }).await;
        assert_eq!(d.result, Err("readback_effort_mismatch"), "輸入框是空的就照回讀的原因回");
    }

    /// 第 3 點：畫面上只要多出任何新的非空行，就不是「不認得」，不能退回選單。
    #[tokio::test]
    async fn any_new_line_on_the_screen_after_sending_is_never_taken_for_an_unknown_syntax() {
        let with_note = format!("{}  (something new on screen)\n", idle_with_footer(BEFORE));
        let d = oneshot_apply(Run {
            name: "new-line", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, before_footer: BEFORE, after: with_note, ..Default::default()
        }).await;
        assert_eq!(d.result, Err(ONESHOT_UNVERIFIED));
        assert_eq!(d.typed.len(), 1, "不退回選單");
        assert!(has_new_nonblank_line("a\nb\n", "a\nb\nb\n"), "同樣的字多出一行也算");
        assert!(!has_new_nonblank_line("a\n\n  b  \n", "b\na\n\n"), "只是空白行、順序不同不算");
    }

    /// 第 4 點：送出前就已經是目標，回應慢（逾時還沒出來）也不能退回選單重送。
    #[tokio::test]
    async fn a_slow_answer_when_the_target_is_already_reached_is_never_resent_through_the_menus() {
        let at_target = "  gpt-6-astra high fast · ~/project/agents-manager · Context 3% used · 5h 10% left";
        let d = oneshot_apply(Run {
            name: "slow", bot: (Some("gpt-6-astra"), Some("high"), 1), fields: &["model", "effort", "fast"], version: V164, before_footer: at_target,
            after: idle_with_footer(at_target), ..Default::default()
        }).await;
        assert_eq!(d.result, Err(ONESHOT_UNVERIFIED));
        assert_eq!(d.typed.len(), 1, "不退回選單");
        // 對照：同樣的畫面，但送出前不在目標上，才會退回選單一次。
        let d = oneshot_apply(Run {
            name: "slow-control", bot: (Some("gpt-6-astra"), Some("high"), 1), fields: &["model", "effort", "fast"], version: V164, before_footer: BEFORE,
            after: idle_with_footer(BEFORE), ..Default::default()
        }).await;
        assert_eq!(d.typed.len(), 2);
    }

    #[tokio::test]
    async fn a_line_that_turned_into_a_prompt_is_never_followed_by_more_typing() {
        let after = format!("› /model gpt-6-astra --reasoning=high --speed=standard\n• Working (1s • esc to interrupt)\n{}", idle_with_footer(BEFORE));
        let d = oneshot_apply(Run {
            name: "prompt", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, before_footer: BEFORE, after, ..Default::default()
        }).await;
        assert_eq!(d.result, Err(ONESHOT_BECAME_PROMPT));
        assert_eq!(d.typed.len(), 1);
    }

    #[tokio::test]
    async fn an_older_or_unseen_codex_keeps_the_menu_path_and_never_types_arguments() {
        for version in [None, Some("0.163.2"), Some("0.162.1")] {
            let d = oneshot_apply(Run {
                name: "old", bot: (Some("gpt-6-astra"), Some("high"), 1), fields: &["model", "effort", "fast"], version, default_effort: Some("low"), before_footer: BEFORE,
                after: idle_with_footer(BEFORE), ..Default::default()
            }).await;
            assert_eq!(d.result, Err("picker_failed"), "{version:?}");
            assert_eq!(d.typed, vec!["/model"], "舊版 `/model` 帶參數會被當 prompt 送出：{version:?}");
        }
    }

    async fn run_with_runtime(env: &crate::testing::Env, fast: i64) -> String {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "payload").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET runtime_model='gpt-5.6-luna', runtime_effort='max', runtime_fast=? WHERE id=?")
            .bind(fast)
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();
        run
    }

    async fn fast_of(env: &crate::testing::Env, run: &str) -> Option<i64> {
        db::run(&env.app.db, run).await.unwrap().unwrap().runtime_fast
    }

    /// 2026-09-22 使用者：標題列寫 fast，codex 狀態列其實沒有 fast。啟動時記的 1 之後沒人改。
    #[tokio::test]
    async fn a_status_line_without_fast_corrects_a_stale_runtime_fast() {
        let env = crate::testing::env().await;
        let run = run_with_runtime(&env, 1).await;
        let screen = "› Ask Codex\n\n  gpt-5.6-luna max · ~/project/hermes-agents · Context 43% used · 5h 12% left\n";
        assert!(correct_runtime_from_screen(&env.app, &run, screen).await);
        assert_eq!(fast_of(&env, &run).await, Some(0));
    }

    #[tokio::test]
    async fn a_status_line_with_fast_corrects_the_other_way() {
        let env = crate::testing::env().await;
        let run = run_with_runtime(&env, 0).await;
        let screen = "  gpt-5.6-luna max fast · /tmp · Context 43% used · 5h 12% left\n";
        assert!(correct_runtime_from_screen(&env.app, &run, screen).await);
        assert_eq!(fast_of(&env, &run).await, Some(1));
    }

    #[tokio::test]
    async fn fast_switches_follow_only_children_and_unchanged_runtime_keeps_pending_settings() {
        for managed in ["child", "user"] {
            let env = crate::testing::env().await;
            let (bot, run) = child_with_effort(&env, managed, Some("high"), Some("high")).await;
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, i64::from(managed != "child"));
            assert_eq!(fast_of(&env, &run).await, Some(0));

            // UI 再要求開 fast，尚未套用；即使這輪其他 runtime 欄位改變，也不能將 fast 蓋回關。
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            let low = ASTRA_HIGH.replace("astra high", "astra low");
            assert!(correct_runtime_from_screen(&env.app, &run, &low).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1);
            assert!(!correct_runtime_from_screen(&env.app, &run, &low).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1);

            sqlx::query("UPDATE bots SET fast=0 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            let on = low.replace("astra low", "astra low fast");
            assert!(correct_runtime_from_screen(&env.app, &run, &on).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, i64::from(managed == "child"));
            assert_eq!(fast_of(&env, &run).await, Some(1));
        }
    }

    #[tokio::test]
    async fn fast_follow_failure_is_not_replayed_and_unknown_is_not_a_switch() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET runtime_fast=NULL WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1, "初次觀察未知不是已知的切換");
        sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_fast_follow BEFORE UPDATE OF fast ON bots BEGIN SELECT RAISE(FAIL, 'test fast write failure'); END").execute(&env.app.db).await.unwrap();
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        assert_eq!(fast_of(&env, &run).await, Some(1));
        sqlx::query("DROP TRIGGER reject_fast_follow").execute(&env.app.db).await.unwrap();
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1, "失敗的 edge 不再回寫設定，只補 runtime");
        assert_eq!(fast_of(&env, &run).await, Some(0));
    }

    /// 讓 fast edge 先嘗試一次並失敗（trigger 擋住 bots.fast 寫入），之後同一個 edge 再出現就是被去重。
    async fn deduped_fast_edge(env: &crate::testing::Env) -> (String, String) {
        let (bot, run) = child_with_effort(env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_fast_follow BEFORE UPDATE OF fast ON bots BEGIN SELECT RAISE(FAIL, 'test fast write failure'); END").execute(&env.app.db).await.unwrap();
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        sqlx::query("DROP TRIGGER reject_fast_follow").execute(&env.app.db).await.unwrap();
        (bot, run)
    }

    async fn bot_model_fast(env: &crate::testing::Env, bot: &str) -> (Option<String>, i64) {
        let b = db::bot(&env.app.db, bot).await.unwrap().unwrap();
        (b.model, b.fast)
    }

    /// #1906：fast edge 被去重時，同一輪畫面上的 model／effort 切換仍要寫進 bots，否則 runtime 已更新、下一輪 same 成立，永遠補不回來。
    #[tokio::test]
    async fn a_deduped_fast_edge_still_follows_a_model_and_effort_change() {
        let env = crate::testing::env().await;
        let (bot, run) = deduped_fast_edge(&env).await;
        let switched = ASTRA_HIGH.replace("gpt-6-astra high", "gpt-6-luna low");
        assert!(correct_runtime_from_screen(&env.app, &run, &switched).await);
        let (model, fast) = bot_model_fast(&env, &bot).await;
        assert_eq!(model.as_deref(), Some("gpt-6-luna"), "model 不能因 fast edge 被去重而丟掉");
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("low"));
        assert_eq!(fast, 1, "被去重的 fast edge 不重播 fast");
        assert_eq!(fast_of(&env, &run).await, Some(0));
        assert!(!correct_runtime_from_screen(&env.app, &run, &switched).await);
    }

    /// 沒有 fast edge（fast 沒變）時 model／effort 照常跟隨，且不碰 bots.fast（UI 尚未套用的 fast 意圖不能被蓋掉）。
    #[tokio::test]
    async fn without_a_fast_edge_model_and_effort_follow_and_fast_is_untouched() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        // runtime_fast 維持 0，畫面也沒有 fast：不是 edge。
        let switched = ASTRA_HIGH.replace("gpt-6-astra high", "gpt-6-luna low");
        assert!(correct_runtime_from_screen(&env.app, &run, &switched).await);
        let (model, fast) = bot_model_fast(&env, &bot).await;
        assert_eq!(model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("low"));
        assert_eq!(fast, 1, "沒有 fast edge 就不寫 fast");
        assert!(super::fast_follow_edges().lock().unwrap().map.get(&run).is_none(), "沒有 edge 就不登記");
    }

    /// 第一次嘗試成功的 fast edge：model 與 fast 在同一個交易一起提交。
    #[tokio::test]
    async fn a_first_fast_edge_attempt_writes_model_and_fast_in_one_transaction() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=0 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        let switched = ASTRA_HIGH.replace("gpt-6-astra high", "gpt-6-luna high fast");
        assert!(correct_runtime_from_screen(&env.app, &run, &switched).await);
        assert_eq!(bot_model_fast(&env, &bot).await, (Some("gpt-6-luna".to_string()), 1));
        assert_eq!(fast_of(&env, &run).await, Some(1));

        // 原子性：同一輪 fast 寫入被擋時，model 也一起 rollback、runtime 不前進（下一輪還能重試）。
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=0 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_fast_follow BEFORE UPDATE OF fast ON bots BEGIN SELECT RAISE(FAIL, 'test fast write failure'); END").execute(&env.app.db).await.unwrap();
        assert!(!correct_runtime_from_screen(&env.app, &run, &switched).await);
        assert_eq!(bot_model_fast(&env, &bot).await, (Some("gpt-6-astra".to_string()), 0));
        assert_eq!(fast_of(&env, &run).await, Some(0));
    }

    fn go(follow: bool, start_sweeper: bool) -> super::Outcome {
        super::Outcome { follow, start_sweeper, full: false }
    }

    #[test]
    fn fast_follow_edges_keep_only_the_latest_edge_and_do_not_evict_active_runs_at_capacity() {
        let mut edges = super::FastFollowEdges::default();
        assert_eq!(edges.attempt("one", false, true), go(true, true));
        assert_eq!(edges.attempt("one", false, true), super::Outcome::default());
        assert_eq!(edges.attempt("one", true, true), go(true, false));
        assert_eq!(edges.attempt("one", false, true), go(true, false));
        assert_eq!(edges.map.len(), 1);
        for i in 1..super::FAST_FOLLOW_LIMIT {
            assert_eq!(edges.attempt(&format!("run-{i}"), false, true), go(true, false), "只有第一個 edge 負責起清理 task");
        }
        assert_eq!(edges.attempt("overflow", true, false), super::Outcome { full: true, ..Default::default() });
        assert_eq!(edges.map.len(), super::FAST_FOLLOW_LIMIT);
        assert_eq!(edges.attempt("one", false, true), super::Outcome::default(), "active edge must remain protected");
    }

    /// 表非空就一定有清理 task：task 結束（表空、旗標清掉）與插入新 edge 兩種順序都要成立。
    #[test]
    fn a_new_edge_never_lands_in_the_gap_after_the_sweeper_decides_to_end() {
        // 順序一：task 先結束，之後插入 → 新 edge 自己起新的 task。
        let mut edges = super::FastFollowEdges::default();
        assert!(edges.attempt("a", true, false).start_sweeper);
        edges.map.remove("a");
        assert!(edges.finish_sweeper_if_empty(), "表空了，task 結束");
        assert!(edges.attempt("b", true, false).start_sweeper, "旗標已清掉，新 edge 要起新的 task");
        assert!(edges.sweeper);
        // 順序二：新 edge 先插進來，task 才檢查 → 表不空，task 不結束，也不重複起。
        let mut edges = super::FastFollowEdges::default();
        assert!(edges.attempt("a", true, false).start_sweeper);
        edges.map.remove("a");
        assert!(!edges.attempt("b", true, false).start_sweeper, "旗標還在，task 還活著");
        assert!(!edges.finish_sweeper_if_empty(), "表不空，task 不能結束");
        assert!(edges.sweeper);
    }

    /// 兩條執行緒搶同一把鎖：一條不斷插新 edge，一條扮演清理 task（移掉全部、檢查、結束）。
    /// 任何時刻清理 task 數 ≤ 1，結束時表非空就必須有一個還在跑。
    #[test]
    fn concurrent_inserts_and_sweeper_exits_never_leave_edges_without_a_sweeper() {
        use std::sync::atomic::{AtomicIsize, Ordering};
        for round in 0..200 {
            let edges = std::sync::Arc::new(std::sync::Mutex::new(super::FastFollowEdges::default()));
            let running = std::sync::Arc::new(AtomicIsize::new(0));
            let max_running = std::sync::Arc::new(AtomicIsize::new(0));
            let inserter = {
                let (edges, running, max_running) = (edges.clone(), running.clone(), max_running.clone());
                std::thread::spawn(move || {
                    for i in 0..50 {
                        // 計數與鎖內的狀態變化一起做（這個計數只是測試的記帳，不是被測的同步）。
                        let mut guard = edges.lock().unwrap();
                        if guard.attempt(&format!("r{round}-{i}"), true, false).start_sweeper {
                            max_running.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                        }
                        drop(guard);
                        std::thread::yield_now();
                    }
                })
            };
            let sweeper = {
                let (edges, running) = (edges.clone(), running.clone());
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let mut guard = edges.lock().unwrap();
                        // 有清理 task 在跑（旗標為 true）才會醒來巡一輪；沒有就沒有人做這件事。
                        if guard.sweeper {
                            guard.map.clear();
                            if guard.finish_sweeper_if_empty() {
                                running.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        drop(guard);
                        std::thread::yield_now();
                    }
                })
            };
            inserter.join().unwrap();
            sweeper.join().unwrap();
            let guard = edges.lock().unwrap();
            assert!(max_running.load(Ordering::SeqCst) <= 1, "同時最多一個清理 task");
            assert_eq!(guard.sweeper, running.load(Ordering::SeqCst) == 1, "旗標與實際在跑的 task 數一致");
            assert!(guard.map.is_empty() || guard.sweeper, "表非空就必須有清理 task");
        }
    }

    #[test]
    fn the_table_full_warning_is_throttled_to_once_an_hour() {
        let mut edges = super::FastFollowEdges::default();
        let t0 = std::time::Instant::now();
        assert!(edges.should_warn_full(t0));
        assert!(!edges.should_warn_full(t0 + std::time::Duration::from_secs(3599)));
        assert!(edges.should_warn_full(t0 + std::time::Duration::from_secs(3600)));
    }

    /// 本機的一張小表（上限 `limit`），測試不碰全域的那張。
    fn small_table(limit: usize) -> super::SharedEdges {
        Box::leak(Box::new(std::sync::Mutex::new(super::FastFollowEdges { limit, ..Default::default() })))
    }

    async fn active_run(env: &crate::testing::Env, name: &str) -> String {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        crate::testing::fake_run(&env.app, &bot.id).await
    }

    fn put(edges: super::SharedEdges, run: &str) {
        assert!(super::lock_edges(edges).attempt(run, true, false).follow);
    }

    /// 清理一次：已結束／不存在的 run 的 edge 移掉，仍在跑的留著；超過一批（500）的表也要全部巡到。
    #[tokio::test]
    async fn one_sweep_drops_ended_runs_in_every_batch_and_keeps_active_ones() {
        let env = crate::testing::env().await;
        let edges = small_table(10_000);
        let (a, b) = (active_run(&env, "a").await, active_run(&env, "b").await);
        let ended = active_run(&env, "ended").await;
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&ended).execute(&env.app.db).await.unwrap();
        for run in [&a, &b, &ended] { put(edges, run); }
        let phantoms: Vec<String> = (0..3 * super::SWEEP_BATCH + 7).map(|i| format!("gone-{i}")).collect();
        for run in &phantoms { put(edges, run); }

        let removed = super::sweep_finished_fast_edges(edges, &env.app.db).await;
        assert_eq!(removed, phantoms.len() + 1);
        let left: std::collections::BTreeSet<String> = super::lock_edges(edges).map.keys().cloned().collect();
        assert_eq!(left, [a, b].into_iter().collect(), "只剩仍在跑的 run");
    }

    /// #1898：表滿時先清掉已結束的 run 再試，新的 run 進得來；清完還是滿才擋下。
    #[tokio::test]
    async fn a_full_table_makes_room_by_sweeping_ended_runs_but_never_evicts_active_ones() {
        let env = crate::testing::env().await;
        let edges = small_table(3);
        let (a, b) = (active_run(&env, "a").await, active_run(&env, "b").await);
        put(edges, &a);
        put(edges, &b);
        put(edges, "gone-1");
        assert!(super::lock_edges(edges).sweeper, "第一個 edge 起了清理 task 的旗標");

        let c = active_run(&env, "c").await;
        let got = super::attempt_fast_follow_in(edges, &env.app.db, &c, true, false).await;
        assert!(got.is_some(), "表滿但有已結束的 run：清完就進得來");
        let keys: std::collections::BTreeSet<String> = super::lock_edges(edges).map.keys().cloned().collect();
        assert_eq!(keys, [a.clone(), b.clone(), c.clone()].into_iter().collect());

        // 三個都還在跑：清完還是滿，第四個被擋下，已有的 edge 一個都不能被擠掉。
        let d = active_run(&env, "d").await;
        assert!(super::attempt_fast_follow_in(edges, &env.app.db, &d, true, false).await.is_none());
        assert_eq!(super::lock_edges(edges).map.len(), 3);
        assert!(super::lock_edges(edges).last_full_warn.is_some(), "擋下時記了 WARN 的時間");
        let e = active_run(&env, "e").await;
        let before = super::lock_edges(edges).last_full_warn;
        assert!(super::attempt_fast_follow_in(edges, &env.app.db, &e, true, false).await.is_none());
        assert_eq!(super::lock_edges(edges).last_full_warn, before, "一小時內不再 WARN");
    }

    #[tokio::test]
    async fn a_tui_fast_switch_after_ui_live_apply_is_a_new_edge_with_or_without_a_matching_poll() {
        for matching_poll in [false, true] {
            let env = crate::testing::env().await;
            let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 0);
            assert_eq!(fast_of(&env, &run).await, Some(0));

            // UI live apply updates both rows, without passing through this screen observer.
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            if matching_poll {
                let on = ASTRA_HIGH.replace("astra high", "astra high fast");
                assert!(!correct_runtime_from_screen(&env.app, &run, &on).await, "matching observation emits no change");
                assert!(super::fast_follow_edges().lock().unwrap().map[&run].attempted);
            }
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(fast_of(&env, &run).await, Some(0));
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 0, "the second TUI off must follow after UI live apply");
            assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        }
    }

    #[tokio::test]
    async fn a_failed_or_cancelled_runtime_write_never_publishes_or_replays_child_settings() {
        for cancel in [false, true] {
            let env = crate::testing::env().await;
            let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            let mut events = env.app.subscribe();
            if cancel {
                let entered = std::sync::Arc::new(tokio::sync::Notify::new());
                let ready = entered.clone();
                crate::race_point::arm("codex_follow_before_runtime_write", &run, move || async move {
                    ready.notify_one();
                    std::future::pending::<()>().await;
                });
                let app = env.app.clone();
                let run_id = run.clone();
                let task = tokio::spawn(async move { correct_runtime_from_screen(&app, &run_id, ASTRA_HIGH).await });
                entered.notified().await;
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                sqlx::query("CREATE TRIGGER reject_runtime_follow BEFORE UPDATE OF runtime_fast ON runs BEGIN SELECT RAISE(FAIL, 'test runtime write failure'); END").execute(&env.app.db).await.unwrap();
                assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
                sqlx::query("DROP TRIGGER reject_runtime_follow").execute(&env.app.db).await.unwrap();
            }
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1, "child UPDATE must roll back with runtime");
            assert_eq!(fast_of(&env, &run).await, Some(1));
            assert!(events.try_recv().is_err(), "no event before atomic commit");
            // 模擬使用者隨後另存開啟 fast；同畫面重試只能補 runtime，不能重放設定寫入。
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1, "preserve the newer UI intent");
            assert_eq!(fast_of(&env, &run).await, Some(0));
            assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            // 新 edge 可以再跟隨：先開再關，不能被前次失敗的標記永久擋住。
            let on = ASTRA_HIGH.replace("astra high", "astra high fast");
            assert!(correct_runtime_from_screen(&env.app, &run, &on).await);
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 0);
            sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            assert!(super::sweep_finished_fast_edges(super::fast_follow_edges(), &env.app.db).await >= 1);
            assert!(!super::fast_follow_edges().lock().unwrap().map.contains_key(&run));
        }
    }

    /// 子 agent（`managed_by=child`）與 run 的強度設定；`prev` 是 run 上一輪記的 `runtime_effort`（`None`＝未知）。
    async fn child_with_effort(env: &crate::testing::Env, managed_by: &str, effort: Option<&str>, prev: Option<&str>) -> (String, String) {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "payload").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE bots SET managed_by=?, model='gpt-6-astra', effort=? WHERE id=?")
            .bind(managed_by)
            .bind(effort)
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET runtime_model='gpt-6-astra', runtime_effort=?, runtime_fast=0 WHERE id=?")
            .bind(prev)
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();
        (bot.id, run)
    }

    async fn bot_effort(env: &crate::testing::Env, bot: &str) -> Option<String> {
        sqlx::query_scalar("SELECT effort FROM bots WHERE id=?").bind(bot).fetch_one(&env.app.db).await.unwrap()
    }

    async fn run_effort(env: &crate::testing::Env, run: &str) -> Option<String> {
        db::run(&env.app.db, run).await.unwrap().unwrap().runtime_effort
    }

    const ASTRA_DEFAULT: &str = "› Ask Codex to do anything\n\n  GPT-6-Astra default · ~/project/agents-manager · Context 3% used\n  ? for shortcuts\n";
    const ASTRA_HIGH: &str = "› Ask Codex to do anything\n\n  gpt-6-astra high · ~/project/agents-manager · Context 3% used\n  ? for shortcuts\n";

    /// 未知→明確 default：runtime 記成 `default`（不是 NULL），但子 agent 設定保留 high，web 會照常警示「實際 CLI 預設、設定 High」。
    #[tokio::test]
    async fn unknown_to_default_records_the_cli_default_and_keeps_the_child_effort() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), None).await;
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("default"));
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"), "未知不等於已套用 default");
        // 再讀一次同一畫面：已經收斂，不改動。
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
    }

    /// 已知具體值 high → 明確 default：子 agent 設定清成 NULL（CLI 預設），runtime 記 `default`。
    #[tokio::test]
    async fn a_known_high_switched_to_the_cli_default_clears_the_child_effort() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
        assert_eq!(bot_effort(&env, &bot).await, None);
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("default"));
    }

    /// default → default：已經是預設，不清也不寫設定（不會因為重複讀到 default 就把設定抹掉）。
    #[tokio::test]
    async fn a_default_that_stays_default_does_not_touch_the_child_effort() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("default")).await;
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"));
    }

    /// 一般 bot（`managed_by` 不是 child）：不跟隨，設定與 runtime 各自照規則走。
    #[tokio::test]
    async fn a_user_bot_keeps_its_effort_when_the_tui_goes_to_default() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "user", Some("high"), Some("high")).await;
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"));
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("default"));
    }

    /// 讀不到強度的畫面（沒有 footer、或冷啟動兩段 footer）：runtime 與子 agent 設定都不清。
    #[tokio::test]
    async fn a_screen_without_a_readable_footer_clears_nothing() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        for screen in ["› Ask Codex to do anything\n", "  GPT-6-Astra default · ~/project/agents-manager\n"] {
            assert!(!correct_runtime_from_screen(&env.app, &run, screen).await, "{screen:?}");
        }
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"));
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("high"));
    }

    /// 網頁另設 low、還沒套用：畫面的 runtime 沒變（仍是 high），設定不能被蓋回去。
    #[tokio::test]
    async fn a_pending_web_change_is_kept_while_the_runtime_is_unchanged() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("low"), Some("high")).await;
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("low"));
    }

    /// 真的差異照列：畫面換成具體值，子 agent 設定跟著改（既有行為不變）。
    #[tokio::test]
    async fn a_real_effort_switch_still_follows_into_the_child_settings() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        let low = "› Ask Codex to do anything\n\n  gpt-6-astra low · ~/project/agents-manager · Context 3% used\n  ? for shortcuts\n";
        assert!(correct_runtime_from_screen(&env.app, &run, low).await);
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("low"));
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("low"));
    }

    /// 有效的 footer 但沒有強度字、也沒有 default（未知）：已知的 high 保留，model／fast 仍照畫面校正。
    #[tokio::test]
    async fn a_valid_footer_without_an_effort_word_keeps_the_known_effort_and_still_corrects_model_and_fast() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        let footer = "› Ask Codex to do anything\n\n  gpt-6-luna fast · ~/project/agents-manager · Context 3% used\n  ? for shortcuts\n";
        assert!(correct_runtime_from_screen(&env.app, &run, footer).await);
        let row = db::run(&env.app.db, &run).await.unwrap().unwrap();
        assert_eq!(row.runtime_model.as_deref(), Some("gpt-6-luna"), "model 照畫面校正");
        assert_eq!(row.runtime_fast, Some(1), "fast 照畫面校正");
        assert_eq!(row.runtime_effort.as_deref(), Some("high"), "未知的強度不清成 NULL");
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"), "未知的強度不跟隨到子 agent 設定");
        // 收斂：再讀同一畫面不改動。
        assert!(!correct_runtime_from_screen(&env.app, &run, footer).await);
    }

    /// codex 子 agent l8 在 TUI 裡手動打 /fast 開了 fast，狀態列顯示主題段（無 Context）。
    #[test]
    fn status_line_with_session_topic_or_truncated_topic_parses_runtime() {
        // codex 子 agent l8 的真實畫面
        let screen = "• Compacting context (7s • esc to interrupt)\n  └ Making room to continue.\n\n› Ask Codex to do anything\n\n  GPT-6-Luna max fast · ~/project/agents-manager · 修正主力 cache 冷 context 規則\n  ? for shortcuts                                                                                                                                                                          ⚠ 2 warnings · f2 to view\n";
        let rt = parse_status_line(screen).expect("讀得到主題段的狀態列");
        assert_eq!(
            rt,
            CodexRuntime {
                model: "gpt-6-luna".into(),
                effort: Some("max".into()),
                fast: true,
            }
        );

        // 截斷主題（…）
        let truncated = "› Ask Codex to do anything\n\n  GPT-6-Luna max fast · ~/project/agents-manager · …\n";
        let rt_trunc = parse_status_line(truncated).expect("讀得到 … 截斷主題的狀態列");
        assert_eq!(
            rt_trunc,
            CodexRuntime {
                model: "gpt-6-luna".into(),
                effort: Some("max".into()),
                fast: true,
            }
        );

        // 手動打 /fast 關閉 fast
        let fast_off = "› Ask Codex to do anything\n\n  GPT-6-Luna max · ~/project/agents-manager · 修正主力 cache 冷 context 規則\n";
        let rt_off = parse_status_line(fast_off).expect("讀得到關閉 fast 的狀態列");
        assert_eq!(
            rt_off,
            CodexRuntime {
                model: "gpt-6-luna".into(),
                effort: Some("max".into()),
                fast: false,
            }
        );

        // 手動打 /model 切換模型與推理強度
        let model_switched = "› Ask Codex to do anything\n\n  gpt-5.6-sol medium fast · ~/project/agents-manager · 修正主力 cache 冷 context 規則\n";
        let rt_switch = parse_status_line(model_switched).expect("讀得到換模型後的狀態列");
        assert_eq!(
            rt_switch,
            CodexRuntime {
                model: "gpt-5.6-sol".into(),
                effort: Some("medium".into()),
                fast: true,
            }
        );

        // 窄 pane 折行主題
        let wrapped_topic = "  GPT-6-Luna max fast · ~/project/agents-manager ·\n  修正主力 cache 冷 context 規則\n";
        let rt_wrapped = parse_status_line(wrapped_topic).expect("折行的主題段仍能解析");
        assert_eq!(
            rt_wrapped,
            CodexRuntime {
                model: "gpt-6-luna".into(),
                effort: Some("max".into()),
                fast: true,
            }
        );
    }

    /// 收編時 runs.runtime_* 全為 NULL，狀態列帶主題段能正確填入 runtime_model、runtime_effort 與 runtime_fast。
    #[tokio::test]
    async fn a_status_line_with_topic_corrects_null_runtime_and_enables_fast() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "payload").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        // 初始狀態如 adopted child：runtime 皆為 NULL
        sqlx::query("UPDATE runs SET runtime_model=NULL, runtime_effort=NULL, runtime_fast=NULL WHERE id=?")
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();

        let screen = "› Ask Codex to do anything\n\n  GPT-6-Luna max fast · ~/project/agents-manager · 修正主力 cache 冷 context 規則\n  ? for shortcuts\n";
        assert!(correct_runtime_from_screen(&env.app, &run, screen).await);

        let row = db::run(&env.app.db, &run).await.unwrap().unwrap();
        assert_eq!(row.runtime_model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(row.runtime_effort.as_deref(), Some("max"));
        assert_eq!(row.runtime_fast, Some(1));
    }

    /// 讀不到狀態列（選單開著、畫面被清）不能當成 fast=false。
    #[tokio::test]
    async fn an_unreadable_screen_leaves_the_runtime_alone() {
        let env = crate::testing::env().await;
        let run = run_with_runtime(&env, 1).await;
        for screen in ["", "› Ask Codex to do anything\n", MODEL_MENU] {
            assert!(!correct_runtime_from_screen(&env.app, &run, screen).await, "{screen:?}");
        }
        assert_eq!(fast_of(&env, &run).await, Some(1));
    }

    /// Real pane text, codex 0.153.4 (2026-09-09).
    const STATUS: &str = "\
╭─────────────────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.153.4)                              │
│                                                         │
│ model:       gpt-5.6-luna max   fast   /model to change │
│ directory:   /tmp                                       │
╰─────────────────────────────────────────────────────────╯

› Ask Codex to do anything

  gpt-5.6-sol high fast · /tmp · Context 0% used · 5h 82% left · weekly 73% left
";

    const MODEL_MENU: &str = "\
  Select Model and Effort
  Access legacy models by running codex -m <model_name> or in your config.toml

  1. gpt-6-astra (default)   Our most capable model for complex, demanding work.
  2. gpt-5.6-sol             Reliable agentic workhorse for everyday tasks.
  3. gpt-5.6-terra           Balanced agentic coding model for everyday work.
› 4. gpt-5.6-luna (current)  Fast and affordable agentic coding model.
  5. gpt-5.5                 Proven previous-generation model for coding and general work.

  Press enter to confirm or esc to go back
";

    const EFFORT_MENU: &str = "\
  Select Reasoning Level for gpt-5.6-sol

› 1. Low (default)    Fast responses with lighter reasoning
  2. Medium           Balances speed and reasoning depth for everyday tasks
  3. High             Greater reasoning depth for complex problems
  4. Extra high       Extra high reasoning depth for complex problems
  5. More reasoning…  Max and Ultra consume usage limits faster

  Press enter to confirm or esc to go back
";

    #[test]
    fn the_status_line_is_what_codex_is_really_on() {
        let rt = parse_status_line(STATUS).unwrap();
        // The banner above says `gpt-5.6-luna max fast`; the live line below it wins.
        assert_eq!(rt, CodexRuntime { model: "gpt-5.6-sol".into(), effort: Some("high".into()), fast: true });
        let off = parse_status_line("  gpt-5.6-sol high · /tmp · Context 0% used · 5h 82% left\n").unwrap();
        assert!(!off.fast);
        // A model on the CLI default effort prints no level at all.
        let bare = parse_status_line("  gpt-5.5 · /tmp · Context 3% used\n").unwrap();
        assert_eq!(bare.effort, None);
        assert!(parse_status_line("› Ask Codex to do anything\n").is_none());
    }

    #[test]
    fn picker_rows_are_matched_by_text_not_by_position() {
        assert_eq!(picker_number(MODEL_MENU, "gpt-5.6-sol"), Some(2));
        // No model of its own → stay on the row codex marks as current.
        assert_eq!(picker_number(MODEL_MENU, "(current)"), Some(4));
        assert_eq!(picker_number(EFFORT_MENU, "Extra high"), Some(4));
        assert_eq!(picker_number(EFFORT_MENU, "(default)"), Some(1));
        assert_eq!(picker_number(EFFORT_MENU, "More reasoning…"), Some(5));
        // `Max` / `Ultra` only appear in row 5's *description*; matching that would press the
        // submenu row as if it were the level itself.
        assert_eq!(picker_number(EFFORT_MENU, "Max"), None, "max / ultra are one menu deeper");
        assert_eq!(picker_number(EFFORT_MENU, "Ultra"), None);
        assert_eq!(picker_number(MODEL_MENU, "gpt-4"), None);
    }

    /// issue #636: a model whose name merely *starts with* the wanted one must not win just
    /// because it is listed first — codex would really switch to the preview / mini model.
    #[test]
    fn picker_rows_match_the_whole_name_not_a_prefix() {
        const PREFIXED: &str = "\
  Select Model and Effort

› 1. gpt-6-sol-preview (current)  Preview build.
  2. gpt-6-sol                    Reliable agentic workhorse.
  3. gpt-5.5-mini                 Cheaper.
  4. gpt-5.5                      Proven previous-generation model.

  Press enter to confirm or esc to go back
";
        assert_eq!(picker_number(PREFIXED, "gpt-6-sol"), Some(2));
        assert_eq!(picker_number(PREFIXED, "gpt-5.5"), Some(4));
        assert_eq!(picker_number(PREFIXED, "gpt-6-sol-preview"), Some(1));
        assert_eq!(picker_number(PREFIXED, "(current)"), Some(1));
        assert_eq!(picker_number(PREFIXED, "gpt-6"), None, "a bare prefix names no row");
        assert_eq!(picker_number(PREFIXED, "sol"), None);
        const AMBIGUOUS: &str = "\
  Select Model and Effort

  1. gpt-5.5     First duplicate
  2. gpt-5.5     Second duplicate

  Press enter to confirm or esc to go back
";
        assert_eq!(picker_number(AMBIGUOUS, "gpt-5.5"), None, "ambiguous rows must not be selected");
        // Same rule for effort labels: `High` must not pick `Extra high` or `Higher`.
        const LEVELS: &str = "\
  Select Reasoning Level for gpt-6-sol

  1. Higher (default)  Hypothetical level listed first
  2. High              Greater reasoning depth
  3. Extra high        Extra high reasoning depth

  Press enter to confirm or esc to go back
";
        assert_eq!(picker_number(LEVELS, "High"), Some(2));
        assert_eq!(picker_number(LEVELS, "Extra high"), Some(3));
        assert_eq!(picker_number(LEVELS, "(default)"), Some(1));
        assert_eq!(picker_number(LEVELS, "Extra"), None);
    }

    #[test]
    fn codex_0157_rate_limit_switch_prompt_is_a_choice_screen_not_a_quota_hit() {
        let screen = include_str!("lifecycle/fixtures/codex-0.157-rate-limit-switch.txt");
        assert!(rate_limit_switch_prompt_open(screen));
        assert!(picker_open(screen));
        assert!(parse_status_line(screen).is_none());
        assert!(parse_status_quota(screen).is_none());

        let quoted = format!("⏺ Codex suggested this option:\n{screen}\n{COMPOSER}");
        assert!(!rate_limit_switch_prompt_open(&quoted), "quoted text outside the popup is not a live selector");
    }

    #[test]
    fn a_numeric_choice_requires_the_target_row_in_a_live_picker() {
        assert!(picker_choice_matches(MODEL_MENU, "gpt-5.6-sol", 2));
        assert!(!picker_choice_matches(MODEL_MENU, "gpt-5.6-sol", 3), "a different row is not the target");
        assert!(!picker_choice_matches(COMPOSER, "gpt-5.6-sol", 2), "a stale selection cannot target the composer");
    }

    #[tokio::test]
    async fn missing_or_ambiguous_picker_rows_do_not_authorize_number_entry() {
        let env = crate::testing::env().await;
        const SCREEN: &str = "\
  Select Model and Effort

  1. gpt-5.5     First duplicate
  2. gpt-5.5     Second duplicate

  Press enter to confirm or esc to go back
";
        for (pane, needle) in [
            ("pane-codex-ambiguous-picker", "gpt-5.5"),
            ("pane-codex-missing-picker", "gpt-6-astra"),
        ] {
            env.herdr.set_screen(pane, SCREEN);
            assert_eq!(press_number(&env.app.herdr, pane, 1, needle).await, Ok(false), "{needle}");
        }
        assert!(env.herdr.calls_to("pane.send_text").is_empty(), "an ambiguous row must not receive a numeric choice");
        assert!(
            env.herdr.calls_to("pane.send_keys").iter().all(|call| call["keys"] == serde_json::json!(["Escape"])),
            "the picker may only be escaped, never assigned an ambiguous row number"
        );
    }

    /// 帶樣式的真空輸入框（灰色佔位字）：閒著套用前會先看輸入框有沒有草稿，純文字的 [`COMPOSER`] 分不出佔位字。
    const EMPTY_BOX_ANSI: &str = include_str!("lifecycle/fixtures/codex-0.155-idle.ansi");

    /// Real composer, codex 0.154.0 (2026-09-10) — nothing in the way.
    const COMPOSER: &str = "\
─ Worked for 1m 17s ────────────────────────────────────

• Model changed to gpt-6-astra high

› Ask Codex to do anything

  gpt-6-astra high · ~/project/agents-manager · Context 44% used · 5h 10% left
";

    /// 2026-09-10 a user message sent into a picker vanished and switched the model.
    #[test]
    fn a_pane_showing_a_picker_is_not_ready_for_text() {
        assert!(picker_open(MODEL_MENU));
        assert!(picker_open(EFFORT_MENU));
        // The footer alone is enough: a short pane can scroll the heading away.
        assert!(picker_open("  Press enter to confirm or esc to go back\n"));
        assert!(!picker_open(COMPOSER));
        assert!(!picker_open(STATUS));
        assert!(!picker_open(""));
        // #1766：對話裡印出來的選單字樣（輸入框還在它下面）不是選單。
        let quoted = format!("• Ran herdr agent read child\n  └\n      Select Reasoning Level for GPT-6-Luna\n      Press enter to confirm or esc to go back\n{COMPOSER}");
        assert!(!picker_open(&quoted), "輸入框在選單字樣下面：只是對話內容");
        assert!(picker_open(&format!("{COMPOSER}\n{EFFORT_MENU}")), "選單真的開在最底下");
        // #1766：對話紀錄瀏覽要 Esc 回輸入框；只認最後一行的底列，對話裡提到這幾個字不算。
        assert!(transcript_browser_open("• Ran herdr agent list\n\nBrowsing transcript · ↑↓/jk scroll · ←→/hl prompts · ^t details · ↵ rewind · esc back\n"));
        assert!(!transcript_browser_open("Browsing transcript · esc back 是 codex 的瀏覽模式\n› Ask Codex to do anything\n"));
        assert!(!transcript_browser_open(COMPOSER));
    }

    #[test]
    fn effort_labels_match_the_menu_codex_draws() {
        assert_eq!(effort_menu_label("xhigh"), "Extra high");
        assert_eq!(effort_menu_label("high"), "High");
        assert!(is_nested_effort("max") && is_nested_effort("ultra"));
        assert!(!is_nested_effort("xhigh"));
    }

    /// codex 0.154.0 兩層選單原文（2026-09-13 實地抓）。導航靠這些字；codex 一改字，改 effort
    /// 就會靜靜退回重啟（2026-09-13 使用者遇過），釘住讓測試先講。
    const MODEL_MENU_0154: &str = "\
  Select Model and Effort
  Access legacy models by running codex -m <model_name> or in your config.toml

› 1. gpt-6-astra (current)  Our most capable model for complex, demanding work.
  2. gpt-5.6-sol            Reliable agentic workhorse for everyday tasks.
  3. gpt-5.6-terra          Balanced agentic coding model for everyday work.
  4. gpt-5.6-luna           Fast and affordable agentic coding model.
  5. gpt-5.5                Proven previous-generation model for coding and general work.

  Press enter to confirm or esc to go back
";

    const EFFORT_MENU_0154: &str = "\
  Select Reasoning Level for gpt-6-astra

  1. Low (default)    Fast responses with lighter reasoning
  2. Medium           Balances speed and reasoning depth for everyday tasks
› 3. High (current)   Greater reasoning depth for complex problems
  4. Extra high       Extra high reasoning depth for complex problems
  5. More reasoning…  Max and Ultra consume usage limits faster

  Press enter to confirm or esc to go back
";

    #[test]
    fn the_0_154_menus_still_read_the_way_the_driver_expects() {
        assert!(MODEL_MENU_0154.contains("Select Model"), "第一層的判斷字");
        assert!(EFFORT_MENU_0154.contains("Select Reasoning Level"), "第二層的判斷字");
        assert_eq!(picker_number(MODEL_MENU_0154, "gpt-6-astra"), Some(1));
        assert_eq!(picker_number(MODEL_MENU_0154, "(current)"), Some(1));
        assert_eq!(picker_number(MODEL_MENU_0154, "gpt-5.6-luna"), Some(4));
        assert_eq!(picker_number(EFFORT_MENU_0154, effort_menu_label("low")), Some(1));
        assert_eq!(picker_number(EFFORT_MENU_0154, effort_menu_label("medium")), Some(2));
        assert_eq!(picker_number(EFFORT_MENU_0154, effort_menu_label("high")), Some(3));
        assert_eq!(picker_number(EFFORT_MENU_0154, effort_menu_label("xhigh")), Some(4));
        assert_eq!(picker_number(EFFORT_MENU_0154, "More reasoning…"), Some(5));
        assert!(picker_number(EFFORT_MENU_0154, effort_menu_label("max")).is_none(), "max 藏在下一層");
        // `(default)` 那一列不能被 `Extra high` 的描述文字搶走（描述在兩格空白之後）。
        assert_eq!(picker_number(EFFORT_MENU_0154, "(default)"), Some(1));
    }

    /// codex 0.162.1（2026-10-13 在 w0:pAJ 實測，#1681）：選單的模型印顯示名（`GPT-6.1-Sol`），設定裡是 id（`gpt-6.1-sol`）；
    /// 以前整串比對、大小寫不同就找不到那一列，live 套用一律 `picker_failed` 退回重啟。
    #[test]
    fn the_0_162_menus_print_display_names_that_match_ids_without_case() {
        let menu = "\
  Select Model and Effort

› 1. GPT-6.1-Sol (current)  Latest workhorse model for coding and everyday work.
  2. GPT-6-Luna             Fast and affordable model for easier tasks.

  enter select · esc back
";
        assert!(picker_open(menu));
        assert_eq!(picker_number(menu, "gpt-6-luna"), Some(2));
        assert_eq!(picker_number(menu, "gpt-6.1-sol"), Some(1));
        assert_eq!(picker_number(menu, "(current)"), Some(1));
        assert_eq!(picker_number(menu, "gpt-6"), None, "不是前綴比對");
    }

    /// 2026-09-13 使用者截圖那一行（行尾被截斷）。
    #[test]
    fn the_status_line_carries_the_accounts_remaining_quota() {
        let q = parse_status_quota(
            "  gpt-6-astra high · ~/project/agents-manager · Context 28% used · 5h 90% left · weekly 48% …\n",
        )
        .unwrap();
        assert_eq!(q.five_hour_left, Some(90.0));
        assert_eq!(q.weekly_left, Some(48.0), "`left` 被截掉也要讀得到");

        // 省略號黏在 `%` 後（2026-09-13 實機），曾讓 header 少了 7d。
        let tight = parse_status_quota(
            "  gpt-6-astra medium · ~/project/agents-manager · Context 9% used · 5h 36% left · weekly 24%…",
        )
        .unwrap();
        assert_eq!((tight.five_hour_left, tight.weekly_left), (Some(36.0), Some(24.0)));
        let odd = parse_status_quota("m x · /tmp · Context 1% used · 5h 7.5% left, weekly 12%.").unwrap();
        assert_eq!((odd.five_hour_left, odd.weekly_left), (Some(7.5), Some(12.0)));

        let full = parse_status_quota("gpt-5.6-sol high fast · /tmp · Context 0% used · 5h 82% left · weekly 73% left")
            .unwrap();
        assert_eq!((full.five_hour_left, full.weekly_left), (Some(82.0), Some(73.0)));

        // 只有 5h 的那種（週窗還沒開始算）。
        let one = parse_status_quota("gpt-6-astra high · /tmp · Context 44% used · 5h 10% left").unwrap();
        assert_eq!((one.five_hour_left, one.weekly_left), (Some(10.0), None));

        assert!(parse_status_quota("› Ask Codex to do anything\n1 background terminal running\n").is_none());
        // 空讀數不能蓋掉 app-server 的。
        assert!(parse_status_quota("gpt-6-astra high · /tmp · Context 44% used").is_none());
    }

    /// #712：0.157 的狀態列印顯示名（`GPT-6-Luna`）。runtime 記成 id 的寫法，設定才比得起來。
    #[test]
    fn a_display_cased_model_is_read_as_the_id() {
        let rt = parse_status_line("  GPT-6-Luna max fast · /tmp · Context 43% used · 5h 12% left\n").unwrap();
        assert_eq!(rt, CodexRuntime { model: "gpt-6-luna".into(), effort: Some("max".into()), fast: true });
    }

    fn bot(model: Option<&str>, effort: Option<&str>, fast: bool) -> crate::db::Bot {
        crate::db::Bot {
            id: "b".into(),
            project_id: "p".into(),
            name: "b".into(),
            kind: "codex".into(),
            model: model.map(Into::into),
            effort: effort.map(Into::into),
            fast: i64::from(fast),
            persona: None,
            args_json: "[]".into(),
            autostart: 0,
            inject_hooks: 1,
            auto_approve: 1,
            identity: None,
            env_json: "{}".into(),
            managed_by: "user".into(),
            cwd: None,
            herdr_session: None,
            parent_bot_id: None,
            is_primary: 0,
            primary_position: 0,
            hook_token: "t".into(),
            deleted_at: None,
            created_at: crate::db::now(),
        }
    }

    /// 讀回只在真的不一樣時失敗：模型大小寫不同不算（以前只切 fast 也因此退回重啟）。
    #[test]
    fn the_readback_ignores_model_case_but_not_a_different_model() {
        let seen = |model: &str, fast: bool| CodexRuntime { model: model.into(), effort: Some("max".into()), fast };
        let want = bot(Some("gpt-6-luna"), Some("max"), true);
        assert_eq!(verify(&seen("GPT-6-Luna", true), &want, &["fast"]), Ok(()));
        assert_eq!(verify(&seen("gpt-6-sol", true), &want, &["fast"]), Err("readback_model_mismatch"));
        assert_eq!(verify(&seen("gpt-6-luna", false), &want, &["fast"]), Err("readback_fast_mismatch"));
        assert_eq!(verify(&seen("gpt-6-luna", false), &want, &["model"]), Ok(()), "沒改 fast 就不比 fast");
        let high = CodexRuntime { model: "gpt-6-luna".into(), effort: Some("high".into()), fast: true };
        assert_eq!(verify(&high, &want, &["fast"]), Err("readback_effort_mismatch"));
    }

    /// 回合中切 fast 之前的畫面檢查（真畫面 fixtures）：輸入框空的才打，有草稿或選單開著就不碰。
    #[test]
    fn a_turn_is_only_typed_into_with_an_empty_composer_and_no_picker() {
        let working = include_str!("lifecycle/fixtures/codex-0.155-working.ansi");
        let draft = include_str!("lifecycle/fixtures/codex-0.155-draft.ansi");
        let migration = include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt");
        assert!(!busy_fast_blocked(working, working), "回合中、輸入框只有灰色佔位字");
        assert!(busy_fast_blocked(draft, draft), "使用者的草稿：/fast 會接在後面一起送出");
        assert!(busy_fast_blocked(migration, working), "選擇畫面開著");
        assert!(busy_fast_blocked(working, "gpt-6-luna max · /tmp · Context 1% used\n"), "讀不出輸入框：不打");
    }
    /// #1671 真實快照（m4p、gpt-6.1-sol、213×55）：沒有 `? for shortcuts` 行，warning 接在模型列右側。
    fn fixture_1671() -> String {
        let v: serde_json::Value = serde_json::from_str(include_str!("lifecycle/fixtures/codex-1671-model-row-with-warning.json")).unwrap();
        v["text"].as_str().unwrap().to_string()
    }

    const MODEL_ROW_1671: &str = "GPT-6.1-Sol high · ~/project/hermes-agents/projects/rt · Context 17% used · weekly 100% left";

    #[test]
    fn the_footer_model_row_loses_its_right_aligned_warning_and_keeps_the_context_percent() {
        use am_base::codex_status::{parse_footer, CodexFooter};
        assert_eq!(parse_footer(&fixture_1671()), Some(CodexFooter { line: MODEL_ROW_1671.into(), context_used_pct: Some(17.0) }));
        // 模型列在 shortcuts 行上面、只有主題段（沒有 Context）：照樣是那一列，沒有百分比。
        let topic = "• 完成\n\n› Ask Codex to do anything\n\n  GPT-6-Astra default · ~/project/agents-manager · 解決 issue #1664\n  ? for shortcuts      ⚠ 1 warning · f2 to view\n";
        assert_eq!(
            parse_footer(topic),
            Some(CodexFooter { line: "GPT-6-Astra default · ~/project/agents-manager · 解決 issue #1664".into(), context_used_pct: None })
        );
        // 沒有 warning、帶 5h 段。
        let five = "› Ask Codex to do anything\n\n  GPT-6.1-Sol high · /tmp · Context 5% used · 5h 96% left · weekly 48% left\n";
        assert_eq!(parse_footer(five).unwrap().line, "GPT-6.1-Sol high · /tmp · Context 5% used · 5h 96% left · weekly 48% left");
    }

    #[test]
    fn a_quoted_model_row_above_the_viewport_tail_is_not_the_footer() {
        let screen = format!("  {MODEL_ROW_1671}\n{}", "• 回覆\n".repeat(10));
        assert_eq!(am_base::codex_status::parse_footer(&screen), None);
        assert_eq!(am_base::codex_status::parse_footer("› 草稿\n"), None);
    }

    #[test]
    fn the_real_1671_snapshot_still_reads_runtime_and_the_weekly_window() {
        let rt = parse_status_line(&fixture_1671()).expect("底部那列是狀態列");
        assert_eq!((rt.model.as_str(), rt.effort.as_deref(), rt.fast), ("gpt-6.1-sol", Some("high"), false));
        let q = parse_status_quota(&fixture_1671()).unwrap();
        assert_eq!((q.five_hour_left, q.weekly_left), (None, Some(100.0)));
    }

    #[tokio::test]
    async fn the_footer_is_written_to_the_run_only_when_it_changes_and_never_cleared_by_an_unreadable_screen() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "footer").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        let read = |env: &crate::testing::Env, run: &str| {
            let db = env.app.db.clone();
            let run = run.to_string();
            async move {
                let r = db::run(&db, &run).await.unwrap().unwrap();
                (r.status_line, r.status_json)
            }
        };
        assert!(record_footer_from_screen(&env.app, &run, &fixture_1671()).await);
        let (line, json) = read(&env, &run).await;
        assert_eq!(line.as_deref(), Some(MODEL_ROW_1671));
        let json: serde_json::Value = serde_json::from_str(&json.unwrap()).unwrap();
        assert_eq!(json["context_window"]["used_percentage"], 17.0);

        assert!(!record_footer_from_screen(&env.app, &run, &fixture_1671()).await, "同一張畫面不重寫");
        assert!(!record_footer_from_screen(&env.app, &run, "• Compacting context (1s • esc to interrupt)\n").await, "讀不到不清掉舊值");
        assert_eq!(read(&env, &run).await.0.as_deref(), Some(MODEL_ROW_1671));

        let next = fixture_1671().replace("Context 17% used", "Context 18% used");
        assert!(record_footer_from_screen(&env.app, &run, &next).await);
        assert!(read(&env, &run).await.0.unwrap().contains("Context 18% used"));
    }

    /// #1924：run 啟動後從開機畫面記版本，不等巡邏。
    async fn codex_boot_run(name: &str) -> (crate::testing::Env, String, String) {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET kind = 'codex' WHERE id = ?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        (env, bot.id, run)
    }

    const BOOT_164: &str = include_str!("lifecycle/fixtures/codex-0.164-startup.txt");

    #[tokio::test]
    async fn the_boot_banner_is_recorded_even_when_it_only_shows_on_a_later_read() {
        let (env, bot_id, run) = codex_boot_run("boot-late").await;
        let reads = std::cell::Cell::new(0);
        let read = || {
            reads.set(reads.get() + 1);
            let n = reads.get();
            async move { (n >= 2).then(|| BOOT_164.to_string()) }
        };
        remember_boot_version(&env.app.db, None, "codex", &bot_id, &run, read, Duration::from_millis(1)).await;
        assert_eq!(reads.get(), 2, "第一次讀還沒畫出來，第二次讀到就停");
        assert_eq!(crate::codex_update::running_version_of(&run).as_deref(), Some("0.164.0"));
    }

    #[tokio::test]
    async fn an_unreadable_boot_screen_leaves_the_version_unknown() {
        let (env, bot_id, run) = codex_boot_run("boot-blank").await;
        let reads = std::cell::Cell::new(0);
        let no_banner = || {
            reads.set(reads.get() + 1);
            async { Some("› Ask Codex to do anything\n".to_string()) }
        };
        remember_boot_version(&env.app.db, None, "codex", &bot_id, &run, no_banner, Duration::from_millis(1)).await;
        assert_eq!(reads.get(), 3, "第一次讀加兩次重試，共三次讀取，全部讀不到就停");
        assert_eq!(crate::codex_update::running_version_of(&run), None, "解析不到維持未知，照舊退回選單");

        let pane_failed = || async { None::<String> };
        remember_boot_version(&env.app.db, None, "codex", &bot_id, &run, pane_failed, Duration::from_millis(1)).await;
        assert_eq!(crate::codex_update::running_version_of(&run), None, "pane 讀失敗也維持未知");
    }

    #[tokio::test]
    async fn only_codex_reads_the_boot_screen() {
        let (env, bot_id, run) = codex_boot_run("boot-claude").await;
        let reads = std::cell::Cell::new(0);
        let read = || {
            reads.set(reads.get() + 1);
            async { Some(BOOT_164.to_string()) }
        };
        remember_boot_version(&env.app.db, None, "claude", &bot_id, &run, read, Duration::from_millis(1)).await;
        assert_eq!(reads.get(), 0, "非 codex 一次都不讀畫面");
        assert_eq!(crate::codex_update::running_version_of(&run), None);
    }

    #[tokio::test]
    async fn a_boot_banner_is_not_recorded_once_the_run_is_no_longer_the_current_one() {
        let (env, bot_id, run) = codex_boot_run("boot-stale").await;
        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&run).execute(&env.app.db).await.unwrap();
        let read = || async { Some(BOOT_164.to_string()) };
        remember_boot_version(&env.app.db, None, "codex", &bot_id, &run, read, Duration::from_millis(1)).await;
        assert_eq!(crate::codex_update::running_version_of(&run), None, "已經不是當前的 run 不記");
    }
}
