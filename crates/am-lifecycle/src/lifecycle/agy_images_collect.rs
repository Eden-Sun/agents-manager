//! agy 產圖收集（#2215）：回合送出時拍對話 brain 目錄的快照；回合終局後收這回合新產的圖，綁成一則系統訊息的附件。

use super::agy_session::{self, Site};
use super::messages::{emit_message_added, insert_message_tx};
use crate::agy_images::{self, Collected, LocalSource, Snapshot, REMOTE_FETCH_CAP};
use crate::attach;
use crate::capabilities::{Db, DataDir, Emit};
use crate::db;
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub trait Host: Db + Emit + DataDir + crate::hosts::HostsAccess + crate::lifecycle::s6_ports::IdentityAccess {}

impl<T> Host for T where T: Db + Emit + DataDir + crate::hosts::HostsAccess + crate::lifecycle::s6_ports::IdentityAccess {}

const COLLECT_EVERY: Duration = Duration::from_secs(5);
const SWEEP_EVERY: Duration = Duration::from_secs(300);
const DUE_BATCH: i64 = 20;

pub async fn begin(app: &(impl Db + crate::hosts::HostsAccess + crate::lifecycle::s6_ports::IdentityAccess), bot_id: &str, turn_id: &str, conversation_id: &str) -> Result<()> {
    let Some(bot) = db::bot(app.db(), bot_id).await? else { return Ok(()) };
    if bot.kind != "agy" {
        return Ok(());
    }
    let host = db::bot_host(app.db(), bot_id).await?;
    let run = db::active_run(app.db(), bot_id).await?;
    let (session, recorded, identity) = match run.as_ref().and_then(|r| r.native_session_id.clone()).filter(|s| agy_session::valid_session_id(s)) {
        Some(sid) => (Some(sid), run.as_ref().and_then(|r| r.transcript_path.clone()), run.as_ref().and_then(|r| r.runtime_identity.clone()).or(bot.identity.clone())),
        None => {
            let last: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT native_session_id, transcript_path, runtime_identity FROM runs WHERE bot_id = ? AND native_session_id IS NOT NULL ORDER BY started_at DESC LIMIT 1",
            )
            .bind(bot_id)
            .fetch_optional(app.db())
            .await?;
            match last.filter(|(sid, _, _)| agy_session::valid_session_id(sid)) {
                Some((sid, path, ident)) => (Some(sid), path, ident.or(bot.identity.clone())),
                None => (None, None, bot.identity.clone()),
            }
        }
    };
    let before = match &session {
        None => Some(Snapshot::new()),
        Some(sid) => match Site::of(app, &host, identity.as_deref(), recorded.as_deref()).await {
            Some(site) => site.brain_list(sid).await,
            None => None,
        },
    };
    agy_images::mark_open(app.db(), turn_id, bot_id, &host, conversation_id, session.as_deref(), before.as_ref(), &db::now()).await?;
    Ok(())
}

pub async fn collect_due(app: &impl Host) {
    let now = db::now();
    let marks = match agy_images::due_marks(app.db(), &now, DUE_BATCH).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = ?e, "agy images: due marks unreadable");
            return;
        }
    };
    for mark in marks {
        match collect(app, &mark).await {
            Ok(state) => {
                if let Err(e) = agy_images::finish_mark(app.db(), &mark.turn_id, state).await {
                    tracing::warn!(turn = %mark.turn_id, error = ?e, "agy images: mark not closed");
                }
            }
            Err(e) => {
                tracing::debug!(turn = %mark.turn_id, error = ?e, "agy images: collection deferred");
                let next = chrono::Utc::now() + chrono::Duration::seconds(30 << mark.attempts.min(6));
                let res = if chrono::DateTime::parse_from_rfc3339(&mark.created_at).is_ok_and(|t| chrono::Utc::now().signed_duration_since(t).num_seconds() > agy_images::GIVE_UP_SECS) {
                    agy_images::finish_mark(app.db(), &mark.turn_id, "skipped").await
                } else {
                    agy_images::defer_mark(app.db(), &mark.turn_id, &next.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).await
                };
                if let Err(e) = res {
                    tracing::warn!(turn = %mark.turn_id, error = ?e, "agy images: mark not updated");
                }
            }
        }
    }
}

