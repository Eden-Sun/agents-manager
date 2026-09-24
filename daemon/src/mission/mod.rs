//! 群組任務（mission）：使用者在群組下指示，由 AGM 調度執行者、reviewer、驗證者完成。
//! 設計見 `docs/goals/agm-missions.md`（D1–D8）；契約見 `docs/API.md` 的「群組任務」一節。
//!
//! daemon 在這裡只做確定性的部分：任務與事件的持久化、身分挑選規則（[`pick`]）、輪數上限、
//! 交付前的 fast-forward 檢查（[`deliver`]），以及流程推進——下一步是哪一關、哪些關卡開著（[`flow`]
//! 推導、[`workflow`] 把關與叫醒）。拆工、判斷 review 與驗證結果是 AGM 的事。

pub mod api;
pub mod deliver;
pub mod flow;
pub mod pick;
pub mod relay;
pub mod store;
pub mod workflow;

use crate::state::App;
use std::sync::Arc;

/// D4：claude 身分的調度順序，用盡才往下一個。
pub const CLAUDE_ORDER: [&str; 3] = ["cc2", "cc1", "cc0"];

/// 這顆 bot 現在在用哪個帳號，**用候選清單裡的名字講**（issue #468）。
///
/// `quota::billing_identity` 對「沒有設身分、跑 CLI 預設帳號」回 `None`——那是誠實的（它就是沒有名字），
/// 但拿去跟 [`CLAUDE_ORDER`] 的名字比就永遠不相等，於是 `pick::quota_policy` 那道「挑到的還是同一個
/// 身分就別換手」的守衛對這種 bot 永遠不成立：第一次撞限就換手，而換手是開新 bot＋新 session。
///
/// 這裡把預設帳號解析成它在候選清單裡的名字：**沒有自己 home 變數的那個身分就是預設帳號**
/// （`quota::identity_shares_default`，跟額度 key 的收斂規則同一份）。順序也照 [`CLAUDE_ORDER`]，
/// 跟 `pick` 挑的順序一致——兩個身分都收斂到預設帳號時，兩邊會講同一個名字。
///
/// 回 `None` ＝**查不出來**（這台主機的身分表還沒偵測完、或 kind 不是 claude）。呼叫端要把它當成
/// 「不知道」，不是「沒有身分」：不知道就不要為了換身分丟掉一個 session。
pub async fn billing_identity_named(app: &Arc<App>, host: &str, bot: &crate::db::Bot) -> anyhow::Result<Option<String>> {
    if let Some(name) = crate::quota::billing_identity(app, bot).await? {
        return Ok(Some(name));
    }
    Ok(default_identity_name(app, host, &bot.kind).await)
}

/// 這台主機上，哪個身分就是這個 kind 的預設帳號（沒有自己的 home 變數那個）。查不到回 `None`。
pub async fn default_identity_name(app: &Arc<App>, host: &str, kind: &str) -> Option<String> {
    if kind != "claude" {
        return None;
    }
    let known = crate::tools::identities_for_host(app, host).await;
    CLAUDE_ORDER.iter().find_map(|name| {
        known
            .iter()
            .find(|i| i.kind == kind && i.name == *name && crate::quota::identity_shares_default(kind, &i.env))
            .map(|i| i.name.clone())
    })
}

