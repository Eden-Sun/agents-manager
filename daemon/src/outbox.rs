//! 給使用者的輸出目錄（SPEC §6.5f）：`<data_dir>/outbox/<bot_id>/`，bot 的 pane 拿到 `AM_OUTBOX` 指到這裡。
//!
//! 使用者 2026-09-16 裁示：scratchpad 暴露了私鑰與正式 DB 複本之後，**scratchpad 不再是輸出目錄**。
//! 要交給使用者的檔案一律放 outbox；只保留 [`TTL_SECS`]，清理由 AGM 的 launchd `com.agm.outbox-gc`
//! 每 10 分鐘做一次（依 mtime 刪超過一小時的檔、收空目錄）——daemon 不清。
//!
//! 空目錄會被那支清理收掉，所以 daemon 在 bot 啟動時建的目錄不保證還在：bot 寫檔前自己 `mkdir -p "$AM_OUTBOX"`。
//!
//! 網頁「檔案暫存」下半段的列表與下載**只讀這裡**，完全不碰 scratchpad。下載一律當附件
//! （`Content-Disposition: attachment` + `nosniff`，白名單外 `application/octet-stream`），
//! 路徑解開符號連結後必須仍在這顆 bot 的 outbox 裡；私鑰／憑證／DB／隱藏檔照樣不列不給（[`withheld`]）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::lifecycle::LcError;
use crate::state::App;

/// 檔案在 outbox 裡保留多久（AGM 清理的門檻，跟 `outbox-gc.sh` 的 `MAX_AGE_MIN=60` 同一個數）。
pub(crate) const TTL_SECS: u64 = 3600;
/// 一次最多列這麼多（新的排前面）。
const MAX_ENTRIES: usize = 300;
/// 單檔下載上限：整份先讀進記憶體，瀏覽器那端也要收得下。
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// 這顆 bot 的 outbox。bot id 會拼進路徑：只收英數（ULID），其他一律不給。
pub(crate) fn dir_for(data_dir: &Path, bot_id: &str) -> Option<PathBuf> {
    if bot_id.is_empty() || !bot_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(data_dir.join("outbox").join(bot_id))
}

/// bot 啟動時把目錄建好。建不起來只記 warning：少一個目錄不該讓 bot 起不來，bot 寫之前本來就要 `mkdir -p`。
pub(crate) fn ensure(data_dir: &Path, bot_id: &str) -> Option<PathBuf> {
    let dir = dir_for(data_dir, bot_id)?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(bot = bot_id, dir = %dir.display(), error = %e, "could not create the bot's outbox");
    }
    Some(dir)
}

/// 不列、不給下載的檔案（2a96096 的黑名單，在 outbox 上保留當第二道：規則本來就禁止放這些，放了也拿不走）。
/// 先看名字（隱藏檔、資料庫與它的旁檔、金鑰與憑證），名字看不出來的再看開頭幾個位元組（改過副檔名的 SQLite、PEM 私鑰）。
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

/// 目錄裡可以列出來的檔案：第一層的一般檔案（符號連結、子目錄不列），不是 [`withheld`]，新的排前面。
/// 每個帶 `expires_at`（mtime + [`TTL_SECS`]，AGM 清理看的也是 mtime）與 `remaining_secs`（到期了是 0，
/// 清理每 10 分鐘才跑一次，所以 0 的檔案還會在清單上待一下）。
pub(crate) fn scan(dir: &Path, now: u64) -> Vec<serde_json::Value> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() || withheld(&e.path()) {
                continue;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            files.push((e.file_name().to_string_lossy().into_owned(), meta.len(), modified));
        }
    }
    files.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    files.truncate(MAX_ENTRIES);
    files
        .into_iter()
        .map(|(name, size, modified)| {
            let expires_at = modified + TTL_SECS;
            json!({"name": name, "size": size, "modified": modified, "expires_at": expires_at, "remaining_secs": expires_at.saturating_sub(now)})
        })
        .collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 這顆 bot 的 outbox。遠端主機的 bot 沒有（檔案在那台機器上）。
async fn outbox_of(app: &Arc<App>, bot_id: &str) -> Result<PathBuf, LcError> {
    let bot = crate::db::bot(&app.db, bot_id).await.ok().flatten().ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project = crate::db::project(&app.db, &bot.project_id).await.ok().flatten().ok_or_else(|| LcError::NotFound("bot".into()))?;
    if project.host != crate::config::LOCAL_HOST {
        return Err(LcError::conflict("outbox_remote", json!({"reason": "outbox_remote", "host": project.host})));
    }
    dir_for(&app.data_dir, &bot.id).ok_or_else(|| LcError::NotFound("bot".into()))
}

