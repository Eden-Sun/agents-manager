//! Image attachments. `agent.prompt` carries **text only**, so an image is written into the
//! agent's cwd on the bot's host (a sandboxed CLI reads it without a path-approval prompt,
//! `.gitignore`d) and its path is named in the prompt. Remote hosts also keep a daemon-side copy
//! so thumbnails need no ssh.
//!
//! **Persistence is staging-first (issue #88)**: `save()` used to write the file(s) first and only
//! `INSERT` the row at the very end, so a crash or DB error between "file written" and "row
//! inserted" left an orphan the daemon could never find again (no DB reference names it). Now the
//! row lands first with `state = 'staging'` — a durable record of exactly which local/remote paths
//! were *intended* — before any bytes move; a failure after that point (write failure, or the
//! final `state = 'ready'` UPDATE itself failing/crashing) leaves a row `reconcile_orphans` can
//! find on the next startup and clean up deterministically. Only `state = 'ready'` rows are
//! resolvable/bindable/readable.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db;
use crate::config::{valid_id, ID_RE};
use crate::hosts::{sh_quote, HostConn};

/// 單檔上限 50 MiB（2026-09-24 使用者：「網頁上傳最大 50mb 而不是 12」）；前端 `store/shelf.ts::MAX_BYTES` 必須同值。
pub const MAX_BYTES: usize = 50 * 1024 * 1024;

pub const SUBDIR: &str = ".agents-manager/attachments";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    /// Absolute path **on the bot's host** — what the agent is told to read.
    pub path: String,
}

pub fn ext_for(name: &str, mime: &str) -> String {
    if let Some((_, e)) = name.rsplit_once('.') {
        let e: String = e.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
        if !e.is_empty() {
            return e.to_ascii_lowercase();
        }
    }
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/heic" => "heic",
        "image/bmp" => "bmp",
        "application/pdf" => "pdf",
        "application/json" => "json",
        "application/zip" => "zip",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "text/html" => "html",
        _ => "bin",
    }
    .to_string()
}

/// 存進 DB、回給 UI 的檔名：控制字元（含 NUL、換行）與雙向覆寫／隔離字元（`evil<RLO>gnp.exe` 會被顯示成 `evilexe.png`）拿掉，
/// 最多 255 個字元；剩下空的就叫 `file`。不改 Unicode 本身（一般的中文檔名照舊）。真正落在磁碟上的檔名另走 [`safe_stem`]。
pub fn clean_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .take(255)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

/// Recognisable, but cannot escape the directory or upset a shell.
pub fn safe_stem(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    let cleaned: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "file".into()
    } else {
        trimmed.chars().take(48).collect()
    }
}

/// Only the UI cares: an image gets a thumbnail, everything else a file chip.
pub fn is_image(mime: &str) -> bool {
    mime.starts_with("image/")
}

pub fn local_copy_dir(app: &impl crate::capabilities::DataDir, bot_id: &str) -> Result<PathBuf> {
    if !valid_id(bot_id) {
        bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    Ok(app.data_dir().join("attachments").join(bot_id))
}

#[cfg(any(test, feature = "test-hooks"))]
pub async fn save(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::hosts::HostsAccess), bot_id: &str, name: &str, mime: &str, data: &[u8]) -> Result<Attachment> {
    save_bytes(app, bot_id, name, mime, Bytes::copy_from_slice(data)).await
}

/// 測試用：附件檔名 → 寫它的那條執行緒（要證明不是 tokio worker 自己寫的）。
#[cfg(any(test, feature = "test-hooks"))]
pub static WRITE_THREADS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, std::thread::ThreadId>>> = std::sync::LazyLock::new(Default::default);

