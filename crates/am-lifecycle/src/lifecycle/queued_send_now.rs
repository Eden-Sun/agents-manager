//! 網頁「送出排隊的補充」（`POST /api/bots/:id/send-queued-now`）。
//!
//! 補充打進 pane 時 claude 若正在跑工具，CLI 把它排進佇列（畫面出現 `Press up to edit queued messages`），
//! 要等工具結束才會送進去。claude 的 send-now 鍵（`ctrl+x ctrl+s`，[`Submit::SendNow`]）會打斷這一輪、立刻送出佇列。
//! 佇列是空的時按它會白白打斷回合，所以**先讀畫面確認有排隊才按**；沒有就 409 `nothing_queued`，一個鍵都不送。

use super::*;

const QUEUED_MARKER: &str = "Press up to edit queued messages";

/// 畫面上有沒有「訊息排在佇列」的提示行。
pub fn screen_has_queued(screen: &str) -> bool {
    screen.lines().any(|l| l.contains(QUEUED_MARKER))
}

pub async fn send_queued_now(
    app: &(impl crate::capabilities::BotLocks + crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess),
    bot_id: &str,
    expect_run_id: Option<String>,
) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    if let Some(exp) = expect_run_id {
        if exp != run.id {
            return Err(LcError::conflict("run mismatch", json!({"run_id": run.id})));
        }
    }
    if super::send_now::supported(&bot.kind, run.status_json.as_deref()).is_err() {
        return Err(LcError::conflict("send_now_unsupported", json!({"run_id": run.id, "sent": false})));
    }
    let client = client_for_run(app, &run).await?;
    let pane = run.pane_id.clone().filter(|p| !p.trim().is_empty()).ok_or_else(|| LcError::NotFound("pane".into()))?;
    let screen = client.pane_read(&pane, "visible", 60).await.map_err(|e| up(e))?.text;
    if !screen_has_queued(&screen) {
        return Err(LcError::conflict("nothing_queued", json!({"run_id": run.id, "sent": false})));
    }
    let target = super::dead_panes::execution_target(&client, &run, &bot).await?;
    let keys: Vec<String> = delivery::Submit::SendNow.keys().iter().map(|k| k.to_string()).collect();
    client.agent_send_keys(&target, &keys).await.map_err(up)?;
    Ok(())
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod tests {
    use super::screen_has_queued;

    #[test]
    fn detects_queue_hint() {
        let s = "● Bash(sleep 600)\n  ⎿  Running…\n\n  Press up to edit queued messages\n╭────╮\n│ > │\n╰────╯";
        assert!(screen_has_queued(s));
    }

    #[test]
    fn plain_screen_has_no_queue() {
        assert!(!screen_has_queued("● Bash(sleep 600)\n  ⎿  Running…\n╭────╮\n│ > │\n╰────╯"));
        assert!(!screen_has_queued(""));
    }
}
