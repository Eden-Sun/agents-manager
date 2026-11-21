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

/// codex 非同步提問（`request_user_input_async`，issue #1879）：run id → 給人看的一句話（含題目）。
/// 畫面上排著 `? N question · shift+← to answer` 時由 `runners::codex_async_question` 讀畫面與 rollout 後寫入。
fn codex_async() -> &'static Mutex<HashMap<String, String>> {
    static P: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// 記下或清掉這個 run 的 codex 非同步提問原因；回傳有沒有變（呼叫端據此推 `bot_status`）。
pub fn set_codex_async(run_id: &str, text: Option<String>) -> bool {
    let mut map = codex_async().lock().unwrap_or_else(|e| e.into_inner());
    match text {
        Some(t) => map.insert(run_id.to_string(), t.clone()).as_deref() != Some(t.as_str()),
        None => map.remove(run_id).is_some(),
    }
}

/// 不在 `active` 裡的 run（結束了）不留記錄。
#[allow(dead_code)]
pub fn retain_runs(active: &[String]) {
    permission().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
    codex_async().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
}

/// 這個 run 不再是權限框（狀態離開 blocked、選單關了、run 結束）。
pub fn forget(run_id: &str) {
    permission().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id);
    codex_async().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id);
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
    if crate::lifecycle::agy_busy::tool_confirmation_pending(run_id) {
        return Some(Reason {
            code: "agy_dialog",
            text: crate::agy_screen::AgyDialog::Permission.label().into(),
        });
    }
    if crate::session_paused::is_forced(run_id) {
        return Some(Reason { code: "session_paused", text: "claude Session paused 選單等待選擇".into() });
    }
    // 一般權限確認（`permission_prompt`）：text 帶工具名。
    if let Some(tool) = permission().lock().unwrap_or_else(|e| e.into_inner()).get(run_id) {
        return Some(Reason { code: "permission_prompt", text: format!("等待權限確認：{tool}") });
    }
    if let Some(text) = codex_async().lock().unwrap_or_else(|e| e.into_inner()).get(run_id) {
        return Some(Reason { code: "codex_async_question", text: text.clone() });
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