/// 同 [`save`]，但 body 已經是 `Bytes`（上傳 handler）：寫檔放到 blocking pool 時只複製參考、不複製最多 50 MiB。
pub async fn save_bytes(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::hosts::HostsAccess), bot_id: &str, name: &str, mime: &str, data: Bytes) -> Result<Attachment> {
    if data.is_empty() {
        bail!("attachment is empty");
    }
    if data.len() > MAX_BYTES {
        bail!("attachment is {} bytes; the limit is {}", data.len(), MAX_BYTES);
    }
    let bot = db::bot(app.db(), bot_id)
        .await?
        .filter(|bot| bot.deleted_at.is_none())
        .ok_or_else(|| anyhow::anyhow!("no such bot"))?;
    let project = db::project(app.db(), &bot.project_id)
        .await?
        .filter(|project| project.deleted_at.is_none())
        .ok_or_else(|| anyhow::anyhow!("no such project"))?;

    let name = clean_name(name);
    let name = name.as_str();
    let id = db::ulid();
    let file = format!("{}-{}.{}", id, safe_stem(name), ext_for(name, mime));
    let dir = format!("{}/{}", project.path.trim_end_matches('/'), SUBDIR);
    let agent_path = format!("{dir}/{file}");

    let host = app
        .hosts()
        .get(&project.host)
        .await
        .ok_or_else(|| anyhow::anyhow!("host `{}` is not configured", project.host))?;

    // The daemon can always read `local_path`; on a remote host that is its own copy. Computed
    // before any I/O so the staging row below already names the exact path(s) a crash needs to
    // clean up — nothing here can fail (no I/O, no fallible parsing).
    let local_path = if host.is_local() { agent_path.clone() } else { local_copy_dir(app, bot_id)?.join(&file).to_string_lossy().into_owned() };

    sqlx::query(
        "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at)
         VALUES (?,?,?,?,?,?,?,?,'staging',?)",
    )
    .bind(&id)
    .bind(bot_id)
    .bind(name)
    .bind(mime)
    .bind(data.len() as i64)
    .bind(&local_path)
    .bind(&agent_path)
    .bind(&project.host)
    .bind(db::now())
    .execute(app.db())
    .await?;

    if let Err(e) = write_bytes(&host, &project.host, project.path.trim_end_matches('/'), &dir, &file, &agent_path, &local_path, &data).await {
        best_effort_cleanup(&host, &local_path, &agent_path).await;
        let _ = sqlx::query("UPDATE attachments SET state = 'failed' WHERE id = ?").bind(&id).execute(app.db()).await;
        return Err(e);
    }
    // Bytes are down; the row still says 'staging' until this commits. A crash or DB error right
    // here leaves exactly that — `reconcile_orphans` picks it up next startup and cleans the files
    // it just wrote, since nothing else can ever know they belong to a message.
    sqlx::query("UPDATE attachments SET state = 'ready' WHERE id = ?").bind(&id).execute(app.db()).await?;

    Ok(Attachment { id, name: name.to_string(), mime: mime.to_string(), size: data.len() as i64, path: agent_path })
}

/// The actual byte transfer — local write, or daemon-side copy + `ssh_put`. Split out of [`save`] so
/// its failure path (leave `state = 'staging'`/`'failed'` for [`reconcile_orphans`], don't touch the
/// DB row here) stays separate from the row bookkeeping.
#[allow(clippy::too_many_arguments)]
async fn write_bytes(host: &Arc<HostConn>, host_name: &str, project_dir: &str, dir: &str, file: &str, agent_path: &str, local_path: &str, data: &Bytes) -> Result<()> {
    // 最多 50 MiB 的同步寫檔放到 blocking pool：在 tokio worker 上做會卡住同一條 worker 上的其他請求。
    if host.is_local() {
        let (project_dir, dir, file, agent_path, data) = (project_dir.to_string(), dir.to_string(), file.to_string(), agent_path.to_string(), data.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            // 專案目錄是 agent 寫得到的地方：`.agents-manager`／`attachments` 被換成連結時不能跟進去（讀取側同一個界線）。
            let comps = [std::ffi::OsStr::new(".agents-manager"), std::ffi::OsStr::new("attachments")];
            let dir_fd = crate::trusted_open::create_bound_dirs(Path::new(&project_dir), &comps).with_context(|| format!("create {dir}"))?;
            // `.gitignore` 只在還沒有時寫；已經有（含被換成連結）就不動。
            let _ = crate::trusted_open::write_new_file_in(&dir_fd, std::ffi::OsStr::new(".gitignore"), b"*\n");
            crate::trusted_open::write_new_file_in(&dir_fd, std::ffi::OsStr::new(&file), &data).with_context(|| format!("write {agent_path}"))?;
            #[cfg(any(test, feature = "test-hooks"))]
            WRITE_THREADS.lock().unwrap_or_else(|e| e.into_inner()).insert(file.clone(), std::thread::current().id());
            Ok(())
        })
        .await??;
    } else {
        let copy_dir = Path::new(local_path).parent().ok_or_else(|| anyhow::anyhow!("`{local_path}` has no parent directory"))?.to_path_buf();
        let (lp, bytes) = (local_path.to_string(), data.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            std::fs::create_dir_all(&copy_dir).with_context(|| format!("create {}", copy_dir.display()))?;
            std::fs::write(&lp, &bytes).with_context(|| format!("write {lp}"))?;
            Ok(())
        })
        .await??;
        host.ssh_put(agent_path, data).await.with_context(|| format!("copy attachment to {host_name}:{agent_path}"))?;
        let _ = host.ssh_exec(&format!("printf '*\\n' > {}", sh_quote(&format!("{dir}/.gitignore")))).await;
    }
    Ok(())
}

