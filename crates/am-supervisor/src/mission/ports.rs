//! mission 對其他 feature 的窄介面（crate 拆分第 3 步 P7，行為不變）。
//!
//! mission／group／relay_auth 的 production 程式碼不再直接 `crate::supervisor::…`／`crate::quota::…`／
//! `crate::tools::…`／`crate::lifecycle::…` 的函式、也不再 `crate::api::…` 借 helper：每個外部能力是這裡的一個小 trait
//! （一個領域一個），由 `App`（或 repo context `SqlitePool`）實作在 `app_ports_p7.rs`，實作逐行委派給原本被呼叫的函式，
//! 所以任務、額度、鎖與事件的語意不變。呼叫端寫成 `app.verified_bot_id(&headers)`，跟原本
//! `supervisor::bot_requests::verified_bot_id(app, &headers)` 同一個 await 點、同一個錯誤型別。
//!
//! 為什麼不是 `am-ports` 的 `TurnControl`／`QuotaAccess`／`EventSink`：這些合約以 am-core 的值型別表達，而 mission 現行
//! 用的是帶 `LcError`／`PromptOut`／`db::Bot`／`quota::Quota` 的細節（含 401／403／409 的區分、群組送字的 `PromptOut`），
//! 硬塞進合約會丟掉這些分類。這些 trait 是合約之上的 daemon 側過渡介面：型別（`LcError`、`db::*`、`quota::*`）搬進
//! am-core／am-store 之後，簽名換成合約型別、trait 搬進 `am-ports`（`crates/am-ports/src/mission.rs`）。
//!
//! 刻意不做的事：不提供「包住整個 App 的 context」，每個 trait 只含一個領域的方法；`async fn in trait` 只用在這些具體
//! 實作的靜態呼叫（`App`／`SqlitePool` 是唯一實作者）。方法名刻意跟 `App` 的 inherent 方法不同名（`emit_event`
//! 而不是 `emit`），免得 inherent 方法優先、呼叫端悄悄繞過這道介面。
//!
//! 還沒切掉的邊（型別層，等型別搬進 core／store 再換）：`lifecycle::{LcError, LcResult, PromptOut}`、
//! `supervisor::store::{Assignment, OPEN_STATES}`、`quota::{Quota, Window, LimitHit, Bucket}`、
//! `config::{LOCAL_HOST, agent_name}`、`herdr::AgentInfo`、`db::*`，以及 `Arc<App>` 出現在 handler／函式簽名上。

#![allow(async_fn_in_trait)]

use crate::db;
use crate::lifecycle::{LcError, LcResult, PromptOut};
use crate::quota::Quota;
use axum::http::HeaderMap;
use serde_json::Value;
use std::future::Future;

/// AGM 的兩個角色（誰是「收 mission 事件的那個 AGM」）。跟 `supervisor::roles::Role` 一一對應，
/// 但 mission 只認這兩個名字，不認 supervisor 的角色表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgmRole {
    Patrol,
    Responder,
}

/// 這台主機已知的 claude 身分，給 [`crate::mission::candidates`] 挑候選用。
/// `shares_default`＝這個身分沒有自己的 home 變數、就是 CLI 預設帳號（`quota::identity_shares_default`，
/// 跟額度 key 的收斂規則同一份）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownIdentity {
    pub kind: String,
    pub name: String,
    pub shares_default: bool,
}

/// 挑身分用的額度與帳號查詢（原 `quota::*`、`tools::identities_for_host`、`app.quotas`）。
pub trait IdentityOps {
    /// `quota::billing_identity`：這顆 bot 現在在用哪個帳號；沒設身分的預設帳號回 `None`。
    async fn billing_identity(&self, bot: &db::Bot) -> anyhow::Result<Option<String>>;
    /// `tools::identities_for_host`，每個身分附上 `quota::identity_shares_default` 的結果。
    async fn known_identities(&self, host: &str) -> Vec<KnownIdentity>;
    /// `quota::quota_base_for_host`：這個身分的額度讀數在哪一把 key（寫入端同一條規則）。
    async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String;
    /// 一次上鎖讀出每個 `base` 在 `quota::quota_key(host, base)` 底下的讀數（順序同 `bases`）。
    /// 一次上鎖＝同一批候選看到的是同一個時間點的額度表。
    async fn quota_readings(&self, host: &str, bases: &[String]) -> Vec<Option<Quota>>;
}

