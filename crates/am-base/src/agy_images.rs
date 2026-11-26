//! agy 產圖的收集（#2215）：回合開始時拍快照，回合結束時快照裡沒有的一般圖片才算這回合新產的。
//! 界線是快照不是 mtime：`cp -R` 複製的舊圖 mtime 會變新。

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::hosts::sh_quote;

pub const MAX_PER_TURN: usize = 8;
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const REMOTE_FETCH_CAP: usize = 16;
pub const RETENTION_SECS: i64 = 7 * 24 * 3600;
pub const BOT_CAP_BYTES: i64 = 200 * 1024 * 1024;
pub const GIVE_UP_SECS: i64 = 2 * 24 * 3600;
pub const QUOTA_BANNER: &str = "image generation quota exceeded";

pub type Snapshot = BTreeSet<PathBuf>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub rel: PathBuf,
    pub kind: &'static str,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub images: Vec<Found>,
    pub too_large: usize,
    pub over_limit: usize,
    pub unreadable: usize,
}

pub trait Source {
    fn size(&self, rel: &Path) -> Option<u64>;
    fn read(&self, rel: &Path) -> Option<Vec<u8>>;
}

#[derive(Debug, Default)]
pub struct Prefetched {
    pub sizes: HashMap<PathBuf, u64>,
    pub bytes: HashMap<PathBuf, Vec<u8>>,
}

impl Source for Prefetched {
    fn size(&self, rel: &Path) -> Option<u64> {
        self.sizes.get(rel).copied()
    }
    fn read(&self, rel: &Path) -> Option<Vec<u8>> {
        self.bytes.get(rel).cloned()
    }
}

pub struct LocalSource<'a> {
    pub root: &'a Path,
}

impl Source for LocalSource<'_> {
    fn size(&self, rel: &Path) -> Option<u64> {
        std::fs::symlink_metadata(self.root.join(rel)).ok().filter(|m| m.is_file()).map(|m| m.len())
    }
    fn read(&self, rel: &Path) -> Option<Vec<u8>> {
        std::fs::read(self.root.join(rel)).ok()
    }
}

pub fn sniff(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("gif")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

pub fn mime_of(kind: &str) -> &'static str {
    match kind {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        _ => "image/webp",
    }
}

pub fn looks_like_image(rel: &Path) -> bool {
    let ext = rel.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
}

pub fn excluded(rel: &Path) -> bool {
    rel.components().any(|c| {
        let s = c.as_os_str().to_str().unwrap_or("");
        s.starts_with('.') || s == "scratch"
    })
}

pub fn snapshot(conv_dir: &Path) -> io::Result<Snapshot> {
    let mut out = BTreeSet::new();
    if !conv_dir.is_dir() {
        return Ok(out);
    }
    walk(conv_dir, Path::new(""), &mut out)?;
    Ok(out)
}

fn walk(root: &Path, rel: &Path, out: &mut Snapshot) -> io::Result<()> {
    for entry in std::fs::read_dir(root.join(rel))? {
        let entry = entry?;
        let child = rel.join(entry.file_name());
        if excluded(&child) {
            continue;
        }
        let kind = std::fs::symlink_metadata(entry.path())?.file_type();
        if kind.is_dir() {
            walk(root, &child, out)?;
        } else if kind.is_file() {
            out.insert(child);
        }
    }
    Ok(())
}

pub fn parse_listing(out: &str) -> Snapshot {
    out.lines()
        .map(|l| l.strip_prefix("./").unwrap_or(l))
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| !excluded(p))
        .collect()
}

pub fn remote_list_script(brain: &str) -> String {
    format!("cd {} 2>/dev/null && find . \\( -name '.?*' -o -name scratch \\) -prune -o -type f -print; :", sh_quote(brain))
}

pub fn remote_fetch_script(brain: &str, rels: &[PathBuf]) -> String {
    let mut s = format!("cd {} 2>/dev/null || exit 0; ", sh_quote(brain));
    for (i, rel) in rels.iter().enumerate() {
        let f = sh_quote(&format!("./{}", rel.to_string_lossy()));
        s.push_str(&format!(
            "if [ -f {f} ] && [ ! -L {f} ]; then n=$(wc -c < {f} | tr -d ' '); echo \"@@{i} $n\"; if [ \"$n\" -le {MAX_BYTES} ]; then base64 < {f} | tr -d '\\n'; echo; fi; fi; "
        ));
    }
    s.push(':');
    s
}