/// 挑身分用的候選清單（照 [`CLAUDE_ORDER`]）。claude 以外的 kind 只有一把額度、沒有身分可輪換，
/// 回一個名字為空的候選。
///
/// 第二個欄位＝「不要挑它」：使用者停用的，或**這台主機上根本沒有的身分**——`pick` 以前照樣回
/// `use`，AGM 照著開 bot 才被 `identity is not known on this host` 409 擋下
/// （review3 c1「沒把握」清單）。查不到任何同 kind 的身分時（偵測還沒跑完、或這台讀不到 alias）
/// 維持原本的三個：不知道不等於沒有。
///
/// 使用者停用的清單讀不到就回 `Err`（issue #160）：以前 `unwrap_or_default()` 當成「沒有停用」，被停用的身分又成了候選。
/// 這是會改變派工身分的政策，讀不到＝不知道，呼叫端要停下來下一次再判，不是照「都可用」挑。
pub async fn candidates(app: &Arc<App>, host: &str, kind: &str) -> anyhow::Result<Vec<(String, bool, Option<crate::quota::Quota>)>> {
    let disabled = store::disabled_identities(&app.db, host, kind).await?;
    if kind != "claude" {
        let quotas = app.quotas.lock().await;
        return Ok(vec![(String::new(), false, quotas.get(&crate::quota::quota_key(host, kind)).cloned())]);
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
    Ok(bases
        .into_iter()
        .map(|(name, base)| {
            let q = quotas.get(&crate::quota::quota_key(host, &base)).cloned();
            (name.to_string(), disabled.iter().any(|d| d == name) || missing_here(name), q)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, kind: &str) -> crate::config::IdentityCfg {
        crate::config::IdentityCfg { name: name.into(), kind: kind.into(), host: None, env: Default::default(), args: Vec::new() }
    }

    fn identity_with(name: &str, kind: &str, env: &[(&str, &str)]) -> crate::config::IdentityCfg {
        let mut c = identity(name, kind);
        c.env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        c
    }

    async fn bot_with_identity(app: &Arc<App>, id: &str, identity: Option<&str>) -> crate::db::Bot {
        sqlx::query("INSERT OR IGNORE INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,identity,hook_token,created_at) VALUES (?,'p',?,'claude',?,?,?)")
            .bind(id)
            .bind(id)
            .bind(identity)
            .bind(format!("tok-{id}"))
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        crate::db::bot(&app.db, id).await.unwrap().unwrap()
    }

    /// issue #468：跑 CLI 預設帳號的 bot 沒有身分名，拿空字串跟候選清單比就永遠「不是同一個身分」，
    /// 於是第一次撞限就換手——而換手是開新 bot＋新 session。解析成候選清單裡的名字之後，
    /// 挑到同一個帳號就是原地等。
    #[tokio::test]
    async fn a_bot_on_the_default_account_is_named_after_the_identity_that_shares_it() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = crate::config::LOCAL_HOST;
        let plain = bot_with_identity(&app, "b-default", None).await;
        let named = bot_with_identity(&app, "b-cc2", Some("cc2")).await;

        // 偵測還沒跑完：**查不出來**，不是「沒有身分」——呼叫端要當成不知道而原地等。
        assert_eq!(billing_identity_named(&app, host, &plain).await.unwrap(), None);

        // cc2 有自己的 CLAUDE_CONFIG_DIR，cc1／cc0 沒有＝它們就是預設帳號。
        app.tools.lock().await.insert(
            host.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![
                    identity_with("cc2", "claude", &[("CLAUDE_CONFIG_DIR", "/home/me/.claude-cc2")]),
                    identity_with("cc1", "claude", &[]),
                    identity_with("cc0", "claude", &[]),
                ],
                utc_offset_secs: None,
                herdr_cli: None,
                checked_at: crate::db::now(),
            },
        );
        // 順序照 CLAUDE_ORDER，跟 `pick` 挑的順序一致——兩個身分都收斂到預設帳號時兩邊講同一個名字。
        assert_eq!(default_identity_name(&app, host, "claude").await.as_deref(), Some("cc1"));
        assert_eq!(billing_identity_named(&app, host, &plain).await.unwrap().as_deref(), Some("cc1"));
        // 有設身分的照舊，不經過這條路。
        assert_eq!(billing_identity_named(&app, host, &named).await.unwrap().as_deref(), Some("cc2"));

        // 解析出名字之後，挑到同一個帳號就是原地等，不是換手。
        use super::pick::{quota_policy, Pick, QuotaPolicy};
        let same = Pick::Use { identity: "cc1".into(), model: None, reason: "額度可用".into() };
        let current = billing_identity_named(&app, host, &plain).await.unwrap();
        assert_eq!(quota_policy(current.as_deref(), None, &same), QuotaPolicy::Wait, "不能為了換身分丟掉 session");
    }

    /// issue #468 的第二個出口：執行者跑預設帳號時，`exclude` 以前是 `None`，reviewer 就可能被挑成
    /// 執行者正在用的那個帳號（review3 c1 L11 加 `--exclude` 要擋的正是這件事）。解析成名字之後擋得住。
    #[tokio::test]
    async fn a_reviewer_does_not_land_on_the_executors_default_account() {
        use super::pick::{pick, Candidate, On5hLimit, Pick, Role};
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = crate::config::LOCAL_HOST;
        app.tools.lock().await.insert(
            host.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![identity_with("cc0", "claude", &[])],
                utc_offset_secs: None,
                herdr_cli: None,
                checked_at: crate::db::now(),
            },
        );
        let executor = bot_with_identity(&app, "b-exec", None).await;
        let exclude = billing_identity_named(&app, host, &executor).await.unwrap();
        assert_eq!(exclude.as_deref(), Some("cc0"), "執行者的預設帳號要有名字才排除得掉");

        // 這台只有 cc0（＝執行者正在用的那個帳號）：reviewer 挑不到別的身分，要講出來，不能退而求其次。
        let cands = [Candidate { name: "cc0", disabled: false, quota: None }];
        let got = pick(Role::Reviewer, &cands, On5hLimit::Wait, exclude.as_deref(), chrono::Utc::now());
        assert!(matches!(got, Pick::NoIndependentReviewer { .. }), "{got:?}");
        // 沒有解析（以前的行為）就擋不住：同一個帳號會被當成獨立的 reviewer。
        let unguarded = pick(Role::Reviewer, &cands, On5hLimit::Wait, None, chrono::Utc::now());
        assert!(matches!(unguarded, Pick::Use { .. }), "{unguarded:?}");
    }

    /// 這台沒有的身分不當候選；偵測還沒跑完時三個都留著（不知道不等於沒有）。
    #[tokio::test]
    async fn a_host_without_that_identity_does_not_offer_it() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let usable = |rows: &[(String, bool, Option<crate::quota::Quota>)]| {
            rows.iter().filter(|(_, skip, _)| !skip).map(|(n, _, _)| n.clone()).collect::<Vec<_>>()
        };
        assert_eq!(usable(&candidates(&app, crate::config::LOCAL_HOST, "claude").await.unwrap()), CLAUDE_ORDER.to_vec(), "偵測前照舊");

        // 偵測過：這台只有 cc1 與 cc0（外加一個 codex 的同名身分，不算）。
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![identity("cc1", "claude"), identity("cc0", "claude"), identity("cc2", "codex")],
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
            },
        );
        assert_eq!(usable(&candidates(&app, crate::config::LOCAL_HOST, "claude").await.unwrap()), vec!["cc1", "cc0"]);

        // 使用者停用的照舊也不挑。
        store::set_identity_disabled(&app.db, crate::config::LOCAL_HOST, "claude", "cc1", true).await.unwrap();
        assert_eq!(usable(&candidates(&app, crate::config::LOCAL_HOST, "claude").await.unwrap()), vec!["cc0"]);
    }
}
