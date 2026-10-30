use std::sync::Arc;

use axum::{extract::State, Extension, Json};
use serde_json::{json, Value};

use crate::{api::RequestPrincipal, lifecycle::LcError, state::App};

fn user_only(principal: &RequestPrincipal) -> Result<(), LcError> {
    if *principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error":"forbidden", "reason":"user_only"})));
    }
    Ok(())
}

pub(crate) async fn get(State(_app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    Ok(Json(json!({"deprecated":true,"effective":false,"message":"全域模型用法模式已廢棄，不再生效。"})))
}

pub(crate) async fn put(State(_app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>, Json(_input): Json<Value>) -> Result<Json<Value>, LcError> {
    user_only(&principal)?;
    Err(LcError::Conflict(json!({"error":"deprecated","effective":false,"message":"全域模型用法模式已廢棄，不再生效。"})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deprecated_mode_cannot_be_changed() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        assert_eq!(get(State(app.clone()), Extension(RequestPrincipal::User)).await.unwrap().0["effective"], false);
        assert!(matches!(put(State(app.clone()), Extension(RequestPrincipal::User), Json(json!({"mode":"claude_codex"}))).await, Err(LcError::Conflict(_))));
        assert!(matches!(get(State(app), Extension(RequestPrincipal::Bot("test".into()))).await, Err(LcError::Forbidden(_))));
    }
}