pub fn parse_fetch(out: &str, rels: &[PathBuf]) -> Prefetched {
    let mut p = Prefetched::default();
    let mut cur: Option<usize> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            cur = None;
            let mut it = rest.split(' ');
            let idx = it.next().and_then(|s| s.parse::<usize>().ok());
            let size = it.next().and_then(|s| s.trim().parse::<u64>().ok());
            if let (Some(i), Some(sz)) = (idx, size) {
                if let Some(rel) = rels.get(i) {
                    p.sizes.insert(rel.clone(), sz);
                    cur = Some(i);
                }
            }
        } else if let Some(i) = cur.take() {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(line.trim()) {
                p.bytes.insert(rels[i].clone(), bytes);
            }
        }
    }
    p
}

pub fn select(new: &Snapshot, src: &dyn Source, known: &BTreeSet<String>) -> Collected {
    let mut out = Collected::default();
    let mut seen = known.clone();
    for rel in new {
        let Some(size) = src.size(rel) else {
            out.unreadable += 1;
            continue;
        };
        if size > MAX_BYTES {
            if looks_like_image(rel) {
                out.too_large += 1;
            }
            continue;
        }
        let Some(bytes) = src.read(rel) else {
            out.unreadable += 1;
            continue;
        };
        let Some(kind) = sniff(&bytes) else { continue };
        let sha256 = hex(&Sha256::digest(&bytes));
        if !seen.insert(sha256.clone()) {
            continue;
        }
        if out.images.len() >= MAX_PER_TURN {
            out.over_limit += 1;
            continue;
        }
        out.images.push(Found { rel: rel.clone(), kind, sha256, bytes });
    }
    out
}

/// 回合開始之後（`since`，RFC 3339）紀錄檔裡出現的產圖額度橫幅；回傳重置時間的描述（沒有就是空字串）。
pub fn image_quota_since(jsonl: &str, since: &str) -> Option<String> {
    let since = chrono::DateTime::parse_from_rfc3339(since).ok()?;
    jsonl.lines().filter(|l| l.to_ascii_lowercase().contains(QUOTA_BANNER)).find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        let at = chrono::DateTime::parse_from_rfc3339(v.get("created_at")?.as_str()?).ok()?;
        if at < since {
            return None;
        }
        let hint = l.find("reset after ").map(|i| {
            let tail = &l[i..];
            let end = tail.find(|c: char| !(c.is_ascii_alphanumeric() || c == ' ')).unwrap_or(tail.len());
            tail[..end].trim().to_string()
        });
        Some(hint.unwrap_or_default())
    })
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 回合的 agy 輸出裡有沒有產圖額度用盡的橫幅（與文字額度的橫幅不同，不走撞限）。
pub fn image_quota_banner(text: &str) -> Option<String> {
    text.lines().map(str::trim).find(|l| l.to_ascii_lowercase().contains(QUOTA_BANNER)).map(|l| l.chars().take(300).collect())
}

