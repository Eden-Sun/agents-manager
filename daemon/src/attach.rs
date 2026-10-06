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
#[cfg(test)]
use crate::state::App;
use crate::config::{valid_id, ID_RE};
use crate::hosts::{sh_quote, HostConn};

/// 單檔上限 50 MiB（2026-09-24 使用者：「網頁上傳最大 50mb 而不是 12」）；前端 `store/shelf.ts::MAX_BYTES` 必須同值。
pub const MAX_BYTES: usize = 50 * 1024 * 1024;

const SUBDIR: &str = ".agents-manager/attachments";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    /// Absolute path **on the bot's host** — what the agent is told to read.
    pub path: String,
}

fn ext_for(name: &str, mime: &str) -> String {
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
pub(crate) fn clean_name(name: &str) -> String {
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
fn safe_stem(name: &str) -> String {
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

fn local_copy_dir(app: &impl crate::capabilities::DataDir, bot_id: &str) -> Result<PathBuf> {
    if !valid_id(bot_id) {
        bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    Ok(app.data_dir().join("attachments").join(bot_id))
}

#[cfg(test)]
pub async fn save(app: &Arc<App>, bot_id: &str, name: &str, mime: &str, data: &[u8]) -> Result<Attachment> {
    save_bytes(app, bot_id, name, mime, Bytes::copy_from_slice(data)).await
}

/// 測試用：附件檔名 → 寫它的那條執行緒（要證明不是 tokio worker 自己寫的）。
#[cfg(test)]
static WRITE_THREADS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, std::thread::ThreadId>>> = std::sync::LazyLock::new(Default::default);

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
            #[cfg(test)]
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
const NAMED_BY_A_MESSAGE: &str = "NOT EXISTS (SELECT 1 FROM messages m WHERE m.attachments_json IS NOT NULL AND m.attachments_json LIKE '%' || a.id || '%')";

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
    #[cfg(test)]
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
pub fn spawn_sweep(app: Arc<impl crate::capabilities::Db + crate::hosts::HostsAccess + 'static>) {
    tokio::spawn(async move {
        loop {
            sweep_unreferenced(&app, UNREFERENCED_KEEP_SECS).await;
            tokio::time::sleep(SWEEP_EVERY).await;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::App;
    use std::sync::Arc;
    use crate::testing as tt;

    /// 清附件問「有沒有訊息點名它」時，只能看帶附件的那幾則（局部索引），不能對每個候選附件把 messages 全表 LIKE 一遍。
    #[tokio::test]
    async fn the_named_by_a_message_check_uses_the_attachment_message_index() {
        let env = tt::env().await;
        let sql = format!("EXPLAIN QUERY PLAN SELECT a.id FROM attachments a WHERE {NAMED_BY_A_MESSAGE}");
        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(&sql).fetch_all(&env.app.db).await.unwrap();
        let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
        assert!(details.iter().any(|d| d.contains("USING INDEX messages_with_attachments")), "{details:#?}");
        assert!(!details.iter().any(|d| d.starts_with("SCAN m") && !d.contains("messages_with_attachments")), "{details:#?}");
    }

    /// #471：上傳時的 mime 是呼叫端自己給的，送回去不能原樣照用——白名單外一律 octet-stream。
    /// **SVG 在白名單裡而且 inline**（瀏覽器對 SVG 不嗅探，落成 octet-stream 會讓現有縮圖變破圖），
    /// 靠回應的 `nosniff` ＋ `Content-Security-Policy: sandbox` 擋；只有 `image/*` inline，
    /// 其餘（含 pdf）都是 attachment。
    #[test]
    fn the_served_mime_is_whitelisted_and_only_images_are_inline() {
        for (given, want) in [
            ("image/png", "image/png"),
            ("image/jpeg; charset=binary", "image/jpeg"),
            ("IMAGE/PNG", "image/png"),
            ("application/pdf", "application/pdf"),
            ("text/plain", "text/plain; charset=utf-8"),
            ("text/html", "application/octet-stream"),
            ("application/xhtml+xml", "application/octet-stream"),
            ("", "application/octet-stream"),
            // svg 留在白名單而且 inline：瀏覽器對 SVG 不嗅探，改成 octet-stream 會讓現有縮圖變破圖。
            // 安全性由回應的 nosniff ＋ `Content-Security-Policy: sandbox` 擔（見 `get_attachment`）。
            ("image/svg+xml", "image/svg+xml"),
        ] {
            assert_eq!(served_mime(given), want, "{given}");
        }
        assert!(is_inline(served_mime("image/png")));
        assert!(is_inline(served_mime("image/svg+xml")), "svg 要能 inline，否則縮圖破圖");
        assert!(!is_inline(served_mime("text/html")), "HTML 不能在這個 origin 內嵌算繪");
        assert!(!is_inline(served_mime("application/pdf")), "白名單管型別，內嵌是另一回事");
    }

    /// #465 的刪除側：附件搬進回收區之後，已刪 bot 的對話仍讀得到縮圖（API.md §10.4），
    /// 所以 `read` 原地讀不到時要去回收區找同一個檔名。
    #[tokio::test]
    async fn a_remote_attachment_is_still_readable_after_its_dir_moved_to_trash() {
        let env = tt::env().await;
        let app = &env.app;
        let bot = tt::claude_bot(app, &env.project_id, "trashy").await;
        let bot_id = bot.id.as_str();
        let dir = crate::bot_trash::attachments_dir(&app.data_dir, bot_id);
        std::fs::create_dir_all(&dir).unwrap();
        let local_path = dir.join("a.png");
        std::fs::write(&local_path, b"bytes").unwrap();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at)
             VALUES ('att1', ?, 'a.png', 'image/png', 5, ?, '/remote/a.png', 'zz', 'ready', ?)",
        )
        .bind(bot_id)
        .bind(local_path.to_string_lossy().into_owned())
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();

        assert_eq!(read(app, "att1").await.unwrap().1, b"bytes", "搬走之前照舊讀得到");
        crate::bot_trash::move_in_kind(&app.data_dir, bot_id, Some(crate::bot_trash::ATTACHMENTS), &dir).unwrap().unwrap();
        assert!(!local_path.exists(), "原地已經沒有了");
        assert_eq!(read(app, "att1").await.unwrap().1, b"bytes", "回收區裡那份仍要讀得到，不能變破圖");
    }

    /// 本機 bot 的附件就放在專案目錄裡（agent 寫得到的地方）：它把那個檔案換成符號連結、指到界線外（私鑰、別的 bot 的檔案），
    /// `GET /api/attachments/:id` 不能照單全收（outbox／local-image 的 #89 同一個形狀）。換成指到 `/dev/zero` 之類也不能把記憶體讀爆。
    #[tokio::test]
    async fn a_local_attachment_swapped_for_a_symlink_is_not_served() {
        let env = tt::env().await;
        let app = &env.app;
        let bot = tt::claude_bot(app, &env.project_id, "planter").await;
        let project = crate::testing::track(std::env::temp_dir().join(format!("am-attach-sym-{}", crate::db::ulid())));
        let dir = project.join(SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        let secret = project.join("outside-secret.txt");
        std::fs::write(&secret, b"TOP SECRET").unwrap();
        let file = dir.join("a.png");
        std::fs::write(&file, b"png-bytes").unwrap();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at)
             VALUES ('att-sym', ?, 'a.png', 'image/png', 9, ?, ?, 'local', 'ready', ?)",
        )
        .bind(&bot.id)
        .bind(file.to_string_lossy().into_owned())
        .bind(file.to_string_lossy().into_owned())
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert_eq!(read(app, "att-sym").await.unwrap().1, b"png-bytes", "正常的本機附件照舊讀得到");

        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&secret, &file).unwrap();
        assert!(read(app, "att-sym").await.is_err(), "被換成符號連結：不給");
        // 附件目錄本身被換成指到別處的連結也一樣。
        std::fs::remove_file(&file).unwrap();
        let elsewhere = project.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("a.png"), b"other").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
        assert!(read(app, "att-sym").await.is_err(), "目錄被換成符號連結：不給");
        std::fs::remove_dir_all(&project).unwrap();
    }

    /// 寫入側：專案目錄是 agent 寫得到的地方，它把 `.agents-manager`（或底下的 `attachments`）換成指到別處的符號連結，
    /// 上傳不能跟著連結把使用者的檔案寫到界線外（讀取側早就逐層 `O_NOFOLLOW`，寫入也要一致）。
    #[tokio::test]
    async fn an_upload_never_writes_through_a_symlinked_attachment_dir() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let project: String = sqlx::query_scalar("SELECT path FROM projects WHERE id = ?").bind(&env.project_id).fetch_one(&env.app.db).await.unwrap();
        let project = PathBuf::from(project);
        let elsewhere = crate::testing::track(std::env::temp_dir().join(format!("am-attach-write-{}", crate::db::ulid())));
        std::fs::create_dir_all(&elsewhere).unwrap();
        // `.agents-manager` 本身是連結。
        std::fs::create_dir_all(&project).unwrap();
        std::os::unix::fs::symlink(&elsewhere, project.join(".agents-manager")).unwrap();
        assert!(save(&env.app, &bot.id, "x.service", "text/plain", b"[Service]").await.is_err(), "連結目錄：不寫");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "界線外什麼都不能出現");
        std::fs::remove_file(project.join(".agents-manager")).unwrap();
        // 只有 `attachments` 是連結。
        std::fs::create_dir_all(project.join(".agents-manager")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, project.join(SUBDIR)).unwrap();
        assert!(save(&env.app, &bot.id, "x.service", "text/plain", b"[Service]").await.is_err(), "連結的 attachments：不寫");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        std::fs::remove_file(project.join(SUBDIR)).unwrap();
        // 正常的還是通。
        let a = save(&env.app, &bot.id, "ok.txt", "text/plain", b"fine").await.unwrap();
        assert_eq!(read(&env.app, &a.id).await.unwrap().1, b"fine");
    }

    /// 最多 50 MiB 的寫檔不能在 tokio worker 上做（會卡住同一條 worker 上的其他請求）：寫的那條執行緒不是跑這個測試的執行緒。
    #[tokio::test]
    async fn the_local_write_runs_on_the_blocking_pool_not_the_async_worker() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "big.bin", "application/octet-stream", &vec![1u8; 4 * 1024 * 1024]).await.unwrap();
        let file = a.path.rsplit('/').next().unwrap().to_string();
        let writer = WRITE_THREADS.lock().unwrap().get(&file).copied().expect("the write was recorded");
        assert_ne!(writer, std::thread::current().id(), "寫檔在跑 async 測試的這條執行緒上做了");
        assert_eq!(read(&env.app, &a.id).await.unwrap().1.len(), 4 * 1024 * 1024);
    }

    /// 存進 DB、回給 UI 的檔名：控制字元與雙向覆寫字元（`evil\u{202E}gnp.exe` 會顯示成 `evilexe.png`）拿掉，長度設上限。
    #[test]
    fn a_stored_attachment_name_has_no_control_or_bidi_characters_and_is_bounded() {
        assert_eq!(clean_name("evil\u{202E}gnp.exe"), "evilgnp.exe");
        assert_eq!(clean_name("a\0b\nc\rd\te.png"), "abcde.png");
        assert_eq!(clean_name("\u{2066}x\u{2069}\u{200E}.txt"), "x.txt");
        assert_eq!(clean_name("  \n "), "file");
        assert_eq!(clean_name(&"長".repeat(1000)).chars().count(), 255);
        assert_eq!(clean_name("報告 final.pdf"), "報告 final.pdf", "一般的 Unicode 檔名不動");
    }

    #[tokio::test]
    async fn the_name_in_the_row_is_the_cleaned_one() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "..\\..\u{202E}/ev\0il.png", "image/png", b"png").await.unwrap();
        let stored: String = sqlx::query_scalar("SELECT name FROM attachments WHERE id = ?").bind(&a.id).fetch_one(&env.app.db).await.unwrap();
        assert!(!stored.chars().any(|c| c.is_control() || ('\u{202A}'..='\u{202E}').contains(&c)), "{stored:?}");
        assert_eq!(a.name, stored);
        assert!(a.path.contains("/.agents-manager/attachments/") && !a.path.contains(".."), "{}", a.path);
    }

    #[tokio::test]
    async fn local_attachment_copy_rejects_unsafe_bot_ids() {
        let env = tt::env().await;
        let protected = env.app.data_dir.join("attachments").join("keep");
        std::fs::create_dir_all(&protected).unwrap();

        for id in ["../..", "x/y", r"..\..", ""] {
            assert!(local_copy_dir(&env.app, id).is_err(), "unsafe id was accepted: {id:?}");
            assert!(protected.exists(), "path construction touched the protected directory for {id:?}");
        }
    }

    /// 2026-09-14 使用者：暫存區要收任意檔。附件不再限圖片，所以延伸檔名與那句提示都得跟著走。
    #[test]
    fn a_non_image_keeps_its_own_name_and_extension() {
        assert_eq!(ext_for("report.PDF", "application/pdf"), "pdf");
        // 沒有副檔名時才看 mime。
        assert_eq!(ext_for("report", "application/pdf"), "pdf");
        assert_eq!(ext_for("blob", "application/octet-stream"), "bin");
        assert_eq!(safe_stem("../../etc/passwd"), "passwd");
        assert_eq!(safe_stem("???"), "file");
    }

    #[test]
    fn the_prompt_says_files_unless_everything_is_an_image() {
        let a = |mime: &str, path: &str| Attachment {
            id: "1".into(),
            name: "n".into(),
            mime: mime.into(),
            size: 1,
            path: path.into(),
        };
        let shot = a("image/png", "/p/shot.png");
        let log = a("text/plain", "/p/run.log");
        assert!(deliver_text("看這個", &[shot.clone()]).contains("附加圖片（請讀取這個檔案來查看）"));
        assert!(deliver_text("看這個", &[log.clone()]).contains("附加檔案（請讀取這個檔案來查看）"));
        let both = deliver_text("看這些", &[shot, log]);
        assert!(both.contains("附加檔案（請讀取這些檔案來查看）"), "{both}");
        assert!(both.contains("/p/shot.png") && both.contains("/p/run.log"));
    }

    #[tokio::test]
    async fn save_lands_a_ready_row_that_resolves_and_reads_back() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "shot.png", "image/png", b"pngbytes").await.unwrap();

        let state: String = sqlx::query_scalar("SELECT state FROM attachments WHERE id = ?").bind(&a.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "ready");

        let resolved = resolve(&env.app, &bot.id, &[a.id.clone()]).await.unwrap();
        assert_eq!(resolved.len(), 1);
        let (mime, data) = read(&env.app, &a.id).await.unwrap();
        assert_eq!((mime.as_str(), data.as_slice()), ("image/png", &b"pngbytes"[..]));
    }

    /// issue #88：寫檔失敗（這裡用「父目錄其實是個檔案」逼 `create_dir_all` 失敗，不靠平台權限假設）
    /// 不能讓一個沒有 DB 紀錄的孤兒檔案消失在系統裡——staging row 先落地，失敗時轉成 `failed`，
    /// `reconcile_orphans` 找得回來清掉。
    #[tokio::test]
    async fn a_write_failure_marks_the_row_failed_and_reconcile_cleans_it_up() {
        let env = tt::env().await;
        let not_a_dir = env.dir.join("not-a-directory");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let pid = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?, 'local', ?)")
            .bind(&pid)
            .bind(not_a_dir.to_string_lossy().to_string())
            .bind("bad")
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = tt::claude_bot(&env.app, &pid, "alfa").await;

        assert!(save(&env.app, &bot.id, "shot.png", "image/png", b"pngbytes").await.is_err(), "writing under a file must fail");

        let state: String =
            sqlx::query_scalar("SELECT state FROM attachments WHERE bot_id = ?").bind(&bot.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "failed", "the durable row survives the write failure instead of vanishing without a trace");

        assert_eq!(reconcile_orphans(&env.app).await, 1);
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE bot_id = ?").bind(&bot.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(reconcile_orphans(&env.app).await, 0, "already cleaned up: a second pass is a no-op");
    }

    /// 上傳了卻從沒送出的附件（`ready`、沒有任何訊息引用）以前永遠不清：檔案留在專案的 `.agents-manager/attachments/`、
    /// row 留在 DB（正式庫 50 筆、近 50 MB）。依保留期清掉；**被訊息引用的絕不刪**——`message_id` 有值的、
    /// 或訊息的 `attachments_json` 點名它的（舊版兩步綁定留下的）、還在 `staging`／`failed` 的（那是 `reconcile_orphans` 的事）都不碰。
    #[tokio::test]
    async fn unreferenced_ready_attachments_are_swept_after_the_retention_but_referenced_ones_never() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        let mut made = std::collections::HashMap::new();
        for name in ["old-unbound", "fresh-unbound", "old-bound", "old-named", "old-staging"] {
            let a = save(&env.app, &bot.id, &format!("{name}.png"), "image/png", name.as_bytes()).await.unwrap();
            made.insert(name, a);
        }
        let three_days_ago = db::iso_in(-3 * 86_400);
        for name in ["old-unbound", "old-bound", "old-named", "old-staging"] {
            sqlx::query("UPDATE attachments SET created_at = ? WHERE id = ?").bind(&three_days_ago).bind(&made[name].id).execute(&env.app.db).await.unwrap();
        }
        sqlx::query("UPDATE attachments SET state = 'staging' WHERE id = ?").bind(&made["old-staging"].id).execute(&env.app.db).await.unwrap();
        let message = |content: String| {
            let (db_, conv) = (env.app.db.clone(), conv.clone());
            async move {
                let id = db::ulid();
                sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'user',?,'web',?)")
                    .bind(&id).bind(conv).bind(content).bind(db::now()).execute(&db_).await.unwrap();
                id
            }
        };
        let bound_msg = message("with a bound attachment".into()).await;
        bind(&env.app, &bound_msg, &[made["old-bound"].clone()]).await.unwrap();
        // 舊版兩步綁定：訊息的 attachments_json 點名了它，attachments.message_id 卻沒設到。
        let named_msg = message("legacy".into()).await;
        sqlx::query("UPDATE messages SET attachments_json = ? WHERE id = ?")
            .bind(serde_json::to_string(&[made["old-named"].clone()]).unwrap())
            .bind(&named_msg)
            .execute(&env.app.db)
            .await
            .unwrap();

        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 1, "只有 old-unbound");
        let alive = |name: &str| {
            let (db_, id, path) = (env.app.db.clone(), made[name].id.clone(), made[name].path.clone());
            async move {
                let row: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id = ?").bind(id).fetch_one(&db_).await.unwrap();
                (row == 1, std::path::Path::new(&path).exists())
            }
        };
        assert_eq!(alive("old-unbound").await, (false, false), "row 與檔案都清掉");
        for kept in ["fresh-unbound", "old-bound", "old-named", "old-staging"] {
            assert_eq!(alive(kept).await, (true, true), "{kept} 不能動");
        }
        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 0, "冪等");
    }

    /// 掃的是「放了多久沒人用」，不是「上傳多久了」：一個三天前上傳的附件，使用者今天才送出——`resolve` 讀到它、`bind` 綁上它
    /// 之間若剛好輪到清理，就會被當成孤兒刪掉，這一次送出變成 `attachment is missing`（而且檔案也沒了）。
    #[tokio::test]
    async fn resolving_an_old_upload_protects_it_from_the_sweep_until_it_is_bound() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "old.png", "image/png", b"aaa").await.unwrap();
        sqlx::query("UPDATE attachments SET created_at = ? WHERE id = ?").bind(db::iso_in(-3 * 86_400)).bind(&a.id).execute(&env.app.db).await.unwrap();
        let msg_id = seed_message(&env.app, &bot.id).await;

        let files = resolve(&env.app, &bot.id, &[a.id.clone()]).await.unwrap();
        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 0, "剛被 resolve 的附件正要綁上訊息，不能當孤兒");
        bind(&env.app, &msg_id, &files).await.expect("resolve 到 bind 之間被清掉的話，這裡會失敗");
        assert!(std::path::Path::new(&a.path).exists());
    }

    /// A candidate can be resolved after the sweep's initial SELECT but before its DELETE; the
    /// refresh must fence that stale snapshot so an upload being sent is not collected.
    #[tokio::test]
    async fn resolving_after_the_sweep_snapshot_keeps_the_attachment_until_bind() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "old-after-snapshot.png", "image/png", b"aaa").await.unwrap();
        sqlx::query("UPDATE attachments SET created_at = ? WHERE id = ?")
            .bind(db::iso_in(-3 * 86_400))
            .bind(&a.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let resolving_app = env.app.clone();
        let resolving_bot = bot.id.clone();
        let resolving_id = a.id.clone();
        crate::lifecycle::race_point::arm("attachment_sweep_after_candidates", &a.id, move || async move {
            let resolved = resolve(&resolving_app, &resolving_bot, &[resolving_id]).await.unwrap();
            assert_eq!(resolved.len(), 1);
        });

        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 0, "a refreshed candidate is not expired");
        let row: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE id = ?")
            .bind(&a.id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(row, 1, "the row remains available for bind");
        assert!(std::path::Path::new(&a.path).exists(), "the bytes remain available for bind");
    }

    /// 同上，另一條路：送出後撤回（`retract_unsent_turn`）會把附件解綁，同一個 `client_request_id` 馬上原樣重送。
    /// 解綁當下要重新算「沒人用」的時間，不然舊上傳在重送之前就被清掉。
    #[tokio::test]
    async fn unbinding_an_old_attachment_restarts_its_sweep_clock() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "old.png", "image/png", b"aaa").await.unwrap();
        sqlx::query("UPDATE attachments SET created_at = ? WHERE id = ?").bind(db::iso_in(-3 * 86_400)).bind(&a.id).execute(&env.app.db).await.unwrap();
        let msg_id = seed_message(&env.app, &bot.id).await;
        bind(&env.app, &msg_id, &[a.clone()]).await.unwrap();

        unbind_message(&mut *env.app.db.acquire().await.unwrap(), &msg_id, &msg_id).await.unwrap();
        sqlx::query("DELETE FROM messages WHERE id = ?").bind(&msg_id).execute(&env.app.db).await.unwrap();
        assert_eq!(sweep_unreferenced(&env.app, 24 * 3600).await, 0, "剛解綁、等著原樣重送的附件不能被清掉");
        let again = seed_message(&env.app, &bot.id).await;
        bind(&env.app, &again, &[a.clone()]).await.unwrap();
    }

    /// issue #88：daemon 在 `INSERT ... 'staging'` 之後、`UPDATE ... 'ready'` 之前死掉（或 `save()` 自己
    /// 標了 `failed`）留下的行——重開機之後不能被 resolve／read 當成 ready，`reconcile_orphans` 要能把
    /// 檔案跟 row 都收乾淨，而且收兩次是安全的（idempotent）。
    #[tokio::test]
    async fn staging_and_failed_rows_are_invisible_until_reconciled_away() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;

        let mut paths = Vec::new();
        for state in ["staging", "failed"] {
            let id = db::ulid();
            let path = env.dir.join(format!("orphan-{state}.bin"));
            std::fs::write(&path, b"orphan bytes").unwrap();
            sqlx::query(
                "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?)",
            )
            .bind(&id)
            .bind(&bot.id)
            .bind("orphan")
            .bind("application/octet-stream")
            .bind(12i64)
            .bind(path.to_string_lossy().to_string())
            .bind(path.to_string_lossy().to_string())
            .bind("local")
            .bind(state)
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();

            assert!(resolve(&env.app, &bot.id, &[id.clone()]).await.is_err(), "{state} attachment must not resolve");
            assert!(read(&env.app, &id).await.is_err(), "{state} attachment must not be readable");
            paths.push(path);
        }

        assert_eq!(reconcile_orphans(&env.app).await, 2, "both staging and failed rows are orphans by now");
        for path in &paths {
            assert!(!path.exists(), "orphan file should be cleaned up: {}", path.display());
        }
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE bot_id = ?").bind(&bot.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(reconcile_orphans(&env.app).await, 0, "nothing left to clean the second time");
    }

    async fn seed_message(app: &Arc<App>, bot_id: &str) -> String {
        let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
        let id = db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?, 'user', 'hi', 'web', ?)")
            .bind(&id)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn bind_sets_the_message_projection_and_every_attachment_atomically() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "a.png", "image/png", b"aaa").await.unwrap();
        let b = save(&env.app, &bot.id, "b.png", "image/png", b"bbb").await.unwrap();
        let msg_id = seed_message(&env.app, &bot.id).await;

        bind(&env.app, &msg_id, &[a.clone(), b.clone()]).await.unwrap();

        let json: Option<String> =
            sqlx::query_scalar("SELECT attachments_json FROM messages WHERE id = ?").bind(&msg_id).fetch_one(&env.app.db).await.unwrap();
        assert!(json.is_some());
        for id in [&a.id, &b.id] {
            let mid: Option<String> =
                sqlx::query_scalar("SELECT message_id FROM attachments WHERE id = ?").bind(id).fetch_one(&env.app.db).await.unwrap();
            assert_eq!(mid.as_deref(), Some(msg_id.as_str()));
        }
    }

    /// issue #88：`bind()` 中任一 UPDATE 失敗（這裡是綁一個不存在的 attachment id），message 的
    /// projection 跟**已經**成功 UPDATE 過的那些 attachment rows 都要一起回滾，不留下半套 binding。
    #[tokio::test]
    async fn bind_rolls_back_everything_when_one_attachment_cannot_be_bound() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let a = save(&env.app, &bot.id, "a.png", "image/png", b"aaa").await.unwrap();
        let missing = Attachment { id: "does-not-exist".into(), name: "x".into(), mime: "image/png".into(), size: 1, path: "/x".into() };
        let msg_id = seed_message(&env.app, &bot.id).await;

        assert!(bind(&env.app, &msg_id, &[a.clone(), missing]).await.is_err());

        // a 排在前面，它的 UPDATE 先成功、遇到第二筆才失敗——先成功的那筆也要被撤銷。
        let mid: Option<String> =
            sqlx::query_scalar("SELECT message_id FROM attachments WHERE id = ?").bind(&a.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(mid, None, "已經成功的那筆 UPDATE 也要跟著整批回滾");
        let json: Option<String> =
            sqlx::query_scalar("SELECT attachments_json FROM messages WHERE id = ?").bind(&msg_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(json, None, "message 的 projection 也要回滾");
    }

    /// 只有 `ready` 能被 bind：一個還在 `staging` 的 row 不該被綁上訊息。
    #[tokio::test]
    async fn bind_refuses_a_staging_attachment() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let id = db::ulid();
        let path = env.dir.join("still-staging.bin");
        std::fs::write(&path, b"x").unwrap();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at)
             VALUES (?,?,?,?,?,?,?,?,'staging',?)",
        )
        .bind(&id)
        .bind(&bot.id)
        .bind("n")
        .bind("application/octet-stream")
        .bind(1i64)
        .bind(path.to_string_lossy().to_string())
        .bind(path.to_string_lossy().to_string())
        .bind("local")
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        let msg_id = seed_message(&env.app, &bot.id).await;

        let item = Attachment { id: id.clone(), name: "n".into(), mime: "application/octet-stream".into(), size: 1, path: path.to_string_lossy().into_owned() };
        assert!(bind(&env.app, &msg_id, &[item]).await.is_err());

        let state: String = sqlx::query_scalar("SELECT state FROM attachments WHERE id = ?").bind(&id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "staging", "還沒 ready 不該被誤綁");
    }
}
