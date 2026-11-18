#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::testing as tt;
    use serde_json::json;
    async fn codex_run(env: &tt::Env, name: &str) -> (String, String) {
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run = tt::fake_run(&env.app, &bot.id).await;
        (bot.id, run)
    }
    async fn run_blocked_reason(env: &tt::Env, bot_id: &str) -> serde_json::Value {
        let state = crate::api::state_json(&env.app).await.unwrap();
        for p in state["projects"].as_array().unwrap() {
            for b in p["bots"].as_array().unwrap() {
                if b["id"] == bot_id {
                    return b["run"]["blocked_reason"].clone();
                }
            }
        }
        panic!("bot not in state");
    }
    /// 擋住輸入列的畫面開著：run 上帶結構化的 `blocked_reason {code, text}`（`/api/state` 與 `bot_status` WS 事件同一份），
    /// 關掉就清掉；網頁不必從對話裡的系統訊息猜。
    #[tokio::test]
    async fn a_blocking_codex_dialog_puts_a_code_and_text_on_the_run_and_clears_with_it() {
        let env = tt::env().await;
        let (bot_id, run_id) = codex_run(&env, "reason").await;
        assert_eq!(run_blocked_reason(&env, &bot_id).await, json!(null), "沒卡住：null（欄位在、值是 null）");

        let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        crate::runners::codex_model_migration::observe_screen(&env.app, &run, include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-update-menu.txt")).await;
        let r = run_blocked_reason(&env, &bot_id).await;
        assert_eq!(r["code"], "codex_update_menu");
        assert!(r["text"].as_str().unwrap().contains("更新"), "{r}");

        // WS：bot_status 事件帶同一份。
        let mut rx = env.app.subscribe();
        env.app.emit_bot_status(&bot_id).await;
        let frame = std::iter::from_fn(|| rx.try_recv().ok()).find(|f| f.kind == "bot_status").expect("bot_status");
        assert_eq!(frame.data["run"]["blocked_reason"]["code"], "codex_update_menu");

        // 換成別種畫面，原因跟著換（同一個 run 的 episode 不重講，但原因要對）。
        let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        crate::runners::codex_model_migration::observe_screen(&env.app, &run, "› Ask Codex to do anything\n").await;
        assert_eq!(run_blocked_reason(&env, &bot_id).await, json!(null), "關掉就清掉");
    }
    /// claude 一般權限確認選單：herdr 判成 blocked 的那一刻讀一次畫面，認得就帶 `permission_prompt`、text 帶工具名
    /// （「等待權限確認：Bash」），**一個鍵都不按**；選單關掉（或狀態不再是 blocked）就清掉。
    #[tokio::test]
    async fn a_claude_permission_menu_gives_permission_prompt_with_the_tool_and_clears() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "perm").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET pane_id='pane-perm', agent_status='blocked' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
        env.herdr.set_screen("pane-perm", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-bash-permission.txt"));
        let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();

        crate::blocked_reason::observe(&env.app, &run).await;
        let r = run_blocked_reason(&env, &bot.id).await;
        assert_eq!(r["code"], "permission_prompt");
        assert_eq!(r["text"], "等待權限確認：Bash");
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "不替使用者按任何鍵");
        assert!(env.herdr.calls_to("pane.send_text").is_empty());

        // 換成另一個工具的框：原因跟著換。
        env.herdr.set_screen("pane-perm", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-fetch-permission.txt"));
        crate::blocked_reason::observe(&env.app, &run).await;
        assert_eq!(run_blocked_reason(&env, &bot.id).await["text"], "等待權限確認：Fetch");

        // 選單關掉：清掉。
        env.herdr.set_screen("pane-perm", "────────────\n❯\n────────────\n");
        crate::blocked_reason::observe(&env.app, &run).await;
        assert_eq!(run_blocked_reason(&env, &bot.id).await, json!(null));

        // 讀不到畫面（pane 不存在）：不動、不報錯、不亂清也不亂設。
        crate::blocked_reason::forget(&run_id);
        sqlx::query("UPDATE runs SET pane_id=NULL WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
        let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        crate::blocked_reason::observe(&env.app, &run).await;
        assert_eq!(run_blocked_reason(&env, &bot.id).await, json!(null));
    }
    /// 權限框只有在 run 現在是 blocked 時才算數：herdr 已經報了別的狀態（選單關了、回合繼續），記憶體裡晚收的舊判斷不能掛著；
    /// 不是 claude、或 daemon 沒看過畫面的 blocked（原因不明）都是 null。
    #[tokio::test]
    async fn a_permission_reason_never_outlives_the_blocked_status_and_codex_is_left_alone() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "perm2").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET pane_id='pane-perm2', agent_status='blocked' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
        env.herdr.set_screen("pane-perm2", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-write-permission.txt"));
        let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        crate::blocked_reason::observe(&env.app, &run).await;
        assert_eq!(run_blocked_reason(&env, &bot.id).await["code"], "permission_prompt");
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
        assert_eq!(run_blocked_reason(&env, &bot.id).await, json!(null), "不是 blocked 就不帶");

        // codex bot：同一張畫面不歸這裡（codex 有自己的對話框偵測），不設原因。
        let (cbot, crun) = codex_run(&env, "perm-codex").await;
        sqlx::query("UPDATE runs SET pane_id='pane-perm3', agent_status='blocked' WHERE id=?").bind(&crun).execute(&env.app.db).await.unwrap();
        env.herdr.set_screen("pane-perm3", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-write-permission.txt"));
        let run = crate::db::run(&env.app.db, &crun).await.unwrap().unwrap();
        crate::blocked_reason::observe(&env.app, &run).await;
        assert_eq!(run_blocked_reason(&env, &cbot).await, json!(null));
    }
    /// 三種 codex 畫面各有自己的代碼；沒有 blocked 的 run、或已經不是 blocked 的狀態，不會殘留舊原因。
    #[tokio::test]
    async fn each_dialog_has_its_own_code_and_a_stale_reason_never_shows_on_a_run_that_is_not_blocked() {
        let env = tt::env().await;
        for (name, screen, code) in [
            ("m", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-model-migration.txt"), "codex_migration"),
            ("r", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-rate-limit-switch.txt"), "rate_limit_switch"),
        ] {
            let (bot_id, run_id) = codex_run(&env, name).await;
            let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();
            crate::runners::codex_model_migration::observe_screen(&env.app, &run, screen).await;
            assert_eq!(run_blocked_reason(&env, &bot_id).await["code"], code);
            // herdr 自己報了新的狀態（working）：記憶體裡的 episode 還沒收，但 run 已經不是 blocked——不能還掛著原因。
            sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
            assert_eq!(run_blocked_reason(&env, &bot_id).await, json!(null), "{code}：status 不是 blocked 就不帶原因");
        }
    }
}
