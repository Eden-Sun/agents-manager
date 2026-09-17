//! 群組任務（mission）：使用者在群組下指示，由 AGM 調度執行者、reviewer、驗證者完成。
//! 設計見 `docs/goals/agm-missions.md`（D1–D8）；契約見 `docs/API.md` 的「群組任務」一節。
//!
//! daemon 在這裡只做確定性的部分：任務與事件的持久化、身分挑選規則（[`pick`]）、輪數上限、
//! 交付前的 fast-forward 檢查（[`deliver`]）。拆工、判斷 review 與驗證結果是 AGM 的事。

pub mod api;
pub mod deliver;
pub mod pick;
pub mod relay;
pub mod store;

use crate::state::App;
use std::sync::Arc;

/// D4：claude 身分的調度順序，用盡才往下一個。
pub const CLAUDE_ORDER: [&str; 3] = ["cc2", "cc1", "cc0"];

/// 挑身分用的候選清單（照 [`CLAUDE_ORDER`]）。claude 以外的 kind 只有一把額度、沒有身分可輪換，
/// 回一個名字為空的候選。
pub async fn candidates(app: &Arc<App>, host: &str, kind: &str) -> Vec<(String, bool, Option<crate::quota::Quota>)> {
    let disabled = store::disabled_identities(&app.db, host, kind).await.unwrap_or_default();
    if kind != "claude" {
        let quotas = app.quotas.lock().await;
        return vec![(String::new(), false, quotas.get(&crate::quota::quota_key(host, kind)).cloned())];
    }
    // 每個身分的讀數在哪一把 key，跟寫入端同一條規則（`quota::quota_base_for_host`）：沒有自己
    // CLAUDE_CONFIG_DIR 的身分落在裸 `claude`，有的只讀自己那把，不借預設帳號的數字。先把 key 算好再上鎖。
    let mut bases = Vec::with_capacity(CLAUDE_ORDER.len());
    for name in CLAUDE_ORDER {
        bases.push((name, crate::quota::quota_base_for_host(app, host, "claude", Some(name)).await));
    }
    let quotas = app.quotas.lock().await;
    bases
        .into_iter()
        .map(|(name, base)| {
            let q = quotas.get(&crate::quota::quota_key(host, &base)).cloned();
            (name.to_string(), disabled.iter().any(|d| d == name), q)
        })
        .collect()
}
