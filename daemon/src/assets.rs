//! Embedded web UI (M8). Enabled by the `embed-ui` cargo feature so the daemon still
//! builds before `web/dist` exists.

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