#[derive(Debug, Clone)]
pub struct Mark {
    pub turn_id: String,
    pub bot_id: String,
    pub host: String,
    pub session_id: Option<String>,
    pub before: Option<Snapshot>,
    pub attempts: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stored {
    pub bot_id: String,
    pub sha256: String,
    pub attachment_id: String,
    pub local_path: String,
    pub size: i64,
}

pub async fn mark_open(
    pool: &SqlitePool,
    turn_id: &str,
    bot_id: &str,
    host: &str,
    conversation_id: &str,
    session_id: Option<&str>,
    before: Option<&Snapshot>,
    now: &str,
) -> sqlx::Result<()> {
    let before_json = before.map(|s| serde_json::to_string(&s.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>()).unwrap_or_default());
    sqlx::query(
        "INSERT OR IGNORE INTO agy_image_marks (turn_id, bot_id, host, conversation_id, session_id, before_json, state, attempts, created_at)
         VALUES (?,?,?,?,?,?,'open',0,?)",
    )
    .bind(turn_id)
    .bind(bot_id)
    .bind(host)
    .bind(conversation_id)
    .bind(session_id)
    .bind(before_json)
    .bind(now)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn due_marks(pool: &SqlitePool, now: &str, limit: i64) -> sqlx::Result<Vec<Mark>> {
    let rows: Vec<(String, String, String, Option<String>, Option<String>, i64, String)> = sqlx::query_as(
        "SELECT m.turn_id, m.bot_id, m.host, m.session_id, m.before_json, m.attempts, m.created_at
         FROM agy_image_marks m JOIN turns t ON t.id = m.turn_id
         WHERE m.state = 'open' AND (m.next_at IS NULL OR m.next_at <= ?) AND t.status NOT IN ('queued','in_flight')
         ORDER BY m.created_at LIMIT ?",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(turn_id, bot_id, host, session_id, before_json, attempts, created_at)| Mark {
            turn_id,
            bot_id,
            host,
            session_id,
            before: before_json.and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok()).map(|v| v.into_iter().map(PathBuf::from).collect()),
            attempts,
            created_at,
        })
        .collect())
}

pub async fn finish_mark(pool: &SqlitePool, turn_id: &str, state: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE agy_image_marks SET state = ? WHERE turn_id = ?").bind(state).bind(turn_id).execute(pool).await.map(|_| ())
}

pub async fn defer_mark(pool: &SqlitePool, turn_id: &str, next_at: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE agy_image_marks SET attempts = attempts + 1, next_at = ? WHERE turn_id = ?").bind(next_at).bind(turn_id).execute(pool).await.map(|_| ())
}

pub async fn give_up_stale_marks(pool: &SqlitePool, cutoff: &str) -> sqlx::Result<u64> {
    sqlx::query("UPDATE agy_image_marks SET state = 'skipped' WHERE state = 'open' AND created_at < ?").bind(cutoff).execute(pool).await.map(|r| r.rows_affected())
}

pub async fn known_hashes(pool: &SqlitePool, bot_id: &str) -> sqlx::Result<BTreeSet<String>> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT sha256 FROM agy_images WHERE bot_id = ?").bind(bot_id).fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

pub async fn expired(pool: &SqlitePool, cutoff: &str) -> sqlx::Result<Vec<Stored>> {
    let rows: Vec<(String, String, String, String, i64)> = sqlx::query_as(
        "SELECT i.bot_id, i.sha256, i.attachment_id, a.local_path, i.size FROM agy_images i JOIN attachments a ON a.id = i.attachment_id
         WHERE i.created_at < ? ORDER BY i.created_at, i.sha256",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(bot_id, sha256, attachment_id, local_path, size)| Stored { bot_id, sha256, attachment_id, local_path, size }).collect())
}

pub async fn over_cap(pool: &SqlitePool, bot_id: &str, cap: i64) -> sqlx::Result<Vec<Stored>> {
    let rows: Vec<(String, String, String, String, i64)> = sqlx::query_as(
        "SELECT i.bot_id, i.sha256, i.attachment_id, a.local_path, i.size FROM agy_images i JOIN attachments a ON a.id = i.attachment_id
         WHERE i.bot_id = ? ORDER BY i.created_at, i.sha256",
    )
    .bind(bot_id)
    .fetch_all(pool)
    .await?;
    let mut total: i64 = rows.iter().map(|r| r.4).sum();
    let mut out = Vec::new();
    for (bot_id, sha256, attachment_id, local_path, size) in rows {
        if total <= cap {
            break;
        }
        total -= size;
        out.push(Stored { bot_id, sha256, attachment_id, local_path, size });
    }
    Ok(out)
}

pub async fn bots_with_images(pool: &SqlitePool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar("SELECT DISTINCT bot_id FROM agy_images").fetch_all(pool).await
}

pub async fn drop_rows(pool: &SqlitePool, gone: &[Stored]) -> sqlx::Result<()> {
    for s in gone {
        sqlx::query("DELETE FROM agy_images WHERE bot_id = ? AND sha256 = ?").bind(&s.bot_id).bind(&s.sha256).execute(pool).await?;
    }
    Ok(())
}

pub const OUTBOX_PREFIX: &str = "agy-images/";

fn ext_of_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        _ => "webp",
    }
}

