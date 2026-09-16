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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::lifecycle::LcError;
use crate::state::App;

/// 一次列這麼多就夠了（新的排前面）：scratchpad 是暫存，不是檔案總管。
const MAX_ENTRIES: usize = 300;
/// 單檔上限。整份下載會先讀進記憶體，而且瀏覽器那端也要收得下。
const MAX_BYTES: u64 = 64 * 1024 * 1024;

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
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            files.push(json!({"name": name, "size": meta.len(), "modified": modified}));
        }
    }
    files.sort_by(|a, b| b["modified"].as_u64().cmp(&a["modified"].as_u64()));
    files.truncate(MAX_ENTRIES);
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
    let path = resolve(&dir, requested).ok_or_else(not_found)?;
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
