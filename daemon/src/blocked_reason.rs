//! run 為什麼停在 `blocked`：結構化的 `{code, text}`，跟著 run 一起出現在 `/api/state` 與 `bot_status` 事件（`run.blocked_reason`）。
//!
//! 以前原因只寫在對話裡的一則系統訊息，網頁的 bot 狀態旁看不到、也沒辦法在選單關掉時跟著清掉。這裡**不存 DB**：
//! 原因來自各處已經在記憶體裡追蹤的「擋住輸入列的畫面」episode（codex 的三種對話框、claude 的防誤刪框與 Session paused 選單），
//! 讀的時候才組出來，所以 episode 一收（選單關了、run 結束）原因就跟著沒了，不會殘留；而且只在 `agent_status == "blocked"` 時才帶，
//! herdr 自己報了別的狀態就不再掛著舊原因。舊的前端忽略這個欄位，什麼都不壞。
//!
//! `code` 是給程式認的短代碼（穩定），`text` 是給人看的一句話（可能改字）。

use serde_json::{json, Value};

/// 一個 run 現在停住的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub code: &'static str,
    pub text: &'static str,
}

/// 這個 run 現在被哪個擋住輸入列的畫面卡著（沒有＝`None`）。只看記憶體裡的 episode，不讀 DB、不讀畫面。
pub fn of(run_id: &str) -> Option<Reason> {
    if let Some(d) = crate::codex_model_migration::open_dialog(run_id) {
        return Some(d.reason());
    }
    if crate::dangerous_rm::is_open(run_id) {
        return Some(Reason { code: "dangerous_rm", text: "claude 防誤刪（Dangerous rm）確認框等待回答" });
    }
    if crate::session_paused::is_forced(run_id) {
        return Some(Reason { code: "session_paused", text: "claude Session paused 選單等待選擇" });
    }
    None
}

/// 放進 run JSON 的值：`{code, text}`，沒有原因是 `null`。
pub fn json(run_id: &str) -> Value {
    match of(run_id) {
        Some(r) => json!({"code": r.code, "text": r.text}),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
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
        crate::codex_model_migration::observe_screen(&env.app, &run, include_str!("lifecycle/fixtures/codex-0.155-update-menu.txt")).await;
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
        crate::codex_model_migration::observe_screen(&env.app, &run, "› Ask Codex to do anything\n").await;
        assert_eq!(run_blocked_reason(&env, &bot_id).await, json!(null), "關掉就清掉");
    }

    /// 三種 codex 畫面各有自己的代碼；沒有 blocked 的 run、或已經不是 blocked 的狀態，不會殘留舊原因。
    #[tokio::test]
    async fn each_dialog_has_its_own_code_and_a_stale_reason_never_shows_on_a_run_that_is_not_blocked() {
        let env = tt::env().await;
        for (name, screen, code) in [
            ("m", include_str!("lifecycle/fixtures/codex-0.157-model-migration.txt"), "codex_migration"),
            ("r", include_str!("lifecycle/fixtures/codex-0.157-rate-limit-switch.txt"), "rate_limit_switch"),
        ] {
            let (bot_id, run_id) = codex_run(&env, name).await;
            let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();
            crate::codex_model_migration::observe_screen(&env.app, &run, screen).await;
            assert_eq!(run_blocked_reason(&env, &bot_id).await["code"], code);
            // herdr 自己報了新的狀態（working）：記憶體裡的 episode 還沒收，但 run 已經不是 blocked——不能還掛著原因。
            sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
            assert_eq!(run_blocked_reason(&env, &bot_id).await, json!(null), "{code}：status 不是 blocked 就不帶原因");
        }
    }
}