/// Remove whatever [`write_bytes`] may have already put down. Best effort and idempotent — safe to
/// call again from [`reconcile_orphans`] if this fails or the process dies before even trying.
async fn best_effort_cleanup(host: &Arc<HostConn>, local_path: &str, agent_path: &str) {
    let _ = std::fs::remove_file(local_path);
    if !host.is_local() {
        let _ = host.ssh_exec(&format!("rm -f {}", sh_quote(agent_path))).await;
    }
}

/// Run once at startup, before anything could be mid-`save()`: any row still `'staging'` is the
/// leftover of a process that died between the `INSERT` and the final `state = 'ready'` UPDATE
/// (issue #88); `'failed'` rows are ones `save()` already gave up on, whose best-effort cleanup may
/// not have finished. Both are unrecoverable as attachments (bytes may be partial or absent) —
/// clean up the file(s) and drop the row. Idempotent: nothing to do once already cleaned.
pub async fn reconcile_orphans(app: &(impl crate::capabilities::Db + crate::hosts::HostsAccess)) -> usize {
    let rows: Vec<(String, String, String, String)> = match sqlx::query_as(
        "SELECT id, local_path, agent_path, host FROM attachments WHERE state IN ('staging','failed')",
    )
    .fetch_all(app.db())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not list orphaned attachments");
            return 0;
        }
    };
    let mut cleaned = 0;
    for (id, local_path, agent_path, host_name) in rows {
        remove_files(app, &local_path, &agent_path, &host_name).await;
        match sqlx::query("DELETE FROM attachments WHERE id = ?").bind(&id).execute(app.db()).await {
            Ok(_) => cleaned += 1,
            Err(e) => tracing::warn!(id = %id, error = %e, "could not remove an orphaned attachment row"),
        }
    }
    if cleaned > 0 {
        tracing::info!(cleaned, "cleaned up orphaned (staging/failed) attachment rows left by a previous run");
    }
    cleaned
}

/// 刪一個附件的位元組：daemon 能讀的那份（本機 bot 就是專案裡那份、遠端 bot 是資料目錄裡的副本），遠端再 best-effort
/// ssh 刪 agent 讀的那份。冪等，失敗只是留下一個沒有 row 的檔，不影響呼叫端。
async fn remove_files(app: &impl crate::hosts::HostsAccess, local_path: &str, agent_path: &str, host_name: &str) {
    let _ = std::fs::remove_file(local_path);
    if let Some(host) = app.hosts().get(host_name).await {
        if !host.is_local() {
            let _ = host.ssh_exec(&format!("rm -f {}", sh_quote(agent_path))).await;
        }
    }
}

