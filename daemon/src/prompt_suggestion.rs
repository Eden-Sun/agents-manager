//! claude 回合結束後輸入框裡那句灰字「建議下一句」（prompt suggestion，終端按 Tab 收下、Enter 送出）。
//!
//! 網頁要看得到這句、一鍵送出（2026-10-03 使用者）。這裡**不存 DB**：建議是畫面上的暫時狀態，跟 [`crate::blocked_reason`]
//! 一樣記在記憶體（run id → 文字），跟著 run 一起出現在 `/api/state` 與 `bot_status` 事件（`run.prompt_suggestion`），
//! 只在 `agent_status == "idle"` 時帶、其他一律 `null`；內容變了或消失才推 `bot_status`。
//!
//! 怎麼讀（只有樣式讀 `format: ansi` 分得出灰字，純文字讀一律 `null`，見 [`crate::lifecycle::prompt_suggestion`]）：
//! * **idle 邊之後的幾次補讀**（[`on_idle`]）：建議是回合結束後 claude 另外算出來的，Stop 的當下通常還沒畫，
//!   所以在 idle 之後 [`BURST`] 的幾個時間點各看一次，看到就停。每個 idle 邊最多 4 次讀。
//! * **既有的 30 秒畫面巡邏**（[`observe_sweep`]，`update_watch`）：已經讀了一份純文字畫面，輸入列是空的就不必再讀（沒有建議可言）；
//!   輸入列有字才多讀一次樣式畫面分辨「灰字建議」與「草稿」，順便抓到使用者在終端打字、建議消失的那一刻。沒有額外的輪詢迴圈。
//! * 回合開始（agent 不再 idle）就忘掉；run 結束由巡邏的 `retain_runs` 清帳。
//!
//! 讀不到畫面什麼都不動：讀不到不等於建議消失了。

use crate::db;
use crate::lifecycle::app_ports_p4::{AppEventSink, AppHerdrPort};
use crate::state::App;
use am_core::PaneReadSource;
use am_ports::{DbContext, EventSink, StyledRunPaneReader};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use sqlx::SqlitePool;

/// idle 邊之後第幾毫秒補讀一次：建議句的產生要一點時間（另一次模型呼叫），畫面也要畫完。
const BURST: [u64; 4] = [1500, 3500, 7000, 14000];

fn store() -> &'static Mutex<HashMap<String, String>> {
    static S: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// 這個 run 現在畫面上的建議句（記憶體帳；是否算「現在」由投影用 `agent_status` 把關，見 [`json`]）。
pub fn of(run_id: &str) -> Option<String> {
    store().lock().unwrap_or_else(|e| e.into_inner()).get(run_id).cloned()
}

/// 記下（或清掉）一個 run 的建議；回傳有沒有變。
pub fn set(run_id: &str, text: Option<String>) -> bool {
    let mut m = store().lock().unwrap_or_else(|e| e.into_inner());
    match text {
        Some(t) => m.insert(run_id.to_string(), t.clone()).as_deref() != Some(t.as_str()),
        None => m.remove(run_id).is_some(),
    }
}

/// 忘掉這個 run 的建議（回合開始、run 結束）。
pub fn forget(run_id: &str) -> bool {
    set(run_id, None)
}

/// 不在 `active` 裡的 run（結束了）不留記錄。
pub fn retain_runs(active: &[String]) {
    store().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
}

/// 放進 run JSON 的值：只在 run 現在是 `idle` 才帶建議，其他狀態（回合中、blocked）一律 `null`。
pub fn json(run_id: &str, agent_status: Option<&str>) -> Value {
    if agent_status != Some("idle") {
        return Value::Null;
    }
    of(run_id).into()
}

/// 讀一次樣式畫面、照結果記下或清掉建議（idle 邊補讀用）。回傳現在有沒有建議；讀不到回 `None`（什麼都不動）。
pub async fn observe(app: &Arc<App>, run: &db::Run) -> Option<bool> {
    let db = DbContext::new(app.db.clone());
    let reader = AppHerdrPort::new(app);
    let events = AppEventSink::new(app);
    observe_with_ports(&db, &reader, &events, run).await
}

