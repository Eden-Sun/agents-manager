//! Claude Code 2.1.281 的「Dangerous rm operation」防誤刪確認框（畫面辨識在 [`crate::tui_prompts::dangerous_rm_prompt`]）。
//!
//! `rm -rf $(…)`、`$VAR`、頂層目錄這類目標，就算 bot 是 `--dangerously-skip-permissions` 起的也會跳這個框，
//! 約 2 分鐘沒人回答 claude 就自動拒絕（那個指令不執行，claude 拿到「被內建安全檢查拒絕」的 tool_result、把回合做完）。
//! 它的用意是**只有人能核准**，所以 daemon 這一側的原則是：
//! * **一個鍵都不按**：不像滿意度問卷、Auto mode 推銷框那樣替使用者選；也不整批設
//!   `CLAUDE_CODE_DISABLE_SUBSTITUTION_RM_PROMPT=1` 把它關掉。
//! * **送達不打進框裡**：`pane_ready_for_prompt` 認到框就回 409 `dangerous_rm_pending`，排隊的留在佇列，框關掉才送。
//! * **通知使用者、帶上目標**：這一次框（同一句警語＋同一個指令）在 bot 的對話裡插一則系統訊息，寫警語、目標與指令，只寫一次。
//! * **標成 blocked**：herdr 通常自己就判成 `blocked`（2026-09-23 m12 的 pane 就是）；判成 `idle`／`unknown`
//!   時由這裡補標。補標後收到任何 herdr 狀態事件，就由 herdr 接手狀態，不再還原這筆標記。
//! * **框消失後回到正常**：不管是有人回答還是 2 分鐘自動拒絕，都補一則說明、叫醒排隊的 flush。
//!
//! 開著的框記在記憶體（`run_id` → 這一次的警語、補標前的狀態）：daemon 重啟後最多重講一次通知；
//! 重啟當下若是由這裡補標的 `blocked`，交給 herdr 下一次狀態事件或 reconcile 更正。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::db;
use crate::tui_prompts::DangerousRm;

struct Episode {
    warning: String,
    /// 框裡的指令：警語是通用句（`command substitution output`）時，不同的 rm 靠它分辨是不是同一個框。
    command: Option<String>,
    /// 由這裡補標成 `blocked` 之前的狀態；`None` ＝herdr 自己判的，框關掉時不必還。
    forced_from: Option<String>,
    /// Only one observer may finish the episode while it awaits a database restore.
    closing: bool,
}

fn open() -> &'static Mutex<HashMap<String, Episode>> {
    static V: OnceLock<Mutex<HashMap<String, Episode>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

/// 這個 run 現在有沒有開著的框（巡邏靠它把已經不是 idle／blocked 的 run 也看一眼，才收得掉）。
pub fn is_open(run_id: &str) -> bool {
    open().lock().unwrap().contains_key(run_id)
}

/// Herdr reports the run's status; a synthetic blocked marker must no longer restore its stale prior value.
pub fn on_herdr_status(run_id: &str) {
    if let Some(episode) = open().lock().unwrap().get_mut(run_id) {
        episode.forced_from = None;
    }
}

/// 給使用者的那一則。
pub fn notice(rm: &DangerousRm) -> String {
    let mut s = format!(
        "⚠ claude 要執行危險的 rm，停在防誤刪確認框等你本人核准。\n{}\n目標：{}\n",
        rm.warning,
        if rm.target.is_empty() { "（畫面上沒寫）" } else { rm.target.as_str() }
    );
    if let Some(cmd) = &rm.command {
        let fence = crate::child_alerts::fence_for(cmd);
        s.push_str(&format!("指令：\n{fence}text\n{cmd}\n{fence}\n"));
    }
    s.push_str(
        "daemon 不會替你按，也不會把訊息打進框裡（送出會回 409，排隊的等框關掉再送）。\
         要做就到「終端」選 1. Yes；不要就選 2. No 或按 Esc。\
         約 2 分鐘沒人回答，claude 會自動拒絕：這個指令不會執行，回合照常往下走。",
    );
    s
}

pub const CLOSED_NOTE: &str =
    "防誤刪確認框已經關掉（有人回答了，或 2 分鐘到了 claude 自動拒絕——拒絕的話那個 rm 沒有執行）。排著要送的訊息照常送。";

