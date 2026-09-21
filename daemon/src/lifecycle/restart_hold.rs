//! 重啟中的 bot（issue #106）：stop 與 start 之間那一段「沒有 active run」不是「bot 不在了」。
//!
//! `revoke_orphaned_queued_turns` 的判準是「這顆 bot 沒有 active run ＝ 排著的 prompt 沒有人會送」。
//! 重啟（`restart_bot_with`：換身分、`?resume=native`、一鍵重啟）先停舊 run 再起新 run，中間那一段
//! 剛好符合這個判準，於是 AGM 排著的派工被當成孤兒撤掉——換身分之後什麼都不會送，`resume_gate`
//! 再完美也沒有東西可以放行。會在那一段撤掉它的不只 stop 自己：`restart_start` 收掉擋路 run 的
//! `mark_run_exited`、不拿 bot 鎖的 pane-exit 事件（`events::end_runs_for_pane`）、定時的
//! `revoke_all_orphaned_queued_turns` 都會。所以不在各處補判斷，而是讓重啟**宣告**自己在進行中，
//! 撤孤兒的那一支只問這一件事。
//!
//! hold 本身是行程記憶體，但重啟的**意圖**是持久的（`restart` intent，#355）：daemon 在 stop 與 start 之間死掉，
//! 開機由 `restart_intents::recover_host` 往前補完那次重啟，所以排著的派工同樣不是孤兒（#378）。
//! 開機時（對帳之前，那會把舊 run 收尾並撤孤兒）[`adopt_open_intents`] 把每件還開著的 `restart` intent 灌成一個 hold，
//! recovery 把那件 intent 收尾（done／abandoned／failed／過期）才放掉；補不成的重試期間 hold 也留著。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn holds() -> &'static Mutex<HashMap<String, u32>> {
    static M: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 重啟進行中的憑證；drop 掉就結束。計數而不是布林：同一顆 bot 理論上不會重疊（都在 bot 鎖裡），
/// 真的重疊了也不會被先結束的那一個提早清掉。
pub(crate) struct Hold {
    bot_id: String,
}

pub(crate) fn begin(bot_id: &str) -> Hold {
    if let Ok(mut m) = holds().lock() {
        *m.entry(bot_id.to_string()).or_insert(0) += 1;
    }
    Hold { bot_id: bot_id.to_string() }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Ok(mut m) = holds().lock() {
            if let Some(n) = m.get_mut(&self.bot_id) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&self.bot_id);
                }
            }
        }
    }
}

/// 開機接回的 hold：(資料目錄, intent id) → 憑證。`recover_host` 收掉那件 intent 才放。
/// 帶資料目錄是因為「只留還開著的」（[`retain_open`]）只能對**同一個 DB** 的 intent 下判斷：測試共用同一個行程，
/// 另一個 App 的 `recover_host` 看到的開著的 intent 不含我們的，不能因此把我們的 hold 放掉（整樹負載下偶發紅）。
fn adopted() -> &'static Mutex<HashMap<(String, String), Hold>> {
    static M: OnceLock<Mutex<HashMap<(String, String), Hold>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 開機（**對帳之前**）把還開著的 `restart` intent 各灌一個 hold。讀不到 DB 只能記 log（此時什麼都撤不了以外的事無從判斷），
