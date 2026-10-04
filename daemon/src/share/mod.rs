//! 分享 bot 給 end user（SPEC「分享 bot」，使用者 2026-10-03）。
//!
//! 拿到連結的人＝網路上任何人，他能對一顆跑在這台機器上的 agent 下指令。所以分成三層，各自有測試釘住：
//!
//! - [`cage`]：只有建 bot 時就選「分享用（受限）」的 bot（`shared_bots` 有一列）能分享。它的 claude 用
//!   `--restricted`（沒有 Bash／WebFetch、檔案工具只在工作目錄）＋`dontAsk`（沒預先允許的一律拒絕，不會停在權限框等人），
//!   工作目錄是建 bot 時選的資料夾（[`folder`]：新資料夾 `~/shared-bots/<名稱>` 或既有資料夾），權限是白名單（只有那個資料夾與自己的 outbox），pane env 只留 hook 要的幾個。
//!   它的 hook token 只能打 `/hook/*`：`/api`、`/relay/*`、`/build-slots/*` 一律拒絕（[`refuses_bot_principal`]）。
//! - [`admin`]：主 API（7788，只收 UI token）上開／關／重產連結、隨時拿回完整連結（DB 存 token 原文與 SHA-256，入口用 hash 查）。
//! - [`portal`]：獨立 listener（`[share] listen`），router 上只有 `/s/{token}/…` 與分享頁的靜態檔，沒有 fallback 到主 API。

pub(crate) mod admin;
pub(crate) mod cage;
pub(crate) mod folder;
pub(crate) mod multipart;
pub(crate) mod portal;
pub(crate) mod store;
pub(crate) mod svg_check;

#[cfg(test)]
mod tests;

/// 分享頁 end user 送進來的訊息記在 `messages.relay_from` 的哨符（跟 `daemon` 同一類：不是 bot id）。
/// 輸出時 `source` 改報 `share`（`db::Message` 的序列化），UI 標「🔗 分享使用者」。
pub(crate) const SHARE_SENDER: &str = "share";

/// daemon 開機時替每顆分享用 bot 的 outbox 補上不清的標記（`outbox-gc.sh` 看它）：開機前就在跑、這次沒重起的那幾顆也算。
/// 遠端的不管（分享用 bot 只給本機）。讀不到就記 warning，下次啟動那顆 bot 時 `cage::prepare` 會補。
pub(crate) async fn keep_share_outboxes(app: &std::sync::Arc<crate::state::App>) {
    let live = sqlx::query_scalar::<_, String>(
        "SELECT r.bot_id FROM shared_bots r JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL
           JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL AND p.host = 'local'",
    )
    .fetch_all(&app.db)
    .await;
    match live {
        Ok(ids) => ids.iter().for_each(|id| crate::outbox::mark_share_keep(&app.data_dir, id)),
        Err(e) => tracing::warn!(error = %e, "could not list share bots to keep their outboxes"),
    }
}

/// Revoke a bot's public share link after its lifecycle has been decided.
pub(crate) async fn revoke_bot_share(app: &std::sync::Arc<crate::state::App>, bot_id: &str) -> Result<(), sqlx::Error> {
    portal::kick(bot_id);
    crate::outbox::unmark_share_keep(&app.data_dir, bot_id);
    store::revoke_bot(&app.db, bot_id).await
}

/// Revoke all share links in a deleted project and close their active event streams.
pub(crate) async fn revoke_project_shares(app: &std::sync::Arc<crate::state::App>, project_id: &str) -> Result<(), sqlx::Error> {
    let ids = store::project_share_ids(&app.db, project_id).await?;
    for bot_id in ids {
        portal::kick(&bot_id);
    }
    // 分享關著的分享用 bot 也有標記：專案底下每一顆都拿掉。
    let restricted: Vec<String> =
        sqlx::query_scalar("SELECT r.bot_id FROM shared_bots r JOIN bots b ON b.id = r.bot_id WHERE b.project_id = ?").bind(project_id).fetch_all(&app.db).await?;
    for bot_id in restricted {
        crate::outbox::unmark_share_keep(&app.data_dir, &bot_id);
    }
    store::revoke_project(&app.db, project_id).await
}

/// 拿 bot 身分打 API 的（實際上只有 AGM 角色過得了 `UserOrAgm` 那道）不能刪、不能停分享用 bot，也不能刪裝著它的專案
/// （2026-10-04 使用者：「let AGM 不清除這類 bot」——它是給外部 end user 隨時來用的）。使用者自己（UI token）照常。
/// 回 `Some(bot_id)`＝擋下；讀不到也擋（fail closed，跟 [`refuses_bot_principal`] 同一個規矩）。重啟不擋：停了會自己起回來。
pub(crate) async fn guards_from_bot_principal(db: &sqlx::SqlitePool, method: &str, path: &str) -> Option<String> {
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').skip(1).collect();
    let protected = |id: String| async move { (!matches!(store::is_share_bot(db, &id).await, Ok(false))).then_some(id) };
    match (method, segs.as_slice()) {
        ("DELETE", ["api", "bots", id]) | ("POST", ["api", "bots", id, "stop"]) => protected((*id).to_string()).await,
        ("DELETE", ["api", "projects", id]) => {
            let found: Result<Option<String>, _> = sqlx::query_scalar(
                "SELECT r.bot_id FROM shared_bots r JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL WHERE b.project_id = ? LIMIT 1",
            )
            .bind(*id)
            .fetch_optional(db)
            .await;
            match found {
                Ok(hit) => hit,
                Err(_) => Some(String::new()),
            }
        }
        _ => None,
    }
}

/// 受限 bot 的 hook token 只用來打自己的 hook（信任分享不算）：其他任何拿 bot 身分進來的路一律擋。讀不到 DB 也擋（fail closed）。
pub(crate) async fn refuses_bot_principal(db: &sqlx::SqlitePool, bot_id: &str) -> bool {
    // 信任分享（trusted）是一般 bot 的權限：bot 身分照常能用；只有受限的關在籠子裡。
    !matches!(store::is_caged(db, bot_id).await, Ok(false))
}
