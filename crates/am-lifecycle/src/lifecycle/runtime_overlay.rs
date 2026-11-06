//! 角色 policy 與 bot 執行期覆寫的**啟動覆蓋**（B 期）：生效順序 bot 覆寫 > 角色 policy > 預設。
//! context 的啟動參數使用共用 runtime helper；保溫門檻不走這裡（見 `primary_keep_warm`）。
//! 生效結果的指紋（[`policy_rev`]）記在 `runs.policy_rev`，之後 policy 變了 bot 就標「待套用（需重啟）」。

use crate::capabilities::Db;
use crate::db;
use am_base::runtime_policy::{effective, policy_rev as rev_of, Effective, RuntimeSettings};
use am_config::runtime_params::{context_window, launch_params, CONTEXT_MIN};
use std::collections::BTreeMap;

/// 啟動時要補上的參數（沒有覆寫就是空的，行為與現在完全相同）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    /// 有覆寫時的啟動模型（claude > 200k 會帶 `[1m]`）；`None`＝用 bot 自己的模型。
    pub model: Option<String>,
    pub env: BTreeMap<String, String>,
    pub args: Vec<String>,
    pub eff: Effective,
}

/// 純函式：給 bot 與它角色的 policy，算出生效結果與要不要補啟動參數。
pub fn resolve(bot: &db::Bot, role_policy: &BTreeMap<String, serde_json::Value>) -> (Effective, bool) {
    let ov: Option<RuntimeSettings> = bot.runtime_override_json.as_deref().and_then(|s| serde_json::from_str(s).ok());
    let role: Option<RuntimeSettings> = bot.role.as_deref().and_then(|r| role_policy.get(r)).and_then(|v| serde_json::from_value(v.clone()).ok());
    let eff = effective(ov.as_ref(), role.as_ref());
    // context 只來自 bot 覆寫或角色 policy，需在啟動時補上對應參數。
    let from_override = ov.as_ref().and_then(|o| o.context_tokens).is_some();
    let from_role = role.as_ref().and_then(|r| r.context_tokens).is_some();
    (eff, from_override || from_role)
}

/// 生效結果的指紋（同一組生效值＝同一個指紋）。
pub fn policy_rev_of(eff: &Effective) -> String {
    rev_of(eff)
}

/// 把生效的 context 換成啟動參數。不支援或超出範圍的不補（記 log，不擋啟動：role policy 是跨 kind 的，各 bot 自己判斷）。
pub fn build_overlay(bot: &db::Bot, eff: &Effective, apply: bool) -> Overlay {
    let mut out = Overlay { model: None, env: BTreeMap::new(), args: Vec::new(), eff: eff.clone() };
    let Some(ctx) = eff.context_tokens.filter(|_| apply) else { return out };
    let model = bot.model.clone().unwrap_or_default();
    let window = context_window(&bot.kind);
    if window == 0 || ctx < CONTEXT_MIN {
        tracing::warn!(bot = %bot.name, kind = %bot.kind, ctx, "role policy context not applied: unsupported kind or too small");
        return out;
    }
    if bot.kind == "claude" && ctx > 200_000 && model.is_empty() {
        tracing::warn!(bot = %bot.name, "role policy context above 200000 needs a model; not applied");
        return out;
    }
    let ctx = ctx.min(window);
    let lp = launch_params(&bot.kind, Some(&model), Some(ctx));
    out.model = lp.model;
    out.env = lp.env;
    out.args = lp.args;
    out
}

/// 啟動時用：讀 bot 覆寫與它角色的 policy，算出要補的參數。
pub async fn overlay_for(app: &impl Db, bot: &db::Bot) -> anyhow::Result<Overlay> {
    let policy = load(app, bot).await?;
    let (eff, apply) = resolve(bot, &policy);
    Ok(build_overlay(bot, &eff, apply))
}

/// 角色 policy（DB 投影）。
async fn load(app: &impl Db, bot: &db::Bot) -> anyhow::Result<BTreeMap<String, serde_json::Value>> {
    let project = db::project(app.db(), &bot.project_id).await?;
    Ok(project.as_ref().map(|p| p.role_policy()).unwrap_or_default())
}

