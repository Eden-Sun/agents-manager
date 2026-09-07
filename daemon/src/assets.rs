//! Embedded web UI (M8). Enabled by the `embed-ui` cargo feature so the daemon still
//! builds before `web/dist` exists.
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
struct WebAssets;

#[cfg(feature = "embed-ui")]
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let candidate = if path.is_empty() { "index.html" } else { path };
    match WebAssets::get(candidate) {
        Some(f) => {
            let mime = mime_guess::from_path(candidate).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], f.data.into_owned()).into_response()
        }
        None => match WebAssets::get("index.html") {
            // SPA fallback
            Some(f) => ([(header::CONTENT_TYPE, "text/html")], f.data.into_owned()).into_response(),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
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