/// 有訊息的 `attachments_json` 點名這個附件（`a` 是 attachments 那列）。`IS NOT NULL` 讓 `messages_with_attachments`
/// 這個局部索引派得上用場：1.7 萬則訊息裡只有幾百則帶附件，以前每個候選附件都把 `messages` 全表 LIKE 過一遍
/// （開機那次掃 50 筆候選花了 1.9 秒）。`NULL LIKE …` 本來就不成立，所以結果不變。
pub const NAMED_BY_A_MESSAGE: &str = "NOT EXISTS (SELECT 1 FROM messages m WHERE m.attachments_json IS NOT NULL AND m.attachments_json LIKE '%' || a.id || '%')";

/// 上傳了卻沒送出的附件（`ready`、沒有訊息引用）留多久。使用者貼圖到輸入框、當天回來送出的都夠；
/// 送出時才 `resolve`＋`bind`（幾毫秒內），所以這個窗口只會碰到「上傳完放著不管」的。
pub const UNREFERENCED_KEEP_SECS: i64 = 24 * 3600;
/// 例行掃的間隔（開機另外跑一次）。
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// 清掉上傳了卻從沒送出的附件：`ready`、`message_id` 空、放超過 `keep_secs`（從上傳、或最後一次被 `resolve`／解綁算起）、**而且沒有任何訊息的 `attachments_json` 點名它**
/// （舊版兩步綁定可能留下「訊息點名了、`message_id` 卻沒設」的列）。被訊息引用的絕不刪；`staging`／`failed` 是
/// [`reconcile_orphans`] 的事。回清掉幾筆。
///
/// 先刪 row 才刪檔：那一句 DELETE 自己帶同樣的條件，跟 [`bind`] 搶同一列——bind 先贏，這裡 0 rows、什麼檔都不動；
/// 這裡先贏，才刪檔。檔案刪不掉只留下一個沒有 row 的檔（沒有人引用它），不會有「row 在、檔沒了」。
pub async fn sweep_unreferenced(app: &(impl crate::capabilities::Db + crate::hosts::HostsAccess), keep_secs: i64) -> usize {
    let cutoff = db::iso_in(-keep_secs);
    let created = db::ts_sql("a.created_at");
    let named = NAMED_BY_A_MESSAGE;
    let rows: Vec<(String, String, String, String)> = match sqlx::query_as(&format!(
        "SELECT a.id, a.local_path, a.agent_path, a.host FROM attachments a
          WHERE a.state = 'ready' AND a.message_id IS NULL AND {created} <= ? AND {named}"
    ))
    .bind(&cutoff)
    .fetch_all(app.db())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not list unreferenced attachments");
            return 0;
        }
    };
    #[cfg(any(test, feature = "test-hooks"))]
    if let Some((id, _, _, _)) = rows.first() {
        crate::race_point::hit("attachment_sweep_after_candidates", id).await;
    }
    let mut swept = 0;
    for (id, local_path, agent_path, host_name) in rows {
        let won = sqlx::query(&format!(
            "DELETE FROM attachments AS a WHERE a.id = ? AND a.state = 'ready' AND a.message_id IS NULL AND {created} <= ? AND {named}"
        ))
        .bind(&id)
        .bind(&cutoff)
        .execute(app.db())
        .await;
        match won {
            Ok(r) if r.rows_affected() == 1 => {
                remove_files(app, &local_path, &agent_path, &host_name).await;
                swept += 1;
            }
            Ok(_) => {} // 剛好被 bind 搶走：它現在有訊息引用了。
            Err(e) => tracing::warn!(id = %id, error = %e, "could not remove an unreferenced attachment row"),
        }
    }
    if swept > 0 {
        tracing::info!(swept, keep_hours = keep_secs / 3600, "removed uploaded attachments no message ever referenced");
    }
    swept
}

/// 開機跑一次、之後每 6 小時一次：常駐好幾天的 daemon 也要收。
pub fn spawn_sweep<H>(app: Arc<H>)
where
    H: crate::capabilities::Db + crate::hosts::HostsAccess + crate::capabilities::BgTasks + crate::capabilities::Shutdown + 'static,
{
    crate::background_loop::spawn_periodic(&app, "attachment sweep", SWEEP_EVERY, std::time::Duration::ZERO, |app| async move {
        sweep_unreferenced(&app, UNREFERENCED_KEEP_SECS).await;
    });
}


