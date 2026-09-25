//! `POST /api/bots/{id}/prompt` 的 `relay_from` 是來源標記，不是 principal（issue #339、#556）。
//!
//! `/api` 認證中介層先選定 User（共用 UI token）、Bot（成對 Bot headers）或 service principal。
//! 本機任何行程都可能取得 UI token（`/api/session`、`ui-token` 檔），所以使用者裁示接受「持有 UI token 就是 User」；
//! 本模組只處理來源標記，不能讓 body claim 改變 principal。User 的 Bot 來源 claim 仍在 #410 相容期內標為未驗證；
//! `daemon` 不接受 HTTP 冒名，因為那會繞過 AGM 協調者的收件匣（`bot_requests::intercept` 不攔 daemon）。
//!
//! Bot 身分由 auth 中介層用那顆 bot 現行的 hook token 驗證（`X-AM-Bot-Token`；pane 一律注入 `AM_BOT_TOKEN`，
//! hooks 開啟時另有 `AM_HOOK_TOKEN`）。這裡再確認 `relay_from` claim 是否和自己的 proof 一致。規則：
//!
//! | relay_from | `X-AM-Bot-Token` | 結果 |
//! |---|---|---|
//! | 省略 | User principal | 使用者本人 |
//! | 省略／空白 | Bot principal | route handler supplies the authenticated `X-AM-Bot-Id` |
//! | `daemon` | 任何 | **403** `relay_from_reserved`：daemon 自己的訊息不走 HTTP |
//! | ＝收件的那顆 bot | 任何 | **400** `relay_self`：自己送給自己沒有「來源」可標 |
//! | bot | 就是那顆的 token | 已驗證 |
//! | bot | **有帶**、但對不上（別顆的／空的／非 UTF-8） | **403** `relay_from_mismatch`：claim 和目前的 per-bot proof 不同 |
//! | 不存在／已刪的 bot | **有帶** | **403** `relay_from_mismatch`（同上，不回 400） |
//! | bot | 沒帶 | **相容期**：照收，訊息標 `relay_unverified = 1`，UI 在來源旁寫「未驗證」 |
//! | 不存在／已刪的 bot | 沒帶 | 400（不變） |
//!
//! 「有帶但對不上」與「有帶但那顆 bot 不存在」回同一個 403：分開回（403／400）等於讓帶錯 token 的
//! 呼叫端拿狀態碼當神諭，一個一個試出某個 bot id 存不存在。空字串與非 UTF-8 的 token 算「有帶」，
//! 不算「沒帶」——否則送 `X-AM-Bot-Token:` 就能走進相容期，等於用一個壞掉的 header 換到冒名放行。
//!
//! 相容期的理由：沒有 Bot proof 的 User 舊呼叫端一被拒絕就會誤判失敗；移除條件寫在 SPEC §6.5d，
//! 不因 Bot principal 上線而提前結束。
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

/// 省略 effective claim＝`Ok(None)`（User principal 本人）。Bot route handlers derive an omitted claim from the authenticated header first.
/// `target` 是這一則要送給誰（`POST /api/bots/{id}` 的 id）。
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
    match prove_bot(app, headers, claimed).await? {
        Proof::Verified(from) => Ok(Some(Relay { from, unverified: false })),
        // 沒帶：相容期照收（bot 還是得是活的）。不記 token（本來就沒有），只記誰冒了誰的名，
        // 方便相容期結束前清點還有誰沒帶。
        Proof::Absent(from) => {
            tracing::warn!(relay_from = %from, "relay_from without X-AM-Bot-Token: accepted as unverified (issue #339 compat)");
            Ok(Some(Relay { from, unverified: true }))
        }
    }
}

/// `prove_bot` 的結果：`Absent` 是「沒帶 token、bot 是活的」，要不要放行由呼叫端決定。
enum Proof {
    Verified(String),
    Absent(String),
}