/// 呼叫端是誰（原 `supervisor::bot_requests`／`supervisor::roles` 的身分驗證）。
pub trait CallerOps {
    /// `bot_requests::verified_bot_id`：標頭宣告的 bot 身分驗過才回；宣告了驗不過是 `Err`。
    async fn verified_bot_id(&self, headers: &HeaderMap) -> Result<Option<String>, LcError>;
    /// `bot_requests::actor_role`：驗過的呼叫端是不是某個 AGM 角色。
    async fn actor_role(&self, headers: &HeaderMap) -> Result<Option<AgmRole>, LcError>;
    /// `roles::role_of_bot`：這顆 bot 是不是某個 AGM 角色（不看標頭）。
    async fn role_of_bot(&self, bot_id: &str) -> anyhow::Result<Option<AgmRole>>;
    /// `roles::responder_configured`：有沒有設協調者。
    async fn responder_configured(&self) -> anyhow::Result<bool>;
    /// `api::ct_eq`：常數時間字串比較（token 比對用，不能換成會短路的 `==`）。
    fn ct_eq(&self, a: &str, b: &str) -> bool;
}

/// 取消任務時對交辦、臨時 bot 的操作（原 `supervisor::lock`、`supervisor::api::post_review`、`api::delete_bot`）。
pub trait SupervisorOps {
    /// `supervisor::lock` 回的守衛：握著它＝握著 supervisor 全域鎖，drop 放鎖。
    type OpGuard: Send;
    /// `supervisor::lock`。刻意不是 `async fn`：`#[track_caller]` 在 `async fn` 上抓不到呼叫端，
    /// 鎖統計（issue #473）要的是「哪一行拿鎖」，所以寫成同步函式回 future。
    #[track_caller]
    fn supervisor_lock(&self) -> impl Future<Output = Self::OpGuard>;
    /// `supervisor::api::post_review` 以 `decision = "cancel"` 取消一件交辦，回它的 JSON 回應本體。
    /// `actor`／`source`／`reason` 就是 `ReviewIn` 的同名欄位，其餘欄位維持預設（沒有 followup、沒有 evidence）。
    async fn cancel_assignment(
        &self,
        assignment_id: &str,
        headers: &HeaderMap,
        actor: &str,
        source: &str,
        reason: String,
    ) -> Result<Value, LcError>;
    /// `api::delete_bot`。呼叫端只需要成功與否與失敗的除錯字串（`{e:?}`），所以失敗回 `Debug` 後的文字。
    async fn delete_bot(&self, bot_id: &str) -> Result<(), String>;
}

/// 交辦與 AGM 收件匣的讀寫（原 `supervisor::store::*`）。實作在 `SqlitePool` 上＝「repo context」：
/// 呼叫端手上只有 pool（`mission::store` 的函式）或 `app.db` 就能用，不需要 `App`。
pub trait SupervisorRepo {
    /// `supervisor::store::SUPERVISOR_ID`（交辦、收件匣的 `supervisor_id` 欄位值）。
    const SUPERVISOR_ID: &'static str;
    /// `store::mission_assignments`：這個任務底下的全部交辦（新舊順序）。
    async fn mission_assignments(&self, mission_id: &str) -> anyhow::Result<Vec<crate::supervisor::store::Assignment>>;
    /// `store::assignment`。
    async fn assignment(&self, id: &str) -> anyhow::Result<Option<crate::supervisor::store::Assignment>>;
    /// `store::push_inbox`。
    async fn push_inbox(
        &self,
        event_key: &str,
        kind: &str,
        assignment_id: Option<&str>,
        bot_id: Option<&str>,
        turn_id: Option<&str>,
        payload: &Value,
    ) -> anyhow::Result<Option<String>>;
    /// `store::review`：直接寫裁示（`mission::store` 取消專案的任務時收掉底下的交辦，沒有 HTTP 呼叫端）。
    #[allow(clippy::too_many_arguments)]
    async fn review_assignment(
        &self,
        id: &str,
        decision: &str,
        actor: &str,
        source: &str,
        reason: Option<&str>,
        evidence: Option<&str>,
        followup_assignment_id: Option<&str>,
    ) -> anyhow::Result<Option<crate::supervisor::store::Assignment>>;
}