async fn collect(app: &impl Host, mark: &agy_images::Mark) -> Result<&'static str> {
    let Some(before) = mark.before.as_ref() else { return Ok("skipped") };
    let (conversation_id, run_id, turn_created): (String, Option<String>, String) =
        sqlx::query_as("SELECT conversation_id, run_id, created_at FROM turns WHERE id = ?").bind(&mark.turn_id).fetch_one(app.db()).await?;
    let run = match run_id.as_deref() {
        Some(r) => db::run(app.db(), r).await?,
        None => None,
    };
    let bot = db::bot(app.db(), &mark.bot_id).await?.ok_or_else(|| anyhow!("bot is gone"))?;
    let Some(sid) = mark.session_id.clone().or_else(|| run.as_ref().and_then(|r| r.native_session_id.clone())).filter(|s| agy_session::valid_session_id(s)) else {
        return Ok("done");
    };
    let identity = run.as_ref().and_then(|r| r.runtime_identity.clone()).or(bot.identity.clone());
    let recorded = run.as_ref().and_then(|r| r.transcript_path.as_deref());
    let site = Site::of(app, &mark.host, identity.as_deref(), recorded).await.ok_or_else(|| anyhow!("host is not reachable"))?;
    let base = if mark.session_id.as_deref() == Some(sid.as_str()) { before.clone() } else { Snapshot::new() };
    let after = site.brain_list(&sid).await.ok_or_else(|| anyhow!("brain listing failed"))?;
    let new: Snapshot = after.difference(&base).cloned().collect();
    let known = agy_images::known_hashes(app.db(), &mark.bot_id).await?;
    let root = site.brain_root(&sid);
    let collected = if new.is_empty() {
        Collected::default()
    } else {
        let head: Vec<PathBuf> = new.iter().take(REMOTE_FETCH_CAP).cloned().collect();
        match site.prefetch(&sid, &head).await? {
            Some(pre) => {
                let mut c = agy_images::select(&head.iter().cloned().collect(), &pre, &known);
                c.over_limit += new.iter().skip(REMOTE_FETCH_CAP).filter(|p| agy_images::looks_like_image(p)).count();
                c
            }
            None => agy_images::select(&new, &LocalSource { root: &root }, &known),
        }
    };
    let quota = match site.transcript(&sid).await {
        Some(text) => agy_images::image_quota_since(&text, &turn_created),
        None => None,
    };
    let notes = notes_for(&collected, quota.as_deref());
    if collected.images.is_empty() && notes.is_empty() {
        return Ok("done");
    }
    let dir = attach::local_copy_dir(app, &mark.bot_id)?;
    tokio::fs::create_dir_all(&dir).await?;
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(collected.images.len());
    let written = store(app, mark, &conversation_id, &root, &dir, &collected, &notes, &mut staged).await;
    let msg = match written {
        Ok(msg) => msg,
        Err(e) => {
            for (tmp, _) in &staged {
                let _ = tokio::fs::remove_file(tmp).await;
            }
            return Err(e);
        }
    };
    for (tmp, local) in &staged {
        if let Err(e) = tokio::fs::rename(tmp, local).await {
            tracing::warn!(path = %local.display(), error = ?e, "agy images: collected image could not be moved into place");
            let _ = tokio::fs::remove_file(tmp).await;
        }
    }
    emit_message_added(app, &mark.bot_id, msg).await;
    Ok("done")
}

/// 圖檔先寫成同目錄的暫存名（`.agy-<sha>.<ext>.stage-<ulid>`），收進 `staged`；交易提交後呼叫端才改名到正式位置，
/// 交易失敗時呼叫端刪掉暫存檔，不留沒有列指著、sweep 永遠清不到的孤兒檔。
#[allow(clippy::too_many_arguments)]
async fn store(
    app: &impl Host,
    mark: &agy_images::Mark,
    conversation_id: &str,
    root: &std::path::Path,
    dir: &std::path::Path,
    collected: &Collected,
    notes: &[String],
    staged: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<db::Message> {
    let mut tx = app.db().begin().await?;
    let mut msg = insert_message_tx(&mut tx, conversation_id, Some(&mark.turn_id), "assistant", &notes.join("\n"), "system", false, None).await?;
    let mut items = Vec::with_capacity(collected.images.len());
    for img in &collected.images {
        let local = dir.join(format!("agy-{}.{}", img.sha256, img.kind));
        let tmp = dir.join(format!(".agy-{}.{}.stage-{}", img.sha256, img.kind, db::ulid()));
        staged.push((tmp.clone(), local.clone()));
        tokio::fs::write(&tmp, &img.bytes).await?;
        let id = db::ulid();
        let name = img.rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| format!("agy-{}.{}", img.sha256, img.kind));
        let mime = agy_images::mime_of(img.kind);
        let size = img.bytes.len() as i64;
        let agent_path = root.join(&img.rel).to_string_lossy().into_owned();
        sqlx::query("INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, message_id, state, created_at) VALUES (?,?,?,?,?,?,?,?,?,'ready',?)")
            .bind(&id)
            .bind(&mark.bot_id)
            .bind(&name)
            .bind(mime)
            .bind(size)
            .bind(local.to_string_lossy().as_ref())
            .bind(&agent_path)
            .bind(&mark.host)
            .bind(&msg.id)
            .bind(db::now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT OR IGNORE INTO agy_images (bot_id, sha256, attachment_id, turn_id, size, created_at) VALUES (?,?,?,?,?,?)")
            .bind(&mark.bot_id)
            .bind(&img.sha256)
            .bind(&id)
            .bind(&mark.turn_id)
            .bind(size)
            .bind(db::now())
            .execute(&mut *tx)
            .await?;
        items.push(attach::Attachment { id, name, mime: mime.to_string(), size, path: agent_path });
    }
    attach::bind_tx(&mut tx, &msg.id, &items).await?;
    msg.attachments_json = message_attachments_json(&items)?;
    sqlx::query("UPDATE agy_image_marks SET state = 'done' WHERE turn_id = ?").bind(&mark.turn_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(msg)
}

/// 推給畫面的 `message_added` 要帶跟 DB 同一份附件清單：訊息是附件綁上去之前插入的，不補的話畫面要重新整理才看得到圖。
fn message_attachments_json(items: &[attach::Attachment]) -> anyhow::Result<Option<String>> {
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(items)?))
}

