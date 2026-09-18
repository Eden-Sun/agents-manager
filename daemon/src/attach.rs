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
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db;
use crate::config::{valid_id, ID_RE};
use crate::hosts::{sh_quote, HostConn};
use crate::state::App;

/// Retina screenshot fits; an ssh push stays sub-second.
pub const MAX_BYTES: usize = 12 * 1024 * 1024;

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

fn local_copy_dir(app: &Arc<App>, bot_id: &str) -> Result<PathBuf> {
    if !valid_id(bot_id) {
        bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    Ok(app.data_dir.join("attachments").join(bot_id))
}

pub async fn save(app: &Arc<App>, bot_id: &str, name: &str, mime: &str, data: &[u8]) -> Result<Attachment> {
    if data.is_empty() {
        bail!("attachment is empty");
    }
    if data.len() > MAX_BYTES {
        bail!("attachment is {} bytes; the limit is {}", data.len(), MAX_BYTES);
    }
    let bot = db::bot(&app.db, bot_id)
        .await?
        .filter(|bot| bot.deleted_at.is_none())
        .ok_or_else(|| anyhow::anyhow!("no such bot"))?;
    let project = db::project(&app.db, &bot.project_id)
        .await?
        .filter(|project| project.deleted_at.is_none())
        .ok_or_else(|| anyhow::anyhow!("no such project"))?;

    let id = db::ulid();
    let file = format!("{}-{}.{}", id, safe_stem(name), ext_for(name, mime));
    let dir = format!("{}/{}", project.path.trim_end_matches('/'), SUBDIR);
    let agent_path = format!("{dir}/{file}");

    let host = app
        .hosts
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
    .execute(&app.db)
    .await?;

    if let Err(e) = write_bytes(&host, &project.host, &dir, &agent_path, &local_path, data).await {
        best_effort_cleanup(&host, &local_path, &agent_path).await;
        let _ = sqlx::query("UPDATE attachments SET state = 'failed' WHERE id = ?").bind(&id).execute(&app.db).await;
        return Err(e);
    }
    // Bytes are down; the row still says 'staging' until this commits. A crash or DB error right
    // here leaves exactly that — `reconcile_orphans` picks it up next startup and cleans the files
    // it just wrote, since nothing else can ever know they belong to a message.
    sqlx::query("UPDATE attachments SET state = 'ready' WHERE id = ?").bind(&id).execute(&app.db).await?;

    Ok(Attachment { id, name: name.to_string(), mime: mime.to_string(), size: data.len() as i64, path: agent_path })
}

/// The actual byte transfer — local write, or daemon-side copy + `ssh_put`. Split out of [`save`] so
/// its failure path (leave `state = 'staging'`/`'failed'` for [`reconcile_orphans`], don't touch the
/// DB row here) stays separate from the row bookkeeping.
async fn write_bytes(host: &Arc<HostConn>, host_name: &str, dir: &str, agent_path: &str, local_path: &str, data: &[u8]) -> Result<()> {
    if host.is_local() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {dir}"))?;
        write_gitignore_local(dir);
        std::fs::write(agent_path, data).with_context(|| format!("write {agent_path}"))?;
    } else {
        let copy_dir = Path::new(local_path).parent().ok_or_else(|| anyhow::anyhow!("`{local_path}` has no parent directory"))?;
        std::fs::create_dir_all(copy_dir).with_context(|| format!("create {}", copy_dir.display()))?;
        std::fs::write(local_path, data).with_context(|| format!("write {local_path}"))?;
        host.ssh_put(agent_path, data).await.with_context(|| format!("copy attachment to {host_name}:{agent_path}"))?;
        // Best effort — a repo that never sees the directory is nicer, but not worth failing over.
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
pub async fn reconcile_orphans(app: &Arc<App>) -> usize {
    let rows: Vec<(String, String, String, String)> = match sqlx::query_as(
        "SELECT id, local_path, agent_path, host FROM attachments WHERE state IN ('staging','failed')",
    )
    .fetch_all(&app.db)
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
        let _ = std::fs::remove_file(&local_path);
        if let Some(host) = app.hosts.get(&host_name).await {
            if !host.is_local() {
                let _ = host.ssh_exec(&format!("rm -f {}", sh_quote(&agent_path))).await;
            }
        }
        match sqlx::query("DELETE FROM attachments WHERE id = ?").bind(&id).execute(&app.db).await {
            Ok(_) => cleaned += 1,
            Err(e) => tracing::warn!(id = %id, error = %e, "could not remove an orphaned attachment row"),
        }
    }
    if cleaned > 0 {
        tracing::info!(cleaned, "cleaned up orphaned (staging/failed) attachment rows left by a previous run");
    }
    cleaned
}

fn write_gitignore_local(dir: &str) {
    let p = PathBuf::from(dir).join(".gitignore");
    if !p.exists() {
        let _ = std::fs::write(&p, "*\n");
    }
}

/// Restricted to the recipient's **project**: an unrelated chat's id must not become a path here.
/// Project (not bot) scope lets one group send share an upload with every recipient.
pub async fn resolve(app: &Arc<App>, bot_id: &str, ids: &[String]) -> Result<Vec<Attachment>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let bot = db::bot(&app.db, bot_id).await?.ok_or_else(|| anyhow::anyhow!("no such bot"))?;
    let mut out = Vec::new();
    for id in ids {
        let row = sqlx::query_as::<_, (String, String, String, i64, String)>(
            "SELECT a.id, a.name, a.mime, a.size, a.agent_path FROM attachments a
             JOIN bots b ON b.id = a.bot_id
             WHERE a.id = ? AND b.project_id = ? AND a.state = 'ready'",
        )
        .bind(id)
        .bind(&bot.project_id)
        .fetch_optional(&app.db)
        .await?;
        match row {
            Some((id, name, mime, size, path)) => out.push(Attachment { id, name, mime, size, path }),
            None => bail!("unknown attachment `{id}`"),
        }
    }
    Ok(out)
}

/// Single transaction (issue #88): the old two-step (update `messages.attachments_json`, then one
/// `UPDATE` per attachment) could die partway, leaving the message's projection naming attachments
/// whose own `message_id` was never set. Any failure — including an attachment `UPDATE` matching
/// zero rows, checked via `rows_affected` — rolls back everything; nothing here is visible until
/// `commit()` succeeds.
pub async fn bind(app: &Arc<App>, message_id: &str, items: &[Attachment]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut tx = app.db.begin().await?;
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
pub async fn read(app: &Arc<App>, id: &str) -> Result<(String, Vec<u8>)> {
    let row = sqlx::query_as::<_, (String, String)>("SELECT mime, local_path FROM attachments WHERE id = ? AND state = 'ready'")
        .bind(id)
        .fetch_optional(&app.db)
        .await?;
    let Some((mime, path)) = row else { bail!("unknown attachment") };
    let data = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    Ok((mime, data))
}

pub fn to_json(a: &Attachment) -> serde_json::Value {
    json!({"id": a.id, "name": a.name, "mime": a.mime, "size": a.size, "path": a.path})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

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
