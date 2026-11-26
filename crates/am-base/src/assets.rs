//! Embedded web UI (M8), behind the default `embed-ui` feature. Release builds embed the files;
//! debug and test builds read them at runtime. `allow_missing` keeps a fresh clone compiling before
//! `web/dist` exists; such a binary answers `/` with a "build the UI first" 404.
//!
//! **重建 UI**：release profile 編譯這個 crate 時，`rust_embed::Embed` 會把 `web/dist` 讀進二進位；`build.rs` 只在這些 profile
//! 對目錄 emit `cargo:rerun-if-changed`，所以 `bun run build` 之後照常 `cargo build --release` 就會重嵌（#1071，舊的
//! `cargo clean -p am-base` 不用了）。debug／test profile 執行期讀檔，不會因前端目錄更新重編（issue #1333）。打包時
//! `scripts/package-dmg.sh` 還會用 `scripts/verify-embedded-ui.sh` 比對一次，不一致就不出包。
//! 驗證方式：`curl -s http://127.0.0.1:7788/ | grep -o 'assets/[^"]*'` 應該和 `web/dist/index.html` 裡的檔名一致。
//!
//! **傳輸**：文字類檔案依 `Accept-Encoding` 回 br（優先）或 gzip，第一次用到時壓一次、之後快取（鍵含內容的 sha256，
//! debug 讀檔改了也不會拿到舊的）。`ETag` 取嵌入檔的 sha256；`/assets/*` 是 hash 檔名，`immutable` 一年；其餘（`index.html`、
//! `sw.js`、SPA fallback、`share.html`）`no-cache`，靠 `ETag`／`If-None-Match` 回 304。

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

#[cfg(feature = "embed-ui")]
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct WebAssets;

use axum::http::{HeaderMap, HeaderValue};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

/// 文字類回應的壓縮編碼（`Accept-Encoding` 認得的兩種）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    Brotli,
    Gzip,
}

impl Encoding {
    fn token(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Gzip => "gzip",
        }
    }
}

/// 太小的檔案壓了不划算。
const MIN_COMPRESS_BYTES: usize = 512;
/// `/assets/…` 是 hash 檔名：內容變了檔名就變，可以長快取。
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// 其餘檔案每次回來都驗證（`ETag`），沒變就 304。
const REVALIDATE: &str = "no-cache";

/// `Accept-Encoding` 裡客戶端能收、而且沒有被 `q=0` 拒絕的編碼；br 優先。沒有＝`None`（原文回）。
pub fn accepted_encoding(headers: &HeaderMap) -> Option<Encoding> {
    let value = headers.get(header::ACCEPT_ENCODING)?.to_str().ok()?;
    let (mut brotli, mut gzip) = (false, false);
    for item in value.split(',') {
        let mut parts = item.split(';');
        let name = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        let refused = parts.any(|p| p.trim().strip_prefix("q=").and_then(|q| q.trim().parse::<f32>().ok()).is_some_and(|q| q <= 0.0));
        if refused {
            continue;
        }
        match name.as_str() {
            "br" => brotli = true,
            "gzip" => gzip = true,
            _ => {}
        }
    }
    if brotli {
        Some(Encoding::Brotli)
    } else if gzip {
        Some(Encoding::Gzip)
    } else {
        None
    }
}

/// 只壓文字類；圖、字型本身已經壓過，再壓只會浪費 CPU。
fn compressible(mime: &str) -> bool {
    mime.starts_with("text/") || matches!(mime, "application/javascript" | "application/json" | "image/svg+xml")
}

fn compress(data: &[u8], encoding: Encoding) -> Option<Vec<u8>> {
    match encoding {
        Encoding::Brotli => {
            let mut out = Vec::new();
            {
                let mut w = brotli::CompressorWriter::new(&mut out, 4096, 9, 22);
                w.write_all(data).ok()?;
                w.flush().ok()?;
            }
            Some(out)
        }
        Encoding::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(9));
            e.write_all(data).ok()?;
            e.finish().ok()
        }
    }
}

type CacheKey = (String, [u8; 32], Encoding);

fn cache() -> &'static Mutex<HashMap<CacheKey, Arc<[u8]>>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Arc<[u8]>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// 壓縮後的內容：第一次壓、之後從快取拿。壓失敗＝`None`，呼叫端改回原文。
fn encoded(path: &str, hash: [u8; 32], data: &[u8], encoding: Encoding) -> Option<Arc<[u8]>> {
    let key = (path.to_string(), hash, encoding);
    if let Some(hit) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Some(hit.clone());
    }
    let out: Arc<[u8]> = compress(data, encoding)?.into();
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(key, out.clone());
    Some(out)
}