/// Run-scoped suggestion observation. The App wrapper only assembles the DB, styled pane reader,
/// and status event capabilities; this path does not select a host or call App methods directly.
pub(crate) async fn observe_with_ports<R, E>(
    db: &DbContext<SqlitePool>,
    reader: &R,
    events: &E,
    run: &db::Run,
) -> Option<bool>
where
    R: StyledRunPaneReader,
    E: EventSink,
{
    if run.state != "running" || run.agent_status != "idle" {
        if forget(&run.id) {
            emit_bot_status(events, &run.bot_id).await;
        }
        return Some(false);
    }
    if !matches!(db::bot(db.pool(), &run.bot_id).await, Ok(Some(b)) if b.kind == "claude") {
        return Some(false);
    }
    let pane = run.pane_id.as_deref().filter(|p| !p.trim().is_empty())?;
    let text = reader
        .read_styled_run_pane(&run.bot_id, run.herdr_session.as_ref(), pane, PaneReadSource::Visible, 80)
        .await
        .ok()??;
    Some(record(events, run, &text).await)
}

async fn emit_bot_status<E: EventSink>(events: &E, bot_id: &str) {
    if let Err(error) = events.bot_status_changed(bot_id).await {
        tracing::warn!(bot = %bot_id, error = ?error, "prompt suggestion bot-status projection failed");
    }
}

/// 把一份樣式畫面的結果記進帳；變了就推 `bot_status`。回傳現在有沒有建議。
async fn record<E: EventSink>(events: &E, run: &db::Run, styled: &str) -> bool {
    let found = crate::lifecycle::prompt_suggestion("claude", styled);
    let has = found.is_some();
    if set(&run.id, found) {
        tracing::info!(run = %run.id, bot = %run.bot_id, suggestion = has, "prompt suggestion changed");
        emit_bot_status(events, &run.bot_id).await;
    }
    has
}

/// herdr 轉成 `idle` 那一刻（[`crate::events`]）：在 [`BURST`] 的幾個時間點各看一次，看到建議或不再 idle 就停。
/// 自己開背景工作，不擋事件迴圈。
pub fn on_idle(app: &Arc<App>, run: &db::Run) {
    let (app, run_id) = (app.clone(), run.id.clone());
    tokio::spawn(async move {
        let db = DbContext::new(app.db.clone());
        let mut waited = 0;
        for at in BURST {
            tokio::time::sleep(Duration::from_millis(at - waited)).await;
            waited = at;
            // 事件帶來的 Run 是更新前的複本：每次重讀，狀態才是現在的。
            let Ok(Some(run)) = db::run(db.pool(), &run_id).await else { return };
            match observe(&app, &run).await {
                Some(true) => return,
                _ if run.state != "running" || run.agent_status != "idle" => return,
                _ => {}
            }
        }
    });
}

/// 30 秒畫面巡邏的一個 claude run：`plain` 是巡邏已經讀到的純文字畫面。
pub async fn observe_sweep(app: &(impl crate::capabilities::Emit + crate::capabilities::BotStatusEmit), run: &db::Run, plain: &str, client: &crate::herdr::HerdrClient, pane: &str) {
    let events = AppEventSink::new(app);
    if run.state != "running" || run.agent_status != "idle" {
        if forget(&run.id) {
            emit_bot_status(&events, &run.bot_id).await;
        }
        return;
    }
    // 輸入列是空的：沒有建議可言，不必再讀。
    if crate::lifecycle::composer_text("claude", plain).is_none() {
        if forget(&run.id) {
            emit_bot_status(&events, &run.bot_id).await;
        }
        return;
    }
    let Ok(styled) = crate::lifecycle::read_styled(client, pane, "visible", 80).await else { return };
    record(&events, run, &styled).await;
}

#[cfg(test)]
mod tests;
