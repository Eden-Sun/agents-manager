//! Changing a **running** codex's model / effort / fast tier (SPEC §4.4a).
//!
//! Unlike claude/grok, codex 0.153.4 (verified 2026-09-09) has no one-line form:
//! * `/model` takes **no arguments** (`/model x high` becomes a prompt); bare `/model` opens two
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
//! the pane back and matches on text. codex saves the choice as the account default in
//! `~/.codex/config.toml` (CLI behaviour, not ours).

use crate::db;
use crate::herdr::HerdrClient;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRuntime {
    pub model: String,
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

/// `<model> [<effort>] [fast] · <cwd> · Context …` — the only place codex states all three, so
/// it is the read-back proving a live change landed (SPEC §4.4a).
pub fn parse_status_line(screen: &str) -> Option<CodexRuntime> {
    // Last match wins: the startup banner has the same shape above the live line.
    let mut out = None;
    for raw in screen.lines() {
        let line = raw.trim();
        let Some((head, _)) = line.split_once('·') else { continue };
        if !line.contains("Context") {
            continue;
        }
        let mut parts = head.split_whitespace();
        // 這一行的開頭沒有東西（`· Context …`）只是別的行：略過，不能整個函式回 None（下面才有真的狀態列）。
        let Some(model) = parts.next().map(str::to_string) else { continue };
        if !model.contains('-') {
            continue;
        }
        let rest: Vec<&str> = parts.collect();
        let fast = rest.iter().any(|w| *w == "fast");
        let effort = rest
            .iter()
            .find(|w| crate::config::efforts_for_kind("codex").contains(&w.to_ascii_lowercase().as_str()))
            .map(|w| w.to_ascii_lowercase());
        out = Some(CodexRuntime { model, effort, fast });
    }
    out
}

/// 以畫面上的狀態列校正 `runs.runtime_*`（SPEC §4.4a）。回傳 `true` = 有改動。
/// * 讀不到狀態列（選單開著、畫面被清、CLI 剛啟動）什麼都不動：讀不到不是 fast=false。
/// * 讀得到就一律以它為準：狀態列有 `fast` 字樣 = 開，**整行讀得到卻沒有 = 關**（tier 關掉時 codex 省略那個字）。
///   使用者在 TUI 手打 `/fast`、`/model`，或當場套用中途失敗，都會讓啟動時記下的值過期。
pub async fn correct_runtime_from_screen(app: &crate::state::App, run_id: &str, screen: &str) -> bool {
    let Some(seen) = parse_status_line(screen) else { return false };
    let Ok(Some(run)) = db::run(&app.db, run_id).await else { return false };
    let same = run.runtime_model.as_deref() == Some(seen.model.as_str())
        && run.runtime_effort == seen.effort
        && run.runtime_fast == Some(i64::from(seen.fast));
    if same {
        return false;
    }
    let wrote = sqlx::query("UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ? WHERE id = ?")
        .bind(&seen.model)
        .bind(&seen.effort)
        .bind(i64::from(seen.fast))
        .bind(run_id)
        .execute(&app.db)
        .await
        .is_ok();
    if wrote {
        app.emit_bot_status(&run.bot_id).await;
        tracing::info!(run = %run_id, model = %seen.model, effort = ?seen.effort, fast = seen.fast,
                       "codex runtime corrected from the status line");
    }
    wrote
}

/// 巡邏用：拿 bot 鎖（當場套用握著同一把，不會讀到選單畫到一半），重讀畫面再校正。
pub async fn sync_runtime(app: &std::sync::Arc<crate::state::App>, client: &HerdrClient, bot_id: &str, run_id: &str, pane_id: &str) {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    // 鎖裡重查：等鎖的時候這個 run 可能已經被換掉。
    if !matches!(db::active_run(&app.db, bot_id).await, Ok(Some(r)) if r.id == run_id) {
        return;
    }
    let Ok(read) = client.pane_read(pane_id, "visible", 60).await else { return };
    correct_runtime_from_screen(app, run_id, &read.text).await;
}

/// codex status line 上的額度剩餘量：CLI 當下的數字，比每 5 分鐘輪詢的 `account/rateLimits/read`
/// 新（2026-09-13 使用者截圖兩者差一整輪）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodexStatusQuota {
    pub five_hour_left: Option<f64>,
    pub weekly_left: Option<f64>,
}

impl CodexStatusQuota {
    pub fn is_empty(&self) -> bool {
        self.five_hour_left.is_none() && self.weekly_left.is_none()
    }
}

