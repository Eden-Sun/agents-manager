//! 回合結束、背景工作還在跑（issue #714，SPEC §6.14）。
//!
//! child 把長工作（遠端 cargo）丟到背景就結束回合：agent 真的是 idle（可以收訊息），但使用者看到「閒置」會以為它停了。
//! CLI 自己在畫面底部標著還有幾個背景工作，這裡從既有的畫面巡邏（`update_watch`，30 秒一輪）順便讀出來：
//!
//! - claude（2.1.281 真機）：模式列 `⏵⏵ bypass permissions on · 1 shell · ← for agents`。回合收尾那行的
//!   `done 1:30 PM · 1 shell still running` **不算**：它留在捲動區，背景早就跑完了還在（實測同一個 pane 前天的那行）。
//! - codex（0.157.1 真機）：輸入框上方 `1 background terminal running · /ps to view · /stop to close`；回合中併在
//!   `• Working (8s • esc to interrupt) · 2 background terminals running · …` 那行。`/stop` 之後整行消失。
//!
//! 只看畫面最底下幾行（狀態列、輸入框附近），對話內容裡引用到的同一句不算。數字記在記憶體（`App.background_jobs`）、
//! 以 run 為鍵：屬於這個 process，新 run 自然歸零；daemon 重啟後等下一輪巡邏補上。只改顯示與 API 投影，不動排隊／送 prompt。

use crate::db;
use crate::state::App;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 畫面底部看幾個非空行：claude 的模式列在最後一兩行；codex 的背景行上面還有額度警告、輸入框、狀態列、快捷鍵提示。
const BOTTOM_LINES: usize = 8;

/// `App.background_jobs`：run id → 背景工作數（只記 > 0 的）。掛在 App 上而不是 process 全域：同一個 process 裡的
/// 另一個 App（測試）清自己的帳時不會清到這一份。
pub type Counts = Mutex<HashMap<String, u32>>;

/// 畫面底部標著的背景工作數；沒有＝0。
pub fn parse(kind: &str, screen: &str) -> u32 {
    let bottom: Vec<&str> = screen.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let bottom = &bottom[bottom.len().saturating_sub(BOTTOM_LINES)..];
    match kind {
        "claude" => bottom.iter().rev().find_map(|l| claude_shells(l)).unwrap_or(0),
        "codex" => bottom.iter().rev().find_map(|l| codex_terminals(l)).unwrap_or(0),
        _ => 0,
    }
}

/// 模式列的一段 `N shell(s)`：前面是行首或 `·`，後面是 `·` 或行尾（`N shells still running` 不符）。
fn claude_shells(line: &str) -> Option<u32> {
    line.split('·').map(str::trim).find_map(|seg| {
        let (n, rest) = seg.split_once(' ')?;
        matches!(rest, "shell" | "shells").then(|| n.parse().ok()).flatten()
    })
}

/// `N background terminal(s) running`（可以接在同一行別的片段後面）。
fn codex_terminals(line: &str) -> Option<u32> {
    line.split('·').map(str::trim).find_map(|seg| {
        let (n, rest) = seg.split_once(' ')?;
        matches!(rest, "background terminal running" | "background terminals running").then(|| n.parse().ok()).flatten()
    })
}

pub fn get(app: &App, run_id: &str) -> u32 {
    app.background_jobs.lock().unwrap_or_else(|e| e.into_inner()).get(run_id).copied().unwrap_or(0)
}

/// 巡邏讀到一份畫面：數字變了才記、才推 `bot_status`。
pub async fn observe(app: &Arc<App>, run: &db::Run, kind: &str, screen: &str) {
    let n = parse(kind, screen);
    let changed = {
        let mut m = app.background_jobs.lock().unwrap_or_else(|e| e.into_inner());
        let before = if n == 0 { m.remove(&run.id) } else { m.insert(run.id.clone(), n) };
        before.unwrap_or(0) != n
    };
    if changed {
        tracing::info!(run = %run.id, bot = %run.bot_id, kind, background_jobs = n, "background jobs changed");
        app.emit_bot_status(&run.bot_id).await;
    }
}

/// 這一輪沒看到的 run（結束了）不留帳。
pub fn retain_runs(app: &App, active: &[String]) {
    app.background_jobs.lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
}

/// API 的 run 物件加上 `background_jobs`（`GET /api/state` 與 `bot_status` 共用）。
pub fn run_json<T: serde::Serialize>(app: &App, run: &Option<T>, run_id: Option<&str>) -> Value {
    let mut v = serde_json::to_value(run).unwrap_or(Value::Null);
    if let (Some(o), Some(id)) = (v.as_object_mut(), run_id) {
        o.insert("background_jobs".into(), get(app, id).into());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/src/lifecycle/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn claude_counts_the_mode_line_not_the_turn_summary() {
        let live = fixture("claude-2.1.281-background-shell.txt");
        assert_eq!(parse("claude", &live), 1);
        assert_eq!(parse("claude", &live.replace("· 1 shell ·", "· 3 shells ·")), 3);
        // 背景跑完：模式列那段消失，收尾行 `· 1 shell still running` 還留在畫面上——不算。
        let done = live.replace("bypass permissions on · 1 shell · ← for agents", "bypass permissions on (shift+tab to cycle) · ← for agents");
        assert!(done.contains("1 shell still running"), "前提：收尾行還在");
        assert_eq!(parse("claude", &done), 0);
        assert_eq!(parse("claude", &fixture("claude-2.1.281-no-background-shell.txt")), 0, "`Ran 1 shell command` 不算");
    }

    #[test]
    fn claude_ignores_the_same_words_quoted_in_the_conversation() {
        let quoted = "  ⏵⏵ bypass permissions on · 1 shell · ← for agents\n".to_string()
            + &"⏺ 一般輸出\n".repeat(BOTTOM_LINES)
            + "❯\n  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n";
        assert_eq!(parse("claude", &quoted), 0, "只看畫面最底下");
    }

    #[test]
    fn codex_counts_idle_and_working_footers_and_clears_after_stop() {
        assert_eq!(parse("codex", &fixture("codex-0.157-background-idle-bg.txt")), 1);
        assert_eq!(parse("codex", &fixture("codex-0.157-background-working-bg.txt")), 2);
        assert_eq!(parse("codex", &fixture("codex-0.157-background-stopped.txt")), 0);
        assert_eq!(parse("claude", &fixture("codex-0.157-background-idle-bg.txt")), 0, "kind 對不上不算");
    }

    #[tokio::test]
    async fn a_count_is_published_on_change_and_dropped_with_its_run() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "alfa").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        let mut rx = app.subscribe();
        let live = fixture("claude-2.1.281-background-shell.txt");

        observe(&app, &run, "claude", &live).await;
        assert_eq!(get(&app, &run_id), 1);
        let frame = rx.try_recv().expect("數字變了要推 bot_status");
        assert_eq!(frame.kind, "bot_status");
        let state = crate::api::state_json(&app).await.unwrap();
        assert_eq!(state["projects"][0]["bots"][0]["run"]["background_jobs"], 1, "GET /api/state 帶得出來");

        observe(&app, &run, "claude", &live).await;
        assert!(rx.try_recv().is_err(), "沒變就不推");

        retain_runs(&app, &[]);
        assert_eq!(get(&app, &run_id), 0, "結束的 run 不留帳");
    }
}