/// 任務閘門的兩條規則（原 `supervisor::api::{mission_gate, user_pause_reason}`）。純規則、沒有 `self`，所以掛在 pool
/// 型別上以 `<SqlitePool as MissionGateRules>::…` 呼叫；規則只准有一份，adapter 直接委派。
pub trait MissionGateRules {
    /// 使用者暫停的 reason（不在 daemon 自己設的那幾種裡）；沒有就 `None`。
    fn user_pause_reason(paused_reason: Option<&str>) -> Option<&str>;
    /// 這個任務現在收不收新交辦：已結案或被使用者暫停就不收。
    fn mission_gate(m: &crate::mission::store::Mission) -> Result<(), LcError>;
}

/// 事件匯流排（原 `app.emit`、`app.subscribe`）。
pub trait EventOps {
    /// `App::emit`：發一則 WS 事件（敏感欄位清理、seq、重播環都在原處）。
    async fn emit_event(&self, kind: &str, data: Value);
}

/// 群組送字用的回合與訊息操作（原 `lifecycle::{prompt_grouped, owed_as_unknown, insert_message_grouped}`）。
pub trait GroupTurnOps {
    #[allow(clippy::too_many_arguments)]
    async fn prompt_grouped(
        &self,
        bot_id: &str,
        text: &str,
        client_request_id: &str,
        group_id: Option<&str>,
        deliver: Option<&str>,
        attachment_ids: &[String],
        relay_from: Option<&str>,
    ) -> LcResult<PromptOut>;
    /// `lifecycle::owed_as_unknown`：送達結果還欠著時當 `unknown`。
    fn owed_as_unknown(&self, res: LcResult<PromptOut>) -> LcResult<PromptOut>;
    #[allow(clippy::too_many_arguments)]
    async fn insert_message_grouped(
        &self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
        group_id: Option<&str>,
    ) -> anyhow::Result<db::Message>;
    /// `api::cursor_not_found`：`before` 游標指的訊息不存在或不在這個對話裡。
    fn cursor_not_found(&self, reason: &str, message_id: &str) -> LcError;
}

#[cfg(test)]
mod tests {
    /// 受護欄管的 mission 側 production 檔（含 `group`／`relay_auth` 等同包檔案）。
    const SOURCES: &[(&str, &str)] = &[
        ("mission/api.rs", include_str!("api.rs")),
        ("mission/deliver.rs", include_str!("deliver.rs")),
        ("mission/flow.rs", include_str!("flow.rs")),
        ("mission/mod.rs", include_str!("mod.rs")),
        ("mission/pick.rs", include_str!("pick.rs")),
        ("mission/relay.rs", include_str!("relay.rs")),
        ("mission/store.rs", include_str!("store.rs")),
        ("mission/workflow.rs", include_str!("workflow.rs")),
        ("group.rs", include_str!("../../../../daemon/src/group.rs")),
        ("agent_relay.rs", include_str!("../../../am-base/src/agent_relay.rs")),
        ("relay_auth.rs", include_str!("../relay_auth.rs")),
        ("handoff.rs", include_str!("../../../am-lifecycle/src/handoff.rs")),
    ];

