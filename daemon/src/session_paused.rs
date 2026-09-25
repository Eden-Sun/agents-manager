//! Claude Code 2.1.281 的「Session paused」選單（畫面辨識在 [`crate::tui_prompts::is_session_paused_menu`]）。
//!
//! API 拒答或額度用完時 claude 停下來問「換模型重試／改 prompt 重試」（或「用額度續跑／換模型」），herdr 卻判成
//! `idle`：網頁不會彈出選項，回合還被終端備援收掉、把選單的一行存成回覆（2026-09-25 cf-ox-fork-fork）。這裡的原則：
//! * **一個鍵都不按**：換不換模型、要不要花額度是使用者的決定。
//! * **標成 blocked**：herdr 判 `idle`／`unknown` 時補標，網頁的 BlockedModal／BlockedPanel 才會彈出、用
//!   BlockedChoices 讓人自己點；備援與 stuck-turn 收尾看到 blocked 也就不收這個回合。
//! * **選單消失就還原**：照補標前的值還回去（CAS：這段期間 herdr 自己改過狀態就不動），叫醒排隊的 flush。
//!
//! 補標記在記憶體（`run_id` → 補標前的狀態）；daemon 重啟當下若是這裡補的 `blocked`，交給 herdr 下一次狀態事件或
//! reconcile 更正，巡邏看到選單還在會再補一次。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::db;
use crate::state::App;

/// herdr 轉成 `idle` 之後等一下再讀畫面：選單還在畫的那一瞬間讀到的是半張。
const SETTLE: Duration = Duration::from_millis(800);

fn forced() -> &'static Mutex<HashMap<String, String>> {
    static V: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

/// 這個 run 現在是不是由這裡補標成 `blocked` 的（巡邏靠它把已經不是 idle 的 run 也看一眼，才還得回去）。
pub fn is_forced(run_id: &str) -> bool {
    forced().lock().unwrap().contains_key(run_id)
}

/// herdr 轉成 `idle` 那一刻（[`crate::events`]）：等畫面畫完再看。自己開背景工作，不擋事件迴圈。
pub fn on_idle(app: &Arc<App>, run: &db::Run) {
    let (app, run) = (app.clone(), run.clone());
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        // 事件帶來的 Run 是更新前的複本：重讀一次，狀態才是現在的。
        if let Ok(Some(run)) = db::run(&app.db, &run.id).await {
            observe(&app, &run).await;
        }
    });
}

/// 讀一次畫面、照結果補標或還原（巡邏與 `idle` 邊共用）。只看 claude；讀不到畫面什麼都不動——讀不到不等於選單關了。
pub async fn observe(app: &Arc<App>, run: &db::Run) {
    if !matches!(db::bot(&app.db, &run.bot_id).await, Ok(Some(b)) if b.kind == "claude") {
        return;
    }
    let Some(pane) = run.pane_id.as_deref().filter(|p| !p.trim().is_empty()) else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    observe_screen(app, run, &read.text).await;
}

pub(crate) async fn observe_screen(app: &Arc<App>, run: &db::Run, screen: &str) {
    if crate::tui_prompts::is_session_paused_menu(screen) {
        if run.agent_status == "blocked" {
            return; // herdr 自己判的（或已經補過）：不動。
        }
        let prev = run.agent_status.clone();
        let marked = sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=? AND agent_status=?")
            .bind(&run.id)
            .bind(&prev)
            .execute(&app.db)
            .await
            .map(|r| r.rows_affected() == 1)
            .unwrap_or(false);
        if marked {
            forced().lock().unwrap().entry(run.id.clone()).or_insert(prev);
            tracing::warn!(run = %run.id, bot = %run.bot_id, "claude 停在 Session paused 選單：補標 blocked，等使用者自己選（不自動按）");
            app.emit_bot_status(&run.bot_id).await;
        }
        return;
    }
    let Some(prev) = forced().lock().unwrap().remove(&run.id) else { return };
    tracing::info!(run = %run.id, bot = %run.bot_id, "Session paused 選單關掉了");
    // 只還我們自己標的那個 blocked：這段期間 herdr 已經報了別的狀態就是它的，不蓋。
    let _ = sqlx::query("UPDATE runs SET agent_status=? WHERE id=? AND agent_status='blocked'")
        .bind(&prev)
        .bind(&run.id)
        .execute(&app.db)
        .await;
    app.emit_bot_status(&run.bot_id).await;
    crate::lifecycle::schedule_flush_queued(app, &run.bot_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use crate::tui_prompts::screens::{IDLE_CLAUDE, SESSION_PAUSED};

    async fn run_of(app: &Arc<App>, run_id: &str) -> db::Run {
        sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id=?").bind(run_id).fetch_one(&app.db).await.unwrap()
    }

    /// 2026-09-25 cf-ox-fork-fork：herdr 判 idle 的 Session paused 選單要補標 blocked；再看一次不重複動；
    /// 使用者選完、選單不見了 → 還原成 idle。從頭到尾一個鍵、一個字都沒送進 pane。
    #[tokio::test]
    async fn an_idle_session_paused_menu_is_marked_blocked_and_released_when_it_closes() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "paused").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle", "前提：herdr 判 idle");

        observe_screen(&app, &run_of(&app, &run_id).await, SESSION_PAUSED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "herdr 判 idle 時補標");
        assert!(is_forced(&run_id));
        observe_screen(&app, &run_of(&app, &run_id).await, SESSION_PAUSED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked");

        observe_screen(&app, &run_of(&app, &run_id).await, IDLE_CLAUDE).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle", "補標的 blocked 還回去");
        assert!(!is_forced(&run_id));

        for m in ["pane.send_keys", "pane.send_text", "agent.prompt"] {
            assert!(e.herdr.calls_to(m).is_empty(), "不能替使用者選：{m}");
        }
    }

    /// herdr 自己判成 blocked 的不補標、不還原；補標後 herdr 報了別的狀態（使用者選完開始 working），還原不蓋掉它。
    #[tokio::test]
    async fn statuses_herdr_set_are_left_to_herdr() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "paused").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        observe_screen(&app, &run_of(&app, &run_id).await, SESSION_PAUSED).await;
        assert!(!is_forced(&run_id));
        observe_screen(&app, &run_of(&app, &run_id).await, IDLE_CLAUDE).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "herdr 的 blocked 不是我們的");

        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        observe_screen(&app, &run_of(&app, &run_id).await, SESSION_PAUSED).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        observe_screen(&app, &run_of(&app, &run_id).await, IDLE_CLAUDE).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "working");
    }
}
