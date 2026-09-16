//! `GET /api/bots/{id}/scratchpad`、`…/scratchpad/file?path=…`：把 bot 寫在自己 scratchpad 裡的檔案
//! 交到使用者手上。
//!
//! bot 常常把整理好的東西寫成檔案（`scratchpad/tracking.tsv`、報告、log）然後在對話裡報路徑——
//! 但那是 **daemon 這台機器上某個暫存目錄**裡的檔案，使用者在瀏覽器（尤其手機）根本拿不到，
//! 只能自己 ssh 進來翻（使用者 2026-09-16）。所以這裡把那個目錄列出來、允許下載。
//!
//! 目錄不是我們發明的：Claude Code 每個 session 有自己的 scratchpad
//! （`/private/tmp/claude-<uid>/<cwd 的 slug>/<session id>/scratchpad`）。slug 的算法是它的實作細節，
//! 所以**不要猜 slug**——用 session id 去掃：session id 是 UUID，掃到就是那一個。
//!
//! 界線跟 [`crate::local_image`] 同一套：只放行那個 scratchpad 目錄底下的一般檔案，符號連結解開後
//! 仍須在裡面；一律當附件下載（`Content-Disposition: attachment` + `nosniff`），不讓瀏覽器把使用者
//! 自己的 HTML 當同源頁面執行。遠端主機的 bot 不支援——那些檔案不在這台機器上。
//!
//! **列什麼**（使用者 2026-09-16：「沒提到過的檔案怎麼也在這裡面，還有 sqlite3」）：scratchpad 是 bot 的
//! 工作桌，大部分是它自己跑過的腳本與中間產物。清單每個檔案帶 `mentioned`——檔名有在這顆 bot 的對話裡
//! 出現過才算交給使用者的東西，前端預設只顯示這些。資料庫與金鑰類（[`withheld`]）不管有沒有提到都不列、
//! 也不給下載：那常是正式 DB 的複本或私鑰，一個 UI token 就能拿走。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use sqlx::SqlitePool;

use crate::lifecycle::LcError;
use crate::state::App;

/// 一次列這麼多就夠了（新的排前面）：scratchpad 是暫存，不是檔案總管。
const MAX_ENTRIES: usize = 300;
/// 單檔上限。整份下載會先讀進記憶體，而且瀏覽器那端也要收得下。
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// 拿這顆 bot 最近這麼多則對話來比對檔名：涵蓋一段工作夠了，也不必每次重整掃整部歷史。
const MENTION_MESSAGES: i64 = 1000;
/// 比對用的文字最多這麼大（新的在前，超過就不再往舊的拿）：檔名數 × 文字長度是每次重整的成本。
const MENTION_BYTES: usize = 2 * 1024 * 1024;

/// Claude Code 的 scratchpad 根：`/private/tmp/claude-<uid>`。測試會覆寫。
fn roots() -> Vec<PathBuf> {
    if let Ok(dir) = std::env::var("AM_SCRATCHPAD_ROOT") {
        return vec![PathBuf::from(dir)];
    }
    let mut out = Vec::new();
    for base in ["/private/tmp", "/tmp"] {
        let Ok(entries) = std::fs::read_dir(base) else { continue };
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("claude-") {
                out.push(e.path());
            }
        }
    }
    out
}

/// 這個 session 的 scratchpad 目錄。用 session id 掃，不重建 slug（那是 CLI 的實作細節）。
pub(crate) fn dir_for_session(session: &str) -> Option<PathBuf> {
    // UUID 以外的東西不拿去掃檔案系統。
    if session.len() < 8 || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    for root in roots() {
        let Ok(slugs) = std::fs::read_dir(&root) else { continue };
        for slug in slugs.flatten() {
            let candidate = slug.path().join(session).join("scratchpad");
            if candidate.is_dir() {
                return std::fs::canonicalize(candidate).ok();
            }
        }
    }
    None
}