    /// 每一行是否落在測試項目裡（含 daemon harness 專用測試，從 cfg 那行到項目的大括號收尾）。
    fn test_mask(src: &str) -> Vec<bool> {
        let lines: Vec<&str> = src.lines().collect();
        let mut mask = vec![false; lines.len()];
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim_start().starts_with("#[cfg(") && lines[i].contains("test") {
                let (mut depth, mut seen, mut j) = (0i32, false, i);
                while j < lines.len() {
                    mask[j] = true;
                    depth += lines[j].matches('{').count() as i32 - lines[j].matches('}').count() as i32;
                    seen |= lines[j].contains('{');
                    if seen && depth <= 0 {
                        break;
                    }
                    if !seen && lines[j].trim_end().ends_with(';') && !lines[j].trim_start().starts_with("#[") {
                        break;
                    }
                    j += 1;
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        mask
    }

    /// 以「呼叫」的形狀擋：函式名後面接 `(`。
    const FORBIDDEN_CALLS: &[&str] = &[
        "supervisor::bot_requests::",
        "supervisor::roles::",
        "supervisor::store::mission_assignments(",
        "supervisor::store::assignment(",
        "supervisor::store::push_inbox(",
        "supervisor::store::review(",
        "supervisor::api::",
        "supervisor::lock(",
        "quota::billing_identity(",
        "quota::quota_base_for_host(",
        "quota::quota_key(",
        "quota::identity_shares_default(",
        "tools::identities_for_host(",
        "lifecycle::prompt_grouped(",
        "lifecycle::owed_as_unknown(",
        "lifecycle::insert_message_grouped(",
        "api::delete_bot(",
        "api::cursor_not_found(",
        "api::ct_eq(",
        "app.emit(",
        "app.subscribe(",
        "app.quotas",
        "SUPERVISOR_ID",
    ];

    #[test]
    fn mission_production_code_reaches_other_features_only_through_ports() {
        let mut offenders = Vec::new();
        for (file, src) in SOURCES {
            let mask = test_mask(src);
            for (n, line) in src.lines().enumerate() {
                if mask[n] || line.trim_start().starts_with("//") {
                    continue;
                }
                for pat in FORBIDDEN_CALLS {
                    // `SupervisorRepo::SUPERVISOR_ID`／`<SqlitePool as …>::SUPERVISOR_ID` 是走介面的寫法，放行。
                    if line.contains(pat) && !(*pat == "SUPERVISOR_ID" && line.contains("SupervisorRepo")) {
                        offenders.push(format!("{file}:{}: {pat}  ← {}", n + 1, line.trim()));
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "這些呼叫要走 mission::ports（由 app_ports_p7.rs 委派）：\n{}", offenders.join("\n"));
    }

    /// 反向確認：護欄本身不是空的——被擋的樣式真的會在 adapter 裡出現（改名或搬走時這條會先紅，提醒更新清單）。
    #[test]
    fn the_adapter_still_calls_what_the_guard_forbids_elsewhere() {
        let adapter = format!("{}{}", include_str!("../../../../daemon/src/app_ports_p7.rs"), include_str!("ports_impl.rs"));
        for pat in [
            "bot_requests::verified_bot_id(",
            "bot_requests::actor_role(",
            "roles::role_of_bot(",
            "store::mission_assignments(",
            "store::push_inbox(",
            "store::review(",
            "supervisor::lock(",
            "api::post_review(",
            "quota::billing_identity(",
            "quota::quota_base_for_host(",
            "tools::identities_for_host(",
            "lifecycle::prompt_grouped(",
            "api::delete_bot(",
            "api::cursor_not_found(",
            "api::ct_eq(",
        ] {
            assert!(adapter.contains(pat), "adapter 不再含 {pat}：更新 FORBIDDEN_CALLS 或 adapter");
        }
    }

    /// 走介面拿 supervisor 鎖，鎖統計（issue #473）記的拿鎖點仍是**呼叫端**那一檔，不是 adapter：
    /// `#[track_caller]` 要能穿過 trait 方法，不然所有 mission 的拿鎖點都會糊成 `app_ports_p7.rs`。
    #[cfg(all(test, feature = "daemon-test-harness"))]
    #[tokio::test]
    async fn supervisor_lock_through_the_port_is_attributed_to_the_caller() {
        use super::SupervisorOps;
        let env = crate::testing::env().await;
        let guard = env.app.supervisor_lock().await;
        drop(guard);
        let snap = crate::supervisor::timing::snapshot();
        let sites: Vec<&String> = snap["stats"].as_object().unwrap().keys().filter(|k| k.starts_with("lock:hold:")).collect();
        assert!(sites.iter().any(|k| k.ends_with("mission/ports.rs")), "拿鎖點沒記在呼叫端：{sites:?}");
    }

    #[test]
    fn test_mask_skips_cfg_test_items() {
        assert!(test_mask("fn a() {}\n#[cfg(test)]\nmod t {\n    fn b() {}\n}\nfn c() {}\n") == vec![false, true, true, true, true, false]);
        assert!(test_mask("fn a() {}\n#[cfg(all(test, feature = \"daemon-test-harness\"))]\n#[tokio::test]\nasync fn t() {\n    let x = 1;\n}\nfn c() {}\n") == vec![false, true, true, true, true, true, false]);
    }
}
