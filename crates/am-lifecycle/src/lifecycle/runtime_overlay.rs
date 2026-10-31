//! 角色 policy 與 bot 執行期覆寫的**啟動覆蓋**（B 期）：生效順序 bot 覆寫 > 池條目 > 角色 policy > 預設。
//! 池條目的 context 已經在 hire 時寫進 bot 的 env／args（[`am_config::executor_pool::launch_params`]），所以這裡只在
//! 「context 來自覆寫或角色 policy」時才補上同一套啟動參數（重用同一個函式）。保溫門檻不走這裡（見 `primary_keep_warm`）。
//! 生效結果的指紋（[`policy_rev`]）記在 `runs.policy_rev`，之後 policy 變了 bot 就標「待套用（需重啟）」。

use crate::capabilities::{Cfg, Db};
use crate::db;
use am_base::runtime_policy::{effective, policy_rev as rev_of, Effective, RuntimeSettings};
use am_config::executor_pool::{context_window, launch_params, ExecutorPoolEntry, CONTEXT_MIN};
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

/// 純函式：給 bot、它角色的 policy 與池條目的 context，算出生效結果與要不要補啟動參數。
/// `pool_context`＝bot 的池條目 `context_tokens`（沒有池條目就 `None`）。
pub fn resolve(bot: &db::Bot, role_policy: &BTreeMap<String, serde_json::Value>, pool_context: Option<u32>) -> (Effective, bool) {
    let ov: Option<RuntimeSettings> = bot.runtime_override_json.as_deref().and_then(|s| serde_json::from_str(s).ok());
    let role: Option<RuntimeSettings> = bot.role.as_deref().and_then(|r| role_policy.get(r)).and_then(|v| serde_json::from_value(v.clone()).ok());
    let eff = effective(ov.as_ref(), pool_context, role.as_ref());
    // 來源是覆寫，或來源是角色 policy 且池條目沒有 context：才需要補參數；池條目的 context 早就在 env／args 裡了。
    let from_override = ov.as_ref().and_then(|o| o.context_tokens).is_some();
    let from_role = pool_context.is_none() && role.as_ref().and_then(|r| r.context_tokens).is_some();
    (eff, from_override || from_role)
}

/// 池條目的 context（bot 在 config 裡帶 `pool_entry` 時）。`pool_entry` 只在 config（不進 DB），所以由呼叫端傳進來。
pub fn pool_context(entries: &[ExecutorPoolEntry], pool_entry: Option<&str>) -> Option<u32> {
    let id = pool_entry?;
    entries.iter().find(|e| e.id == id).and_then(|e| e.context_tokens)
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
    let window = context_window(&bot.kind, &model);
    if window == 0 || ctx < CONTEXT_MIN {
        tracing::warn!(bot = %bot.name, kind = %bot.kind, ctx, "role policy context not applied: unsupported kind or too small");
        return out;
    }
    if bot.kind == "claude" && ctx > 200_000 && model.is_empty() {
        tracing::warn!(bot = %bot.name, "role policy context above 200000 needs a model; not applied");
        return out;
    }
    let ctx = ctx.min(window);
    let lp = launch_params(&ExecutorPoolEntry { id: String::new(), kind: bot.kind.clone(), model, effort: None, context_tokens: Some(ctx), count: 0 });
    out.model = lp.model;
    out.env = lp.env;
    out.args = lp.args;
    out
}

/// 啟動時用：讀 bot 覆寫、它角色的 policy 與池條目（config），算出要補的參數。
pub async fn overlay_for(app: &(impl Cfg + Db), bot: &db::Bot) -> anyhow::Result<Overlay> {
    let (policy, pool, pool_entry) = load(app, bot).await?;
    let (eff, apply) = resolve(bot, &policy, pool_context(&pool, pool_entry.as_deref()));
    Ok(build_overlay(bot, &eff, apply))
}

