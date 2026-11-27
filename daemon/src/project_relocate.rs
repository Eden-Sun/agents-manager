//! `PATCH /api/projects/:id {"path": …}`：專案目錄搬家後改 AG Man 記錄的路徑（relocate）。
//!
//! 操作者先在主機上把檔案搬好，再呼叫這裡；daemon 不搬檔、不動 transcript、不重啟 bot。
//! 改的是 config 的 `projects.path` 與 `[agents.projects]` 指示檔路徑，以及 DB 裡以舊路徑為前綴的
//! `bots.cwd`、`attachments.agent_path`、`shared_bots.workspace`、`pane_aliases.key`。歷史文字欄不動。

use crate::db;
use crate::lifecycle::LcError;
use crate::state::App;
use am_config::LOCAL_HOST;
use serde_json::{json, Value};
use std::sync::Arc;

/// 驗過、可以寫的一次改路徑。`_guards` 持有專案內所有 live bot 的鎖，寫完才放，期間沒有 bot 能啟動。
pub(crate) struct Relocate {
    pub(crate) old: String,
    pub(crate) new: String,
    pub(crate) host: String,
    pub(crate) label: String,
    pub(crate) home: String,
    _guards: Vec<tokio::sync::OwnedMutexGuard<()>>,
}

fn any_err<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(format!("{e:#}"))
}

fn bad(reason: &str) -> LcError {
    LcError::BadValue(json!({"error": "bad_request", "reason": reason}))
}

/// 新路徑的字面檢查：絕對路徑、去掉尾端 `/`、不是根目錄、不含控制字元與 `.`／`..` 段。
pub(crate) fn normalize_path(raw: &str) -> Result<String, LcError> {
    let p = raw.trim();
    if !p.starts_with('/') || p.chars().any(char::is_control) {
        return Err(bad("path_not_absolute"));
    }
    let p = p.trim_end_matches('/');
    if p.is_empty() || p.split('/').skip(1).any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return Err(bad("path_not_absolute"));
    }
    Ok(p.to_string())
}

/// `value` 等於 `old` 或以 `old/` 開頭時，換成 `new` 開頭；其他（含 `old2` 這種）回 `None`。
pub(crate) fn swap_prefix(value: &str, old: &str, new: &str) -> Option<String> {
    if value == old {
        return Some(new.to_string());
    }
    value.strip_prefix(old).filter(|rest| rest.starts_with('/')).map(|rest| format!("{new}{rest}"))
}

/// `[agents.projects]` 的一個指示檔路徑：`~` 以 `home` 展開後比對前綴，原本是 `~` 寫法且新路徑仍在 `home` 底下就保留 `~`。
pub(crate) fn rewrite_md_path(path: &str, old: &str, new: &str, home: &str) -> Option<String> {
    let tilde = path == "~" || path.starts_with("~/");
    if !tilde {
        return swap_prefix(path, old, new);
    }
    if home.is_empty() {
        return None;
    }
    let expanded = format!("{home}{}", &path[1..]);
    let swapped = swap_prefix(&expanded, old, new)?;
    if swapped == home {
        return Some("~".into());
    }
    Some(match swapped.strip_prefix(home).filter(|rest| rest.starts_with('/')) {
        Some(rest) => format!("~{rest}"),
        None => swapped,
    })
}

/// 改寫 `[agents.projects]` 裡 key 為專案 id 或 label 的那幾筆。
pub(crate) fn rewrite_agent_md(agents: &mut am_config::AgentsCfg, keys: &[&str], old: &str, new: &str, home: &str) {
    for key in keys {
        let Some(files) = agents.projects.get_mut(*key) else { continue };
        let fix = |p: &mut String| {
            if let Some(n) = rewrite_md_path(p, old, new, home) {
                *p = n;
            }
        };
        match files {
            am_config::AgentMdFiles::One(p) => fix(p),
            am_config::AgentMdFiles::Many(v) => v.iter_mut().for_each(fix),
        }
    }
}

/// 只有使用者，或 `[agents] fleet_operators` 列名的頂層 bot 能改路徑；其他一律 403 `user_only`。
pub(crate) async fn authorize(app: &Arc<App>, principal: &crate::api::RequestPrincipal) -> Result<(), LcError> {
    use crate::api::RequestPrincipal;
    match principal {
        RequestPrincipal::User => Ok(()),
        RequestPrincipal::Bot(caller) if crate::api::is_fleet_operator(app, caller).await? => Ok(()),
        _ => Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"}))),
    }
}