/// 這一次框（同一句警語＋同一個指令）只講一次；送達閘門與巡邏共用。回傳這次有沒有真的寫。
pub async fn notify_once(app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands), run: &db::Run, rm: &DangerousRm) -> bool {
    {
        let mut m = open().lock().unwrap();
        match m.get(&run.id) {
            Some(ep) if ep.warning == rm.warning && ep.command == rm.command => return false,
            // 另一個框接在上一個後面（上一個框沒被巡邏看到關掉）：補標的 blocked 還是我們的，沿用。
            prev => {
                let forced_from = prev.and_then(|ep| ep.forced_from.clone());
                m.insert(run.id.clone(), Episode { warning: rm.warning.clone(), command: rm.command.clone(), forced_from, closing: false });
            }
        }
    }
    tracing::warn!(run = %run.id, bot = %run.bot_id, target = %rm.target, "claude 停在 Dangerous rm 確認框，等使用者本人核准（不自動按）");
    match db::conversation_id(app.db(), &run.bot_id).await {
        Ok(conv) => {
            let _ = app.insert_message(&conv, None, "system", &notice(rm), "system", false, None).await;
        }
        Err(e) => tracing::warn!(run = %run.id, error = ?e, "could not post the dangerous-rm notice"),
    }
    true
}

/// 巡邏收尾：開著框的 run 已經結束或被刪（不在 active 名單上、DB 也確認不是進行中）就把那筆記錄拿掉。
/// 巡邏只看 active run，結束的 run 不會再被 [`observe`] 讀到「框關了」，記錄不拿掉就只增不減。
/// 以重讀 DB 為準；讀不到就留著等下一輪。
pub async fn forget_ended(app: &impl crate::capabilities::Db, active: &[db::Run]) {
    let candidates: Vec<String> = open()
        .lock()
        .unwrap()
        .keys()
        .filter(|id| !active.iter().any(|r| &r.id == *id))
        .cloned()
        .collect();
    for id in candidates {
        match db::run(app.db(), &id).await {
            Ok(Some(r)) if matches!(r.state.as_str(), "starting" | "running" | "stopping") => {}
            Ok(_) => {
                open().lock().unwrap().remove(&id);
            }
            Err(_) => {}
        }
    }
}