/// 這顆 bot 現在的生效指紋（與啟動時 `runs.policy_rev` 比較，決定 role_pending）。
pub async fn current_policy_rev(app: &(impl Cfg + Db), bot: &db::Bot) -> anyhow::Result<String> {
    let (policy, pool, pool_entry) = load(app, bot).await?;
    Ok(policy_rev_of(&resolve(bot, &policy, pool_context(&pool, pool_entry.as_deref())).0))
}

/// 角色 policy（DB 投影）、專案的池條目與這顆 bot 的 `pool_entry`（config）。
async fn load(app: &(impl Cfg + Db), bot: &db::Bot) -> anyhow::Result<(BTreeMap<String, serde_json::Value>, Vec<ExecutorPoolEntry>, Option<String>)> {
    let project = db::project(app.db(), &bot.project_id).await?;
    let policy = project.as_ref().map(|p| p.role_policy()).unwrap_or_default();
    let cfg = app.cfg().get().await;
    let (pool, pool_entry) = match cfg.projects.iter().find(|p| p.id.as_deref() == Some(bot.project_id.as_str())) {
        Some(p) => (p.executor_pool.clone(), p.bots.iter().find(|b| b.id.as_deref() == Some(bot.id.as_str())).and_then(|b| b.pool_entry.clone())),
        None => (Vec::new(), None),
    };
    Ok((policy, pool, pool_entry))
}

/// 同步版本（`/api/state` 逐顆算用，不碰 DB／config）：呼叫端自己把專案 policy、池條目與 `pool_entry` 給進來。
pub fn policy_rev_now(bot: &db::Bot, role_policy: &BTreeMap<String, serde_json::Value>, pool: &[ExecutorPoolEntry], pool_entry: Option<&str>) -> String {
    policy_rev_of(&resolve(bot, role_policy, pool_context(pool, pool_entry)).0)
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

    /// 生效順序四層：bot 覆寫 > 池條目 > 角色 policy > 預設。
    #[test]
    fn override_then_pool_then_role_then_default() {
        let pol = policy("executor", r#"{"context_tokens": 200000}"#);
        // 0. 預設：什麼都沒有，不補參數。
        let b = bot("claude", Some("opus"), None, None);
        assert_eq!(resolve(&b, &pol, None), (Effective::default(), false));
        // 1. 角色 policy：補參數（但 executor 角色的 bot 沒有 role 時不套用）。
        let b = bot("claude", Some("opus"), Some("executor"), None);
        let (eff, apply) = resolve(&b, &pol, None);
        assert_eq!((eff.context_tokens, apply), (Some(200_000), true));
        // 2. 池條目 context 蓋過角色，且不補（池條目的參數早就在 bot env 裡）。
        assert_eq!(resolve(&b, &pol, Some(128_000)), (Effective { context_tokens: Some(128_000), ..Default::default() }, false));
        // 3. bot 覆寫最大：蓋過池條目與角色。
        let b = bot("claude", Some("opus"), Some("executor"), Some(r#"{"context_tokens": 512000}"#));
        assert_eq!(resolve(&b, &pol, Some(128_000)), (Effective { context_tokens: Some(512_000), ..Default::default() }, true));
    }

    #[test]
    fn claude_overlay_uses_the_existing_launch_params() {
        let b = bot("claude", Some("opus"), Some("manager"), None);
        let eff = Effective { context_tokens: Some(512_000), ..Default::default() };
        let o = build_overlay(&b, &eff, true);
        assert_eq!(o.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str), Some("512000"));
        assert_eq!(o.model.as_deref(), Some("opus[1m]"), "超過 200k 改用 [1m] 變體");
        // 夾到模型視窗：haiku 只有 200k。
        let h = bot("claude", Some("haiku"), Some("manager"), None);
        assert_eq!(build_overlay(&h, &eff, true).env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str), Some("200000"));
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
        // apply=false（池條目已經有 context）：什麼都不補。
        assert_eq!(build_overlay(&bot("codex", None, None, None), &eff, false), Overlay { eff: eff.clone(), ..Default::default() });
    }
}