/// `GET /api/bots/{id}/outbox` — 這顆 bot 交給使用者的檔案（新的排前面），各自還剩多久被清掉。
pub async fn list(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Result<Response, LcError> {
    let dir = match outbox_of(&app, &id).await {
        Ok(d) => d,
        // 遠端不是錯誤，是常態：回空清單並說明原因，前端不用畫成紅字。
        Err(LcError::Conflict(detail)) => {
            return Ok((StatusCode::OK, axum::Json(json!({"files": [], "ttl_secs": TTL_SECS, "reason": detail.get("reason")}))).into_response())
        }
        Err(e) => return Err(e),
    };
    // 讀目錄、讀檔頭是同步的：別卡在 async worker 上。目錄不在（還沒寫過、被清理收掉）就是空清單。
    let scan_dir = dir.clone();
    let files = tokio::task::spawn_blocking(move || scan(&scan_dir, now_secs())).await.unwrap_or_default();
    Ok((StatusCode::OK, axum::Json(json!({"dir": dir.to_string_lossy(), "ttl_secs": TTL_SECS, "files": files}))).into_response())
}

/// `GET /api/bots/{id}/outbox/file?path=…` — 一律當附件下載。
pub async fn file(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, LcError> {
    let not_found = || LcError::NotFound("file".into());
    let requested = q.get("path").ok_or_else(|| LcError::Bad("path required".into()))?;
    let dir = outbox_of(&app, &id).await?;
    let path = servable(&dir, requested).ok_or_else(not_found)?;
    let meta = tokio::fs::metadata(&path).await.map_err(|_| not_found())?;
    if meta.len() > MAX_BYTES {
        return Err(LcError::conflict("file_too_large", json!({"reason": "file_too_large", "size": meta.len(), "max": MAX_BYTES})));
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

/// 舊的 `/bots/{id}/scratchpad*`：scratchpad 不再給使用者（使用者 2026-09-16 裁示）。明確回 404，
/// 不讓它掉進 SPA fallback 回一頁 HTML 200，舊分頁也看得出「這條路沒了」。
pub async fn scratchpad_gone() -> LcError {
    LcError::NotFound("scratchpad".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bot_id_is_the_only_thing_that_picks_the_directory() {
        let data = Path::new("/data");
        assert_eq!(dir_for(data, "01M2MC36YBQZWXCQN83RKD61TE"), Some(PathBuf::from("/data/outbox/01M2MC36YBQZWXCQN83RKD61TE")));
        for bad in ["", "..", "../x", "a/b", "b1 ", "b.1"] {
            assert_eq!(dir_for(data, bad), None, "{bad:?} 不能拼進路徑");
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("am-outbox-{tag}-{}", crate::db::ulid()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::canonicalize(base).unwrap()
    }

    /// outbox 裡的檔案（相對或絕對）放行；`..`、指到外面的符號連結、目錄、不存在都擋。
    #[test]
    fn only_files_inside_the_outbox_resolve() {
        let base = scratch("resolve");
        let root = base.join("outbox");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("tracking.tsv"), b"a\tb\n").unwrap();
        std::fs::write(root.join("sub/report.md"), b"# hi").unwrap();
        std::fs::write(base.join("outside.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(base.join("outside.txt"), root.join("link.txt")).unwrap();

        assert!(servable(&root, "tracking.tsv").is_some());
        assert!(servable(&root, "sub/report.md").is_some(), "子目錄也算在裡面");
        assert!(servable(&root, root.join("tracking.tsv").to_str().unwrap()).is_some(), "絕對路徑但在裡面");
        assert!(servable(&root, "../outside.txt").is_none(), "用 .. 逃出去");
        assert!(servable(&root, base.join("outside.txt").to_str().unwrap()).is_none(), "絕對路徑在外面");
        assert!(servable(&root, "link.txt").is_none(), "符號連結指到外面");
        assert!(servable(&root, "sub").is_none(), "目錄不是檔案");
        assert!(servable(&root, "missing.tsv").is_none());
        assert!(servable(&root, "  ").is_none());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 資料庫、金鑰、隱藏檔不管名字怎麼取都不列、不給下載；一般檔案照常。
    #[test]
    fn databases_and_keys_are_never_listed_or_served() {
        let base = scratch("withheld");
        let root = base.join("outbox");
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

        let listed: Vec<String> = scan(&root, 0).iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(listed, vec!["report.md".to_string()], "只剩一般檔案");
        assert!(servable(&root, "report.md").is_some());
        for p in ["migrate-check.sqlite3", "bak1644.db", "bak1644.db-wal", "agents-manager.sqlite3.bak-20260916", "innocent.bin", "server.pem", "notes.txt", "id_ed25519", "prod.env", ".env", ".secret/plain.txt"] {
            assert!(servable(&root, p).is_none(), "{p} 不能下載");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 每個檔案帶到期時間與剩餘秒數：mtime + 1 小時，過了是 0（不是負數，也不會溢位）。
    #[test]
    fn every_listed_file_says_how_long_it_has_left() {
        let base = scratch("ttl");
        std::fs::write(base.join("fresh.txt"), b"x").unwrap();
        std::fs::create_dir_all(base.join("folder")).unwrap();
        let files = scan(&base, 0);
        assert_eq!(files.len(), 1, "子目錄不列：{files:?}");
        let modified = files[0]["modified"].as_u64().unwrap();
        assert!(modified > 0);
        assert_eq!(files[0]["expires_at"], json!(modified + TTL_SECS));
        assert_eq!(scan(&base, modified + 600)[0]["remaining_secs"], json!(TTL_SECS - 600), "放了十分鐘剩五十分鐘");
        assert_eq!(scan(&base, modified + TTL_SECS + 1)[0]["remaining_secs"], json!(0), "過期是 0，等清理");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 下載一律是附件，而且中文檔名帶得回去。
    #[test]
    fn every_download_is_an_attachment_with_a_usable_filename() {
        assert_eq!(mime_of(Path::new("a.tsv")), "text/plain; charset=utf-8");
        assert_eq!(mime_of(Path::new("a.pdf")), "application/pdf");
        // 白名單以外一律 octet-stream：bot 寫出來的 HTML 不在這個 origin 跑起來。
        assert_eq!(mime_of(Path::new("evil.html")), "application/octet-stream");
        assert_eq!(mime_of(Path::new("evil.svg")), "application/octet-stream");
        assert_eq!(mime_of(Path::new("noext")), "application/octet-stream");
        let d = content_disposition("出貨追蹤 v2.tsv");
        assert!(d.starts_with("attachment; "), "{d}");
        assert!(d.contains("%E5%87%BA"), "中文要 percent-encode：{d}");
        assert!(!d.contains("出貨"), "ASCII 的那份不能夾原字元：{d}");
        assert!(content_disposition("a\"; rm -rf /.txt").contains("filename=\"a__"), "引號不能逃出去");
    }

    async fn body(resp: Response) -> (StatusCode, Vec<u8>) {
        let status = resp.status();
        (status, axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec())
    }

    async fn get_file(app: &Arc<App>, bot: &str, path: &str) -> (StatusCode, Vec<u8>) {
        let q = Query([("path".to_string(), path.to_string())].into_iter().collect());
        match file(State(app.clone()), UrlPath(bot.to_string()), q).await {
            Ok(r) => body(r).await,
            Err(e) => body(e.into_response()).await,
        }
    }

    /// 端到端：列表與下載只看 outbox。bot 的 scratchpad 就算有檔案、就算從 outbox 用符號連結指過去、
    /// 就算直接給絕對路徑，一律 404；舊的 scratchpad 路徑也是 404。
    #[tokio::test]
    async fn the_endpoints_serve_the_outbox_and_never_the_scratchpad() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let outbox = ensure(&env.app.data_dir, &bot.id).unwrap();
        std::fs::write(outbox.join("report.md"), b"# for you").unwrap();
        std::fs::write(outbox.join("dump.sqlite3"), b"SQLite format 3\0").unwrap();
        let scratchpad = env.dir.join("claude-501/-slug/0004cea2-a8cd-4c0c-aeb5-6ddaa7fd480c/scratchpad");
        std::fs::create_dir_all(&scratchpad).unwrap();
        std::fs::write(scratchpad.join("w1.py"), b"print(1)").unwrap();
        std::os::unix::fs::symlink(scratchpad.join("w1.py"), outbox.join("w1.py")).unwrap();

        let (status, bytes) = body(list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let names: Vec<&str> = v["files"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["report.md"], "符號連結與 DB 都不列：{v}");
        assert_eq!(v["ttl_secs"], json!(TTL_SECS));
        assert!(v["files"][0]["remaining_secs"].as_u64().unwrap() > TTL_SECS - 60);

        let (status, bytes) = get_file(&env.app, &bot.id, "report.md").await;
        assert_eq!((status, bytes.as_slice()), (StatusCode::OK, &b"# for you"[..]));
        for p in ["w1.py", scratchpad.join("w1.py").to_str().unwrap(), "../../../claude-501", "dump.sqlite3"] {
            assert_eq!(get_file(&env.app, &bot.id, p).await.0, StatusCode::NOT_FOUND, "{p}");
        }
        assert_eq!(scratchpad_gone().await.into_response().status(), StatusCode::NOT_FOUND);

        // outbox 還沒建（或被清理收掉了）：空清單，不是錯誤。
        std::fs::remove_dir_all(&outbox).unwrap();
        let (status, bytes) = body(list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["files"], json!([]));
    }

    /// 遠端主機的 bot：清單回空＋原因，下載不給。
    #[tokio::test]
    async fn a_remote_bot_has_no_outbox_here() {
        let env = crate::testing::env().await;
        let pid = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r', 'r', 'box', ?)")
            .bind(&pid)
            .bind(crate::db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = crate::testing::claude_bot(&env.app, &pid, "remote").await;
        let (status, bytes) = body(list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!((v["files"].clone(), v["reason"].clone()), (json!([]), json!("outbox_remote")));
        assert_ne!(get_file(&env.app, &bot.id, "x.txt").await.0, StatusCode::OK);
        assert_eq!(get_file(&env.app, "nope", "x.txt").await.0, StatusCode::NOT_FOUND);
    }
}
