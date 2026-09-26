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
//!
//! 補標記是巡邏回頭看這個 run 的唯一理由（它已經不是 `idle`），所以**還原寫進 DB 之後才拿掉**（#565）：寫失敗就留著，
//! 下一輪巡邏重試；CAS 沒命中（herdr 或別的路徑已經改掉 `blocked`）算被取代，拿掉；run 不在或已結束也拿掉。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::db;
use crate::state::App;

/// herdr 轉成 `idle` 之後等一下再讀畫面：選單還在畫的那一瞬間讀到的是半張。
const SETTLE: Duration = Duration::from_millis(800);

/// 補標記：補標前的狀態，加上最後一次補標的代號——還原寫回 DB 的那段期間選單又冒出來、重新補標的話，
/// 代號變了，還原就不能把新的補標記一起拿掉。
struct Forced {
    prev: String,
    epoch: u64,
}

fn forced() -> &'static Mutex<HashMap<String, Forced>> {
    static V: OnceLock<Mutex<HashMap<String, Forced>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

fn next_epoch() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed) + 1
}

/// 拿掉補標記——只在它還是 `epoch` 那一次補的時候。
fn retire(run_id: &str, epoch: u64) {
    let mut m = forced().lock().unwrap();
    if m.get(run_id).is_some_and(|f| f.epoch == epoch) {
        m.remove(run_id);
    }
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
            let epoch = next_epoch();
            forced().lock().unwrap().entry(run.id.clone()).or_insert(Forced { prev, epoch: 0 }).epoch = epoch;
            tracing::warn!(run = %run.id, bot = %run.bot_id, "claude 停在 Session paused 選單：補標 blocked，等使用者自己選（不自動按）");
            app.emit_bot_status(&run.bot_id).await;
        }
        return;
    }
    let Some((prev, epoch)) = forced().lock().unwrap().get(&run.id).map(|f| (f.prev.clone(), f.epoch)) else { return };
    tracing::info!(run = %run.id, bot = %run.bot_id, "Session paused 選單關掉了");
    // 只還我們自己標的那個 blocked：這段期間 herdr 已經報了別的狀態就是它的，不蓋。
    let restored = sqlx::query("UPDATE runs SET agent_status=? WHERE id=? AND agent_status='blocked'")
        .bind(&prev)
        .bind(&run.id)
        .execute(&app.db)
        .await;
    match restored {
        Ok(r) if r.rows_affected() == 1 => {
            retire(&run.id, epoch);
            app.emit_bot_status(&run.bot_id).await;
            crate::lifecycle::schedule_flush_queued(app, &run.bot_id);
        }
        // 已經不是 blocked（herdr／別的路徑改過）或 run 不在了：被取代，補標記沒有要還的東西了。
        Ok(_) => retire(&run.id, epoch),
        // 寫不進去：補標記留著，下一輪巡邏（`is_forced`）重試；DB 仍是 blocked，不發狀態、不叫 flush。
        Err(e) => tracing::warn!(run = %run.id, bot = %run.bot_id, error = %e, "Session paused 還原寫入失敗，下一輪巡邏重試"),
    }
}

/// 巡邏收尾：補標記的 run 已經不在 active 名單上（結束或被刪）就拿掉，巡邏再也不會看它。
/// 以重讀 DB 為準；讀不到（DB 錯）就留著等下一輪。
pub async fn forget_ended(app: &Arc<App>, active: &[db::Run]) {
    let marked: Vec<(String, u64)> = forced()
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _)| !active.iter().any(|r| &r.id == *id))
        .map(|(id, f)| (id.clone(), f.epoch))
        .collect();
    for (id, epoch) in marked {
        match db::run(&app.db, &id).await {
            Ok(Some(r)) if matches!(r.state.as_str(), "starting" | "running" | "stopping") => {}
            Ok(_) => retire(&id, epoch),
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use crate::tui_prompts::screens::{IDLE_CLAUDE, SESSION_PAUSED};

    /// 補標記是全域的，[`forget_ended`] 會把「在這個測試 DB 讀不到」的別的測試的補標記清掉：這個模組的測試一個一個跑。
    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        static L: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        L.lock().await
    }

    async fn run_of(app: &Arc<App>, run_id: &str) -> db::Run {
        sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id=?").bind(run_id).fetch_one(&app.db).await.unwrap()
    }

    /// 2026-09-25 cf-ox-fork-fork：herdr 判 idle 的 Session paused 選單要補標 blocked；再看一次不重複動；
    /// 使用者選完、選單不見了 → 還原成 idle。從頭到尾一個鍵、一個字都沒送進 pane。
    #[tokio::test]
    async fn an_idle_session_paused_menu_is_marked_blocked_and_released_when_it_closes() {
        let _serial = serial().await;
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
        let _serial = serial().await;
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
        assert!(!is_forced(&run_id), "被 herdr 取代的補標記要拿掉，不然巡邏一直回頭看");
    }

    fn bot_status_events(rx: &mut tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> usize {
        std::iter::from_fn(|| rx.try_recv().ok()).filter(|ev| ev.kind == "bot_status").count()
    }

    /// #565：選單關了、還原 UPDATE 寫失敗——補標記要留著（巡邏才會回頭看），不發像是還原成功的狀態；
    /// DB 好了，下一輪巡邏不用等 herdr 再來一個狀態事件就還得回去，之後才拿掉補標記。
    #[tokio::test]
    async fn a_failed_restore_keeps_the_marker_and_the_next_sweep_restores() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "paused").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, SESSION_PAUSED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked");

        sqlx::query(
            "CREATE TRIGGER refuse_restore BEFORE UPDATE OF agent_status ON runs
             WHEN OLD.agent_status='blocked' AND NEW.agent_status='idle' BEGIN SELECT RAISE(ABORT, 'database is locked'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        let mut rx = app.subscribe();
        observe_screen(&app, &run_of(&app, &run_id).await, IDLE_CLAUDE).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "前提：還原真的沒寫進去");
        assert!(is_forced(&run_id), "還原沒寫進去就不能拿掉補標記：巡邏靠它回頭看這個 blocked 的 run");
        assert_eq!(bot_status_events(&mut rx), 0, "寫失敗不發像是還原成功的狀態");

        sqlx::query("DROP TRIGGER refuse_restore").execute(&app.db).await.unwrap();
        observe_screen(&app, &run_of(&app, &run_id).await, IDLE_CLAUDE).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle", "下一輪巡邏還原");
        assert!(!is_forced(&run_id), "寫進去之後才拿掉");
        assert_eq!(bot_status_events(&mut rx), 1, "還原成功發一次");
        observe_screen(&app, &run_of(&app, &run_id).await, IDLE_CLAUDE).await;
        assert_eq!(bot_status_events(&mut rx), 0, "拿掉之後不再重發");
    }

    /// 補標記的 run 結束了（不在 active 名單），巡邏再也不會看它：收尾時拿掉；還在跑的留著。
    #[tokio::test]
    async fn markers_of_ended_runs_are_forgotten_by_the_sweep() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "paused").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, SESSION_PAUSED).await;
        assert!(is_forced(&run_id));

        forget_ended(&app, &[]).await;
        assert!(is_forced(&run_id), "名單沒列但 DB 重讀還是 running：留著");

        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        let active = db::all_active_runs(&app.db).await.unwrap();
        forget_ended(&app, &active).await;
        assert!(!is_forced(&run_id), "結束的 run 補標記要拿掉");
    }
}
