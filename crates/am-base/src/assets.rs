//! Embedded web UI (M8), behind the default `embed-ui` feature. `allow_missing` keeps a
//! fresh clone compiling before `web/dist` exists (`cargo dev` builds the daemon before
//! any `bun run build`); such a binary answers `/` with a "build the UI first" 404.
//!
//! **重建 UI**：`rust_embed::Embed` 在編譯這個 crate 時把 `web/dist` 讀進二進位；`build.rs` 對 `web/dist`
//! emit `cargo:rerun-if-changed`，所以 `bun run build` 之後照常 `cargo build` 就會重嵌（#1071，舊的 `cargo clean -p am-base`
//! 不用了）。打包時 `scripts/package-dmg.sh` 還會用 `scripts/verify-embedded-ui.sh` 比對一次，不一致就不出包。
//! 驗證方式：`curl -s http://127.0.0.1:7788/ | grep -o 'assets/[^"]*'` 應該和 `web/dist/index.html` 裡的檔名一致。

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

#[cfg(feature = "embed-ui")]
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct WebAssets;

#[cfg(feature = "embed-ui")]
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // `//api/x`、`//ws` 不會進 /api 的 router，但也不能拿 200 的 index.html：
    // 呼叫端（與權限矩陣）會把 2xx 當成路由存在。跟 /api 打錯路徑一樣回 JSON 404。
    if is_api_like(path) {
        return crate::lc_error::LcError::NotFound("route".into()).into_response();
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

    /// `#[folder = "…"]` 的字面值（相對這個 crate 的 Cargo.toml）。
    fn embed_folder() -> &'static str {
        include_str!("assets.rs")
            .lines()
            .find_map(|l| l.trim().strip_prefix("#[folder = \"")?.strip_suffix("\"]"))
            .expect("assets.rs 要有 #[folder = \"…\"]")
    }

    /// `#[folder]` 是相對這個 crate 的 Cargo.toml；檔案從 daemon/ 搬到 crates/am-base/ 時少了一層，
    /// `allow_missing` 讓它安靜地編成一顆沒有前端的 binary（7788 整個 404）。
    #[test]
    fn the_embed_folder_points_at_the_repo_web_directory() {
        let folder = embed_folder();
        let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(folder);
        assert!(dist.ends_with("web/dist"), "{folder}");
        let web = dist.parent().unwrap();
        assert!(web.join("package.json").is_file(), "{} 不是 repo 的 web/：前端不會被嵌進 binary", web.display());
    }

    /// 前端單獨重建時 am-base 要重編（#1071）：cargo 只看 .rs 與依賴，build.rs 得對同一個目錄 emit rerun-if-changed。
    #[test]
    fn the_build_script_watches_the_embedded_folder() {
        let build = include_str!("../build.rs");
        let want = format!("cargo:rerun-if-changed={}", embed_folder());
        assert!(build.contains(&want), "build.rs 要有 `{want}`，不然前端重建後 binary 還是舊的");
    }
}