pub async fn observe_screen(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::Emit + crate::events::ports::TurnCommands), run: &db::Run, screen: &str) {
    match crate::tui_prompts::dangerous_rm_prompt(screen) {
        Some(rm) => {
            notify_once(app, run, &rm).await;
            if run.agent_status == "blocked" {
                return;
            }
            // herdr 沒把它判成 blocked：補標，燈號與「需要回應」才會亮，一般送出也照 blocked 擋。
            let prev = run.agent_status.clone();
            let marked = sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=? AND agent_status=?")
                .bind(&run.id)
                .bind(&prev)
                .execute(app.db())
                .await
                .map(|r| r.rows_affected() == 1)
                .unwrap_or(false);
            if marked {
                if let Some(ep) = open().lock().unwrap().get_mut(&run.id) {
                    ep.forced_from.get_or_insert(prev);
                }
                app.emit_bot_status(&run.bot_id).await;
            }
        }
        None => {
            let forced_from = {
                let mut episodes = open().lock().unwrap();
                let Some(episode) = episodes.get_mut(&run.id) else { return };
                if episode.closing {
                    return;
                }
                episode.closing = true;
                episode.forced_from.clone()
            };

            let (resolved, status_changed) = if let Some(previous) = &forced_from {
                match sqlx::query("UPDATE runs SET agent_status=? WHERE id=? AND state='running' AND agent_status='blocked'")
                    .bind(previous)
                    .bind(&run.id)
                    .execute(app.db())
                    .await
                {
                    Ok(result) if result.rows_affected() == 1 => (true, true),
                    Ok(_) => match sqlx::query_as::<_, (String, String)>("SELECT state, agent_status FROM runs WHERE id=?")
                        .bind(&run.id)
                        .fetch_optional(app.db())
                        .await
                    {
                        Ok(Some((state, status))) => (state != "running" || status != "blocked", false),
                        Ok(None) => (true, false),
                        Err(error) => {
                            tracing::warn!(run = %run.id, error = ?error, "could not verify synthetic Dangerous rm blocked status");
                            (false, false)
                        }
                    },
                    Err(error) => {
                        tracing::warn!(run = %run.id, error = ?error, "could not restore synthetic Dangerous rm blocked status");
                        (false, false)
                    }
                }
            } else {
                (true, false)
            };
            if !resolved {
                if let Some(episode) = open().lock().unwrap().get_mut(&run.id) {
                    episode.closing = false;
                }
                return;
            }
            open().lock().unwrap().remove(&run.id);
            tracing::info!(run = %run.id, bot = %run.bot_id, "Dangerous rm 確認框關掉了（回答或自動拒絕）");
            if let Ok(conv) = db::conversation_id(app.db(), &run.bot_id).await {
                let _ = app.insert_message(&conv, None, "system", CLOSED_NOTE, "system", false, None).await;
            }
            if status_changed {
                app.emit_bot_status(&run.bot_id).await;
            }
            app.schedule_flush_queued(&run.bot_id);
        }
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;
    use crate::testing as tt;
    use crate::tui_prompts::screens::{DANGEROUS_RM, DANGEROUS_RM_AUTO_DENIED};

    /// 記錄是全域的，[`forget_ended`] 會把「在這個測試 DB 讀不到」的別的測試的記錄清掉：這個模組的測試一個一個跑。
    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        static L: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        L.lock().await
    }

    async fn run_of(app: &impl crate::capabilities::Db, run_id: &str) -> db::Run {
        sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id=?").bind(run_id).fetch_one(app.db()).await.unwrap()
    }

    async fn system_messages(app: &impl crate::capabilities::Db, bot_id: &str) -> Vec<String> {
        let conv = db::conversation_id(app.db(), bot_id).await.unwrap();
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at, rowid")
            .bind(conv)
            .fetch_all(app.db())
            .await
            .unwrap()
    }

    /// 真畫面：通知只寫一次、帶目標與指令；herdr 判成 idle 時補標 blocked；倒數到 0 框不見了 → 說明、還原成 idle；
    /// 從頭到尾一個鍵、一個字都沒送進 pane。
    #[tokio::test]
    async fn the_prompt_is_announced_once_marked_blocked_and_released_after_the_auto_deny() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm").await;
        let run_id = tt::fake_run(&app, &bot.id).await;

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        let msgs = system_messages(&app, &bot.id).await;
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("command substitution output") && msgs[0].contains("zz-nonexistent)/*"), "目標與指令都要寫：{}", msgs[0]);
        assert!(msgs[0].contains("不會替你按"), "{}", msgs[0]);
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "herdr 判 idle 時補標");
        assert!(is_open(&run_id));

        // 巡邏再看到同一個框（倒數在跳、畫面在變）：不再寫。
        let later = DANGEROUS_RM.replace("in 1:52", "in 0:31");
        observe_screen(&app, &run_of(&app, &run_id).await, &later).await;
        assert_eq!(system_messages(&app, &bot.id).await.len(), 1, "同一個框只講一次");

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        let msgs = system_messages(&app, &bot.id).await;
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[1], CLOSED_NOTE);
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle", "補標的 blocked 還回去");
        assert!(!is_open(&run_id));

        for m in ["pane.send_keys", "pane.send_text", "agent.prompt"] {
            assert!(e.herdr.calls_to(m).is_empty(), "不能替使用者按、也不能打字進框：{m}");
        }
    }

    /// herdr 自己判成 blocked 的（常見情形）不補標，框關掉時也不去改——那是 herdr 的狀態，它會自己報下一個。
    #[tokio::test]
    async fn a_blocked_status_that_herdr_set_is_left_to_herdr() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked");
        assert_eq!(system_messages(&app, &bot.id).await.len(), 2);
    }

    /// 補標之後 herdr 報了別的狀態（例如使用者答完它開始 working）：還原是 CAS，不蓋掉 herdr 的新狀態。
    #[tokio::test]
    async fn restoring_does_not_overwrite_a_newer_herdr_status() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        crate::lifecycle::take_scheduled_flush_count(&bot.id);
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "working");
        assert!(!is_open(&run_id), "the newer herdr status authoritatively superseded our marker");
        assert_eq!(system_messages(&app, &bot.id).await.last().map(String::as_str), Some(CLOSED_NOTE));
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 1);
    }

    #[tokio::test]
    async fn a_herdr_blocked_status_after_the_synthetic_marker_is_not_restored_to_idle() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-herdr-blocked").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "dangerous-rm detection adds a synthetic blocked status");

        let event = |status: &str| crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: serde_json::json!({"pane_id": pane, "agent_status": status}),
        };
        crate::runners::events::handle_status(&app, crate::config::LOCAL_HOST, "test", &event("working")).await;
        crate::runners::events::handle_status(&app, crate::config::LOCAL_HOST, "test", &event("blocked")).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "herdr's question must remain blocked");

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;

        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "closing the old rm episode must not erase herdr's later blocked state");
        assert!(!is_open(&run_id));
    }

    #[tokio::test]
    async fn an_ended_run_releases_the_synthetic_blocked_episode() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-ended").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?")
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        crate::lifecycle::take_scheduled_flush_count(&bot.id);

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;

        let run = run_of(&app, &run_id).await;
        assert_eq!(run.state, "exited");
        assert_eq!(run.agent_status, "blocked", "don't rewrite an ended run");
        assert!(!is_open(&run_id), "the ended run authoritatively resolves the restore debt");
        assert_eq!(system_messages(&app, &bot.id).await.len(), 2);
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 1);
    }

    /// A failed restore is still owed: keep the episode for the next patrol, and don't announce
    /// closure or wake queued prompts until the database accepts the restore.
    #[tokio::test]
    async fn a_failed_restore_keeps_the_episode_until_a_later_patrol_succeeds() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-restore").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let run = run_of(&app, &run_id).await;
        let conversation = db::conversation_id(&app.db, &bot.id).await.unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending','next prompt',?)",
        )
        .bind(db::ulid())
        .bind(&conversation)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        observe_screen(&app, &run, DANGEROUS_RM).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked");
        crate::lifecycle::take_scheduled_flush_count(&bot.id);
        sqlx::query(
            "CREATE TRIGGER refuse_dangerous_rm_restore BEFORE UPDATE OF agent_status ON runs
             WHEN OLD.agent_status='blocked' AND NEW.agent_status='idle' BEGIN SELECT RAISE(ABORT, 'database is locked'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "restore did not commit");
        assert!(is_open(&run_id), "the patrol needs the retained episode to retry");
        assert_eq!(system_messages(&app, &bot.id).await.len(), 1, "don't announce closure before restore");
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 0, "don't wake the queue yet");

        sqlx::query("DROP TRIGGER refuse_dangerous_rm_restore")
            .execute(&app.db)
            .await
            .unwrap();
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle");
        assert!(!is_open(&run_id));
        assert_eq!(system_messages(&app, &bot.id).await, [notice(&crate::tui_prompts::dangerous_rm_prompt(DANGEROUS_RM).unwrap()), CLOSED_NOTE.to_string()]);
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 1);

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(system_messages(&app, &bot.id).await.len(), 2, "closure note is written once");
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 0, "resolved episode wakes the queue once");
    }

    /// 稽核：框開著時 run 結束（pane 被關、bot 停掉）。巡邏只掃 active run，這筆記錄再也等不到「框關了」，
    /// 不拿掉就一直留在行程記憶體裡。
    #[tokio::test]
    async fn an_episode_of_a_run_that_ended_while_the_box_was_open_is_forgotten() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-gone").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        assert!(is_open(&run_id));

        // 還在跑：不動。
        let active = vec![run_of(&app, &run_id).await];
        forget_ended(&app, &[]).await;
        assert!(is_open(&run_id), "DB 說還在進行中（只是這一輪名單沒帶到）：留著");
        forget_ended(&app, &active).await;
        assert!(is_open(&run_id));

        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        forget_ended(&app, &[]).await;
        assert!(!is_open(&run_id), "結束的 run 不留記錄");
    }

    /// 稽核：兩個不同的指令都落在同一句通用警語（`command substitution output`）時，以前只比警語，第二個框
    /// 沒人講——使用者看到的目標與指令還是第一個。框的內容不同就是新的一次，要再講一次，補標的 blocked 沿用。
    #[tokio::test]
    async fn a_second_box_with_the_same_warning_but_another_command_is_announced_again() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-two-cmds").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        let second = DANGEROUS_RM.replace("which-dir", "other-dir");
        assert_eq!(
            crate::tui_prompts::dangerous_rm_prompt(DANGEROUS_RM).unwrap().warning,
            crate::tui_prompts::dangerous_rm_prompt(&second).unwrap().warning,
            "前提：警語一樣"
        );
        observe_screen(&app, &run_of(&app, &run_id).await, &second).await;
        let msgs = system_messages(&app, &bot.id).await;
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert!(msgs[1].contains("other-dir"), "第二則要寫第二個指令：{}", msgs[1]);

        // 同一個框再被看到：不重複。框關掉：補標的 blocked 還回去（沿用第一個框補的標記）。
        observe_screen(&app, &run_of(&app, &run_id).await, &second).await;
        assert_eq!(system_messages(&app, &bot.id).await.len(), 2);
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle");
        assert!(!is_open(&run_id));
    }
}
