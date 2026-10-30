use std::sync::Arc;

use axum::{extract::State, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{api::RequestPrincipal, config::ModelMode, lifecycle::LcError, state::App};

fn user_only(principal: &RequestPrincipal) -> Result<(), LcError> {
    if *principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error":"forbidden", "reason":"user_only"})));
    }
    Ok(())
}

fn mode_id(mode: ModelMode) -> &'static str {
    match mode { ModelMode::Claude => "claude", ModelMode::ClaudeCodex => "claude_codex" }
}

pub(crate) async fn get(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    let mode = app.cfg.agents_fresh().await.model_mode;
    Ok(Json(json!({"mode":mode_id(mode),"modes":[
        {"id":"claude","label":"claude","description":"現有 Claude 模式，維持目前行為。"},
        {"id":"claude_codex","label":"claude主codex輔助模式","description":"主 parent 使用 opus；顧問 fable、gpt-6-astra 負責決策及 review；執行者依難度選 gpt-6.1-terra medium、gpt-6-luna xhigh、haiku 或 sonnet，使用者點名時照指定。codex child 長駐專職，同專案或模組沿用，不相關的新題從它 fork --last；claude child 驗收後收掉，除非一小時內有同區塊後續。codex 不當 parent，需要另派時交由 claude parent 決定。"}
    ]})))
}

#[derive(Deserialize)]
pub(crate) struct ModeInput { mode: String }

pub(crate) async fn put(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>, Json(input): Json<ModeInput>) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    let mode = match input.mode.as_str() {
        "claude" => ModelMode::Claude,
        "claude_codex" => ModelMode::ClaudeCodex,
        _ => return Err(LcError::BadValue(json!({"error":"invalid_model_mode"}))),
    };
    app.cfg.update(|cfg| { cfg.agents.model_mode = mode; Ok(()) }).await.map_err(crate::api::cfg_err)?;
    Ok(Json(json!({"mode":mode_id(mode),"needs_restart_bots":true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mode_rules_are_injected_only_for_opted_in_bots_and_children() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let project = crate::db::project(&app.db, &env.project_id).await.unwrap().unwrap();
        let before = crate::lifecycle::agent_md::load(&*app, &project).await;
        assert!(!before.text.contains("claude主codex輔助模式"));
        app.cfg.update(|cfg| { cfg.agents.model_mode = ModelMode::ClaudeCodex; Ok(()) }).await.unwrap();
        let after = crate::lifecycle::agent_md::load(&*app, &project).await;
        assert!(after.text.contains("gpt-6.1-terra medium"));
        let child = crate::lifecycle::setup::child_instructions(&after.text);
        assert!(child.contains("gpt-6-astra"));
        assert!(child.contains("fork --last"));
    }

    #[tokio::test]
    async fn model_mode_round_trip_and_user_only() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let response = get(State(app.clone()), Extension(RequestPrincipal::User)).await.unwrap().0;
        assert_eq!(response["mode"], "claude");
        assert_eq!(response["modes"].as_array().unwrap().len(), 2);
        assert!(put(State(app.clone()), Extension(RequestPrincipal::User), Json(ModeInput { mode: "wrong".into() })).await.is_err());
        let response = put(State(app.clone()), Extension(RequestPrincipal::User), Json(ModeInput { mode: "claude_codex".into() })).await.unwrap().0;
        assert_eq!(response, json!({"mode":"claude_codex","needs_restart_bots":true}));
        assert!(std::fs::read_to_string(&app.cfg.path).unwrap().contains("model_mode = \"claude_codex\""));
        assert_eq!(get(State(app.clone()), Extension(RequestPrincipal::User)).await.unwrap().0["mode"], "claude_codex");
        let bot = RequestPrincipal::Bot("test".into());
        assert!(matches!(get(State(app.clone()), Extension(bot.clone())).await, Err(LcError::Forbidden(_))));
        assert!(matches!(put(State(app), Extension(bot), Json(ModeInput { mode: "claude".into() })).await, Err(LcError::Forbidden(_))));
    }
}