/// Restricted to the recipient's **project**: an unrelated chat's id must not become a path here.
/// Project (not bot) scope lets one group send share an upload with every recipient.
pub async fn resolve(app: &impl crate::capabilities::Db, bot_id: &str, ids: &[String]) -> Result<Vec<Attachment>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let bot = db::bot(app.db(), bot_id).await?.ok_or_else(|| anyhow::anyhow!("no such bot"))?;
    // Serialize lookup+refresh against the orphan sweeper. If the sweep wins first, the row is
    // gone before this lookup; if resolve wins, refreshing created_at fences its stale candidate.
    let mut tx = db::begin_write(app.db()).await?;
    let mut out = Vec::new();
    for id in ids {
        let row = sqlx::query_as::<_, (String, String, String, i64, String)>(
            "SELECT a.id, a.name, a.mime, a.size, a.agent_path FROM attachments a
             JOIN bots b ON b.id = a.bot_id
             WHERE a.id = ? AND b.project_id = ? AND a.state = 'ready'",
        )
        .bind(id)
        .bind(&bot.project_id)
        .fetch_optional(&mut *tx)
        .await?;
        match row {
            Some((id, name, mime, size, path)) => {
                // 正要被用了：重新算「沒人用」的時間，resolve 到 bind 之間輪到清理也不會把三天前上傳的它當孤兒刪掉
                // （`created_at` 除了清理沒有別的讀者，所以它就是「最後一次被用到」）。
                let _ = sqlx::query("UPDATE attachments SET created_at = ? WHERE id = ? AND message_id IS NULL")
                    .bind(db::now())
                    .bind(&id)
                    .execute(&mut *tx)
                    .await;
                out.push(Attachment { id, name, mime, size, path })
            }
            None => bail!("unknown attachment `{id}`"),
        }
    }
    tx.commit().await?;
    Ok(out)
}

/// Single transaction (issue #88): the old two-step (update `messages.attachments_json`, then one
/// `UPDATE` per attachment) could die partway, leaving the message's projection naming attachments
/// whose own `message_id` was never set. Any failure — including an attachment `UPDATE` matching
/// zero rows, checked via `rows_affected` — rolls back everything; nothing here is visible until
/// `commit()` succeeds.
pub async fn bind(app: &impl crate::capabilities::Db, message_id: &str, items: &[Attachment]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut tx = app.db().begin().await?;
    bind_tx(&mut tx, message_id, items).await?;
    tx.commit().await?;
    Ok(())
}

/// [`bind`] 放進呼叫端的交易：訊息、turn 與附件要一起成立或一起不算（issue #122 先收下再啟動）。
pub async fn bind_tx(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, message_id: &str, items: &[Attachment]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let payload = serde_json::to_string(items)?;
    let msg = sqlx::query("UPDATE messages SET attachments_json = ? WHERE id = ?")
        .bind(&payload)
        .bind(message_id)
        .execute(&mut **tx)
        .await?;
    if msg.rows_affected() != 1 {
        bail!("message `{message_id}` does not exist");
    }
    for a in items {
        // `state = 'ready'`: binding a staging/failed row would let a message point at bytes that
        // may never exist.
        let res = sqlx::query("UPDATE attachments SET message_id = ? WHERE id = ? AND state = 'ready'")
            .bind(message_id)
            .bind(&a.id)
            .execute(&mut **tx)
            .await?;
        if res.rows_affected() != 1 {
            bail!("attachment `{}` is missing or not ready", a.id);
        }
    }
    Ok(())
}

