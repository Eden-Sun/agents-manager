//! `GET /api/bots/{id}/local-image?path=…`：讓對話裡 Markdown 的 `![](docs/shot.png)` 真的顯示出來。
//!
//! bot 回覆寫的是它工作目錄裡的檔案路徑，瀏覽器無從讀起，只會畫一個破圖（2026-09-15 使用者截圖）。
//! 這裡只放行「這顆 bot 所屬專案目錄底下的圖片檔」：相對路徑先以 bot 的工作目錄（`bots.cwd`，child 在
//! worktree 裡跑）為底、再退回專案目錄，符號連結解開後仍須在專案裡；副檔名白名單、大小上限，其他一律 404。
//! 只支援本機專案（遠端主機的檔案不在這台）。

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

/// `%XX` 解碼。Markdown 渲染會把圖片網址正規化：`docs/截圖.png` 到前端是 `docs/%E6%88%AA%E5%9C%96.png`、
/// `<docs/my shot.png>` 是 `docs/my%20shot.png`，`file://` 網址本來就是編過的。不是合法 UTF-8 就放棄。
fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// 純路徑判斷（可測）：`requested` 解開、canonicalize 後必須仍在 `root` 裡且是圖片檔。
///
/// 相對路徑先以 `cwd`（bot 的工作目錄）為底、找不到再退回 `root`：child 在 `.claude/worktrees/<名>` 裡截圖、
/// 回覆寫 `![](docs/shots/a.png)`，只看專案根目錄會讀到主樹的同名舊圖（review3 c4 L3）。
/// 字面路徑讀不到時再試 `%XX` 解碼後的路徑（檔名真的含 `%20` 的照樣讀得到）。
pub(crate) fn resolve(root: &Path, cwd: Option<&Path>, requested: &str) -> Option<(PathBuf, &'static str)> {
    let requested = requested.trim();
    let requested = requested.strip_prefix("file://").unwrap_or(requested);
    if requested.is_empty() {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let mut spellings = vec![requested.to_string()];
    spellings.extend(percent_decode(requested).filter(|d| d != requested));
    for spelled in &spellings {
        let p = Path::new(spelled);
        let candidates: Vec<PathBuf> = if p.is_absolute() {
            vec![p.to_path_buf()]
        } else {
            cwd.map(|c| c.join(p)).into_iter().chain([root.join(p)]).collect()
        };
        for candidate in candidates {
            let Ok(real) = std::fs::canonicalize(&candidate) else { continue };
            if !real.starts_with(&root) || !real.is_file() {
                continue;
            }
            if let Some(mime) = mime_of(&real) {
                return Some((real, mime));
            }
        }
    }
    None
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
    let cwd = bot.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(Path::new);
    let (path, mime) = resolve(Path::new(&project.path), cwd, requested).ok_or_else(not_found)?;
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

        let ok = resolve(&root, None, "docs/shots/a.png").expect("relative inside");
        assert_eq!(ok.1, "image/png");
        let abs = root.join("docs/shots/a.png");
        assert!(resolve(&root, None, abs.to_str().unwrap()).is_some(), "absolute inside");
        assert!(resolve(&root, None, &format!("file://{}", abs.display())).is_some(), "file:// URL");
        assert!(resolve(&root, None, "../outside.png").is_none(), "escapes with ..");
        assert!(resolve(&root, None, "..%2Foutside.png").is_none(), "escapes with an encoded ..");
        assert!(resolve(&root, None, base.join("outside.png").to_str().unwrap()).is_none(), "absolute outside");
        assert!(resolve(&root, None, "link.png").is_none(), "symlink pointing outside");
        assert!(resolve(&root, None, "notes.txt").is_none(), "not an image");
        assert!(resolve(&root, None, "docs/shots/missing.png").is_none());
        assert!(resolve(&root, None, "docs").is_none(), "a directory");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// review3 c4 L3：child 在自己的 worktree 截圖、回覆寫相對路徑，要讀 worktree 那張，不是主樹的同名舊圖；
    /// worktree 裡沒有才退回專案根目錄。bot 的工作目錄在專案外時照樣不放行。
    #[test]
    fn relative_paths_start_from_the_bots_working_dir() {
        let base = std::env::temp_dir().join(format!("am-local-image-cwd-{}", crate::db::ulid()));
        let root = base.join("proj");
        let wt = root.join(".claude/worktrees/child");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(wt.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.png"), b"old").unwrap();
        std::fs::write(wt.join("docs/a.png"), b"new").unwrap();
        std::fs::write(root.join("docs/only-root.png"), b"root").unwrap();
        let outside = base.join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("b.png"), b"png").unwrap();

        let (got, _) = resolve(&root, Some(&wt), "docs/a.png").expect("worktree copy");
        assert_eq!(std::fs::read(&got).unwrap(), b"new", "讀到的是 {}", got.display());
        let (got, _) = resolve(&root, None, "docs/a.png").unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), b"old", "沒有 cwd 就是專案根目錄");
        let (got, _) = resolve(&root, Some(&wt), "docs/only-root.png").expect("falls back to the project root");
        assert_eq!(std::fs::read(&got).unwrap(), b"root");
        assert!(resolve(&root, Some(&outside), "b.png").is_none(), "工作目錄在專案外：範圍仍是專案目錄");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Markdown 渲染把網址編過：中文檔名、空白、`file://` 網址都是 `%XX`。
    #[test]
    fn percent_encoded_paths_resolve() {
        let base = std::env::temp_dir().join(format!("am-local-image-pct-{}", crate::db::ulid()));
        let root = base.join("proj");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/截圖.png"), b"png").unwrap();
        std::fs::write(root.join("docs/my shot.png"), b"png").unwrap();
        std::fs::write(root.join("docs/100%20.png"), b"literal").unwrap();

        assert!(resolve(&root, None, "docs/%E6%88%AA%E5%9C%96.png").is_some(), "中文檔名");
        assert!(resolve(&root, None, "docs/my%20shot.png").is_some(), "空白");
        let abs = std::fs::canonicalize(root.join("docs")).unwrap().join("%E6%88%AA%E5%9C%96.png");
        assert!(resolve(&root, None, &format!("file://{}", abs.display())).is_some(), "file:// 網址");
        let (got, _) = resolve(&root, None, "docs/100%20.png").expect("字面檔名先試");
        assert_eq!(std::fs::read(&got).unwrap(), b"literal");
        assert_eq!(percent_decode("a%2"), Some("a%2".into()), "不完整的 % 原樣保留");
        assert_eq!(percent_decode("%FF.png"), None, "不是 UTF-8 就放棄");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