/// 窄 pane 會把行尾截成 `weekly 48% …`，所以 `left` 不是必要的字。
pub fn parse_status_quota(screen: &str) -> Option<CodexStatusQuota> {
    let mut out = None;
    for raw in screen.lines() {
        let line = raw.trim();
        // fork／resume 起來的 codex 狀態列可能沒有 `Context` 那一段（2026-09-14 實況：
        // `gpt-5.6-sol medium · ~/project/agents-manager · 5h 82% left · weekly 97% left`），`% left` 也算。
        if !line.contains('·') || !(line.contains("Context") || line.contains("% left")) {
            continue;
        }
        let q = CodexStatusQuota { five_hour_left: pct_after(line, "5h"), weekly_left: pct_after(line, "weekly") };
        if !q.is_empty() {
            // 最後一個相符的才是現在那行（開頭的 banner 有同樣形狀）。
            out = Some(q);
        }
    }
    out
}

fn pct_after(line: &str, label: &str) -> Option<f64> {
    let mut words = line.split_whitespace().peekable();
    while let Some(w) = words.next() {
        if !w.eq_ignore_ascii_case(label) {
            continue;
        }
        // 只留開頭數字：截斷時省略號直接黏在 `%` 後（2026-09-13 實機 `weekly 24%…`）。
        let raw = *words.peek()?;
        let n: String = raw.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if let Ok(v) = n.parse::<f64>() {
            if (0.0..=100.0).contains(&v) {
                return Some(v);
            }
        }
    }
    None
}

/// Digit for the row whose **label** contains `needle`. The description after the two-space gap
/// is excluded, else `Max` would match `More reasoning…  Max and Ultra …`.
pub fn picker_number(screen: &str, needle: &str) -> Option<u32> {
    for raw in screen.lines() {
        let line = raw.trim_start().trim_start_matches('›').trim_start();
        let Some((num, rest)) = line.split_once('.') else { continue };
        let Ok(n) = num.trim().parse::<u32>() else { continue };
        let label = rest.trim_start().split("  ").next().unwrap_or("").trim();
        if label.contains(needle) {
            return Some(n);
        }
    }
    None
}

async fn read(client: &HerdrClient, pane_id: &str) -> String {
    client.pane_read(pane_id, "visible", 60).await.map(|r| r.text).unwrap_or_default()
}

async fn key(client: &HerdrClient, pane_id: &str, k: &str) -> bool {
    client.pane_send_keys(pane_id, &[k]).await.is_ok()
}

async fn text(client: &HerdrClient, pane_id: &str, t: &str) -> bool {
    client.pane_send_text(pane_id, t).await.is_ok()
}

/// Is a `/model` picker on screen? Footer or heading (a short pane can scroll either away).
/// Must be checked before typing into codex: 2026-09-10 a user message sent into a picker was
/// eaten and its Enter switched the model.
pub fn picker_open(screen: &str) -> bool {
    let t = screen.to_lowercase();
    t.contains("press enter to confirm or esc to go back")
        || t.contains("select model and effort")
        || t.contains("select reasoning level")
}

