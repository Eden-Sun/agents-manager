//! run 為什麼停在 `blocked`：結構化的 `{code, text}`，跟著 run 一起出現在 `/api/state` 與 `bot_status` 事件（`run.blocked_reason`）。
//!
//! 以前原因只寫在對話裡的一則系統訊息，網頁的 bot 狀態旁看不到、也沒辦法在選單關掉時跟著清掉。這裡**不存 DB**：
//! 原因來自各處已經在記憶體裡追蹤的「擋住輸入列的畫面」episode（codex 的三種對話框、claude 的防誤刪框與 Session paused 選單），
//! 讀的時候才組出來，所以 episode 一收（選單關了、run 結束）原因就跟著沒了，不會殘留；而且只在 `agent_status == "blocked"` 時才帶，
//! herdr 自己報了別的狀態就不再掛著舊原因。舊的前端忽略這個欄位，什麼都不壞。
//!
//! `code` 是給程式認的短代碼（穩定），`text` 是給人看的一句話（可能改字）。

use crate::db;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 一個 run 現在停住的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub code: &'static str,
    pub text: String,
}

/// claude 一般權限確認選單：run id → 工具名。blocked 那一刻讀一次畫面（[`observe`]）判斷，只存記憶體。
fn permission() -> &'static Mutex<HashMap<String, String>> {
    static P: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// 不在 `active` 裡的 run（結束了）不留記錄。
#[allow(dead_code)]
pub fn retain_runs(active: &[String]) {
    permission().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
}

/// 這個 run 不再是權限框（狀態離開 blocked、選單關了、run 結束）。
pub fn forget(run_id: &str) {
    permission().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id);
}

/// 讀一次畫面，判斷 blocked 的 claude run 是不是停在一般權限確認選單（[`crate::tui_prompts::permission_prompt`]）：是就記下工具名，
/// 不是就清掉。**只看、一個鍵都不按**；讀不到畫面什麼都不動（讀不到不等於選單關了）。只看 claude：codex 有自己的對話框偵測。
pub async fn observe(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::HerdrRoutes), run: &db::Run) {
    if !matches!(db::bot(app.db(), &run.bot_id).await, Ok(Some(b)) if b.kind == "claude") {
        return;
    }
    let Some(pane) = run.pane_id.as_deref().filter(|p| !p.trim().is_empty()) else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    let before = permission().lock().unwrap_or_else(|e| e.into_inner()).get(&run.id).cloned();
    let now = crate::tui_prompts::permission_prompt(&read.text);
    match &now {
        Some(tool) => {
            permission().lock().unwrap_or_else(|e| e.into_inner()).insert(run.id.clone(), tool.clone());
        }
        None => forget(&run.id),
    }
    if before != now {
        app.emit_bot_status(&run.bot_id).await;
    }
}

/// 這個 run 現在被哪個擋住輸入列的畫面卡著（沒有＝`None`）。只看記憶體裡的 episode，不讀 DB、不讀畫面。
pub fn of(run_id: &str) -> Option<Reason> {
    if let Some(d) = crate::codex_model_migration::open_dialog(run_id) {
        return Some(d.reason());
    }
    if crate::dangerous_rm::is_open(run_id) {
        return Some(Reason { code: "dangerous_rm", text: "claude 防誤刪（Dangerous rm）確認框等待回答".into() });
    }
    if let Some(text) = crate::session_paused::agy_label(run_id) {
        return Some(Reason { code: "agy_dialog", text: text.into() });
    }
    if crate::session_paused::is_forced(run_id) {
        return Some(Reason { code: "session_paused", text: "claude Session paused 選單等待選擇".into() });
    }
    // 一般權限確認（`permission_prompt`）：text 帶工具名。
    if let Some(tool) = permission().lock().unwrap_or_else(|e| e.into_inner()).get(run_id) {
        return Some(Reason { code: "permission_prompt", text: format!("等待權限確認：{tool}") });
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
        crate::runners::codex_model_migration::observe_screen(&env.app, &run, include_str!("lifecycle/fixtures/codex-0.155-update-menu.txt")).await;
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
        env.herdr.set_screen("pane-perm", include_str!("lifecycle/fixtures/claude-2.1.287-bash-permission.txt"));
        let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();

        super::observe(&env.app, &run).await;
        let r = run_blocked_reason(&env, &bot.id).await;
        assert_eq!(r["code"], "permission_prompt");
        assert_eq!(r["text"], "等待權限確認：Bash");
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "不替使用者按任何鍵");
        assert!(env.herdr.calls_to("pane.send_text").is_empty());

        // 換成另一個工具的框：原因跟著換。
        env.herdr.set_screen("pane-perm", include_str!("lifecycle/fixtures/claude-2.1.287-fetch-permission.txt"));
        super::observe(&env.app, &run).await;
        assert_eq!(run_blocked_reason(&env, &bot.id).await["text"], "等待權限確認：Fetch");

        // 選單關掉：清掉。
        env.herdr.set_screen("pane-perm", "────────────\n❯\n────────────\n");
        super::observe(&env.app, &run).await;
        assert_eq!(run_blocked_reason(&env, &bot.id).await, json!(null));

        // 讀不到畫面（pane 不存在）：不動、不報錯、不亂清也不亂設。
        super::forget(&run_id);
        sqlx::query("UPDATE runs SET pane_id=NULL WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
        let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        super::observe(&env.app, &run).await;
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
        env.herdr.set_screen("pane-perm2", include_str!("lifecycle/fixtures/claude-2.1.287-write-permission.txt"));
        let run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        super::observe(&env.app, &run).await;
        assert_eq!(run_blocked_reason(&env, &bot.id).await["code"], "permission_prompt");
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
        assert_eq!(run_blocked_reason(&env, &bot.id).await, json!(null), "不是 blocked 就不帶");

        // codex bot：同一張畫面不歸這裡（codex 有自己的對話框偵測），不設原因。
        let (cbot, crun) = codex_run(&env, "perm-codex").await;
        sqlx::query("UPDATE runs SET pane_id='pane-perm3', agent_status='blocked' WHERE id=?").bind(&crun).execute(&env.app.db).await.unwrap();
        env.herdr.set_screen("pane-perm3", include_str!("lifecycle/fixtures/claude-2.1.287-write-permission.txt"));
        let run = crate::db::run(&env.app.db, &crun).await.unwrap().unwrap();
        super::observe(&env.app, &run).await;
        assert_eq!(run_blocked_reason(&env, &cbot).await, json!(null));
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
            crate::runners::codex_model_migration::observe_screen(&env.app, &run, screen).await;
            assert_eq!(run_blocked_reason(&env, &bot_id).await["code"], code);
            // herdr 自己報了新的狀態（working）：記憶體裡的 episode 還沒收，但 run 已經不是 blocked——不能還掛著原因。
            sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&env.app.db).await.unwrap();
            assert_eq!(run_blocked_reason(&env, &bot_id).await, json!(null), "{code}：status 不是 blocked 就不帶原因");
        }
    }
}
