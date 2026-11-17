//! CLI 自動更新設定（`[updates]`）：`GET`／`PUT /api/settings/auto-update`。
//! 每個 kind 一個開關：claude／codex／agy／grok（herdr 不支援自動更新）。預設全關。

use std::sync::Arc;

use axum::{extract::State, Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{api::RequestPrincipal, lifecycle::LcError, state::App};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoUpdateSettings {
    pub claude: bool,
    pub codex: bool,
    pub agy: bool,
    pub grok: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PutAutoUpdateSettings {
    pub claude: Option<bool>,
    pub codex: Option<bool>,
    pub agy: Option<bool>,
    pub grok: Option<bool>,
}

pub(crate) async fn get(State(app): State<Arc<App>>) -> Result<Json<AutoUpdateSettings>, LcError> {
    let cfg = app.cfg.updates_fresh().await;
    Ok(Json(AutoUpdateSettings {
        claude: cfg.is_auto("claude"),
        codex: cfg.is_auto("codex"),
        agy: cfg.is_auto("agy"),
        grok: cfg.is_auto("grok"),
    }))
}

pub(crate) async fn put(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(input): Json<PutAutoUpdateSettings>,
) -> Result<Json<AutoUpdateSettings>, LcError> {
    if principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    app.cfg
        .update(move |cfg| {
            let mut set: std::collections::BTreeSet<String> = cfg.updates.auto.drain(..).collect();
            let mut apply = |k: &str, opt: Option<bool>| {
                if let Some(enabled) = opt {
                    if enabled {
                        set.insert(k.to_string());
                    } else {
                        set.remove(k);
                    }
                }
            };
            apply("claude", input.claude);
            apply("codex", input.codex);
            apply("agy", input.agy);
            apply("grok", input.grok);
            let order = ["claude", "codex", "agy", "grok"];
            cfg.updates.auto = order.iter().filter(|k| set.contains(**k)).map(|k| k.to_string()).collect();
            Ok(())
        })
        .await
        .map_err(crate::api::cfg_err)?;

    get(State(app)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_update_settings_get_and_put_round_trip() {
        let env = crate::testing::env().await;
        let app = env.app.clone();

        // 預設全關
        let Json(initial) = get(State(app.clone())).await.unwrap();
        assert_eq!(
            initial,
            AutoUpdateSettings {
                claude: false,
                codex: false,
                agy: false,
                grok: false,
            }
        );

        // 部分開啟：codex = true
        let Json(updated) = put(
            State(app.clone()),
            Extension(RequestPrincipal::User),
            Json(PutAutoUpdateSettings {
                codex: Some(true),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            updated,
            AutoUpdateSettings {
                claude: false,
                codex: true,
                agy: false,
                grok: false,
            }
        );

        // 再開 agy，關閉 codex
        let Json(updated2) = put(
            State(app.clone()),
            Extension(RequestPrincipal::User),
            Json(PutAutoUpdateSettings {
                codex: Some(false),
                agy: Some(true),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            updated2,
            AutoUpdateSettings {
                claude: false,
                codex: false,
                agy: true,
                grok: false,
            }
        );

        // bot 呼叫 PUT 應被 403 Forbidden
        let bot_err = put(
            State(app.clone()),
            Extension(RequestPrincipal::Bot("test-bot".into())),
            Json(PutAutoUpdateSettings {
                claude: Some(true),
                ..Default::default()
            }),
        )
        .await;
        assert!(matches!(bot_err, Err(LcError::Forbidden(_))));
    }
}
