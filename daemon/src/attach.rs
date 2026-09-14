//! Image attachments. `agent.prompt` carries **text only**, so an image is written into the
//! agent's cwd on the bot's host (a sandboxed CLI reads it without a path-approval prompt,
//! `.gitignore`d) and its path is named in the prompt. Remote hosts also keep a daemon-side copy
//! so thumbnails need no ssh.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db;
use crate::config::{valid_id, ID_RE};
use crate::hosts::sh_quote;
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

    // The daemon can always read `local_path`; on a remote host that is its own copy.
    let local_path = if host.is_local() {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {dir}"))?;
        write_gitignore_local(&dir);
        std::fs::write(&agent_path, data).with_context(|| format!("write {agent_path}"))?;
        agent_path.clone()
    } else {
        let copy_dir = local_copy_dir(app, bot_id)?;
        std::fs::create_dir_all(&copy_dir).with_context(|| format!("create {}", copy_dir.display()))?;
        let copy = copy_dir.join(&file);
        std::fs::write(&copy, data).with_context(|| format!("write {}", copy.display()))?;
        host.ssh_put(&agent_path, data)
            .await
            .with_context(|| format!("copy attachment to {}:{}", project.host, agent_path))?;
        // Best effort — a repo that never sees the directory is nicer, but not worth failing over.
        let _ = host
            .ssh_exec(&format!("printf '*\\n' > {}", sh_quote(&format!("{dir}/.gitignore"))))
            .await;
        copy.to_string_lossy().to_string()
    };

    sqlx::query(
        "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at)
         VALUES (?,?,?,?,?,?,?,?,?)",
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

    Ok(Attachment { id, name: name.to_string(), mime: mime.to_string(), size: data.len() as i64, path: agent_path })
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
             WHERE a.id = ? AND b.project_id = ?",
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

pub async fn bind(app: &Arc<App>, message_id: &str, items: &[Attachment]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let payload = serde_json::to_string(items)?;
    sqlx::query("UPDATE messages SET attachments_json = ? WHERE id = ?")
        .bind(&payload)
        .bind(message_id)
        .execute(&app.db)
        .await?;
    for a in items {
        sqlx::query("UPDATE attachments SET message_id = ? WHERE id = ?")
            .bind(message_id)
            .bind(&a.id)
            .execute(&app.db)
            .await?;
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
    let row = sqlx::query_as::<_, (String, String)>("SELECT mime, local_path FROM attachments WHERE id = ?")
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
}