/// 一顆 bot 的 id 配上 `X-AM-Bot-Token`：`/prompt` 與 mission 端點共用的那一段（issue #409）。
/// `claimed` 已 trim、非空、不是 `daemon`。
async fn prove_bot(app: &Arc<App>, headers: &HeaderMap, claimed: &str) -> Result<Proof, LcError> {
    // A Bot principal (the `/api` middleware already proved `X-AM-Bot-Id` + its token) may only
    // claim itself. Compared by id, not only by token, so the rule does not lean on tokens being
    // unique (issue #556: relay_from never overrides the authenticated identity).
    if let Some(id) = headers.get("X-AM-Bot-Id") {
        if id.to_str().ok().map(str::trim) != Some(claimed) {
            return Err(mismatch());
        }
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
        (Some(t), Some(b)) if !t.is_empty() && crate::api::ct_eq(&t, &b.hook_token) => Ok(Proof::Verified(b.id)),
        // 有帶但對不上（別顆的、空的、非 UTF-8），或指到不存在／已刪的 bot：同一個 403。
        // 不按「bot 存不存在」分成 400／403——那會讓狀態碼變成探測 bot id 的神諭。
        (Some(_), _) => Err(mismatch()),
        (None, Some(b)) => Ok(Proof::Absent(b.id)),
        (None, None) => Err(LcError::Bad(format!("relay_from must be a live bot id or `{}`", crate::agent_relay::DAEMON_SENDER))),
    }
}

fn mismatch() -> LcError {
    LcError::Forbidden(json!({
        "error": "forbidden",
        "reason": "relay_from_mismatch",
        "message": "X-AM-Bot-Id／X-AM-Bot-Token 不是 relay_from 那顆 bot 的；只能以自己的身分轉述",
    }))
}

/// mission 端點（`events`／`question`／`answer`／`revise`／`complete`／`deliver`）的 `relay_from`（issue #409）。
/// 省略 effective claim＝`Ok(None)`（User principal 本人）；the mission API handler derives a missing or blank Bot claim from the authenticated `X-AM-Bot-Id`.
/// 跟 `/prompt` 共用 `prove_bot`，差在兩格：
///
/// | relay_from | 條件 | 結果 |
/// |---|---|---|
/// | `daemon` | 呼叫端是驗證過的 AGM 角色 bot（`X-AM-Bot-Id`＋`X-AM-Bot-Token`） | 照收：`agm mission … --as-daemon` |
/// | `daemon` | 其他 | **403** `relay_from_reserved` |
/// | bot | 沒帶 `X-AM-Bot-Token` | **403** `relay_from_token_required`，沒有相容期 |
///
/// 沒有相容期：唯一帶 `relay_from` 的呼叫端是 `bin/agm`，它在角色自己的 pane 裡一律帶那顆的 token
/// （`scripts/agm.py` `bot_auth_headers`）；web 從不帶。照收就得在 `mission_events` 另記「未驗證」。
pub async fn authenticate_mission(app: &Arc<App>, headers: &HeaderMap, claimed: Option<&str>) -> Result<Option<String>, LcError> {
    let Some(claimed) = claimed.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    if claimed == crate::agent_relay::DAEMON_SENDER {
        // API middleware already rejected invalid Bot proof with 401; here `None` means a valid Bot that is not an AGM role.
        if crate::supervisor::bot_requests::actor_role(app, headers).await?.is_some() {
            return Ok(Some(claimed.to_string()));
        }
        return Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "relay_from_reserved",
            "message": "relay_from=daemon 只給驗證過的 AGM 角色 bot 代記（X-AM-Bot-Id＋X-AM-Bot-Token）",
        })));
    }
    match prove_bot(app, headers, claimed).await? {
        Proof::Verified(from) => Ok(Some(from)),
        Proof::Absent(_) => Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "relay_from_token_required",
            "message": "relay_from 是 bot 時要帶那顆 bot 自己的 X-AM-Bot-Token",
        }))),
    }
}
