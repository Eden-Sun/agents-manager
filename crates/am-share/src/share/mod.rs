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

pub mod admin;
pub mod budget;
pub mod cage;
pub mod compose;
pub mod folder;
pub mod multipart;
pub mod portal;
pub mod remote_fs;
pub mod site;
pub mod store;
pub mod svg_check;

#[cfg(test)]
mod remote_fs_tests;

#[cfg(test)]
mod test_dirs;

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests;

#[cfg(test)]
mod budget_remote_tests;

pub use crate::agent_relay::SHARE_SENDER;

/// daemon 開機時替每顆分享用 bot 的 outbox 補上不清的標記（`outbox-gc.sh` 看它）：開機前就在跑、這次沒重起的那幾顆也算。
/// 遠端專案的分享 bot 也補（遠端 outbox 那台若也跑 `outbox-gc.sh` 就會尊重它；daemon 自己的巡邏不靠它，見 `budget::sweep`）：
/// 主機斷線／ssh 失敗只記 warning，下次啟動那顆 bot 時 `cage::prepare` 會補。讀不到就記 warning。
pub async fn keep_share_outboxes<S: crate::outbox::ShareStorage + site::SiteEnv>(app: &S) {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT r.bot_id, p.host FROM shared_bots r JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL
           JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL",
    )
    .fetch_all(crate::outbox::ShareStorage::db_pool(app))
    .await;
    match rows {
        Ok(rows) => {
            for (id, host) in rows {
                if host == crate::config::LOCAL_HOST {
                    crate::outbox::mark_share_keep(crate::outbox::ShareStorage::data_dir(app), &id);
                } else {
                    set_remote_keep(app, &id, true, false).await;
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not list share bots to keep their outboxes"),
    }
}

/// 遠端專案的分享 bot：放（`keep=true`）或拿掉（`false`）遠端 outbox 的 `.am-share-keep`。本機的 bot 不做；失敗只記 warning。
/// `include_deleted`：收尾時 bot／專案常常已經標成刪除，仍要找得到它在哪台主機。
async fn set_remote_keep<S: site::SiteEnv>(app: &S, bot_id: &str, keep: bool, include_deleted: bool) {
    let resolved = if include_deleted { site::resolve_including_deleted(app, bot_id).await } else { site::resolve(app, bot_id).await };
    match resolved {
        Ok(site::ShareSite::Remote(remote)) => {
            if let Err(e) = remote.mark_share_keep(keep).await {
                tracing::warn!(bot = %bot_id, host = %remote.host, keep, error = %e, "could not update the remote share outbox keep mark");
            }
        }
        Ok(site::ShareSite::Local { .. }) | Err(site::SiteError::NotShareBot) => {}
        Err(site::SiteError::Unavailable) => {
            tracing::warn!(bot = %bot_id, keep, "remote share outbox keep mark skipped: the host is unreachable or unknown");
        }
    }
}

/// Revoke a bot's public share link after its lifecycle has been decided.
pub async fn revoke_bot_share<S: crate::outbox::ShareStorage + site::SiteEnv>(app: &S, bot_id: &str) -> Result<(), sqlx::Error> {
    portal::kick(bot_id);
    crate::outbox::unmark_share_keep(crate::outbox::ShareStorage::data_dir(app), bot_id);
    set_remote_keep(app, bot_id, false, true).await;
    store::revoke_bot(crate::outbox::ShareStorage::db_pool(app), bot_id).await
}

/// Revoke all share links in a deleted project and close their active event streams.
pub async fn revoke_project_shares<S: crate::outbox::ShareStorage + site::SiteEnv>(app: &S, project_id: &str) -> Result<(), sqlx::Error> {
    let pool = crate::outbox::ShareStorage::db_pool(app);
    let ids = store::project_share_ids(pool, project_id).await?;
    for bot_id in ids {
        portal::kick(&bot_id);
    }
    // 分享關著的分享用 bot 也有標記：專案底下每一顆都拿掉（遠端專案的拿掉遠端那份）。
    let restricted: Vec<String> =
        sqlx::query_scalar("SELECT r.bot_id FROM shared_bots r JOIN bots b ON b.id = r.bot_id WHERE b.project_id = ?").bind(project_id).fetch_all(pool).await?;
    for bot_id in restricted {
        crate::outbox::unmark_share_keep(crate::outbox::ShareStorage::data_dir(app), &bot_id);
        set_remote_keep(app, &bot_id, false, true).await;
    }
    store::revoke_project(pool, project_id).await
}

/// 拿 bot 身分打 API 的（實際上只有 AGM 角色過得了 `UserOrAgm` 那道）不能刪、不能停分享用 bot，也不能刪裝著它的專案
/// （2026-10-04 使用者：「let AGM 不清除這類 bot」——它是給外部 end user 隨時來用的）。使用者自己（UI token）照常。
/// 回 `Some(bot_id)`＝擋下；讀不到也擋（fail closed，跟 [`refuses_bot_principal`] 同一個規矩）。重啟不擋：停了會自己起回來。
pub async fn guards_from_bot_principal(db: &sqlx::SqlitePool, method: &str, path: &str) -> Option<String> {
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

pub use crate::db::refuses_bot_principal;
