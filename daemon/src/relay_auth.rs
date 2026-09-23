//! `POST /api/bots/{id}/prompt` 的 `relay_from` 要跟**呼叫者自己的身分**綁在一起（issue #339）。
//!
//! 那支 API 只要 UI token，而 UI token 本機任何行程都拿得到（`/api/session`、`ui-token` 檔），
//! 所以「relay_from 指到一顆活著的 bot」證明不了任何事：任何呼叫端都能把話掛在別顆 bot 名下，
//! 或冒充 `daemon`——後者還會繞過 AGM 協調者的收件匣（`bot_requests::intercept` 不攔 daemon）。
//!
//! 證明身分用那顆 bot 自己的 hook token（`X-AM-Bot-Token`；pane 環境裡的 `AM_HOOK_TOKEN`，
//! `/relay/announce` 與 build slot 認的同一個）。規則：
//!
//! | relay_from | `X-AM-Bot-Token` | 結果 |
//! |---|---|---|
//! | 省略 | — | 使用者本人（不變） |
//! | `daemon` | 任何 | **403** `relay_from_reserved`：daemon 自己的訊息不走 HTTP |
//! | 不存在／已刪的 bot | 任何 | 400（不變） |
//! | bot | 就是那顆的 token | 已驗證 |
//! | bot | 有帶、但不是那顆的 | **403** `relay_from_mismatch`：hook token 一顆 bot 一個、永不換，對不上只會是冒名 |
//! | bot | 沒帶 | **相容期**：照收，訊息標 `relay_unverified = 1`，UI 在來源旁寫「未驗證」 |
//!
//! 相容期的理由：不帶 token 的既有呼叫端（launchd 跑的 `daemon-swap.sh` 換版自測、裝在資料目錄的
//! 維運腳本、照 README 手打 curl 的 bot）一被 403 就會誤判失敗——換版自測失敗會觸發回滾。
//! 移除條件寫在 SPEC §6.5d。

use std::sync::Arc;

use axum::http::HeaderMap;
use serde_json::json;

use crate::lifecycle::LcError;
use crate::state::App;

/// 驗過之後的來源。`unverified` 只在相容期那一格是 true。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relay {
    pub from: String,
    pub unverified: bool,
}

/// 省略 relay_from＝`Ok(None)`（使用者本人）。
pub async fn authenticate(app: &Arc<App>, headers: &HeaderMap, claimed: Option<&str>) -> Result<Option<Relay>, LcError> {
    let Some(claimed) = claimed.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    if claimed == crate::agent_relay::DAEMON_SENDER {
        return Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "relay_from_reserved",
            "message": "relay_from=daemon 只給 daemon 自己用，不能從 API 帶進來；bot 請帶自己的 bot id 與 X-AM-Bot-Token",
        })));
    }
    let bot = match crate::db::bot(&app.db, claimed).await.map_err(|e| LcError::Upstream(e.to_string()))? {
        Some(b) if b.deleted_at.is_none() => b,
        _ => return Err(LcError::Bad(format!("relay_from must be a live bot id or `{}`", crate::agent_relay::DAEMON_SENDER))),
    };
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).map(str::trim).filter(|t| !t.is_empty());
    match token {
        Some(t) if crate::api::ct_eq(t, &bot.hook_token) => Ok(Some(Relay { from: bot.id, unverified: false })),
        Some(_) => Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "relay_from_mismatch",
            "message": "X-AM-Bot-Token 不是 relay_from 那顆 bot 的；只能以自己的身分轉述",
        }))),
        None => {
            // 不記 token（本來就沒有），只記誰冒了誰的名，方便相容期結束前清點還有誰沒帶。
            tracing::warn!(relay_from = %bot.id, "relay_from without X-AM-Bot-Token: accepted as unverified (issue #339 compat)");
            Ok(Some(Relay { from: bot.id, unverified: true }))
        }
    }
}
