//! `GET /api/bots/{id}/local-image?path=…`：讓對話裡 Markdown 的 `![](docs/shot.png)` 真的顯示出來。
//!
//! bot 回覆寫的是它工作目錄裡的檔案路徑，瀏覽器無從讀起，只會畫一個破圖（2026-09-15 使用者截圖）。
//! 這裡只放行「這顆 bot 所屬專案目錄底下的圖片檔」：相對路徑先以 bot 的工作目錄（`bots.cwd`，child 在
//! worktree 裡跑）為底、再退回專案目錄，符號連結解開後仍須在專案裡；副檔名白名單、大小上限，其他一律 404。
//! 只支援本機專案（遠端主機的檔案不在這台）。
//!
//! **跟 [`crate::outbox`] 共用 [`crate::trusted_open`]**：從專案目錄開始逐層 `openat(O_NOFOLLOW)` 一路開
//! 到要的檔案，拿到的 fd 直接 `fstat`／讀內容，不再先驗證路徑、再用路徑名字重新 open 一次（issue #89——
//! 舊實作 canonicalize 完確認在界線內之後，`metadata`／`read` 是分開的兩次路徑操作，中間有機會被換成
//! 指到界線外的符號連結）。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::lc_error::LcError;
use crate::trusted_open;

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
pub fn percent_decode(s: &str) -> Option<String> {
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

/// 純路徑判斷（可測）：`requested` 解開之後，逐個候選（`cwd` 為底、`root` 為底）拆成相對於 `root` 的
/// component 鏈，再用 [`trusted_open::open_bound_file`] 逐層 `openat(O_NOFOLLOW)` 打開；成功、是圖片
/// 副檔名的第一個候選就是答案。字面路徑打不開時再試 `%XX` 解碼後的路徑（檔名真的含 `%20` 的照樣讀得到）。
/// 回傳的 [`std::fs::File`] 綁在真正打開當下的那個 inode，呼叫端之後的 `fstat`／讀內容都要用它，不能再用
/// 路徑名字重新 open（issue #89）。
///
/// `root`／`cwd` 各自 canonicalize 一次——這兩個不是每個請求都能換的攻擊面（`root` 是專案路徑，`cwd` 是
/// `bots.cwd`，都是 daemon 自己維護的），用來算「`cwd` 相對 `root` 是哪串 component」；**`requested`
/// 本身完全不 canonicalize**，純粹拆 component（[`trusted_open::safe_relative_components`]），是不是
/// 符號連結、要打開哪個 inode 全部留給 `open_bound_file` 的 `O_NOFOLLOW` walk 決定——不會有「查的時候還
/// 安全、開的時候已經被換掉」的中間步驟。
///
/// 相對路徑先以 `cwd`（bot 的工作目錄）為底、找不到再退回 `root`：child 在 `.claude/worktrees/<名>` 裡
/// 截圖、回覆寫 `![](docs/shots/a.png)`，只看專案根目錄會讀到主樹的同名舊圖（review3 c4 L3）。
pub fn resolve(root: &Path, cwd: Option<&Path>, requested: &str) -> Option<(std::fs::File, &'static str)> {
    let requested = requested.trim();
    let requested = requested.strip_prefix("file://").unwrap_or(requested);
    if requested.is_empty() {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let cwd = cwd.and_then(|c| std::fs::canonicalize(c).ok());

    let mut spellings = vec![requested.to_string()];
    spellings.extend(percent_decode(requested).filter(|d| d != requested));
    for spelled in &spellings {
        let p = Path::new(spelled);
        let chains: Vec<Vec<&OsStr>> = if p.is_absolute() {
            p.strip_prefix(&root).ok().and_then(trusted_open::safe_relative_components).into_iter().collect()
        } else {
            let Some(own) = trusted_open::safe_relative_components(p) else { continue };
            let mut chains = Vec::new();
            if let Some(cwd_rel) =
                cwd.as_deref().and_then(|c| c.strip_prefix(&root).ok()).and_then(trusted_open::safe_relative_components)
            {
                let mut chain = cwd_rel;
                chain.extend(own.iter().copied());
                chains.push(chain);
            }
            chains.push(own);
            chains
        };
        for chain in chains {
            let Some(last) = chain.last() else { continue };
            let Some(mime) = mime_of(Path::new(*last)) else { continue };
            if let Ok(file) = trusted_open::open_bound_file(&root, &chain, None) {
                return Some((file, mime));
            }
        }
    }
    None
}

pub async fn run_in_blocking_pool<T, F>(work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(work).await.ok()
}

pub async fn get_for(app: &impl crate::outbox::OutboxEnv, id: String, q: std::collections::HashMap<String, String>) -> Result<Response, LcError> {
    let not_found = || LcError::NotFound("image".into());
    let requested = q.get("path").ok_or_else(|| LcError::Bad("path required".into()))?;
    let place = app.bot_place(&id).await.map_err(|e| match e {
        // bot 不在與專案不在，對外講法不同（原本的行為）：前者 bot，後者 image；DB 出錯照「不在」處理。
        crate::outbox::BotLookup::BotMissing | crate::outbox::BotLookup::BotUnavailable => LcError::NotFound("bot".into()),
        crate::outbox::BotLookup::ProjectMissing | crate::outbox::BotLookup::ProjectUnavailable => not_found(),
    })?;
    if place.host != crate::config::LOCAL_HOST {
        return Err(not_found());
    }
    let cwd = place.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(Path::new);
    let root = PathBuf::from(place.project_path);
    let cwd = cwd.map(Path::to_path_buf);
    let requested = requested.clone();
    let (file, mime) = run_in_blocking_pool(move || resolve(&root, cwd.as_deref(), &requested))
        .await
        .flatten()
        .ok_or_else(not_found)?;
    let meta = file.metadata().map_err(|_| not_found())?;
    if meta.len() > MAX_BYTES {
        return Err(not_found());
    }
    let read = tokio::task::spawn_blocking(move || trusted_open::read_limited(file, MAX_BYTES)).await.map_err(|_| not_found())?;
    let data = match read {
        Ok(data) => data,
        Err(_) => return Err(not_found()),
    };
    Ok((StatusCode::OK, [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "private, no-cache")], data).into_response())
}