/// 撤回一則沒送出的訊息之前，把它帶的附件解綁（`messages` 要刪，`attachments.message_id` 沒有 ON DELETE）。
/// 訊息要是 `msg_id` 或 `turn_id` 底下的。解綁當下重新算「沒人用」的時間：同一個 `client_request_id` 馬上原樣重送，
/// 舊上傳不能在重送之前被清理掃掉。
pub async fn unbind_message(conn: &mut sqlx::SqliteConnection, msg_id: &str, turn_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE attachments SET message_id = NULL, created_at = ?
          WHERE message_id IN (SELECT id FROM messages WHERE id = ? OR turn_id = ?)",
    )
    .bind(db::now())
    .bind(msg_id)
    .bind(turn_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Paths spelled out plainly enough that every CLI reads them with its file tool.
pub fn deliver_text(text: &str, items: &[Attachment]) -> String {
    if items.is_empty() {
        return text.to_string();
    }
    let mut s = text.trim_end().to_string();
    if !s.is_empty() {
        s.push_str("\n\n");
    }
    // 全是圖片就照舊說「圖片」——那是最常見的情況，講得具體一點；混到別的檔案就講「檔案」。
    let images_only = items.iter().all(|a| is_image(&a.mime));
    let kind = if images_only { "圖片" } else { "檔案" };
    let one = items.len() == 1;
    s.push_str(&format!("附加{kind}（請讀取{}檔案來查看）：\n", if one { "這個" } else { "這些" }));
    for a in items {
        s.push_str(&a.path);
        s.push('\n');
    }
    s.trim_end().to_string()
}

/// `GET /api/attachments/:id`
///
/// 本機 bot 的附件放在專案目錄裡（agent 寫得到的地方），所以讀取跟 [`crate::outbox`]／[`crate::local_image`] 一樣走
/// [`crate::trusted_open`]：從信任邊界逐層 `openat(O_NOFOLLOW)`、拿 fd 讀，並設大小上限。被換成符號連結（指到私鑰、別的 bot 的檔案、
/// `/dev/zero`）一律讀不到。以前是 `std::fs::read(local_path)`：跟著連結走、沒有上限。
pub async fn read(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db), id: &str) -> Result<(String, Vec<u8>)> {
    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT mime, local_path, bot_id FROM attachments WHERE id = ? AND state = 'ready'",
    )
    .bind(id)
    .fetch_optional(app.db())
    .await?;
    let Some((mime, path, bot_id)) = row else { bail!("unknown attachment") };
    let data = match read_contained(app.data_dir(), Path::new(&path)).await {
        Ok(d) => d,
        // 刪 bot 會把 `attachments/<id>/` 搬進 `bots-trash`（#465），但已刪 bot 的對話仍讀得到
        // （API.md §10.4），前端照樣會來抓縮圖——原地讀不到就去回收區裡那一份找同一個檔名。
        // 只有遠端 bot 的 `local_path` 在資料目錄底下；本機 bot 指的是專案裡那份，不受影響。
        Err(e) => match trashed_copy(app, &bot_id, &path) {
            Some(alt) => read_contained(app.data_dir(), &alt).await.with_context(|| format!("read {} (trashed)", alt.display()))?,
            None => return Err(e).with_context(|| format!("read {path}")),
        },
    };
    Ok((mime, data))
}

/// 信任邊界內讀一個附件。資料目錄底下的（遠端 bot 的副本、回收區）以資料目錄為界；本機專案裡的
/// （`<專案>/.agents-manager/attachments/<檔>`，形狀是 [`save`] 寫死的）以記下來的專案目錄為界——不看 `projects.path`
/// 現在是什麼：專案搬家之後舊附件的路徑照樣要讀得到。其他形狀的路徑一律不讀。
async fn read_contained(data_dir: &Path, path: &Path) -> Result<Vec<u8>> {
    let (base, rel): (PathBuf, PathBuf) = if let Ok(rel) = path.strip_prefix(data_dir) {
        (data_dir.to_path_buf(), rel.to_path_buf())
    } else {
        let comps: Vec<&std::ffi::OsStr> = path.components().filter_map(|c| match c { std::path::Component::Normal(s) => Some(s), _ => None }).collect();
        let n = comps.len();
        let shaped = path.is_absolute() && n >= 4 && comps[n - 3] == ".agents-manager" && comps[n - 2] == "attachments";
        if !shaped {
            bail!("`{}` is not an attachment location", path.display());
        }
        let base = path.ancestors().nth(3).context("attachment path has no project directory")?.to_path_buf();
        let rel = path.strip_prefix(&base)?.to_path_buf();
        (base, rel)
    };
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let comps = crate::trusted_open::safe_relative_components(&rel).context("unsafe attachment path")?;
        let file = crate::trusted_open::open_bound_file(&base, &comps, None)?;
        crate::trusted_open::read_limited(file, MAX_BYTES as u64).map_err(|_| anyhow::anyhow!("attachment is too large or unreadable"))
    })
    .await?
}