/// 純路徑判斷（可測）：`requested` 以 `root` 為底解開、canonicalize 之後必須仍在 `root` 裡的一般檔案。
pub(crate) fn resolve(root: &Path, requested: &str) -> Option<PathBuf> {
    let requested = requested.trim();
    if requested.is_empty() {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let p = Path::new(requested);
    let candidate = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
    let real = std::fs::canonicalize(&candidate).ok()?;
    if !real.starts_with(&root) || !real.is_file() {
        return None;
    }
    Some(real)
}

/// 不管對話有沒有提到都不列、不給下載的檔案。先看名字（隱藏檔、資料庫與它的旁檔、金鑰與憑證），
/// 名字看不出來的再看開頭幾個位元組（改過副檔名的 SQLite、PEM 私鑰）。
pub(crate) fn withheld(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    withheld_name(&name) || withheld_content(path)
}

fn withheld_name(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    // `x.sqlite3`、`x.sqlite3-wal`、`x.sqlite3.bak-20260916`、`bak1644.db-shm`。
    if name.contains(".sqlite") || name.ends_with(".db") || name.contains(".db-") || name.contains(".db.") {
        return true;
    }
    const KEYS: [&str; 9] = [".pem", ".key", ".p12", ".pfx", ".jks", ".keystore", ".ppk", ".kdbx", ".env"];
    KEYS.iter().any(|ext| name.ends_with(ext)) || ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"].iter().any(|k| name.starts_with(k))
}

fn withheld_content(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 64];
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let n = f.read(&mut head).unwrap_or(0);
    let head = &head[..n];
    if head.starts_with(b"SQLite format 3\0") {
        return true;
    }
    head.starts_with(b"-----BEGIN") && head.windows(11).any(|w| w == b"PRIVATE KEY")
}

/// 檔名有沒有在對話裡被提到（`haystack` 是對話全文）。要像一個獨立的名字出現：`tracking.tsv` 不算
/// 出現在 `old-tracking.tsv` 或 `tracking.tsvx` 裡。前後接中文、標點、路徑斜線都算。
/// 沒有副檔名的名字（`raw`、`collect`）在一般句子裡太容易撞到，要前面是 `/` 或反引號才算。
pub(crate) fn mentioned(haystack: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let hay = haystack.as_bytes();
    let glued = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let bare = !name.contains('.');
    let step = name.chars().next().map_or(1, char::len_utf8);
    let mut from = 0;
    while let Some(i) = haystack[from..].find(name) {
        let start = from + i;
        let before = start.checked_sub(1).map(|j| hay[j]);
        let before_ok = match before {
            Some(b) if bare => b == b'/' || b == b'`',
            Some(b) => !glued(b) && b != b'.',
            None => !bare,
        };
        let after_ok = hay.get(start + name.len()).is_none_or(|b| !glued(*b));
        if before_ok && after_ok {
            return true;
        }
        from = start + step;
    }
    false
}

/// 這顆 bot 最近的對話（使用者與 bot 說的話），新的在前，接成一段文字給 [`mentioned`] 比對。
async fn mention_haystack(pool: &SqlitePool, bot_id: &str) -> String {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT m.content FROM messages m JOIN conversations c ON c.id = m.conversation_id
          WHERE c.bot_id = ? AND m.role IN ('user','assistant')
          ORDER BY m.created_at DESC LIMIT ?",
    )
    .bind(bot_id)
    .bind(MENTION_MESSAGES)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut out = String::new();
    for content in rows {
        if out.len() + content.len() > MENTION_BYTES {
            break;
        }
        out.push_str(&content);
        out.push('\n');
    }
    out
}

/// 目錄裡可以列出來的檔案（新的排前面，最多 [`MAX_ENTRIES`]），每個帶 `mentioned`。純檔案系統＋字串，可測。
pub(crate) fn scan(dir: &Path, haystack: &str) -> Vec<serde_json::Value> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() || withheld(&e.path()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            files.push((name, meta.len(), modified));
        }
    }
    files.sort_by(|a, b| b.2.cmp(&a.2));
    files.truncate(MAX_ENTRIES);
    files
        .into_iter()
        .map(|(name, size, modified)| {
            let mentioned = mentioned(haystack, &name);
            json!({"name": name, "size": size, "modified": modified, "mentioned": mentioned})
        })
        .collect()
}

/// 下載放行哪個檔案：[`resolve`] 的界線之外，路徑上不能有隱藏目錄，檔案本身也不能是 [`withheld`]。
pub(crate) fn servable(root: &Path, requested: &str) -> Option<PathBuf> {
    let real = resolve(root, requested)?;
    let root = std::fs::canonicalize(root).ok()?;
    let rel = real.strip_prefix(&root).ok()?;
    if rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) || withheld(&real) {
        return None;
    }
    Some(real)
}