/// 新路徑在專案主機上是不是目錄，順便拿那台的家目錄（展開 `~` 用）。
async fn probe_dir(app: &Arc<App>, host: &str, path: &str) -> Result<(bool, String), LcError> {
    if host == LOCAL_HOST {
        let home = crate::home::dir().map(|h| h.to_string_lossy().trim_end_matches('/').to_string()).unwrap_or_default();
        return Ok((std::path::Path::new(path).is_dir(), home));
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    let script = format!(
        "if [ -d {p} ]; then printf 'AM_DIR=1\\n'; else printf 'AM_DIR=0\\n'; fi; printf 'AM_HOME=%s\\n' \"$HOME\"",
        p = crate::hosts::sh_quote(path)
    );
    let out = conn.ssh_exec(&script).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    let field = |k: &str| out.lines().find_map(|l| l.strip_prefix(k)).map(str::to_string);
    let dir = field("AM_DIR=").ok_or_else(|| LcError::Upstream("remote directory probe gave no answer".into()))? == "1";
    let home = match field("AM_HOME=").filter(|h| !h.is_empty()) {
        Some(h) => h,
        None => conn.remote_home.lock().await.clone().unwrap_or_default(),
    };
    Ok((dir, home.trim_end_matches('/').to_string()))
}

/// 驗證並鎖住專案內的 bot。新路徑等於目前路徑回 `None`（什麼都不用改）。
pub(crate) async fn prepare(app: &Arc<App>, id: &str, raw: &str) -> Result<Option<Relocate>, LcError> {
    let new = normalize_path(raw)?;
    let project = db::project(&app.db, id)
        .await
        .map_err(any_err)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    if let Some(h) = &project.handed_off_to {
        return Err(LcError::conflict("handed_off", json!({"project_id": id, "handed_off_to": h})));
    }
    let old = project.path.trim_end_matches('/').to_string();
    if new == old {
        return Ok(None);
    }
    let taken = db::live_projects(&app.db)
        .await
        .map_err(any_err)?
        .into_iter()
        .any(|p| p.id != id && p.host == project.host && p.path.trim_end_matches('/') == new);
    if taken {
        return Err(LcError::conflict("path_taken", json!({"project_id": id, "host": project.host, "path": new})));
    }
    let (is_dir, home) = probe_dir(app, &project.host, &new).await?;
    if !is_dir {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "path_not_found", "host": project.host, "path": new})));
    }
    let bots: Vec<db::Bot> = db::live_bots(&app.db).await.map_err(any_err)?.into_iter().filter(|b| b.project_id == id).collect();
    let (_, guards) = crate::api::lock_bots_in_order(app, bots.iter().map(|b| b.id.clone()).collect()).await;
    let mut running = Vec::new();
    for b in db::live_bots(&app.db).await.map_err(any_err)?.into_iter().filter(|b| b.project_id == id) {
        if db::active_run(&app.db, &b.id).await.map_err(any_err)?.is_some() {
            running.push(json!({"id": b.id, "name": b.name}));
        }
    }
    if !running.is_empty() {
        return Err(LcError::conflict("bots_running", json!({"project_id": id, "bots": running})));
    }
    Ok(Some(Relocate { old, new, host: project.host, label: project.label, home, _guards: guards }))
}

/// DB 端的前綴改寫，全部在同一個交易裡。
pub(crate) async fn rewrite_db(pool: &sqlx::SqlitePool, project_id: &str, r: &Relocate) -> anyhow::Result<()> {
    let prefix = format!("{}/", r.old);
    let from = r.old.chars().count() as i64 + 1;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE bots SET cwd = ? || substr(cwd, ?) WHERE project_id = ? AND deleted_at IS NULL AND (cwd = ? OR substr(cwd, 1, ?) = ?)",
    )
    .bind(&r.new).bind(from).bind(project_id).bind(&r.old).bind(from).bind(&prefix)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE attachments SET agent_path = ? || substr(agent_path, ?)
         WHERE bot_id IN (SELECT id FROM bots WHERE project_id = ?) AND (agent_path = ? OR substr(agent_path, 1, ?) = ?)",
    )
    .bind(&r.new).bind(from).bind(project_id).bind(&r.old).bind(from).bind(&prefix)
    .execute(&mut *tx)
    .await?;
    let has_shared: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'shared_bots')")
        .fetch_one(&mut *tx)
        .await?;
    if has_shared {
        sqlx::query(
            "UPDATE shared_bots SET workspace = ? || substr(workspace, ?)
             WHERE bot_id IN (SELECT id FROM bots WHERE project_id = ?) AND (workspace = ? OR substr(workspace, 1, ?) = ?)",
        )
        .bind(&r.new).bind(from).bind(project_id).bind(&r.old).bind(from).bind(&prefix)
        .execute(&mut *tx)
        .await?;
    }
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM pane_aliases").fetch_all(&mut *tx).await?;
    for key in keys {
        let Some(next) = rewrite_alias_key(&key, &r.host, &r.old, &r.new) else { continue };
        sqlx::query("UPDATE OR REPLACE pane_aliases SET key = ? WHERE key = ?").bind(&next).bind(&key).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `pane_aliases.key` 是 `[host, cwd, identity]` 的 JSON；同主機、cwd 以舊路徑為前綴才改。
pub(crate) fn rewrite_alias_key(key: &str, host: &str, old: &str, new: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(key).ok()?;
    let arr = v.as_array_mut()?;
    if arr.first()?.as_str()? != host {
        return None;
    }
    let cwd = swap_prefix(arr.get(1)?.as_str()?.trim_end_matches('/'), old, new)?;
    arr[1] = Value::String(cwd);
    Some(v.to_string())
}
