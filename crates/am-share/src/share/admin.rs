//! 分享連結的管理端點，在主 API（7788）上，只收 UI token（SPEC「分享 bot」、API.md）：
//!
//! - `GET  /api/bots/{id}/share` → `{shareable, enabled, url, needs_rotate, token_hint, created_at, last_used_at, allow_embed}`：開著就回完整 `url`
//!   （舊資料只有 hash：`url:null`、`needs_rotate:true`，重產一次就有）
//! - `POST /api/bots/{id}/share` `{"enabled":true|false, "allow_embed"?:bool}` → 開（已經開著就沿用同一條）／關（清掉 token）；`allow_embed` 只給信任分享
//! - `POST /api/bots/{id}/share/rotate` → 換新 token，舊連結當下失效，回新的 `url`

use std::path::Path;

use serde_json::{json, Value};

use crate::lifecycle::LcError;
use crate::share::folder::{self, ShareFolderIn};
use crate::share::remote_fs::RfsError;
use crate::share::site::{FencedConn, RemoteSite};
use crate::share::{cage, site, store};

pub fn db_err(e: sqlx::Error) -> LcError {
    LcError::Upstream(format!("share store: {e}"))
}

/// 活著、而且是分享用 bot（受限或信任分享）。不存在 404；不是分享用 bot 409 `not_shareable`。
pub async fn shareable_bot(app: &impl crate::capabilities::Db, id: &str) -> Result<bool, LcError> {
    let bot = crate::db::bot(app.db(), id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project = crate::db::project(app.db(), &bot.project_id).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if project.is_none_or(|p| p.deleted_at.is_some()) {
        return Err(LcError::NotFound("bot".into()));
    }
    store::is_share_bot(app.db(), id).await.map_err(db_err)
}

pub fn not_shareable(id: &str) -> LcError {
    LcError::conflict(
        "not_shareable",
        json!({"bot_id": id, "message": "只有建立時選「分享用（受限）」或「信任分享」的 bot 能分享；既有 bot 不能切換，要分享請新建一顆"}),
    )
}

pub async fn base_url(app: &impl crate::capabilities::Cfg) -> Result<String, LcError> {
    app.cfg().get().await.share.base().ok_or_else(|| {
        LcError::conflict(
            "share_not_configured",
            json!({"message": "config.toml 的 [share] base_url（Tailscale Funnel 的 https 網址）還沒設定"}),
        )
    })
}

/// 目前的分享狀態。開著就從 DB 存的 token 組完整網址；`base_url` 沒設時 `url:null`（開不了新的，但舊列照樣報狀態）。
pub async fn state(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db), id: &str) -> Result<Value, LcError> {
    let row = store::share(app.db(), id).await.map_err(db_err)?;
    let base = app.cfg().get().await.share.base();
    let url = row.as_ref().and_then(|r| r.token.as_deref()).zip(base).map(|(t, b)| format!("{b}/s/{t}"));
    Ok(json!({
        "shareable": true,
        "enabled": row.is_some(),
        "url": url,
        // 加 `token` 欄之前開的分享只有 hash：連結照樣能用，但拿不回完整網址，重產一次就有。
        "needs_rotate": row.as_ref().is_some_and(|r| r.token.is_none()),
        "token_hint": row.as_ref().map(|r| r.token_hint.clone()),
        "created_at": row.as_ref().map(|r| r.created_at.clone()),
        "last_used_at": row.as_ref().and_then(|r| r.last_used_at.clone()),
        "allow_embed": row.as_ref().is_some_and(|r| r.allow_embed),
    }))
}