fn if_none_match(req: &HeaderMap, etag: &str) -> bool {
    req.get_all(header::IF_NONE_MATCH).iter().filter_map(|v| v.to_str().ok()).any(|v| {
        v.split(',').map(str::trim).any(|t| t == "*" || t == etag || t.strip_prefix("W/") == Some(etag))
    })
}

/// 一個嵌入檔的回應：依 `Accept-Encoding` 壓縮、`ETag`（嵌入檔的 sha256）、快取標頭、`Vary`；`If-None-Match` 相符回 304。
pub fn file_response(path: &str, data: &[u8], hash: [u8; 32], req: &HeaderMap) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
    let hex: String = hash[..16].iter().map(|b| format!("{b:02x}")).collect();
    let cache_control = if path.starts_with("assets/") { IMMUTABLE } else { REVALIDATE };
    let encoding = if compressible(&mime) && data.len() >= MIN_COMPRESS_BYTES { accepted_encoding(req) } else { None };
    let compressed = encoding.and_then(|e| encoded(path, hash, data, e).map(|body| (e, body)));
    let (etag, body) = match &compressed {
        Some((e, body)) => (format!("\"{hex}-{}\"", e.token()), Some(body.clone())),
        None => (format!("\"{hex}\""), None),
    };
    let mut res = if if_none_match(req, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        match &body {
            Some(b) => b.to_vec().into_response(),
            None => data.to_vec().into_response(),
        }
    };
    let not_modified = res.status() == StatusCode::NOT_MODIFIED;
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if not_modified {
        return res;
    }
    if let Ok(v) = HeaderValue::from_str(&mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Some((e, _)) = &compressed {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(e.token()));
    }
    res
}

