//! Embedded web UI (M8), behind the default `embed-ui` feature. `allow_missing` keeps a
//! fresh clone compiling before `web/dist` exists (`cargo dev` builds the daemon before
//! any `bun run build`); such a binary answers `/` with a "build the UI first" 404.
//!
//! **重建 UI 時要注意**：`rust_embed::Embed` 是巨集，在編譯這個檔案的當下把 `web/dist` 讀進
//! 二進位裡，但它沒有 build script 去 emit `cargo:rerun-if-changed`——cargo 只看得到 `.rs`
//! 檔的內容有沒有變。所以 `npm run build` 之後單純 `cargo build --release` **不會**重嵌前端
//! （`touch` 這個檔也沒用，cargo 比的是內容不是 mtime），跑起來的 daemon 還是送舊的
//! `assets/index-*.js`。要換前端請用：
//!
//! ```sh
//! cd web && npm run build && cd ..
//! cargo clean -p agents-managerd --release   # 或改動這個檔的內容
//! cargo build --release -p agents-managerd
//! ```
//!
//! 驗證方式：`curl -s http://127.0.0.1:7788/ | grep -o 'assets/[^"]*'` 應該和
//! `web/dist/index.html` 裡的檔名一致。

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

#[cfg(feature = "embed-ui")]
#[derive(rust_embed::Embed)]
#[folder = "../web/dist"]
#[allow_missing = true]
struct WebAssets;

#[cfg(feature = "embed-ui")]
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // `//api/x`、`//ws` 不會進 /api 的 router，但也不能拿 200 的 index.html：
    // 呼叫端（與權限矩陣）會把 2xx 當成路由存在。跟 /api 打錯路徑一樣回 JSON 404。
    if is_api_like(path) {
        return crate::lifecycle::LcError::NotFound("route".into()).into_response();
    }
    let candidate = if path.is_empty() { "index.html" } else { path };
    match WebAssets::get(candidate) {
        Some(f) => {
            let mime = mime_guess::from_path(candidate).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], f.data.into_owned()).into_response()
        }
        None => match WebAssets::get("index.html") {
            // SPA fallback
            Some(f) => ([(header::CONTENT_TYPE, "text/html")], f.data.into_owned()).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                "web UI not embedded: web/dist was missing when agents-managerd was compiled; \
                 run `cd web && bun install && bun run build`, then rebuild the daemon",
            )
                .into_response(),
        },
    }
}

/// 去掉前導 `/` 之後是 `api`、`api/…`、`ws`、`ws/…`：這是打 API 的路徑，不是前端頁面。
fn is_api_like(path: &str) -> bool {
    ["api", "ws"].iter().any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

/// 嵌入的 `web/dist` 裡的一個檔（分享入口用，`share::portal`）。沒嵌或沒有這個檔＝`None`，**沒有** SPA fallback。
#[cfg(feature = "embed-ui")]
pub fn embedded(path: &str) -> Option<Vec<u8>> {
    WebAssets::get(path).map(|f| f.data.into_owned())
}

#[cfg(not(feature = "embed-ui"))]
pub fn embedded(_path: &str) -> Option<Vec<u8>> {
    None
}

#[cfg(not(feature = "embed-ui"))]
pub async fn serve(_uri: Uri) -> Response {
    let _ = header::CONTENT_TYPE;
    (
        StatusCode::NOT_FOUND,
        "agents-managerd was built without the `embed-ui` feature; run the Vite dev server instead",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn api_shaped_paths_never_fall_back_to_the_spa() {
        for p in ["api", "api/intents", "ws", "ws/x"] {
            assert!(super::is_api_like(p), "{p}");
        }
        for p in ["", "index.html", "assets/app.js", "apix", "wsx/y", "bots/api"] {
            assert!(!super::is_api_like(p), "{p}");
        }
    }
}