/// 建分享用 bot（受限或信任分享）的前半：決定資料夾（新資料夾就建出來）、`shared_bots` 記下來（在寫 config 之前）。
/// 回 `(資料夾, 是不是這次建的)`；`replay`＝config 裡已經有同一個 `client_request_id` 的 bot：新資料夾已經在了就照用，不回 409。
pub async fn reserve_share_bot(
    app: &(impl crate::capabilities::Cfg + crate::capabilities::DataDir + crate::capabilities::Db),
    bot_id: &str,
    profile: &str,
    folder: &crate::share::folder::ShareFolderIn,
    replay: bool,
) -> Result<(String, bool), LcError> {
    use crate::share::folder;
    let home = crate::share::cage::local_home();
    let home_p = std::path::Path::new(&home);
    let (dir, created) = match folder {
        folder::ShareFolderIn::Existing { path } => (folder::check_existing(path, home_p, app.data_dir())?, false),
        folder::ShareFolderIn::New { name } => {
            let root = folder::root(app.cfg().get().await.share.folders_root.as_deref(), &home);
            match folder::create_new(&root, name, home_p, app.data_dir()) {
                Ok(d) => (d, true),
                Err(LcError::Conflict(_)) if replay => (std::fs::canonicalize(root.join(name)).unwrap_or_else(|_| root.join(name)), false),
                Err(e) => return Err(e),
            }
        }
    };
    if let Err(e) = folder::ensure_inbox(&dir) {
        if created {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Err(LcError::Upstream(format!("share folder inbox: {e}")));
    }
    let ws = dir.to_string_lossy().into_owned();
    if let Err(e) = store::insert_share_bot(app.db(), bot_id, profile, &ws).await {
        if created {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Err(db_err(e));
    }
    Ok((ws, created))
}

#[cfg(test)]
pub async fn reserve_restricted(app: &(impl crate::capabilities::Cfg + crate::capabilities::DataDir + crate::capabilities::Db), bot_id: &str, folder: &crate::share::folder::ShareFolderIn, replay: bool) -> Result<(String, bool), LcError> {
    reserve_share_bot(app, bot_id, store::PROFILE_RESTRICTED, folder, replay).await
}

/// 建分享用 bot 的資料夾與記錄，依專案主機分流：本機走 [`reserve_share_bot`]，遠端走 [`reserve_remote_share_bot`]。
pub async fn reserve_share_bot_on(
    app: &(impl crate::capabilities::Cfg + crate::capabilities::DataDir + crate::capabilities::Db + crate::hosts::HostsAccess),
    host: &str,
    bot_id: &str,
    profile: &str,
    folder: &ShareFolderIn,
    replay: bool,
) -> Result<(String, bool), LcError> {
    if host == am_base::config::LOCAL_HOST {
        return reserve_share_bot(app, bot_id, profile, folder, replay).await;
    }
    reserve_remote_share_bot(app, host, bot_id, profile, folder, replay).await
}

/// 遠端專案的分享 bot（R-S2 §4.1）：先 preflight（受限才查 claude 版本與 managed settings），再解析或建資料夾，
/// 最後 `ensure_inbox` 與 `shared_bots` 記錄。任何一步失敗，這次建的資料夾只 `rmdir`（遠端從不 `rm -rf`）。
pub async fn reserve_remote_share_bot(
    app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::hosts::HostsAccess),
    host: &str,
    bot_id: &str,
    profile: &str,
    folder: &ShareFolderIn,
    replay: bool,
) -> Result<(String, bool), LcError> {
    // 整段建立都綁在同一個權威圍籬上：中途 repoint 之後，後面的步驟會拿不到權威（fail closed），不會寫到換過的主機（#1026）。
    let conn = FencedConn::new(app.hosts().fence(host).await.ok_or_else(|| cage::preflight_conflict(crate::share::remote_fs::PreflightError::Unreachable))?);
    if profile == store::PROFILE_RESTRICTED {
        cage::preflight_restricted(&conn).await?;
    } else {
        crate::share::remote_fs::preflight_connected(&conn).map_err(cage::preflight_conflict)?;
    }
    let home = conn.home().await.map_err(|_| cage::preflight_conflict(crate::share::remote_fs::PreflightError::Unreachable))?;
    let instance = conn.instance().map(str::to_string);
    let root_cfg = app.cfg().get().await.share.folders_root.clone();
    let (ws, created) = match folder {
        ShareFolderIn::Existing { path } => (remote_existing(&conn, path).await?, false),
        ShareFolderIn::New { name } => {
            folder::check_new_name(name)?;
            let root = folder::remote_root(root_cfg.as_deref(), &home);
            match RemoteSite::create_folder(&conn, &root, name).await {
                Ok(p) => {
                    if let Err(e) = remote_check_path(&conn, &p, &home).await {
                        rmdir_made(&conn, host, &home, instance.as_deref(), bot_id, &p).await;
                        return Err(e);
                    }
                    (p, true)
                }
                Err(RfsError::Exists) if replay => (remote_existing(&conn, &format!("{root}/{name}")).await?, false),
                Err(RfsError::Exists) => {
                    return Err(LcError::conflict(
                        "folder_exists",
                        json!({"path": format!("{root}/{name}"), "host": host, "message": "遠端這個名字的資料夾已經有了；要用它請選「既有資料夾」，不然換個名字"}),
                    ))
                }
                Err(e) => return Err(remote_folder_error(e)),
            }
        }
    };
    let site = site::remote_site_at(conn.clone(), host, home.clone(), instance.as_deref(), bot_id, ws.clone())
        .ok_or_else(|| cage::preflight_conflict(crate::share::remote_fs::PreflightError::Unreachable))?;
    if let Err(e) = site.ensure_inbox().await {
        if created {
            let _ = site.remove_created_folder().await;
        }
        return Err(match e {
            RfsError::Unavailable => cage::preflight_conflict(crate::share::remote_fs::PreflightError::Unreachable),
            other => LcError::Upstream(format!("share folder inbox on {host}: {other}")),
        });
    }
    if let Err(e) = store::insert_share_bot(app.db(), bot_id, profile, &ws).await {
        if created {
            let _ = site.remove_created_folder().await;
        }
        return Err(db_err(e));
    }
    Ok((ws, created))
}

/// 既有的遠端資料夾：解析實體路徑，然後照本機同一套規則檢查（[`check_remote_path`]）。
async fn remote_existing(conn: &FencedConn, path: &str) -> Result<String, LcError> {
    if !path.trim().starts_with('/') {
        return Err(folder::bad("既有資料夾要給絕對路徑"));
    }
    let resolved = RemoteSite::resolve_folder(conn, path.trim()).await.map_err(|e| match e {
        RfsError::NotFound => folder::bad("找不到這個資料夾"),
        RfsError::Untrusted => folder::bad("這個路徑含符號連結、或不是資料夾（遠端只收實體路徑）"),
        RfsError::Unavailable => cage::preflight_conflict(crate::share::remote_fs::PreflightError::Unreachable),
        other => LcError::Upstream(format!("resolve remote folder: {other}")),
    })?;
    if !resolved.owned_by_me {
        return Err(folder::bad("這個資料夾不是 SSH 登入的使用者擁有的"));
    }
    if let Some(why) = folder::unsafe_reason(Path::new(&resolved.physical), Path::new(&resolved.home_physical), Path::new(&resolved.root_physical)) {
        return Err(folder::bad(why));
    }
    Ok(resolved.physical)
}

/// 新建的遠端資料夾（或重送時拿回的）實體路徑要過 `unsafe_reason`：家目錄的上層、帳號目錄、資料目錄、系統目錄都不行。
async fn remote_check_path(conn: &FencedConn, p: &str, home: &str) -> Result<(), LcError> {
    let r = RemoteSite::resolve_folder(conn, home).await.map_err(remote_folder_error)?;
    if let Some(why) = folder::unsafe_reason(Path::new(p), Path::new(&r.home_physical), Path::new(&r.root_physical)) {
        return Err(folder::bad(why));
    }
    Ok(())
}

/// 建失敗或重送拿不回來時，把這次建的遠端資料夾收掉（只 `rmdir`，非空就留著）。
async fn rmdir_made(conn: &FencedConn, host: &str, home: &str, instance: Option<&str>, bot_id: &str, p: &str) {
    if let Some(site) = site::remote_site_at(conn.clone(), host, home.to_string(), instance, bot_id, p.to_string()) {
        let _ = site.remove_created_folder().await;
    }
}

fn remote_folder_error(e: RfsError) -> LcError {
    match e {
        RfsError::Unavailable => cage::preflight_conflict(crate::share::remote_fs::PreflightError::Unreachable),
        RfsError::Unsafe(why) => folder::bad(why),
        RfsError::Untrusted => folder::bad("遠端的資料夾路徑含符號連結，或不是資料夾"),
        RfsError::NotFound => folder::bad("找不到這個資料夾"),
        other => LcError::Upstream(format!("remote folder: {other}")),
    }
}

/// 建受限 bot 的後半：建成了就把 `bots.cwd` 指到資料夾；沒建成（失敗、重送拿回舊的那顆）就把前半收回。
/// 資料夾只有「這次新建的」才刪：既有資料夾、重送時已經在的，一律不動。
pub async fn finish_restricted(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db), bot_id: &str, workspace: &str, created_folder: bool, created: bool) {
    if created {
        if let Err(e) = sqlx::query("UPDATE bots SET cwd = ? WHERE id = ?").bind(workspace).bind(bot_id).execute(app.db()).await {
            // 啟動時以 `shared_bots.workspace` 為準（`cage::prepare`），這裡寫不進去只影響側欄顯示的目錄。
            tracing::warn!(bot = bot_id, error = %e, "could not point the restricted bot's cwd at its folder");
        }
        crate::outbox::mark_share_keep(app.data_dir(), bot_id);
        return;
    }
    if let Err(e) = store::delete_restricted(app.db(), bot_id).await {
        tracing::warn!(bot = bot_id, error = %e, "could not roll back a restricted bot reservation");
    }
    if created_folder {
        let _ = std::fs::remove_dir_all(workspace);
    }
}