/// 下載時的 content type。白名單以外一律 octet-stream：使用者自己的 HTML 不該在這個 origin 跑起來
/// （token 就放在這個 origin 的 localStorage）。
fn mime_of(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "log" | "md" | "csv" | "tsv" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// `filename*=UTF-8''…`：中文檔名在 `filename=` 裡會變亂碼或被截斷。
fn content_disposition(name: &str) -> String {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    let encoded: String = name
        .as_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(b) {
                (*b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"{safe}\"; filename*=UTF-8''{encoded}")
}

async fn scratchpad_of(app: &Arc<App>, bot_id: &str) -> Result<(PathBuf, String), LcError> {
    let bot = crate::db::bot(&app.db, bot_id).await.ok().flatten().ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project = crate::db::project(&app.db, &bot.project_id).await.ok().flatten().ok_or_else(|| LcError::NotFound("bot".into()))?;
    if project.host != crate::config::LOCAL_HOST {
        return Err(LcError::conflict("scratchpad_remote", json!({"reason": "scratchpad_remote", "host": project.host})));
    }
    // 最近一次有 native session 的 run：bot 停了之後那些檔案還在，使用者照樣拿得到。
    let session: Option<String> = sqlx::query_scalar(
        "SELECT native_session_id FROM runs WHERE bot_id = ? AND native_session_id IS NOT NULL AND native_session_id != ''
          ORDER BY started_at DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(&app.db)
    .await
    .ok()
    .flatten();
    let Some(session) = session else {
        return Err(LcError::conflict("scratchpad_no_session", json!({"reason": "scratchpad_no_session"})));
    };
    let dir = dir_for_session(&session)
        .ok_or_else(|| LcError::conflict("scratchpad_missing", json!({"reason": "scratchpad_missing", "session_id": session})))?;
    Ok((dir, session))
}

/// `GET /api/bots/{id}/scratchpad` — 這顆 bot 的 scratchpad 裡有什麼（新的排前面）。
pub async fn list(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Result<Response, LcError> {
    let (dir, session) = match scratchpad_of(&app, &id).await {
        Ok(v) => v,
        // 沒有 scratchpad 不是錯誤，是常態（codex／grok、遠端、還沒跑過）：回空清單並說明原因，
        // 前端才不用把每一種情況都畫成紅字。
        Err(LcError::Conflict(detail)) => {
            return Ok((StatusCode::OK, axum::Json(json!({"files": [], "reason": detail.get("reason")}))).into_response())
        }
        Err(e) => return Err(e),
    };
    let haystack = mention_haystack(&app.db, &id).await;
    // 讀目錄、讀檔頭、比對字串都是同步的：別卡在 async worker 上。
    let scan_dir = dir.clone();
    let files = tokio::task::spawn_blocking(move || scan(&scan_dir, &haystack)).await.unwrap_or_default();
    Ok((StatusCode::OK, axum::Json(json!({"dir": dir.to_string_lossy(), "session_id": session, "files": files}))).into_response())
}

/// `GET /api/bots/{id}/scratchpad/file?path=…` — 一律當附件下載。
pub async fn file(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, LcError> {
    let not_found = || LcError::NotFound("file".into());
    let requested = q.get("path").ok_or_else(|| LcError::Bad("path required".into()))?;
    let (dir, _) = scratchpad_of(&app, &id).await?;
    let path = servable(&dir, requested).ok_or_else(not_found)?;
    let meta = tokio::fs::metadata(&path).await.map_err(|_| not_found())?;
    if meta.len() > MAX_BYTES {
        return Err(LcError::conflict(
            "file_too_large",
            json!({"reason": "file_too_large", "size": meta.len(), "max": MAX_BYTES}),
        ));
    }
    let data = tokio::fs::read(&path).await.map_err(|_| not_found())?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime_of(&path).to_string()),
            (header::CONTENT_DISPOSITION, content_disposition(&name)),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        data,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// scratchpad 裡的檔案（相對或絕對）放行；`..`、指到外面的符號連結、目錄、不存在都擋。
    #[test]
    fn only_files_inside_the_scratchpad_resolve() {
        let base = std::env::temp_dir().join(format!("am-scratch-{}", crate::db::ulid()));
        let root = base.join("scratchpad");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("tracking.tsv"), b"a\tb\n").unwrap();
        std::fs::write(root.join("sub/report.md"), b"# hi").unwrap();
        std::fs::write(base.join("outside.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(base.join("outside.txt"), root.join("link.txt")).unwrap();

        assert!(resolve(&root, "tracking.tsv").is_some());
        assert!(resolve(&root, "sub/report.md").is_some(), "子目錄也算在裡面");
        let abs = root.join("tracking.tsv");
        assert!(resolve(&root, abs.to_str().unwrap()).is_some(), "絕對路徑但在裡面");
        assert!(resolve(&root, "../outside.txt").is_none(), "用 .. 逃出去");
        assert!(resolve(&root, base.join("outside.txt").to_str().unwrap()).is_none(), "絕對路徑在外面");
        assert!(resolve(&root, "link.txt").is_none(), "符號連結指到外面");
        assert!(resolve(&root, "sub").is_none(), "目錄不是檔案");
        assert!(resolve(&root, "missing.tsv").is_none());
        assert!(resolve(&root, "  ").is_none());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 用 session id 掃得到目錄，而且不猜 slug——slug 的算法是 CLI 的實作細節。
    #[test]
    fn the_session_id_finds_the_directory_whatever_the_slug_is() {
        let root = std::env::temp_dir().join(format!("am-scratch-root-{}", crate::db::ulid()));
        let session = "0004cea2-a8cd-4c0c-aeb5-6ddaa7fd480c";
        let slug = root.join("-some-slug-nobody-should-reconstruct");
        std::fs::create_dir_all(slug.join(session).join("scratchpad")).unwrap();
        // 同一個根底下還有別的 session，不能挑錯。
        std::fs::create_dir_all(root.join("-other").join("1111cea2-a8cd-4c0c-aeb5-6ddaa7fd480c").join("scratchpad")).unwrap();
        temp_env(&root, || {
            let found = dir_for_session(session).expect("找得到");
            assert!(found.ends_with(format!("{session}/scratchpad")), "{found:?}");
            assert!(dir_for_session("2222cea2-a8cd-4c0c-aeb5-6ddaa7fd480c").is_none(), "沒有的 session");
            assert!(dir_for_session("../../etc").is_none(), "不是 session id 的東西不拿去掃");
            assert!(dir_for_session("").is_none());
        });
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 下載一律是附件，而且中文檔名帶得回去。
    #[test]
    fn every_download_is_an_attachment_with_a_usable_filename() {
        assert_eq!(mime_of(Path::new("a.tsv")), "text/plain; charset=utf-8");
        assert_eq!(mime_of(Path::new("a.pdf")), "application/pdf");
        // 白名單以外一律 octet-stream：使用者自己的 HTML 不在這個 origin 跑起來。
        assert_eq!(mime_of(Path::new("evil.html")), "application/octet-stream");
        assert_eq!(mime_of(Path::new("evil.svg")), "application/octet-stream");
        assert_eq!(mime_of(Path::new("noext")), "application/octet-stream");

        let d = content_disposition("出貨追蹤 v2.tsv");
        assert!(d.starts_with("attachment; "), "{d}");
        assert!(d.contains("filename*=UTF-8''"), "{d}");
        assert!(d.contains("%E5%87%BA"), "中文要 percent-encode：{d}");
        assert!(!d.contains("出貨"), "ASCII 的那份不能夾原字元：{d}");
        assert!(content_disposition("a\"; rm -rf /.txt").contains("filename=\"a__"), "引號不能逃出去");
    }

    /// 檔名要像一個獨立的名字出現才算提到過。
    #[test]
    fn a_file_counts_as_mentioned_only_when_its_name_stands_on_its_own() {
        let hay = "檔案好了：`/private/tmp/x/scratchpad/download-test.txt`（496 bytes）\n寫到tracking.tsv了。\n報告在 出貨追蹤 v2.md，\n看 raw data";
        assert!(mentioned(hay, "download-test.txt"), "完整路徑裡");
        assert!(mentioned(hay, "tracking.tsv"), "前後接中文");
        assert!(mentioned(hay, "出貨追蹤 v2.md"), "中文與空白的檔名");
        assert!(mentioned("see report.md.", "report.md"), "句尾句點");

        assert!(!mentioned(hay, "test.txt"), "只是別人檔名的一截（前面接 -）");
        assert!(!mentioned("old_tracking.tsv", "tracking.tsv"), "前面接底線");
        assert!(!mentioned("a.tsv.tracking.tsv", "tracking.tsv"), "前面接句點：是別的檔名的一截");
        assert!(!mentioned("run w1.pyc", "w1.py"), "後面還接著字");
        assert!(!mentioned(hay, "m3.txt"), "沒提到");
        assert!(!mentioned(hay, "raw"), "沒有副檔名的短名字在句子裡不算");
        assert!(mentioned("輸出在 scratchpad/raw", "raw"), "像路徑一樣出現就算");
        assert!(mentioned("看 `collect`", "collect"));
        assert!(!mentioned(hay, ""), "空名字");
        // 多位元組字開頭、第一個命中不合格時要能往下找，不能切在字元中間 panic。
        assert!(mentioned("x出貨.md 與 出貨.md", "出貨.md"));
    }

    /// 資料庫、金鑰、隱藏檔不管名字怎麼取都不列、不給下載；一般檔案照常。
    #[test]
    fn databases_and_keys_are_never_listed_or_served() {
        let base = std::env::temp_dir().join(format!("am-scratch-withheld-{}", crate::db::ulid()));
        let root = base.join("scratchpad");
        std::fs::create_dir_all(root.join(".secret")).unwrap();
        let put = |name: &str, body: &[u8]| std::fs::write(root.join(name), body).unwrap();
        put("report.md", b"# ok");
        put("migrate-check.sqlite3", b"SQLite format 3\0....");
        put("bak1644.db", b"SQLite format 3\0....");
        put("bak1644.db-wal", b"x");
        put("agents-manager.sqlite3.bak-20260916", b"x");
        put("innocent.bin", b"SQLite format 3\0 renamed");
        put("server.pem", b"-----BEGIN CERTIFICATE-----");
        put("notes.txt", b"-----BEGIN OPENSSH PRIVATE KEY-----\nabc");
        put("id_ed25519", b"x");
        put("prod.env", b"TOKEN=x");
        put(".env", b"TOKEN=x");
        std::fs::write(root.join(".secret/plain.txt"), b"hi").unwrap();

        let hay = "report.md migrate-check.sqlite3 innocent.bin notes.txt";
        let listed: Vec<String> = scan(&root, hay).iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(listed, vec!["report.md".to_string()], "只剩一般檔案，提到了也不列");

        let ok = |p: &str| servable(&root, p).is_some();
        assert!(ok("report.md"));
        for p in ["migrate-check.sqlite3", "bak1644.db", "bak1644.db-wal", "agents-manager.sqlite3.bak-20260916", "innocent.bin", "server.pem", "notes.txt", "id_ed25519", "prod.env", ".env", ".secret/plain.txt"] {
            assert!(!ok(p), "{p} 不能下載");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 清單的 `mentioned` 來自這顆 bot 自己的對話（使用者與 bot 的話），別顆 bot 提到的不算。
    #[tokio::test]
    async fn mentions_come_from_this_bots_own_conversation() {
        let dir = std::env::temp_dir().join(format!("am-scratch-mention-{}", crate::db::ulid()));
        std::fs::create_dir_all(dir.join("scratchpad")).unwrap();
        let pool = crate::db::open(&dir.join("t.sqlite3")).await.unwrap();
        let now = "2026-09-16T00:00:00.000Z";
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now).execute(&pool).await.unwrap();
        for b in ["b", "other"] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p',?,'claude','t',?)")
                .bind(b).bind(b).bind(now).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES (?,?,?)")
                .bind(format!("c-{b}")).bind(b).bind(now).execute(&pool).await.unwrap();
        }
        let say = |conv: &'static str, id: &'static str, role: &'static str, text: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query("INSERT INTO messages (id,conversation_id,role,content,source,created_at) VALUES (?,?,?,?,'web',?)")
                    .bind(id).bind(conv).bind(role).bind(text).bind(now).execute(&pool).await.unwrap();
            }
        };
        say("c-b", "m1", "user", "把結果寫成 tracking.tsv").await;
        say("c-b", "m2", "assistant", "好了：scratchpad/report.md").await;
        say("c-b", "m3", "system", "system 提到 sys.txt 不算").await;
        say("c-other", "m4", "assistant", "別顆 bot 提到 elsewhere.txt").await;

        let sp = dir.join("scratchpad");
        for f in ["tracking.tsv", "report.md", "sys.txt", "elsewhere.txt", "w1.py"] {
            std::fs::write(sp.join(f), b"x").unwrap();
        }
        let hay = mention_haystack(&pool, "b").await;
        let files = scan(&sp, &hay);
        let flag = |n: &str| files.iter().find(|f| f["name"] == n).map(|f| f["mentioned"].as_bool().unwrap());
        assert_eq!(flag("tracking.tsv"), Some(true), "使用者說的");
        assert_eq!(flag("report.md"), Some(true), "bot 說的");
        assert_eq!(flag("sys.txt"), Some(false), "system 訊息不算");
        assert_eq!(flag("elsewhere.txt"), Some(false), "別顆 bot 的對話不算");
        assert_eq!(flag("w1.py"), Some(false), "沒人提過的工作檔");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 測試改寫 scratchpad 根，不要掃到真的 /private/tmp。
    fn temp_env(root: &Path, f: impl FnOnce()) {
        let prev = std::env::var("AM_SCRATCHPAD_ROOT").ok();
        // SAFETY: 測試單執行緒內設定自己的環境變數，結束就還原。
        unsafe { std::env::set_var("AM_SCRATCHPAD_ROOT", root) };
        f();
        match prev {
            Some(v) => unsafe { std::env::set_var("AM_SCRATCHPAD_ROOT", v) },
            None => unsafe { std::env::remove_var("AM_SCRATCHPAD_ROOT") },
        }
    }
}