/// `local_path` 原地不在時，回收區裡對應的那一份（`bots-trash/<id>.attachments.<毫秒>/<檔名>`）。
/// 只認「本來就在 `<資料目錄>/attachments/<bot_id>/` 底下」的路徑，其他一律不找。
fn trashed_copy(app: &impl crate::capabilities::DataDir, bot_id: &str, local_path: &str) -> Option<PathBuf> {
    let under = crate::bot_trash::attachments_dir(app.data_dir(), bot_id);
    let name = Path::new(local_path).strip_prefix(&under).ok()?;
    let dir = crate::bot_trash::latest_kind(app.data_dir(), bot_id, Some(crate::bot_trash::ATTACHMENTS))?;
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

/// 送回瀏覽器的 `Content-Type`：白名單以外一律 `application/octet-stream`（#471，理由同
/// [`crate::outbox::file`]——上傳的 mime 是呼叫端自己給的，使用者的 HTML 不該在 daemon 這個
/// origin 跑起來，UI token 就放在這個 origin 的 localStorage）。
///
/// **跟 `outbox::file` 在 `image/svg+xml` 這一型上是不一樣的**（#476，別讀成完全一致）：
/// outbox 把 svg 排除在白名單外，這裡留著而且 inline。差別在用途——outbox 的檔案是**下載**，
/// 擋掉不影響任何畫面；附件會被 UI 當縮圖 `<img src={blobUrl}>` 畫出來，而瀏覽器對 SVG
/// **不做內容嗅探**，型別一換就是看得見的破圖。這裡改用回應標頭擋（`nosniff` ＋
/// `Content-Security-Policy: sandbox; default-src 'none'`，見 `api::get_attachment`）：
/// `<img>` 裡的 SVG 本來就不跑腳本，真的被導航到時 sandbox 讓它拿不到這個 origin。
///
/// **只影響送出去的標頭**：`attachments.mime` 照舊原樣存、原樣回給 UI 判斷要不要畫縮圖，
/// 所以前端 `item.mime.startsWith('image/')` 那條邏輯不受影響。
pub fn served_mime(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/gif" => "image/gif",
        "image/webp" => "image/webp",
        // svg 留在名單裡而且照舊 inline：拿掉它會讓現有的 svg 縮圖**變破圖**——前端是
        // `<img src={blobUrl}>`，而瀏覽器對 SVG 不做內容嗅探，型別不是 `image/svg+xml` 就不算繪。
        // 為了修一個目前打不穿的硬化缺口而製造看得見的回歸不划算；改成靠回應本身的標頭擋：
        // `nosniff` ＋ `Content-Security-Policy: sandbox`（見 `get_attachment`）。`<img>` 裡的 SVG
        // 本來就不會跑腳本，真的被導航到時 sandbox 讓它拿不到這個 origin。
        "image/svg+xml" => "image/svg+xml",
        "application/pdf" => "application/pdf",
        "text/plain" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 圖片才讓瀏覽器內嵌（UI 用 `<img>`）；其他一律當附件下載，不在這個 origin 算繪。
///
/// `application/pdf` 在 [`served_mime`] 的白名單裡**但不 inline**：白名單管的是「下載下來的型別要對」，
/// 內嵌與否是另一件事，而 UI 目前沒有 pdf 預覽。要做 pdf 預覽時再一起評估（見 docs/API.md）。
pub fn is_inline(served: &str) -> bool {
    served.starts_with("image/")
}

pub fn to_json(a: &Attachment) -> serde_json::Value {
    json!({"id": a.id, "name": a.name, "mime": a.mime, "size": a.size, "path": a.path})
}