#[cfg(feature = "embed-ui")]
pub async fn serve(uri: Uri, req: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    if is_api_like(path) {
        return crate::lc_error::LcError::NotFound("route".into()).into_response();
    }
    let candidate = if path.is_empty() { "index.html" } else { path };
    match WebAssets::get(candidate) {
        Some(f) => file_response(candidate, &f.data, f.metadata.sha256_hash(), &req),
        None => match WebAssets::get("index.html") {
            Some(f) => file_response("index.html", &f.data, f.metadata.sha256_hash(), &req),
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

/// 嵌入的 `web/dist` 裡的一個檔（分享入口用，`share::portal`）的回應。沒嵌或沒有這個檔＝`None`，**沒有** SPA fallback。
#[cfg(feature = "embed-ui")]
pub fn embedded_response(path: &str, req: &HeaderMap) -> Option<Response> {
    WebAssets::get(path).map(|f| file_response(path, &f.data, f.metadata.sha256_hash(), req))
}

#[cfg(not(feature = "embed-ui"))]
pub fn embedded_response(_path: &str, _req: &HeaderMap) -> Option<Response> {
    None
}

#[cfg(not(feature = "embed-ui"))]
pub async fn serve(_uri: Uri, _req: HeaderMap) -> Response {
    let _ = header::CONTENT_TYPE;
    (
        StatusCode::NOT_FOUND,
        "agents-managerd was built without the `embed-ui` feature; run the Vite dev server instead",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::{header, StatusCode};

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

    #[test]
    fn the_build_script_only_watches_in_profiles_that_embed() {
        let build = include_str!("../build.rs");
        assert!(build.contains("cfg!(debug_assertions)"), "前端目錄只有 release 嵌入時才要監看");
        let profile_guard = build.find("if !cfg!(debug_assertions)").expect("build.rs 要只在非 debug profile 監看");
        let watch = build.find("rerun-if-changed=../../web/dist").expect("build.rs 要保留嵌入目錄的檔案監看");
        assert!(profile_guard < watch, "web/dist 的監看指令必須受 profile 條件包住");
    }

    fn headers(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), axum::http::HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn js(n: usize) -> Vec<u8> {
        "export const value = 'abc';\n".repeat(n).into_bytes()
    }

    async fn bytes(res: axum::response::Response) -> Vec<u8> {
        axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()
    }

    #[test]
    fn accept_encoding_prefers_brotli_and_honours_q_zero() {
        use super::{accepted_encoding, Encoding};
        assert_eq!(accepted_encoding(&headers(&[("accept-encoding", "gzip, deflate, br")])), Some(Encoding::Brotli));
        assert_eq!(accepted_encoding(&headers(&[("accept-encoding", "gzip, deflate")])), Some(Encoding::Gzip));
        assert_eq!(accepted_encoding(&headers(&[("accept-encoding", "br;q=0, gzip")])), Some(Encoding::Gzip));
        assert_eq!(accepted_encoding(&headers(&[("accept-encoding", "gzip;q=0")])), None);
        assert_eq!(accepted_encoding(&headers(&[("accept-encoding", "identity")])), None);
        assert_eq!(accepted_encoding(&headers(&[])), None);
    }

    #[test]
    fn both_encodings_round_trip_to_the_original_bytes() {
        use std::io::Read;
        let data = js(200);
        let br = super::compress(&data, super::Encoding::Brotli).unwrap();
        let mut out = Vec::new();
        brotli::Decompressor::new(&br[..], 4096).read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
        assert!(br.len() * 10 < data.len(), "文字要真的變小：{} → {}", data.len(), br.len());
        let gz = super::compress(&data, super::Encoding::Gzip).unwrap();
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn an_asset_is_brotli_with_an_immutable_etag_and_vary() {
        let data = js(200);
        let res = super::file_response("assets/main-abc.js", &data, [9u8; 32], &headers(&[("accept-encoding", "br, gzip")]));
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_ENCODING], "br");
        assert_eq!(res.headers()[header::CACHE_CONTROL], super::IMMUTABLE);
        assert_eq!(res.headers()[header::VARY], "Accept-Encoding");
        let etag = res.headers()[header::ETAG].to_str().unwrap().to_string();
        assert!(etag.ends_with("-br\""), "{etag}");
        assert_eq!(res.headers()[header::CONTENT_TYPE].to_str().unwrap().split(';').next(), Some("text/javascript"));
        let body = bytes(res).await;
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut brotli::Decompressor::new(&body[..], 4096), &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn the_shell_revalidates_and_a_matching_etag_gets_a_304() {
        let data = b"<!doctype html><html></html>".repeat(40);
        let first = super::file_response("index.html", &data, [1u8; 32], &headers(&[]));
        assert_eq!(first.headers()[header::CACHE_CONTROL], "no-cache");
        assert!(first.headers().get(header::CONTENT_ENCODING).is_none(), "沒宣告 Accept-Encoding 就原文");
        let etag = first.headers()[header::ETAG].to_str().unwrap().to_string();
        let again = super::file_response("index.html", &data, [1u8; 32], &headers(&[("if-none-match", etag.as_str())]));
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(again.headers()[header::ETAG], etag.as_str());
        assert!(bytes(again).await.is_empty());
        let changed = super::file_response("index.html", &data, [2u8; 32], &headers(&[("if-none-match", etag.as_str())]));
        assert_eq!(changed.status(), StatusCode::OK, "內容變了（hash 不同）就給新的");
    }

    #[test]
    fn small_and_binary_files_are_served_as_they_are() {
        let small = js(1);
        let res = super::file_response("assets/tiny.js", &small, [3u8; 32], &headers(&[("accept-encoding", "br")]));
        assert!(res.headers().get(header::CONTENT_ENCODING).is_none());
        let png = vec![7u8; 4096];
        let res = super::file_response("assets/logo.png", &png, [4u8; 32], &headers(&[("accept-encoding", "br")]));
        assert!(res.headers().get(header::CONTENT_ENCODING).is_none(), "圖不再壓");
        assert_eq!(res.headers()[header::CACHE_CONTROL], super::IMMUTABLE);
    }

    #[test]
    fn the_compressed_body_is_cached_per_content_and_encoding() {
        let data = js(200);
        let a = super::encoded("assets/x.js", [5u8; 32], &data, super::Encoding::Brotli).unwrap();
        let b = super::encoded("assets/x.js", [5u8; 32], &data, super::Encoding::Brotli).unwrap();
        assert!(std::sync::Arc::ptr_eq(&a, &b), "同一份內容只壓一次");
        let c = super::encoded("assets/x.js", [6u8; 32], &data, super::Encoding::Brotli).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&a, &c), "內容（hash）變了就重壓");
    }
}