fn epoch(ts: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(ts).ok().map(|t| t.timestamp().max(0) as u64)
}

fn cutoff_at(now: u64) -> String {
    let at = chrono::DateTime::<chrono::Utc>::from_timestamp(now as i64 - RETENTION_SECS, 0).unwrap_or_default();
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 產圖區裡還沒過期的圖，以 outbox 清單的格式列出（`name` 就是下載用的路徑）。
pub async fn outbox_entries(pool: &SqlitePool, bot_id: &str, now: u64) -> sqlx::Result<Vec<serde_json::Value>> {
    let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
        "SELECT i.sha256, i.size, i.created_at, a.mime FROM agy_images i JOIN attachments a ON a.id = i.attachment_id
         WHERE i.bot_id = ? AND i.created_at >= ? ORDER BY i.created_at DESC LIMIT ?",
    )
    .bind(bot_id)
    .bind(cutoff_at(now))
    .bind(crate::outbox::MAX_ENTRIES as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(sha, size, created, mime)| {
            let modified = epoch(&created)?;
            let expires_at = modified + RETENTION_SECS as u64;
            Some(serde_json::json!({
                "name": format!("{OUTBOX_PREFIX}{sha}.{}", ext_of_mime(&mime)),
                "size": size,
                "modified": modified,
                "expires_at": expires_at,
                "remaining_secs": expires_at.saturating_sub(now),
                "source": "agy",
            }))
        })
        .collect())
}