fn notes_for(c: &Collected, quota: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if !c.images.is_empty() {
        out.push(format!("這回合產出 {} 張圖。", c.images.len()));
    }
    if c.too_large > 0 {
        out.push(format!("{} 張圖超過 10 MB，沒有收進來。", c.too_large));
    }
    if c.over_limit > 0 {
        out.push(format!("每回合最多收 {} 張，另外 {} 張沒有收進來。", agy_images::MAX_PER_TURN, c.over_limit));
    }
    if let Some(hint) = quota {
        let reset = if hint.is_empty() { String::new() } else { format!("（{hint}）") };
        out.push(format!("產圖額度已用盡{reset}，這回合沒有產出圖片。產圖額度與文字額度分開計算，不影響這顆 bot 的文字回合。"));
    }
    out
}

pub async fn sweep(app: &impl Host) {
    let pool = app.db();
    let cutoff = |secs: i64| (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut gone = match agy_images::expired(pool, &cutoff(agy_images::RETENTION_SECS)).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, "agy images: expiry check failed");
            return;
        }
    };
    if let Ok(bots) = agy_images::bots_with_images(pool).await {
        for bot in bots {
            match agy_images::over_cap(pool, &bot, agy_images::BOT_CAP_BYTES).await {
                Ok(v) => gone.extend(v),
                Err(e) => tracing::warn!(bot = %bot, error = ?e, "agy images: cap check failed"),
            }
        }
    }
    gone.sort_by(|a, b| (&a.bot_id, &a.sha256).cmp(&(&b.bot_id, &b.sha256)));
    gone.dedup_by(|a, b| a.bot_id == b.bot_id && a.sha256 == b.sha256);
    for s in &gone {
        if let Err(e) = tokio::fs::remove_file(&s.local_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %s.local_path, error = ?e, "agy images: could not remove an expired image");
            }
        }
    }
    if let Err(e) = agy_images::drop_rows(pool, &gone).await {
        tracing::warn!(error = ?e, "agy images: expired rows not dropped");
    }
    if let Err(e) = agy_images::give_up_stale_marks(pool, &cutoff(agy_images::GIVE_UP_SECS)).await {
        tracing::warn!(error = ?e, "agy images: stale marks not closed");
    }
}

pub fn spawn<H>(app: Arc<H>)
where
    H: Host + crate::capabilities::BgTasks + crate::capabilities::Shutdown + Send + Sync + 'static,
{
    crate::background_loop::spawn_periodic(&app, "agy image collect", COLLECT_EVERY, Duration::ZERO, |app| async move {
        collect_due(app.as_ref()).await;
    });
    crate::background_loop::spawn_periodic(&app, "agy image sweep", SWEEP_EVERY, Duration::from_secs(60), |app| async move {
        sweep(app.as_ref()).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const BANNER: &str = "RESOURCE_EXHAUSTED (code 8): image generation quota exceeded, reset after 4h44m";

    #[test]
    fn the_image_banner_is_never_a_text_quota_so_it_cannot_pause_the_bot() {
        assert!(!crate::turn_error::is_quota_exhaustion(BANNER));
        assert!(!crate::turn_error::is_quota_limit(BANNER));
        assert!(crate::turn_error::api_error_line(&format!("⏺ {BANNER}\n")).is_none());
    }

    #[test]
    fn the_pushed_message_carries_the_same_attachment_list_as_the_row() {
        assert_eq!(message_attachments_json(&[]).unwrap(), None);
        let items = vec![attach::Attachment { id: "a1".into(), name: "x.png".into(), mime: "image/png".into(), size: 3, path: "/brain/x.png".into() }];
        let json = message_attachments_json(&items).unwrap().unwrap();
        assert_eq!(json, serde_json::to_string(&items).unwrap());
        assert!(json.contains("\"id\":\"a1\""));
    }

    #[test]
    fn the_quota_note_names_images_and_says_it_does_not_touch_text() {
        let notes = notes_for(&Collected::default(), Some("reset after 4h44m"));
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("產圖額度已用盡") && notes[0].contains("文字額度分開計算"));
        assert!(notes_for(&Collected::default(), None).is_empty());
    }
}
