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
            if run.agent_status == "blocked" {
                if db::compare_and_set_running_run_agent_status(app.db(), run, "blocked", "blocked").await.unwrap_or(false) {
                    notify_once(app, run, &rm).await;
                }
                return;
            }
            // herdr 沒把它判成 blocked：補標，燈號與「需要回應」才會亮，一般送出也照 blocked 擋。
            let prev = run.agent_status.clone();
            let marked = db::compare_and_set_running_run_agent_status(app.db(), run, &prev, "blocked").await.unwrap_or(false);
            if marked {
                notify_once(app, run, &rm).await;
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
                match db::compare_and_set_running_run_agent_status(app.db(), run, "blocked", previous).await {
                    Ok(true) => (true, true),
                    Ok(false) => match db::is_current_running_run(app.db(), run).await {
                        Ok(true) => (true, false),
                        Ok(false) => {
                            open().lock().unwrap().remove(&run.id);
                            return;
                        }
                        Err(error) => {
                            tracing::warn!(run = %run.id, error = ?error, "could not verify current Dangerous rm run");
                            (false, false)
                        }
                    },
                    Err(error) => {
                        tracing::warn!(run = %run.id, error = ?error, "could not restore synthetic Dangerous rm blocked status");
                        (false, false)
                    }
                }
            } else {
                if !db::is_current_running_run(app.db(), run).await.unwrap_or(false) {
                    open().lock().unwrap().remove(&run.id);
                    return;
                }
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