/// 下載 `agy-images/<sha>.<ext>`：要在產圖區、沒過期、副檔名與內容型別一致。回傳（本機檔案、MIME、下載檔名）。
pub async fn outbox_file(pool: &SqlitePool, bot_id: &str, requested: &str, now: u64) -> sqlx::Result<Option<(PathBuf, String, String)>> {
    let Some(rest) = requested.strip_prefix(OUTBOX_PREFIX) else { return Ok(None) };
    let Some((sha, ext)) = rest.rsplit_once('.') else { return Ok(None) };
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Ok(None);
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT a.local_path, a.mime FROM agy_images i JOIN attachments a ON a.id = i.attachment_id
         WHERE i.bot_id = ? AND i.sha256 = ? AND i.created_at >= ?",
    )
    .bind(bot_id)
    .bind(sha)
    .bind(cutoff_at(now))
    .fetch_optional(pool)
    .await?;
    Ok(row.filter(|(_, mime)| ext_of_mime(mime) == ext).map(|(path, mime)| (PathBuf::from(path), mime, format!("{sha}.{ext}"))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-body";
    const JPG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16];

    fn dir(tag: &str) -> PathBuf {
        let d = crate::test_dirs::scratch_dir(&format!("am-agy-images-{tag}-{}", N.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn new_images(root: &Path, before: &Snapshot) -> Collected {
        let now = snapshot(root).unwrap();
        select(&now.difference(before).cloned().collect(), &LocalSource { root }, &BTreeSet::new())
    }

    fn names(c: &Collected) -> Vec<String> {
        c.images.iter().map(|i| i.rel.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn only_images_created_after_the_snapshot_are_collected() {
        let root = dir("new");
        put(&root, "old.png", PNG);
        let before = snapshot(&root).unwrap();
        put(&root, "mockup.png", PNG);
        put(&root, "nested/icon.jpg", JPG);
        let got = new_images(&root, &before);
        assert_eq!(names(&got), vec!["mockup.png", "nested/icon.jpg"]);
        assert_eq!(got.images[1].kind, "jpg");
    }

    #[test]
    fn a_copied_old_image_with_a_fresh_mtime_is_not_new_once_the_original_is_known() {
        let root = dir("copy");
        put(&root, "base/render.png", PNG);
        let before = snapshot(&root).unwrap();
        put(&root, "copy/render.png", PNG);
        let now = snapshot(&root).unwrap();
        let known = BTreeSet::from([hex(&Sha256::digest(PNG))]);
        let got = select(&now.difference(&before).cloned().collect(), &LocalSource { root: &root }, &known);
        assert!(got.images.is_empty(), "複本內容與已收的圖相同，靠去重擋");
    }

    #[test]
    fn scratch_and_hidden_directories_are_never_collected() {
        let root = dir("excluded");
        let before = snapshot(&root).unwrap();
        put(&root, "scratch/tmp.png", PNG);
        put(&root, ".system_generated/logs/a.png", PNG);
        put(&root, ".user_uploaded/photo.png", PNG);
        put(&root, "kept.png", PNG);
        assert_eq!(names(&new_images(&root, &before)), vec!["kept.png"]);
    }

    #[test]
    fn extension_is_not_trusted_content_is() {
        let root = dir("sniff");
        let before = snapshot(&root).unwrap();
        put(&root, "notes.png", b"<html>not an image</html>");
        put(&root, "photo.bin", PNG);
        assert_eq!(names(&new_images(&root, &before)), vec!["photo.bin"]);
    }

    #[test]
    fn symlinks_are_never_followed() {
        let root = dir("symlink");
        let outside = dir("symlink-outside");
        put(&outside, "secret.png", PNG);
        let before = snapshot(&root).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.png"), root.join("link.png")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap();
        assert!(new_images(&root, &before).images.is_empty());
        assert!(snapshot(&root).unwrap().is_empty());
    }

    #[test]
    fn per_turn_cap_and_size_limit_are_reported_not_silent() {
        let root = dir("caps");
        let before = snapshot(&root).unwrap();
        for i in 0..(MAX_PER_TURN + 2) {
            put(&root, &format!("img{i:02}.png"), &[PNG, &[i as u8]].concat());
        }
        let mut huge = PNG.to_vec();
        huge.resize((MAX_BYTES + 1) as usize, 0);
        put(&root, "zz-huge.png", &huge);
        let got = new_images(&root, &before);
        assert_eq!(got.images.len(), MAX_PER_TURN);
        assert_eq!(got.over_limit, 2);
        assert_eq!(got.too_large, 1);
    }

    #[test]
    fn identical_content_is_collected_once_within_a_turn_and_never_again() {
        let root = dir("dedupe");
        let before = snapshot(&root).unwrap();
        put(&root, "a.png", PNG);
        put(&root, "b.png", PNG);
        let got = new_images(&root, &before);
        assert_eq!(got.images.len(), 1);
        assert_eq!(got.over_limit, 0);
        let again = select(&BTreeSet::from([PathBuf::from("a.png")]), &LocalSource { root: &root }, &BTreeSet::from([hex(&Sha256::digest(PNG))]));
        assert!(again.images.is_empty());
    }

    #[test]
    fn conversation_dir_that_does_not_exist_is_an_empty_snapshot() {
        assert!(snapshot(&dir("missing").join("nope")).unwrap().is_empty());
    }

    #[test]
    fn sniff_knows_the_four_formats_and_nothing_else() {
        assert_eq!(sniff(PNG), Some("png"));
        assert_eq!(sniff(JPG), Some("jpg"));
        assert_eq!(sniff(b"GIF89a...."), Some("gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(sniff(b"%PDF-1.7"), None);
    }

    #[test]
    fn remote_listing_round_trips_and_drops_excluded_paths() {
        let out = "./mockup.png\n./nested/icon.jpg\n./.system_generated/a.png\n./scratch/x.png\n\n";
        let got: Vec<String> = parse_listing(out).into_iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert_eq!(got, vec!["mockup.png", "nested/icon.jpg"]);
    }

    #[test]
    fn remote_fetch_parses_sizes_and_bytes_and_skips_oversize_bodies() {
        let rels = vec![PathBuf::from("a.png"), PathBuf::from("big.png")];
        let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
        let out = format!("@@0 {}\n{b64}\n@@1 {}\n", PNG.len(), MAX_BYTES + 1);
        let p = parse_fetch(&out, &rels);
        assert_eq!(p.size(Path::new("a.png")), Some(PNG.len() as u64));
        assert_eq!(p.read(Path::new("a.png")).as_deref(), Some(PNG));
        assert_eq!(p.size(Path::new("big.png")), Some(MAX_BYTES + 1));
        assert!(p.read(Path::new("big.png")).is_none());
        let got = select(&rels.iter().cloned().collect(), &p, &BTreeSet::new());
        assert_eq!(got.images.len(), 1);
        assert_eq!(got.too_large, 1);
    }

    #[test]
    fn remote_fetch_script_quotes_paths_and_checks_links() {
        let s = remote_fetch_script("/home/u/brain/c1", &[PathBuf::from("it's.png")]);
        assert!(s.contains("'./it'\\''s.png'"));
        assert!(s.contains("! -L"));
        assert!(remote_list_script("/b").contains("-name '.?*'"));
    }

    #[test]
    fn image_quota_banner_is_its_own_thing() {
        let line = "RESOURCE_EXHAUSTED (code 8): image generation quota exceeded, reset after 4h44m";
        assert_eq!(image_quota_banner(&format!("noise\n{line}\n")).as_deref(), Some(line));
        assert!(image_quota_banner("Individual quota reached. Resets in 13m").is_none());
    }

    async fn store(tag: &str) -> SqlitePool {
        let d = dir(tag);
        let pool = crate::db::open_with(&d.join("db.sqlite3"), &[]).await.unwrap();
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1', '/p', 'p', '2026-11-26T00:00:00.000Z')").execute(&pool).await.unwrap();
        for bot in ["b1", "b2"] {
            sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?, 'p1', ?, 'agy', 't', '2026-11-26T00:00:00.000Z')").bind(bot).bind(bot).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?, ?, '2026-11-26T00:00:00.000Z')").bind(format!("c-{bot}")).bind(bot).execute(&pool).await.unwrap();
        }
        pool
    }

    async fn add_turn(pool: &SqlitePool, id: &str, status: &str) {
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?, 'c-b1', 'web', ?, 'ok', ?)")
            .bind(id)
            .bind(status)
            .bind("2026-11-26T01:00:00.000Z")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn add_image(pool: &SqlitePool, bot: &str, sha: &str, size: i64, created: &str) {
        let att = format!("att-{sha}");
        sqlx::query("INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at) VALUES (?, ?, 'x.png', 'image/png', ?, ?, '/a', 'local', 'ready', ?)")
            .bind(&att)
            .bind(bot)
            .bind(size)
            .bind(format!("/nonexistent/{sha}.png"))
            .bind(created)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO agy_images (bot_id, sha256, attachment_id, turn_id, size, created_at) VALUES (?, ?, ?, NULL, ?, ?)")
            .bind(bot)
            .bind(sha)
            .bind(&att)
            .bind(size)
            .bind(created)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn marks_wait_for_a_finished_turn_and_back_off_when_deferred() {
        let pool = store("marks").await;
        add_turn(&pool, "t-run", "in_flight").await;
        add_turn(&pool, "t-done", "completed").await;
        let before = Snapshot::from([PathBuf::from("old.png")]);
        mark_open(&pool, "t-run", "b1", "local", "c1", Some("s1"), Some(&before), "2026-11-26T01:00:00.000Z").await.unwrap();
        mark_open(&pool, "t-done", "b1", "local", "c1", Some("s1"), None, "2026-11-26T01:00:00.000Z").await.unwrap();
        let due = due_marks(&pool, "2026-11-26T02:00:00.000Z", 10).await.unwrap();
        assert_eq!(due.iter().map(|m| m.turn_id.as_str()).collect::<Vec<_>>(), vec!["t-done"]);
        assert!(due[0].before.is_none(), "拍不到快照的回合留 NULL");
        defer_mark(&pool, "t-done", "2026-11-26T03:00:00.000Z").await.unwrap();
        assert!(due_marks(&pool, "2026-11-26T02:30:00.000Z", 10).await.unwrap().is_empty());
        assert_eq!(due_marks(&pool, "2026-11-26T03:00:00.000Z", 10).await.unwrap().len(), 1);
        add_turn(&pool, "t-done-2", "completed").await;
        mark_open(&pool, "t-done-2", "b1", "local", "c1", Some("s1"), Some(&before), "2026-11-26T01:00:00.000Z").await.unwrap();
        let due = due_marks(&pool, "2026-11-26T04:00:00.000Z", 10).await.unwrap();
        assert_eq!(due.iter().find(|m| m.turn_id == "t-done-2").unwrap().before.as_ref(), Some(&before));
        assert_eq!(give_up_stale_marks(&pool, "2026-11-26T00:00:00.000Z").await.unwrap(), 0);
        assert_eq!(give_up_stale_marks(&pool, "2026-11-27T00:00:00.000Z").await.unwrap(), 3);
    }

    #[tokio::test]
    async fn cap_evicts_the_oldest_images_until_the_bot_fits() {
        let pool = store("cap").await;
        add_image(&pool, "b1", "aa", 60, "2026-11-20T00:00:00.000Z").await;
        add_image(&pool, "b1", "bb", 60, "2026-11-21T00:00:00.000Z").await;
        add_image(&pool, "b1", "cc", 60, "2026-11-22T00:00:00.000Z").await;
        add_image(&pool, "b2", "dd", 500, "2026-11-19T00:00:00.000Z").await;
        let gone: Vec<String> = over_cap(&pool, "b1", 120).await.unwrap().into_iter().map(|s| s.sha256).collect();
        assert_eq!(gone, vec!["aa"]);
        let gone = expired(&pool, "2026-11-21T12:00:00.000Z").await.unwrap();
        assert_eq!(gone.iter().map(|s| s.sha256.as_str()).collect::<Vec<_>>(), vec!["dd", "aa", "bb"]);
        drop_rows(&pool, &gone).await.unwrap();
        assert!(known_hashes(&pool, "b1").await.unwrap().contains("cc"));
        assert!(!known_hashes(&pool, "b1").await.unwrap().contains("aa"));
        assert_eq!(bots_with_images(&pool).await.unwrap(), vec!["b1".to_string()]);
    }

    #[tokio::test]
    async fn the_outbox_lists_unexpired_images_and_serves_only_their_own_valid_names() {
        let pool = store("outbox").await;
        let a = "a".repeat(64);
        add_image(&pool, "b1", &a, 10, "2026-11-26T00:00:00.000Z").await;
        add_image(&pool, "b1", &"b".repeat(64), 10, "2026-10-01T00:00:00.000Z").await;
        add_image(&pool, "b2", &"c".repeat(64), 10, "2026-11-26T00:00:00.000Z").await;
        let now = epoch("2026-11-26T12:00:00Z").unwrap();
        let list = outbox_entries(&pool, "b1", now).await.unwrap();
        assert_eq!(list.len(), 1, "過期的不列");
        assert_eq!(list[0]["name"], format!("agy-images/{a}.png"));
        assert_eq!(list[0]["remaining_secs"], RETENTION_SECS as u64 - 12 * 3600);
        let got = outbox_file(&pool, "b1", &format!("agy-images/{a}.png"), now).await.unwrap();
        assert_eq!(got.map(|(_, m, n)| (m, n)), Some(("image/png".to_string(), format!("{a}.png"))));
        assert!(outbox_file(&pool, "b1", &format!("agy-images/{a}.jpg"), now).await.unwrap().is_none(), "副檔名要跟內容型別一致");
        assert!(outbox_file(&pool, "b2", &format!("agy-images/{a}.png"), now).await.unwrap().is_none(), "別顆 bot 的圖不給");
        assert!(outbox_file(&pool, "b1", "agy-images/../../etc/passwd", now).await.unwrap().is_none());
        assert!(outbox_file(&pool, "b1", &format!("agy-images/{}.png", "A".repeat(64)), now).await.unwrap().is_none());
        assert!(outbox_file(&pool, "b1", &format!("agy-images/{}.png", "b".repeat(64)), now).await.unwrap().is_none(), "過期的不給下載");
    }

    #[test]
    fn the_image_banner_is_neither_a_text_quota_nor_an_api_error_pause() {
        let line = "RESOURCE_EXHAUSTED (code 8): image generation quota exceeded, reset after 4h44m";
        assert!(image_quota_banner(line).is_some());
        assert!(image_quota_since(&format!("{{\"created_at\":\"2026-11-26T02:00:00Z\",\"text\":\"{line}\"}}"), "2026-11-26T01:00:00Z").is_some());
        assert!(image_quota_since(&format!("{{\"created_at\":\"2026-11-26T00:00:00Z\",\"text\":\"{line}\"}}"), "2026-11-26T01:00:00Z").is_none(), "前一回合的橫幅不算這回合");
    }
}
