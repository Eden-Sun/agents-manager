//! `GET /api/bots/{id}/local-image?path=…`：讓對話裡 Markdown 的 `![](docs/shot.png)` 真的顯示出來。
//!
//! bot 回覆寫的是它工作目錄裡的檔案路徑，瀏覽器無從讀起，只會畫一個破圖（2026-09-15 使用者截圖）。
//! 這裡只放行「這顆 bot 所屬專案目錄底下的圖片檔」：相對路徑以專案目錄為底，符號連結解開後仍須在
//! 專案裡；副檔名白名單、大小上限，其他一律 404。只支援本機專案（遠端主機的檔案不在這台）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::lifecycle::LcError;
use crate::state::App;

/// 截圖等級的圖片就夠了；太大的檔不該塞進一則對話。
const MAX_BYTES: u64 = 20 * 1024 * 1024;

/// 不含 svg：svg 可以帶腳本，這條路不需要冒這個險。
fn mime_of(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// 純路徑判斷（可測）：`requested` 以 `root` 為底解開、canonicalize 後必須仍在 `root` 裡且是圖片檔。
pub(crate) fn resolve(root: &Path, requested: &str) -> Option<(PathBuf, &'static str)> {
    let requested = requested.trim();
    let requested = requested.strip_prefix("file://").unwrap_or(requested);
    if requested.is_empty() {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let candidate = {
        let p = Path::new(requested);
        if p.is_absolute() { p.to_path_buf() } else { root.join(p) }
    };
    let real = std::fs::canonicalize(&candidate).ok()?;
    if !real.starts_with(&root) || !real.is_file() {
        return None;
    }
    let mime = mime_of(&real)?;
    Some((real, mime))
}

pub async fn get(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, LcError> {
    let not_found = || LcError::NotFound("image".into());
    let requested = q.get("path").ok_or_else(|| LcError::Bad("path required".into()))?;
    let bot = crate::db::bot(&app.db, &id).await.ok().flatten().ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project = crate::db::project(&app.db, &bot.project_id).await.ok().flatten().ok_or_else(not_found)?;
    if project.host != crate::config::LOCAL_HOST {
        return Err(not_found());
    }
    let (path, mime) = resolve(Path::new(&project.path), requested).ok_or_else(not_found)?;
    let meta = tokio::fs::metadata(&path).await.map_err(|_| not_found())?;
    if meta.len() > MAX_BYTES {
        return Err(not_found());
    }
    let data = tokio::fs::read(&path).await.map_err(|_| not_found())?;
    Ok((StatusCode::OK, [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "private, no-cache")], data).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 專案裡的圖片（相對或絕對）放行；`..` 逃出去、符號連結指到外面、非圖片、不存在都擋。
    #[test]
    fn only_images_inside_the_project_resolve() {
        let base = std::env::temp_dir().join(format!("am-local-image-{}", crate::db::ulid()));
        let root = base.join("proj");
        std::fs::create_dir_all(root.join("docs/shots")).unwrap();
        std::fs::write(root.join("docs/shots/a.png"), b"png").unwrap();
        std::fs::write(root.join("notes.txt"), b"secret").unwrap();
        std::fs::write(base.join("outside.png"), b"png").unwrap();
        std::os::unix::fs::symlink(base.join("outside.png"), root.join("link.png")).unwrap();

        let ok = resolve(&root, "docs/shots/a.png").expect("relative inside");
        assert_eq!(ok.1, "image/png");
        let abs = root.join("docs/shots/a.png");
        assert!(resolve(&root, abs.to_str().unwrap()).is_some(), "absolute inside");
        assert!(resolve(&root, &format!("file://{}", abs.display())).is_some(), "file:// URL");
        assert!(resolve(&root, "../outside.png").is_none(), "escapes with ..");
        assert!(resolve(&root, base.join("outside.png").to_str().unwrap()).is_none(), "absolute outside");
        assert!(resolve(&root, "link.png").is_none(), "symlink pointing outside");
        assert!(resolve(&root, "notes.txt").is_none(), "not an image");
        assert!(resolve(&root, "docs/shots/missing.png").is_none());
        assert!(resolve(&root, "docs").is_none(), "a directory");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