/// 依專案主機收尾或回滾分享 bot 的建立：
/// 本機走 [`finish_restricted`]；遠端若未建成只在遠端 `rmdir`，絕不刪除本機同路徑資料夾。
pub async fn finish_share_bot_on(
    app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::hosts::HostsAccess),
    host: &str,
    bot_id: &str,
    workspace: &str,
    created_folder: bool,
    created: bool,
) {
    if host == am_base::config::LOCAL_HOST {
        finish_restricted(app, bot_id, workspace, created_folder, created).await;
        return;
    }
    if created {
        if let Err(e) = sqlx::query("UPDATE bots SET cwd = ? WHERE id = ?").bind(workspace).bind(bot_id).execute(app.db()).await {
            tracing::warn!(bot = bot_id, error = %e, "could not point the restricted bot's cwd at its folder");
        }
        return;
    }
    if let Err(e) = store::delete_restricted(app.db(), bot_id).await {
        tracing::warn!(bot = bot_id, error = %e, "could not roll back a restricted bot reservation");
    }
    if created_folder {
        if let Some(fence) = app.hosts().fence(host).await {
            let conn = FencedConn::new(fence);
            match conn.home().await {
                Ok(home) => {
                    rmdir_made(&conn, host, &home, conn.instance(), bot_id, workspace).await;
                }
                Err(e) => {
                    tracing::warn!(bot = bot_id, host = host, error = %e, "could not resolve remote home to clean up folder {workspace}");
                }
            }
        } else {
            tracing::warn!(bot = bot_id, host = host, "could not get host connection to clean up folder {workspace}");
        }
    }
}