/// 同步版本（`/api/state` 逐顆算用，不碰 DB／config）。
pub fn policy_rev_now(bot: &db::Bot, role_policy: &BTreeMap<String, serde_json::Value>) -> String {
    policy_rev_of(&resolve(bot, role_policy).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use am_base::runtime_policy::RuntimeSettings;

    fn bot(kind: &str, model: Option<&str>, role: Option<&str>, ov: Option<&str>) -> db::Bot {
        db::Bot {
            role: role.map(String::from),
            manager_use: 1,
            track: None,
            runtime_override_json: ov.map(String::from),
            id: "b".into(),
            project_id: "p".into(),
            name: "n".into(),
            kind: kind.into(),
            model: model.map(String::from),
            effort: None,
            fast: 0,
            persona: None,
            args_json: "[]".into(),
            autostart: 0,
            inject_hooks: 1,
            auto_approve: 1,
            identity: None,
            env_json: "{}".into(),
            managed_by: "user".into(),
            cwd: None,
            herdr_session: None,
            parent_bot_id: None,
            is_primary: 0,
            primary_position: 0,
            hook_token: "t".into(),
            deleted_at: None,
            created_at: String::new(),
        }
    }

    fn policy(role: &str, json: &str) -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([(role.to_string(), serde_json::from_str::<RuntimeSettings>(json).map(|s| serde_json::to_value(s).unwrap()).unwrap())])
    }

    /// 生效順序：bot 覆寫 > 角色 policy > 預設。
    #[test]
    fn bot_override_then_role_policy_then_default() {
        let pol = policy("executor", r#"{"context_tokens": 200000}"#);
        // 0. 預設：什麼都沒有，不補參數。
        let b = bot("claude", Some("opus"), None, None);
        assert_eq!(resolve(&b, &pol), (Effective::default(), false));
        // 1. 角色 policy：補參數（但 executor 角色的 bot 沒有 role 時不套用）。
        let b = bot("claude", Some("opus"), Some("executor"), None);
        let (eff, apply) = resolve(&b, &pol);
        assert_eq!((eff.context_tokens, apply), (Some(200_000), true));
        // 2. bot 覆寫最大：蓋過角色 policy。
        let b = bot("claude", Some("opus"), Some("executor"), Some(r#"{"context_tokens": 512000}"#));
        assert_eq!(resolve(&b, &pol), (Effective { context_tokens: Some(512_000), ..Default::default() }, true));
    }

    #[test]
    fn claude_overlay_uses_the_existing_launch_params() {
        let b = bot("claude", Some("opus"), Some("manager"), None);
        let eff = Effective { context_tokens: Some(512_000), ..Default::default() };
        let o = build_overlay(&b, &eff, true);
        assert_eq!(o.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str), Some("512000"));
        assert_eq!(o.model.as_deref(), Some("opus[1m]"), "超過 200k 改用 [1m] 變體");
        // 夾到模型視窗：codex 只有 400k。
        let c = bot("codex", Some("gpt-6-luna"), Some("manager"), None);
        assert!(build_overlay(&c, &eff, true).args.contains(&"model_auto_compact_token_limit=400000".to_string()));
        // 沒有模型又超過 200k：不補（記 log）。
        assert_eq!(build_overlay(&bot("claude", None, Some("manager"), None), &eff, true), Overlay { eff: eff.clone(), ..Default::default() });
    }

    #[test]
    fn codex_overlay_is_a_compact_limit_argument_and_agy_is_skipped() {
        let eff = Effective { context_tokens: Some(256_000), ..Default::default() };
        let o = build_overlay(&bot("codex", Some("gpt-6"), Some("specialist"), None), &eff, true);
        assert_eq!(o.args, vec!["-c".to_string(), "model_auto_compact_token_limit=256000".to_string()]);
        assert!(o.env.is_empty() && o.model.as_deref() == Some("gpt-6"), "模型沿用 bot 自己的");
        let agy = build_overlay(&bot("agy", None, Some("executor"), None), &eff, true);
        assert_eq!(agy, Overlay { eff: eff.clone(), ..Default::default() }, "agy 不支援 context");
        // apply=false：什麼都不補。
        assert_eq!(build_overlay(&bot("codex", None, None, None), &eff, false), Overlay { eff: eff.clone(), ..Default::default() });
    }
}