/// Escapes until no picker is left. One Escape is not enough: from the level menu it only goes
/// back to the model menu, and anything typed next would land in it.
pub async fn close_picker(client: &HerdrClient, pane_id: &str) -> bool {
    for _ in 0..PICKER_ESCAPES {
        if !picker_open(&read(client, pane_id).await) {
            return true;
        }
        if !key(client, pane_id, "Escape").await {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    !picker_open(&read(client, pane_id).await)
}

/// Two levels + nested `More reasoning…` + one spare.
const PICKER_ESCAPES: u32 = 4;

/// A failed send leaves the menu open, so back out before reporting failure.
async fn press_number(client: &HerdrClient, pane_id: &str, n: u32) -> bool {
    if !text(client, pane_id, &n.to_string()).await {
        close_picker(client, pane_id).await;
        return false;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    true
}

/// `model: None` → `(current)` row; `effort: None` → `(default)` row. Any unexpected menu returns
/// `false` and the caller falls back to "needs restart" (always safe).
async fn apply_model_and_effort(
    client: &HerdrClient,
    pane_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> bool {
    if !text(client, pane_id, "/model").await {
        close_picker(client, pane_id).await;
        return false;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    if !key(client, pane_id, "Enter").await {
        close_picker(client, pane_id).await;
        return false;
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let screen = read(client, pane_id).await;
    if !screen.contains("Select Model") {
        close_picker(client, pane_id).await;
        return false;
    }
    let needle = model.map(str::to_string).unwrap_or_else(|| "(current)".to_string());
    let Some(n) = picker_number(&screen, &needle) else {
        close_picker(client, pane_id).await;
        return false;
    };
    if !press_number(client, pane_id, n).await {
        return false;
    }

    let screen = read(client, pane_id).await;
    if !screen.contains("Select Reasoning Level") {
        close_picker(client, pane_id).await;
        return false;
    }
    let want = effort.unwrap_or("");
    let needle = if want.is_empty() { "(default)".to_string() } else { effort_menu_label(want).to_string() };
    if needle.is_empty() {
        close_picker(client, pane_id).await;
        return false;
    }
    let screen = if is_nested_effort(want) && picker_number(&screen, &needle).is_none() {
        let Some(more) = picker_number(&screen, "More reasoning") else {
            close_picker(client, pane_id).await;
            return false;
        };
        if !press_number(client, pane_id, more).await {
            return false;
        }
        read(client, pane_id).await
    } else {
        screen
    };
    let Some(n) = picker_number(&screen, &needle) else {
        close_picker(client, pane_id).await;
        return false;
    };
    if !press_number(client, pane_id, n).await {
        return false;
    }
    // Should be closed now; if not, the next prompt would be typed into it.
    close_picker(client, pane_id).await
}

async fn toggle_fast(client: &HerdrClient, pane_id: &str) -> bool {
    if !text(client, pane_id, "/fast").await {
        return false;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    if !key(client, pane_id, "Enter").await {
        return false;
    }
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

/// On `Err` the caller keeps `needs_restart: true`.
pub async fn apply(
    client: &HerdrClient,
    pane_id: &str,
    bot: &db::Bot,
    was_fast: Option<bool>,
    fields: &[&str],
) -> Result<CodexRuntime, &'static str> {
    if fields.iter().any(|f| *f == "model" || *f == "effort") {
        let model = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let effort = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if !apply_model_and_effort(client, pane_id, model, effort).await {
            return Err("picker_failed");
        }
    }
    if fields.contains(&"fast") {
        let want = bot.fast != 0;
        // The screen is the truth about the tier right now; `runs.runtime_fast` may be stale.
        let now = parse_status_line(&read(client, pane_id).await).map(|r| r.fast).or(was_fast);
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
                let seen = parse_status_line(&read(client, pane_id).await).map(|r| r.fast);
                if needs_second_toggle(seen, want) && !toggle_fast(client, pane_id).await {
                    return Err("fast_toggle_failed");
                }
            }
        }
    }
    // Read-back (SPEC §4.4a).
    tokio::time::sleep(Duration::from_millis(900)).await;
    let Some(seen) = parse_status_line(&read(client, pane_id).await) else { return Err("no_status_line") };
    let want_model = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(m) = want_model {
        if seen.model != m {
            return Err("readback_model_mismatch");
        }
    }
    if let Some(e) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if seen.effort.as_deref() != Some(e) {
            return Err("readback_effort_mismatch");
        }
    }
    if fields.contains(&"fast") && seen.fast != (bot.fast != 0) {
        return Err("readback_fast_mismatch");
    }
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::{fast_plan, needs_second_toggle, FastPlan};

    /// #393：`/fast on|off` 不是 slash 形式（0.154.0 實測會被當一般 prompt 送給模型），所以只能靠開關＋讀畫面。
    /// 已知現況就只在不對時按一下；未知現況以前直接拒絕（unknown_fast_tier），現在先按一下、讀回、方向錯才再按。
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
        assert_eq!(picker_number(EFFORT_MENU, "More reasoning"), Some(5));
        // `Max` / `Ultra` only appear in row 5's *description*; matching that would press the
        // submenu row as if it were the level itself.
        assert_eq!(picker_number(EFFORT_MENU, "Max"), None, "max / ultra are one menu deeper");
        assert_eq!(picker_number(EFFORT_MENU, "Ultra"), None);
        assert_eq!(picker_number(MODEL_MENU, "gpt-4"), None);
    }

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
        assert_eq!(picker_number(EFFORT_MENU_0154, "More reasoning"), Some(5));
        assert!(picker_number(EFFORT_MENU_0154, effort_menu_label("max")).is_none(), "max 藏在下一層");
        // `(default)` 那一列不能被 `Extra high` 的描述文字搶走（描述在兩格空白之後）。
        assert_eq!(picker_number(EFFORT_MENU_0154, "(default)"), Some(1));
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
}
