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
///
/// 第二個欄位＝「不要挑它」：使用者停用的，或**這台主機上根本沒有的身分**——`pick` 以前照樣回
/// `use`，AGM 照著開 bot 才被 `identity is not known on this host` 409 擋下
/// （review3 c1「沒把握」清單）。查不到任何同 kind 的身分時（偵測還沒跑完、或這台讀不到 alias）
/// 維持原本的三個：不知道不等於沒有。
pub async fn candidates(app: &Arc<App>, host: &str, kind: &str) -> Vec<(String, bool, Option<crate::quota::Quota>)> {
    let disabled = store::disabled_identities(&app.db, host, kind).await.unwrap_or_default();
    if kind != "claude" {
        let quotas = app.quotas.lock().await;
        return vec![(String::new(), false, quotas.get(&crate::quota::quota_key(host, kind)).cloned())];
    }
    let known: Vec<String> =
        crate::tools::identities_for_host(app, host).await.into_iter().filter(|i| i.kind == kind).map(|i| i.name).collect();
    let missing_here = |name: &str| !known.is_empty() && !known.iter().any(|k| k == name);
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
            (name.to_string(), disabled.iter().any(|d| d == name) || missing_here(name), q)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, kind: &str) -> crate::config::IdentityCfg {
        crate::config::IdentityCfg { name: name.into(), kind: kind.into(), host: None, env: Default::default(), args: Vec::new() }
    }

    /// 這台沒有的身分不當候選；偵測還沒跑完時三個都留著（不知道不等於沒有）。
    #[tokio::test]
    async fn a_host_without_that_identity_does_not_offer_it() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let usable = |rows: &[(String, bool, Option<crate::quota::Quota>)]| {
            rows.iter().filter(|(_, skip, _)| !skip).map(|(n, _, _)| n.clone()).collect::<Vec<_>>()
        };
        assert_eq!(usable(&candidates(&app, crate::config::LOCAL_HOST, "claude").await), CLAUDE_ORDER.to_vec(), "偵測前照舊");

        // 偵測過：這台只有 cc1 與 cc0（外加一個 codex 的同名身分，不算）。
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![identity("cc1", "claude"), identity("cc0", "claude"), identity("cc2", "codex")],
                checked_at: crate::db::now(),
            },
        );
        assert_eq!(usable(&candidates(&app, crate::config::LOCAL_HOST, "claude").await), vec!["cc1", "cc0"]);

        // 使用者停用的照舊也不挑。
        store::set_identity_disabled(&app.db, crate::config::LOCAL_HOST, "claude", "cc1", true).await.unwrap();
        assert_eq!(usable(&candidates(&app, crate::config::LOCAL_HOST, "claude").await), vec!["cc0"]);
    }
}
