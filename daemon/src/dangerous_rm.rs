//! Claude Code 2.1.281 的「Dangerous rm operation」防誤刪確認框（畫面辨識在 [`crate::tui_prompts::dangerous_rm_prompt`]）。
//!
//! `rm -rf $(…)`、`$VAR`、頂層目錄這類目標，就算 bot 是 `--dangerously-skip-permissions` 起的也會跳這個框，
//! 約 2 分鐘沒人回答 claude 就自動拒絕（那個指令不執行，claude 拿到「被內建安全檢查拒絕」的 tool_result、把回合做完）。
//! 它的用意是**只有人能核准**，所以 daemon 這一側的原則是：
//! * **一個鍵都不按**：不像滿意度問卷、Auto mode 推銷框那樣替使用者選；也不整批設
//!   `CLAUDE_CODE_DISABLE_SUBSTITUTION_RM_PROMPT=1` 把它關掉。
//! * **送達不打進框裡**：`pane_ready_for_prompt` 認到框就回 409 `dangerous_rm_pending`，排隊的留在佇列，框關掉才送。
//! * **通知使用者、帶上目標**：這一次框（同一句警語）在 bot 的對話裡插一則系統訊息，寫警語、目標與指令，只寫一次。
//! * **標成 blocked**：herdr 通常自己就判成 `blocked`（2026-09-23 m12 的 pane 就是）；判成 `idle`／`unknown`
//!   時由這裡補標，框關掉時照原值還回去（CAS：這段期間 herdr 自己改過狀態就不動）。
//! * **框消失後回到正常**：不管是有人回答還是 2 分鐘自動拒絕，都補一則說明、叫醒排隊的 flush。
//!
//! 開著的框記在記憶體（`run_id` → 這一次的警語、補標前的狀態）：daemon 重啟後最多重講一次通知；
//! 重啟當下若是由這裡補標的 `blocked`，交給 herdr 下一次狀態事件或 reconcile 更正。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::db;
use crate::state::App;
use crate::tui_prompts::DangerousRm;

/// herdr 轉成 `blocked` 之後等一下再讀畫面：框還在畫的那一瞬間讀到的是半張。
const SETTLE: Duration = Duration::from_millis(800);

struct Episode {
    warning: String,
    /// 由這裡補標成 `blocked` 之前的狀態；`None` ＝herdr 自己判的，框關掉時不必還。
    forced_from: Option<String>,
}

fn open() -> &'static Mutex<HashMap<String, Episode>> {
    static V: OnceLock<Mutex<HashMap<String, Episode>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

/// 這個 run 現在有沒有開著的框（巡邏靠它把已經不是 idle／blocked 的 run 也看一眼，才收得掉）。
pub fn is_open(run_id: &str) -> bool {
    open().lock().unwrap().contains_key(run_id)
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

/// 這一次框（同一句警語）只講一次；送達閘門與巡邏共用。回傳這次有沒有真的寫。
pub(crate) async fn notify_once(app: &Arc<App>, run: &db::Run, rm: &DangerousRm) -> bool {
    {
        let mut m = open().lock().unwrap();
        match m.get(&run.id) {
            Some(ep) if ep.warning == rm.warning => return false,
            _ => {
                m.insert(run.id.clone(), Episode { warning: rm.warning.clone(), forced_from: None });
            }
        }
    }
    tracing::warn!(run = %run.id, bot = %run.bot_id, target = %rm.target, "claude 停在 Dangerous rm 確認框，等使用者本人核准（不自動按）");
    match db::conversation_id(&app.db, &run.bot_id).await {
        Ok(conv) => {
            let _ = crate::lifecycle::insert_message(app, &conv, None, "system", &notice(rm), "system", false, None).await;
        }
        Err(e) => tracing::warn!(run = %run.id, error = ?e, "could not post the dangerous-rm notice"),
    }
    true
}

/// herdr 轉成 `blocked` 那一刻（[`crate::events`]）：等畫面畫完再看。自己開背景工作，不擋事件迴圈。
pub fn on_blocked(app: &Arc<App>, run: &db::Run) {
    let (app, run) = (app.clone(), run.clone());
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        observe(&app, &run).await;
    });
}

/// 讀一次畫面、照結果開或收（巡邏與 `blocked` 邊共用）。只看 claude；讀不到畫面什麼都不動——讀不到不等於框關了。
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
                .execute(&app.db)
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
            let Some(ep) = open().lock().unwrap().remove(&run.id) else { return };
            tracing::info!(run = %run.id, bot = %run.bot_id, "Dangerous rm 確認框關掉了（回答或自動拒絕）");
            if let Ok(conv) = db::conversation_id(&app.db, &run.bot_id).await {
                let _ = crate::lifecycle::insert_message(app, &conv, None, "system", CLOSED_NOTE, "system", false, None).await;
            }
            if let Some(prev) = ep.forced_from {
                // 只還我們自己標的那個 blocked：這段期間 herdr 已經報了別的狀態就是它的，不蓋。
                let _ = sqlx::query("UPDATE runs SET agent_status=? WHERE id=? AND agent_status='blocked'")
                    .bind(&prev)
                    .bind(&run.id)
                    .execute(&app.db)
                    .await;
                app.emit_bot_status(&run.bot_id).await;
            }
            crate::lifecycle::schedule_flush_queued(app, &run.bot_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use crate::tui_prompts::screens::{DANGEROUS_RM, DANGEROUS_RM_AUTO_DENIED};

    async fn run_of(app: &Arc<App>, run_id: &str) -> db::Run {
        sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id=?").bind(run_id).fetch_one(&app.db).await.unwrap()
    }

    async fn system_messages(app: &Arc<App>, bot_id: &str) -> Vec<String> {
        let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at, rowid")
            .bind(conv)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 真畫面：通知只寫一次、帶目標與指令；herdr 判成 idle 時補標 blocked；倒數到 0 框不見了 → 說明、還原成 idle；
    /// 從頭到尾一個鍵、一個字都沒送進 pane。
    #[tokio::test]
    async fn the_prompt_is_announced_once_marked_blocked_and_released_after_the_auto_deny() {
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
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "working");
    }
}