/// 之後 `recover_host` 照樣會補完，只是這段視窗沒有保護。
pub(crate) async fn adopt_open_intents(app: &crate::state::App) {
    match crate::intents::open(&app.db).await {
        Ok(open) => {
            for i in open.into_iter().filter(|i| i.kind == "restart") {
                let hold = begin(&i.subject_id);
                if let Ok(mut m) = adopted().lock() {
                    m.entry((owner(app), i.id)).or_insert(hold);
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot list open restart intents; queued prompts are unprotected until recovery"),
    }
}

/// 那件 intent 已經收尾：放掉它的 hold（沒有就什麼都不做）。
pub(crate) fn release_intent(app: &crate::state::App, intent_id: &str) {
    let hold = adopted().lock().ok().and_then(|mut m| m.remove(&(owner(app), intent_id.to_string())));
    drop(hold);
}

fn owner(app: &crate::state::App) -> String {
    app.data_dir.display().to_string()
}

/// 只留還開著的 intent 的 hold；其餘（例如過期被收掉的）放掉。
pub(crate) fn retain_open(app: &crate::state::App, open_ids: &std::collections::HashSet<String>) {
    let me = owner(app);
    let gone: Vec<Hold> = match adopted().lock() {
        Ok(mut m) => {
            let keys: Vec<(String, String)> = m.keys().filter(|(o, id)| *o == me && !open_ids.contains(id)).cloned().collect();
            keys.into_iter().filter_map(|k| m.remove(&k)).collect()
        }
        Err(_) => return,
    };
    drop(gone);
}

/// 這顆 bot 現在是不是在重啟（stop 到 start 之間）。鎖壞了當成「不是」：照舊行為撤孤兒，不會卡住佇列。
pub(crate) fn in_progress(bot_id: &str) -> bool {
    holds().lock().map(|m| m.contains_key(bot_id)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{begin, in_progress};
    use crate::testing as tt;

    async fn queued_turn(app: &Arc<App>, bot_id: &str) -> String {
        let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
        let id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&id)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    async fn status(app: &Arc<App>, turn_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn_id).fetch_one(&app.db).await.unwrap()
    }

    /// 不拿 bot 鎖的兩條路（定時 sweeper、pane-exit 事件的 `mark_run_exited`）在重啟那一段都不撤；
    /// 重啟結束（憑證 drop）之後，沒有 run 的照舊是孤兒、照舊撤。
    #[tokio::test]
    async fn nothing_revokes_a_queued_prompt_while_its_bot_is_restarting() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "restarting").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let queued = queued_turn(&app, &bot.id).await;

        let hold = begin(&bot.id);
        assert!(in_progress(&bot.id));
        mark_run_exited(&app, &run, "pane exited").await;
        assert_eq!(status(&app, &queued).await, "queued", "pane-exit 事件在重啟途中到：不撤");
        assert!(revoke_all_orphaned_queued_turns(&app).await.is_empty(), "定時 sweeper 也不撤");
        assert_eq!(status(&app, &queued).await, "queued");

        drop(hold);
        assert!(!in_progress(&bot.id));
        assert_eq!(revoke_all_orphaned_queued_turns(&app).await, vec![queued.clone()], "重啟結束還是沒有 run：真的孤兒，撤");
        assert_eq!(status(&app, &queued).await, "failed");
    }

    /// 重啟沒能把 bot 開回來：這時候排著的才是真的孤兒，撤掉並說明原因（issue #106 驗收第二條）。
    #[tokio::test]
    async fn a_restart_that_cannot_bring_the_bot_back_revokes_what_was_queued() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "left-down").await;
        start_bot(&app, &bot.id).await.unwrap();
        let queued = queued_turn(&app, &bot.id).await;
        // 換成這台沒有的身分：stop 做完之後 start 會拒絕（不會退回預設帳號，GH #83）。
        sqlx::query("UPDATE bots SET identity='ghost' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        assert!(restart_bot_with(&app, &bot.id, StartOpts::default()).await.is_err());
        assert!(db::active_run(&app.db, &bot.id).await.unwrap().is_none(), "bot 沒回來");
        assert!(!in_progress(&bot.id), "重啟結束了");
        assert_eq!(status(&app, &queued).await, "failed");
        let why: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&queued)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(why.contains("重啟之後沒能把 bot 開回來"), "{why}");
    }

    /// 一般的 stop（不是重啟）照舊撤：使用者要它停，就沒有人會送。
    #[tokio::test]
    async fn a_plain_stop_still_revokes_what_was_queued() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "stopped").await;
        start_bot(&app, &bot.id).await.unwrap();
        let queued = queued_turn(&app, &bot.id).await;
        stop_bot(&app, &bot.id).await.unwrap();
        assert_eq!(status(&app, &queued).await, "failed");
    }

    /// 巢狀的憑證：先結束的那一個不會把還在進行的清掉。
    #[test]
    fn a_hold_ends_only_when_the_last_one_is_dropped() {
        let id = format!("b-{}", crate::db::ulid());
        let a = begin(&id);
        let b = begin(&id);
        drop(a);
        assert!(in_progress(&id));
        drop(b);
        assert!(!in_progress(&id));
    }
}
