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
//! | ＝收件的那顆 bot | 任何 | **400** `relay_self`：自己送給自己沒有「來源」可標 |
//! | bot | 就是那顆的 token | 已驗證 |
//! | bot | **有帶**、但對不上（別顆的／空的／非 UTF-8） | **403** `relay_from_mismatch`：hook token 一顆 bot 一個、永不換，對不上只會是冒名 |
//! | 不存在／已刪的 bot | **有帶** | **403** `relay_from_mismatch`（同上，不回 400） |
//! | bot | 沒帶 | **相容期**：照收，訊息標 `relay_unverified = 1`，UI 在來源旁寫「未驗證」 |
//! | 不存在／已刪的 bot | 沒帶 | 400（不變） |
//!
//! 「有帶但對不上」與「有帶但那顆 bot 不存在」回同一個 403：分開回（403／400）等於讓帶錯 token 的
//! 呼叫端拿狀態碼當神諭，一個一個試出某個 bot id 存不存在。空字串與非 UTF-8 的 token 算「有帶」，
//! 不算「沒帶」——否則送 `X-AM-Bot-Token:` 就能走進相容期，等於用一個壞掉的 header 換到冒名放行。
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

/// 省略 relay_from＝`Ok(None)`（使用者本人）。`target` 是這一則要送給誰（`POST /api/bots/{id}` 的 id）。
pub async fn authenticate(
    app: &Arc<App>,
    headers: &HeaderMap,
    claimed: Option<&str>,
    target: &str,
) -> Result<Option<Relay>, LcError> {
    let Some(claimed) = claimed.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    if claimed == crate::agent_relay::DAEMON_SENDER {
        return Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "relay_from_reserved",
            "message": "relay_from=daemon 只給 daemon 自己用，不能從 API 帶進來；bot 請帶自己的 bot id 與 X-AM-Bot-Token",
        })));
    }
    // 自己轉述給自己：`relay_from` 的意思是「這句話不是收件者自己想的」，指向收件者本人就沒有來源可標，
    // UI 會畫出「A → A」。沒有正當呼叫端這樣送（daemon 的 child 警示是 child → 母代，兩顆不同的 bot）。
    if claimed == target.trim() {
        return Err(LcError::BadValue(json!({
            "error": "bad_request",
            "reason": "relay_self",
            "message": "relay_from 不能是收件的那顆 bot 自己：自己送給自己沒有「來源」可標",
            "relay_from": claimed,
        })));
    }
    // **header 在不在**才是分歧點，值長什麼樣都不算「沒帶」：空字串與非 UTF-8 以前都掉進相容期，
    // 等於送一個壞掉的 header 就能冒名放行。
    let presented = headers.get("X-AM-Bot-Token").map(|v| v.to_str().unwrap_or_default().trim().to_string());
    let live = crate::db::bot(&app.db, claimed)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|b| b.deleted_at.is_none());
    match (presented, live) {
        // 有帶而且對得上：本人。
        (Some(t), Some(b)) if !t.is_empty() && crate::api::ct_eq(&t, &b.hook_token) => {
            Ok(Some(Relay { from: b.id, unverified: false }))
        }
        // 有帶但對不上（別顆的、空的、非 UTF-8），或指到不存在／已刪的 bot：同一個 403。
        // 不按「bot 存不存在」分成 400／403——那會讓狀態碼變成探測 bot id 的神諭。
        (Some(_), _) => Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "relay_from_mismatch",
            "message": "X-AM-Bot-Token 不是 relay_from 那顆 bot 的；只能以自己的身分轉述",
        }))),
        // 沒帶：相容期照收（bot 還是得是活的）。不記 token（本來就沒有），只記誰冒了誰的名，
        // 方便相容期結束前清點還有誰沒帶。
        (None, Some(b)) => {
            tracing::warn!(relay_from = %b.id, "relay_from without X-AM-Bot-Token: accepted as unverified (issue #339 compat)");
            Ok(Some(Relay { from: b.id, unverified: true }))
        }
        (None, None) => Err(LcError::Bad(format!("relay_from must be a live bot id or `{}`", crate::agent_relay::DAEMON_SENDER))),
    }
}
