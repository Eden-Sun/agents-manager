//! `session_paused` runner 與選單觀察。

use crate::db;
use crate::events::ports::TurnCommands;
use crate::session_paused::{get_forced, record_forced, retire};
use crate::state::App;
use std::sync::Arc;
use std::time::Duration;

/// herdr 轉成 `idle` 之後等一下再讀畫面：選單還在畫的那一瞬間讀到的是半張。
const SETTLE: Duration = Duration::from_millis(800);

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

/// 讀一次畫面、照結果補標或還原（巡邏與 `idle` 邊共用）。claude 看 Session paused 選單，agy 看它會停下來等人的對話框
/// （[`crate::agy_screen`]）；讀不到畫面什麼都不動——讀不到不等於選單關了。
pub async fn observe(app: &Arc<App>, run: &db::Run) {
    let kind = match db::bot(&app.db, &run.bot_id).await {
        Ok(Some(b)) if matches!(b.kind.as_str(), "claude" | "agy") => b.kind,
        _ => return,
    };
    let Some(pane) = run.pane_id.as_deref().filter(|p| !p.trim().is_empty()) else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    if kind == "agy" {
        observe_agy_screen(app, run, &read.text).await;
    } else {
        observe_screen(app, run, &read.text).await;
    }
}

pub async fn observe_agy_screen(app: &Arc<App>, run: &db::Run, screen: &str) {
    let dialog = crate::agy_screen::blocking_dialog(screen);
    observe_with(app, run, dialog.is_some(), dialog.map(|d| d.label())).await;
}

pub async fn observe_screen(app: &Arc<App>, run: &db::Run, screen: &str) {
    observe_with(app, run, crate::tui_prompts::is_session_paused_menu(screen), None).await;
}

async fn observe_with(app: &Arc<App>, run: &db::Run, open: bool, label: Option<&'static str>) {
    if open {
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
            record_forced(&run.id, prev, label);
            tracing::warn!(run = %run.id, bot = %run.bot_id, dialog = label, "停在等人回答的選單／對話框：補標 blocked，等使用者自己選（不自動按）");
            app.emit_bot_status(&run.bot_id).await;
        }
        return;
    }
    let Some((prev, epoch)) = get_forced(&run.id) else { return };
    tracing::info!(run = %run.id, bot = %run.bot_id, "Session paused 選單關掉了");
    // 在還原 idle 與叫醒 queued flush 之前取回選單所屬的 turn；沒有 in-flight 就不排會影響未來回合的 timer。
    let paused_turn = if prev == "idle" {
        match db::in_flight_turn(&app.db, &run.id).await {
            Ok(turn) => turn,
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "Session paused 選單關閉時讀取 in-flight turn 失敗，不排快關計時器");
                None
            }
        }
    } else {
        None
    };
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
            app.schedule_flush_queued(&run.bot_id);
            if let Some(turn) = paused_turn {
                close_paused_turn_later(app, &run.id, &turn.id);
            }
        }
        // 已經不是 blocked（herdr／別的路徑改過）或 run 不在了：被取代，補標記沒有要還的東西了。
        Ok(_) => retire(&run.id, epoch),
        // 寫不進去：補標記留著，下一輪巡邏（`is_forced`）重試；DB 仍是 blocked，不發狀態、不叫 flush。
        Err(e) => tracing::warn!(run = %run.id, bot = %run.bot_id, error = %e, "Session paused 還原寫入失敗，下一輪巡邏重試"),
    }
}

/// 選單關掉後多久再看一次：選「換模型重試」的話 claude 會接著跑同一回合，herdr 要一點時間報 working。
const AFTER_CLOSE: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(5) };

/// 選單關掉、還原成 idle 之後：等一下，只檢查選單關閉時捕捉的 turn（不等 5 分鐘閒置門檻）。
fn close_paused_turn_later(app: &Arc<App>, run_id: &str, turn_id: &str) {
    let (app, run_id, turn_id) = (app.clone(), run_id.to_string(), turn_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(AFTER_CLOSE).await;
        if let Some(turn) = app.close_after_session_paused(&run_id, &turn_id).await {
            tracing::info!(run = %run_id, turn = %turn, "Session paused 選單關掉、沒有回覆：直接收掉這個回合");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_paused::{forget_ended, is_forced};
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

    /// 2026-09-26：選單關掉、還原成 idle，而那一回合沒有回覆——不留「等待中」到 5 分鐘後，馬上收掉。
    #[tokio::test]
    async fn closing_the_menu_on_an_idle_run_closes_the_turn_it_left_open() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "paused-turn").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','unknown',?)")
            .bind(&turn_id)
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        observe_screen(&app, &run_of(&app, &run_id).await, SESSION_PAUSED).await;
        observe_screen(&app, &run_of(&app, &run_id).await, IDLE_CLAUDE).await;
        let status = || async {
            sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap()
        };
        // 被等的 closer 自己先睡 `AFTER_CLOSE`（1 秒）：固定 2 秒的輪詢在高負載下會先到期（issue #952），改 30 秒上限。
        assert!(crate::testing::eventually!(status().await != "in_flight"), "選單關掉、run 仍 idle：那筆沒有回覆的 in-flight 要馬上收");
        for m in ["pane.send_keys", "pane.send_text", "agent.prompt"] {
            assert!(e.herdr.calls_to(m).is_empty(), "不能替使用者按：{m}");
        }
    }

    /// #575：選單關閉時捕捉 T1；延遲 Stop hook 正常收掉 T1 後，佇列已開始 T2、run 狀態事件仍未到，舊 timer 不能收 T2。
    #[tokio::test]
    async fn a_delayed_stop_hook_and_next_turn_make_the_paused_timer_stale() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "paused-next-turn").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let t1_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&t1_id)
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        observe_screen(&app, &run_of(&app, &run_id).await, SESSION_PAUSED).await;
        observe_screen(&app, &run_of(&app, &run_id).await, IDLE_CLAUDE).await;
        assert_eq!(
            crate::lifecycle::turn_controller::set_status(&app.db, &t1_id, "in_flight", "completed", "Stop hook").await.unwrap(),
            crate::lifecycle::turn_controller::Outcome::Applied,
            "延遲到達的 Stop hook 正常收掉 T1"
        );

        let t2_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&t2_id)
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let run_status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(run_status, "idle", "T2 已開始，但 herdr 的 working 事件還沒到");

        tokio::time::sleep(AFTER_CLOSE + Duration::from_millis(50)).await;
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&t2_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "in_flight", "T1 的快關 timer 到期後不可誤關 T2");
        let t1_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&t1_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(t1_status, "completed", "T1 維持 Stop hook 的正常終態");
    }

    /// 選單關閉當下沒有 in-flight turn，就不該替未來送出的 turn 排快關 timer。
    #[tokio::test]
    async fn closing_the_menu_without_a_turn_does_not_arm_a_future_closer() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "paused-no-turn").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, SESSION_PAUSED).await;
        observe_screen(&app, &run_of(&app, &run_id).await, IDLE_CLAUDE).await;

        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let later_turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&later_turn)
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        tokio::time::sleep(AFTER_CLOSE + Duration::from_millis(50)).await;
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&later_turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "in_flight", "選單關閉時沒有 turn，不可建立 run-scoped 的未來 closer");
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
