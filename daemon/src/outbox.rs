//! 給使用者的輸出目錄（SPEC §6.5f）：`<data_dir>/outbox/<bot_id>/`，bot 的 pane 拿到 `AM_OUTBOX` 指到這裡。
//!
//! 使用者 2026-09-16 裁示：scratchpad 暴露了私鑰與正式 DB 複本之後，**scratchpad 不再是輸出目錄**。
//! 要交給使用者的檔案一律放 outbox；只保留 [`TTL_SECS`]，清理由 AGM 的 launchd `com.agm.outbox-gc`
//! 每 10 分鐘做一次（mtime 與 ctime 都超過一小時才刪、收空目錄）——daemon 不清。
//!
//! 空目錄會被那支清理收掉，所以 daemon 在 bot 啟動時建的目錄不保證還在：bot 寫檔前自己 `mkdir -p "$AM_OUTBOX"`。
//!
//! 網頁「檔案暫存」下半段的列表與下載**只讀這裡**，完全不碰 scratchpad。下載一律當附件
//! （`Content-Disposition: attachment` + `nosniff`，白名單外 `application/octet-stream`），
//! 路徑解開符號連結後必須仍在這顆 bot 的 outbox 裡；私鑰／憑證／DB／隱藏檔照樣不列不給（[`withheld`]）。
//!
//! **下載（`file()`）與列表（`list()`）都用 [`crate::trusted_open`]**：從 `data_dir` 開始逐層
//! `openat(O_NOFOLLOW)` 一路開到要的檔案／目錄（`outbox` 與 `<bot_id>` 兩段的符號連結／擁有者檢查、
//! 隱藏路徑、下面任一段被換成符號連結，全部是同一次系統呼叫鏈擋下來），拿到的 fd 直接拿去
//! `fstat`／讀內容，不再用路徑名字重新 open（issue #89）。`list()`（[`scan`]）過去只有「這個目錄
//! 可不可信」的檢查是 fd-bound，真正列舉那一步仍是路徑 `read_dir`——檢查通過之後、列舉之前，這顆
//! bot 自己能把整個目錄換成指到界線外的符號連結，讓清單改列出界線外的檔名／大小。現在改成
//! `open_trusted_dir` 拿到的目錄 fd 直接交給 [`trusted_open::read_dir_bound`]（`fdopendir`／
//! `readdir`／`fstatat`），連內容判斷（[`content_is_withheld`]）也用同一個 fd 底下的
//! `trusted_open::open_entry_in` 打開，全程不再用任何路徑重新解析（issue #96，docs/SPEC.md §6.5f）。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::lifecycle::LcError;
use crate::trusted_open;


/// 分享／outbox 幾乎所有動作只需要的兩樣：資料目錄與 DB 連線（`App` 在 `app_ports_p10` 實作）。
pub trait ShareStorage: Send + Sync + 'static {
    fn data_dir(&self) -> &Path;
    fn db_pool(&self) -> &sqlx::SqlitePool;
}

impl<T: ShareStorage + ?Sized> ShareStorage for Arc<T> {
    fn data_dir(&self) -> &Path {
        (**self).data_dir()
    }
    fn db_pool(&self) -> &sqlx::SqlitePool {
        (**self).db_pool()
    }
}

/// 查不到一顆 bot 的位置：確實沒有（bot 或專案不在）／查詢本身失敗；分 bot 與專案兩步，呼叫端各自決定對外怎麼講（原本各處的講法不同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotLookup {
    /// bot 不在。
    BotMissing,
    /// 查 bot 的那一步 DB 出錯。
    BotUnavailable,
    /// bot 在、專案不在。
    ProjectMissing,
    /// 查專案的那一步 DB 出錯。
    ProjectUnavailable,
}

/// 一顆 bot 住在哪：專案所在主機、專案路徑、bot 自己的工作目錄。
#[derive(Debug, Clone)]
pub struct BotPlace {
    pub host: String,
    pub project_path: String,
    pub cwd: Option<String>,
}

/// outbox／本機圖片下載要先確認「這顆 bot 在本機嗎」所需的查詢（`App` 在 `app_ports_p10` 實作，底層是 `db::bot`＋`db::project`）。
pub trait OutboxEnv: ShareStorage {
    fn bot_place(&self, bot_id: &str) -> impl std::future::Future<Output = Result<BotPlace, BotLookup>> + Send;
}

impl<T: OutboxEnv + ?Sized> OutboxEnv for Arc<T> {
    fn bot_place(&self, bot_id: &str) -> impl std::future::Future<Output = Result<BotPlace, BotLookup>> + Send {
        (**self).bot_place(bot_id)
    }
}

/// 檔案在 outbox 裡保留多久（AGM 清理的門檻，跟 `outbox-gc.sh` 的 `MAX_AGE_MIN=60` 同一個數）。
pub(crate) const TTL_SECS: u64 = 3600;
/// 一次最多列這麼多（新的排前面）。
pub(crate) const MAX_ENTRIES: usize = 300;
/// 單檔下載上限：整份先讀進記憶體，瀏覽器那端也要收得下。
pub(crate) const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// 這顆 bot 的 outbox。bot id 會拼進路徑：只收英數（ULID），其他一律不給。
pub(crate) fn dir_for(data_dir: &Path, bot_id: &str) -> Option<PathBuf> {
    if bot_id.is_empty() || !bot_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(data_dir.join("outbox").join(bot_id))
}

/// 分享用 bot（SPEC §20）的 outbox 標記檔：AGM 的 `outbox-gc.sh` 看到它就整個目錄不清（使用者 2026-10-04：end user 是外部的人，
/// 隔天才回來拿檔是常態）。點開頭，清單本來就不列（[`withheld_name`]）。
pub(crate) const SHARE_KEEP_MARK: &str = ".am-share-keep";

/// 替分享用 bot 的 outbox 放 [`SHARE_KEEP_MARK`]（啟動、建立、daemon 開機時都補一次；bot 自己刪掉也會被補回）。
/// 寫不起來只記 warning：少了標記只是檔案照一般的 1 小時清，不該讓 bot 起不來。
pub(crate) fn mark_share_keep(data_dir: &Path, bot_id: &str) {
    let Some(dir) = ensure(data_dir, bot_id) else { return };
    let mark = dir.join(SHARE_KEEP_MARK);
    if std::fs::symlink_metadata(&mark).is_ok() {
        return;
    }
    if let Err(e) = std::fs::write(&mark, b"share bot outbox: kept by outbox-gc.sh (SPEC 20)\n") {
        tracing::warn!(bot = bot_id, dir = %dir.display(), error = %e, "could not mark the share bot's outbox as kept");
    }
}

/// bot 或專案刪掉了：拿掉標記，outbox 回到一般的 1 小時清（不然刪掉的分享用 bot 的檔永遠留著）。還原後啟動時再補。
pub(crate) fn unmark_share_keep(data_dir: &Path, bot_id: &str) {
    let Some(dir) = dir_for(data_dir, bot_id) else { return };
    match std::fs::remove_file(dir.join(SHARE_KEEP_MARK)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(bot = bot_id, error = %e, "could not unmark a deleted share bot's outbox"),
    }
}

/// 分享用 bot 的檔案不會被清：清單上不給 `expires_at`／`remaining_secs`（前端不畫倒數）。
pub(crate) fn without_expiry(files: &mut [serde_json::Value]) {
    for f in files {
        if let Some(o) = f.as_object_mut() {
            o.insert("expires_at".into(), serde_json::Value::Null);
            o.insert("remaining_secs".into(), serde_json::Value::Null);
        }
    }
}

/// bot 啟動時把目錄建好。建不起來只記 warning：少一個目錄不該讓 bot 起不來，bot 寫之前本來就要 `mkdir -p`。
pub(crate) fn ensure(data_dir: &Path, bot_id: &str) -> Option<PathBuf> {
    let dir = dir_for(data_dir, bot_id)?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(bot = bot_id, dir = %dir.display(), error = %e, "could not create the bot's outbox");
    }
    Some(dir)
}

/// 這顆 bot 的 outbox 這條路本身能不能信，順便把已經驗證過的目錄 fd 一併回傳：`outbox` 與 `<bot_id>`
/// 兩段都不能是符號連結，擁有者要跟資料目錄同一個 uid。用 [`trusted_open::open_bound_dir`] 逐層
/// `openat(O_NOFOLLOW)`＋`fstat` 查，不是分開 `stat` 每一段再指望名字不變：bot 把自己的 outbox 換成
/// 指向 `~/.codex` 的連結，界線就整個搬過去，`auth.json` 列得出來、載得下來（review 2026-09-16 core 11
/// 洞 1——原本在 scratchpad，換成 outbox 之後同一個形狀還在）。`list()` 直接拿這裡回傳的 fd 交給
/// [`trusted_open::read_dir_bound`]，可信檢查跟列舉共用同一次 `openat` 鏈開出來的同一個 fd，不是
/// 「查完路徑安全 → 再用路徑名字重新 open 一次去列」（issue #96，跟 #89 是同一個形狀）。
/// `Ok(None)`：這一段還沒建過，不算不安全（還沒寫過、被清理收掉）。`Err(())`：符號連結或不是同一個 owner。
pub(crate) fn open_trusted_dir(data_dir: &Path, dir: &Path) -> Result<Option<std::fs::File>, ()> {
    use std::os::unix::fs::MetadataExt as _;
    let owner = std::fs::metadata(data_dir).map(|m| m.uid()).map_err(|_| ())?;
    let rel = dir.strip_prefix(data_dir).map_err(|_| ())?;
    if rel.as_os_str().is_empty() {
        return trusted_open::open_bound_dir(data_dir, &[], Some(owner)).map(Some).map_err(|_| ());
    }
    let components = trusted_open::safe_relative_components(rel).ok_or(())?;
    match trusted_open::open_bound_dir(data_dir, &components, Some(owner)) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

/// 不列、不給下載的檔案（2a96096 的黑名單，在 outbox 上保留當第二道：規則本來就禁止放這些，放了也拿不走）。
/// 先看名字（隱藏檔、資料庫與它的旁檔、金鑰與憑證），名字看不出來的再看開頭幾個位元組（改過副檔名的 SQLite、PEM 私鑰，
/// 見 [`content_is_withheld`]，[`scan`] 與 [`file`] 都是讀已經開好的 fd，不重新用路徑名字 open）。
pub(crate) fn withheld_name(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    // `x.sqlite3`、`x.sqlite3-wal`、`x.sqlite3.bak-20260916`、`bak1644.db-shm`。
    if name.contains(".sqlite") || name.ends_with(".db") || name.contains(".db-") || name.contains(".db.") {
        return true;
    }
    const KEYS: [&str; 12] =
        [".pem", ".key", ".p12", ".pfx", ".jks", ".keystore", ".ppk", ".kdbx", ".env", ".token", ".keychain", ".keychain-db"];
    // 名單以外、但就是憑證的常見檔名：codex OAuth、gcloud ADC、gh 的 hosts.yml、這個 daemon 自己的 ui-token（review core 11 洞 2）。
    const CREDENTIAL_FILES: [&str; 5] = ["auth.json", "credentials.json", "application_default_credentials.json", "hosts.yml", "ui-token"];
    KEYS.iter().any(|ext| name.ends_with(ext))
        || CREDENTIAL_FILES.contains(&name)
        || ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"].iter().any(|k| name.starts_with(k))
}

/// 內容判斷本體，吃已經讀進來的位元組：[`scan`]（列表，開 [`trusted_open::open_entry_in`] 讀檔頭）與
/// [`file`]（下載，見 [`open_outbox_entry`] 的呼叫端）都用同一個 fd 讀出來的內容餵這裡，不重新用路徑名字 open。
pub(crate) fn content_is_withheld(head: &[u8]) -> bool {
    let head = &head[..head.len().min(64)];
    if head.starts_with(b"SQLite format 3\0") {
        return true;
    }
    head.starts_with(b"-----BEGIN") && head.windows(11).any(|w| w == b"PRIVATE KEY")
}

/// `requested` 解成安全的相對 component 鏈：絕對路徑必須落在 `root` 底下（字串比對，不 canonicalize——
/// 這裡不對攻擊者能控制的輸入解符號連結，真正的界線檢查交給後面的 fd-bound open），相對路徑直接拆；
/// `..`、隱藏目錄／檔名一律拒絕。回傳最後一段的檔名（給 `withheld_name`／`mime_of`／回應檔名用）與完整的
/// component 鏈。
fn safe_outbox_path<'a>(root: &Path, requested: &'a str) -> Option<(String, Vec<&'a OsStr>)> {
    let requested = requested.trim();
    if requested.is_empty() {
        return None;
    }
    let p = Path::new(requested);
    let rel = if p.is_absolute() { p.strip_prefix(root).ok()? } else { p };
    let components = trusted_open::safe_relative_components(rel)?;
    if components.iter().any(|c| c.to_string_lossy().starts_with('.')) {
        return None; // 隱藏目錄／檔名——withheld_name 只查最後一段，中間的目錄這裡先擋。
    }
    let name = components.last()?.to_string_lossy().into_owned();
    if withheld_name(&name.to_ascii_lowercase()) {
        return None;
    }
    Some((name, components))
}

/// 下載放行哪個檔案：從 `base` 開始逐層 `openat(O_NOFOLLOW)` 走到 `requested`，`prefix` 是 `base` 到
/// 這顆 bot 的 outbox 之間固定要先走的那幾段（正式路徑是 `data_dir` → `outbox` → `<bot_id>`；單元測試
/// 直接把 `prefix` 給空、`base` 當成 outbox 本身）。回傳打開好的檔案與檔名；內容判斷
/// （[`content_is_withheld`]）與大小上限由呼叫端用同一個 fd 做，這裡不重複讀。
fn open_outbox_entry(base: &Path, prefix: &[&OsStr], requested: &str, owner_uid: Option<u32>) -> Option<(std::fs::File, String)> {
    let root = prefix.iter().fold(base.to_path_buf(), |mut p, part| {
        p.push(part);
        p
    });
    let (name, rel) = safe_outbox_path(&root, requested)?;
    let mut components: Vec<&OsStr> = prefix.to_vec();
    components.extend(rel);
    let file = trusted_open::open_bound_file(base, &components, owner_uid).ok()?;
    Some((file, name))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShareFileError {
    NotFound,
    TooLarge,
    Unavailable,
}

fn open_share_outbox_entry(base: &Path, bot_id: &str, requested: &str, owner_uid: Option<u32>) -> Result<(std::fs::File, String), ShareFileError> {
    let prefix = [OsStr::new("outbox"), OsStr::new(bot_id)];
    let root = prefix.iter().fold(base.to_path_buf(), |mut p, part| {
        p.push(part);
        p
    });
    let (name, rel) = safe_outbox_path(&root, requested).ok_or(ShareFileError::NotFound)?;
    let mut components: Vec<&OsStr> = prefix.to_vec();
    components.extend(rel);
    let file = trusted_open::open_bound_file(base, &components, owner_uid).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { ShareFileError::NotFound } else { ShareFileError::Unavailable }
    })?;
    Ok((file, name))
}

/// 下載時的 content type。白名單以外一律 octet-stream：使用者自己的 HTML 不該在這個 origin 跑起來
/// （token 就放在這個 origin 的 localStorage）。
pub(crate) fn mime_of(path: &Path) -> &'static str {
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
pub(crate) fn content_disposition(name: &str) -> String {
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

/// 目錄裡可以列出來的檔案：第一層的一般檔案（符號連結、子目錄不列），名字與內容都不是黑名單
/// （[`withheld_name`]／[`content_is_withheld`]），新的排前面。每個帶 `expires_at`（mtime 與 ctime 較晚的那個 + [`TTL_SECS`]，
/// 即「搬進來」的時間：`mv`／`cp -p` 保留舊 mtime，ctime 才是搬入那一刻；AGM 清理要兩個都過期才刪）與 `remaining_secs`（到期了是 0，清理每 10 分鐘才跑一次，所以 0 的檔案還會在
/// 清單上待一下）。`dir` 是呼叫端已經驗證過（[`open_trusted_dir`]）拿到的目錄 fd：列舉
/// （[`trusted_open::read_dir_bound`]）與逐一開檔看內容（[`trusted_open::open_entry_in`]）全程都掛在
/// 這個 fd 底下，不再用任何路徑名字重新解析——檢查通過之後這個目錄被整個換成符號連結也不影響列出來的內容
/// （issue #96）。
pub(crate) fn scan(dir: &std::fs::File, now: u64) -> Vec<serde_json::Value> {
    let mut files = Vec::new();
    if let Ok(entries) = trusted_open::read_dir_bound(dir) {
        for e in entries {
            if !e.is_file {
                continue;
            }
            let name = e.name.to_string_lossy().into_owned();
            if withheld_name(&name.to_ascii_lowercase()) {
                continue;
            }
            let is_withheld = trusted_open::open_entry_in(dir, &e.name)
                .ok()
                .map(|mut f| {
                    use std::io::Read;
                    let mut head = [0u8; 64];
                    let n = f.read(&mut head).unwrap_or(0);
                    content_is_withheld(&head[..n])
                })
                .unwrap_or(true); // 開不起來（例如列舉之後又被換掉）就當作要擋，不列。
            if is_withheld {
                continue;
            }
            let secs = |t: std::time::SystemTime| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let modified = secs(e.modified);
            files.push((name, e.size, modified, modified.max(secs(e.changed))));
        }
    }
    files.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    files.truncate(MAX_ENTRIES);
    files
        .into_iter()
        .map(|(name, size, modified, landed)| {
            let expires_at = landed + TTL_SECS;
            json!({"name": name, "size": size, "modified": modified, "expires_at": expires_at, "remaining_secs": expires_at.saturating_sub(now)})
        })
        .collect()
}

/// 分享入口只能回傳完整且逐項驗證過的清單；讀取失敗時呼叫端回 503，而不把部分結果當成空清單。
pub(crate) fn scan_checked(dir: &std::fs::File, now: u64) -> Result<Vec<serde_json::Value>, ()> {
    use std::io::Read;
    let entries = trusted_open::read_dir_bound(dir).map_err(|_| ())?;
    let mut files = Vec::new();
    for e in entries {
        if !e.is_file {
            continue;
        }
        let name = e.name.to_string_lossy().into_owned();
        if withheld_name(&name.to_ascii_lowercase()) {
            continue;
        }
        let mut file = trusted_open::open_entry_in(dir, &e.name).map_err(|_| ())?;
        let mut head = [0u8; 64];
        let n = file.read(&mut head).map_err(|_| ())?;
        if content_is_withheld(&head[..n]) {
            continue;
        }
        let secs = |t: std::time::SystemTime| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let modified = secs(e.modified);
        files.push((name, e.size, modified, modified.max(secs(e.changed))));
    }
    files.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    files.truncate(MAX_ENTRIES);
    Ok(files
        .into_iter()
        .map(|(name, size, modified, landed)| {
            let expires_at = landed + TTL_SECS;
            json!({"name": name, "size": size, "modified": modified, "expires_at": expires_at, "remaining_secs": expires_at.saturating_sub(now)})
        })
        .collect())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 這顆 bot 的 outbox。遠端主機的 bot 沒有（檔案在那台機器上）。
async fn outbox_of(app: &impl OutboxEnv, bot_id: &str) -> Result<PathBuf, LcError> {
    let place = app.bot_place(bot_id).await.map_err(|_| LcError::NotFound("bot".into()))?;
    if place.host != crate::config::LOCAL_HOST {
        return Err(LcError::conflict("outbox_remote", json!({"reason": "outbox_remote", "host": place.host})));
    }
    dir_for(app.data_dir(), bot_id).ok_or_else(|| LcError::NotFound("bot".into()))
}

/// `GET /api/bots/{id}/outbox` — 這顆 bot 交給使用者的檔案（新的排前面），各自還剩多久被清掉。
pub async fn list_for(app: &impl crate::outbox_remote::OutboxRemoteEnv, id: String) -> Result<Response, LcError> {
    // 遠端主機的 bot：outbox 在那台機器上，走 ssh 列（`outbox_remote`）。
    // 連不上那台不是錯誤：回空清單並講原因（下載才回 409）。
    match crate::outbox_remote::target(app, &id).await {
        Ok(Some(t)) => return crate::outbox_remote::list(t, now_secs()).await,
        Ok(None) => {}
        Err(LcError::Conflict(detail)) => {
            return Ok((StatusCode::OK, axum::Json(json!({"files": [], "ttl_secs": TTL_SECS, "reason": detail.get("reason"), "host": detail.get("host")}))).into_response())
        }
        Err(e) => return Err(e),
    }
    let dir = match outbox_of(app, &id).await {
        Ok(d) => d,
        // 遠端不是錯誤，是常態：回空清單並說明原因，前端不用畫成紅字。
        Err(LcError::Conflict(detail)) => {
            return Ok((StatusCode::OK, axum::Json(json!({"files": [], "ttl_secs": TTL_SECS, "reason": detail.get("reason")}))).into_response())
        }
        Err(e) => return Err(e),
    };
    // 可信檢查（拿到目錄 fd）跟真正列舉擺進同一個 blocking closure、共用同一個 fd：中間沒有
    // `.await`，也沒有「查完路徑 → 之後再用路徑重新 open 去列」這一步，這顆 bot 自己把目錄整個換成
    // 符號連結也換不掉已經拿在手上的 fd（issue #96）。
    let data_dir = app.data_dir().to_path_buf();
    let scan_dir = dir.clone();
    let now = now_secs();
    let outcome = tokio::task::spawn_blocking(move || match open_trusted_dir(&data_dir, &scan_dir) {
        Ok(Some(fd)) => Ok(scan(&fd, now)),
        Ok(None) => Ok(Vec::new()),
        Err(()) => Err(()),
    })
    .await
    .unwrap_or(Err(()));
    let mut files = match outcome {
        Ok(files) => files,
        Err(()) => {
            tracing::warn!(bot = %id, dir = %dir.display(), "outbox path is a symlink or not ours; not listing it");
            return Ok((StatusCode::OK, axum::Json(json!({"files": [], "ttl_secs": TTL_SECS, "reason": "outbox_untrusted"}))).into_response());
        }
    };
    // 分享用 bot（SPEC §20）的 outbox 不清：`ttl_secs:null`、每個檔不帶到期。讀不到是不是分享用 bot 就照一般的報（只影響顯示）。
    if matches!(crate::share::store::is_share_bot(app.db_pool(), &id).await, Ok(true)) {
        without_expiry(&mut files);
        return Ok((StatusCode::OK, axum::Json(json!({"dir": dir.to_string_lossy(), "ttl_secs": null, "kept": true, "files": files}))).into_response());
    }
    Ok((StatusCode::OK, axum::Json(json!({"dir": dir.to_string_lossy(), "ttl_secs": TTL_SECS, "files": files}))).into_response())
}

/// `GET /api/bots/{id}/outbox/file?path=…` — 一律當附件下載。整段路徑驗證＋open＋fstat＋讀內容都走
/// [`open_outbox_entry`] 那條 fd-bound 的鏈，不再分開「驗證路徑」與「用路徑重新讀」兩步（issue #89）。
pub async fn file_for(app: &impl crate::outbox_remote::OutboxRemoteEnv, id: String, q: std::collections::HashMap<String, String>) -> Result<Response, LcError> {
    use std::os::unix::fs::MetadataExt as _;
    let not_found = || LcError::NotFound("file".into());
    let requested = q.get("path").ok_or_else(|| LcError::Bad("path required".into()))?.clone();
    if let Some(t) = crate::outbox_remote::target(app, &id).await? {
        return crate::outbox_remote::file(t, &requested).await;
    }
    outbox_of(app, &id).await?; // 確認本機、bot 存在；拿到的路徑只是拿來確認，不再用它重新 open。
    let data_dir = app.data_dir().to_path_buf();
    let bot_id = id.clone();
    let opened = tokio::task::spawn_blocking(move || {
        let owner = std::fs::metadata(&data_dir).ok()?.uid();
        open_outbox_entry(&data_dir, &[OsStr::new("outbox"), OsStr::new(&bot_id)], &requested, Some(owner))
    })
    .await
    .ok()
    .flatten();
    let Some((f, name)) = opened else { return Err(not_found()) };
    let meta = f.metadata().map_err(|_| not_found())?;
    if meta.len() > MAX_BYTES {
        return Err(LcError::conflict("file_too_large", json!({"reason": "file_too_large", "size": meta.len(), "max": MAX_BYTES})));
    }
    let read = tokio::task::spawn_blocking(move || trusted_open::read_limited(f, MAX_BYTES)).await.map_err(|_| not_found())?;
    let data = match read {
        Ok(data) => data,
        Err(trusted_open::BoundedReadError::TooLarge { observed }) => {
            return Err(LcError::conflict("file_too_large", json!({"reason": "file_too_large", "size": observed, "max": MAX_BYTES})));
        }
        Err(trusted_open::BoundedReadError::Io) => return Err(not_found()),
    };
    if content_is_withheld(&data) {
        return Err(not_found());
    }
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime_of(Path::new(&name)).to_string()),
            (header::CONTENT_DISPOSITION, content_disposition(&name)),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        data,
    )
        .into_response())
}

/// 分享入口用的下載分類：確實不存在／遭黑名單擋下回 NotFound；可信邊界、DB、讀取或工作失敗保留成 Unavailable。
pub(crate) async fn share_file(app: &impl OutboxEnv, bot_id: &str, requested: &str) -> Result<Response, ShareFileError> {
    let (name, data) = share_file_bytes(app, bot_id, requested).await?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime_of(Path::new(&name)).to_string()),
            (header::CONTENT_DISPOSITION, content_disposition(&name)),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        data,
    )
        .into_response())
}

/// [`share_file`] 的讀檔那一段（同一條 fd-bound 鏈、同樣的上限與黑名單），回（檔名, 內容）。分享頁下載與 `.svg` 檢查共用。
pub(crate) async fn share_file_bytes(app: &impl OutboxEnv, bot_id: &str, requested: &str) -> Result<(String, Vec<u8>), ShareFileError> {
    share_file_bytes_with_limit(app, bot_id, requested, MAX_BYTES).await
}

/// 同 [`share_file_bytes`]，但回傳包含 metadata（ino、mtime_ns、ctime_ns）。
pub(crate) struct ShareFileRead {
    pub name: String,
    pub data: Vec<u8>,
    pub ino: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
}

/// 同 [`share_file_bytes`]，但允許呼叫端指定讀取上限（例如 SVG 檢查只需 4 MiB）。
/// 回傳包含 metadata（ino、mtime_ns、ctime_ns），方便做版本指紋檢查。
/// 超過上限會在讀檔前由 fstat 拒絕，不進行記憶體配置與實際內容讀取。
pub(crate) async fn share_file_read_with_limit(
    app: &impl OutboxEnv,
    bot_id: &str,
    requested: &str,
    max_bytes: u64,
) -> Result<ShareFileRead, ShareFileError> {
    use std::os::unix::fs::MetadataExt as _;

    let place = app.bot_place(bot_id).await.map_err(|e| match e {
        BotLookup::BotMissing | BotLookup::ProjectMissing => ShareFileError::NotFound,
        BotLookup::BotUnavailable | BotLookup::ProjectUnavailable => ShareFileError::Unavailable,
    })?;
    if place.host != crate::config::LOCAL_HOST {
        return Err(ShareFileError::NotFound);
    }

    let data_dir = app.data_dir().to_path_buf();
    let file_bot = bot_id.to_string();
    let requested = requested.to_string();
    let (file, name) = tokio::task::spawn_blocking(move || {
        let owner = std::fs::metadata(&data_dir).map_err(|_| ShareFileError::Unavailable)?.uid();
        open_share_outbox_entry(&data_dir, &file_bot, &requested, Some(owner))
    })
    .await
    .map_err(|_| ShareFileError::Unavailable)??;

    let metadata = file.metadata().map_err(|_| ShareFileError::Unavailable)?;
    if metadata.len() > max_bytes {
        return Err(ShareFileError::TooLarge);
    }
    let ino = metadata.ino();
    let mtime_ns = metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128;
    let ctime_ns = metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128;
    let data = tokio::task::spawn_blocking(move || trusted_open::read_limited(file, max_bytes))
        .await
        .map_err(|_| ShareFileError::Unavailable)?
        .map_err(|e| match e {
            trusted_open::BoundedReadError::TooLarge { .. } => ShareFileError::TooLarge,
            trusted_open::BoundedReadError::Io => ShareFileError::Unavailable,
        })?;
    if content_is_withheld(&data) {
        return Err(ShareFileError::NotFound);
    }
    Ok(ShareFileRead { name, data, ino, mtime_ns, ctime_ns })
}

/// 同 [`share_file_bytes`]，但允許呼叫端指定讀取上限（例如 SVG 檢查只需 4 MiB）。
/// 超過上限會在讀檔前由 fstat 拒絕，不進行記憶體配置與實際內容讀取。
pub(crate) async fn share_file_bytes_with_limit(
    app: &impl OutboxEnv,
    bot_id: &str,
    requested: &str,
    max_bytes: u64,
) -> Result<(String, Vec<u8>), ShareFileError> {
    let read = share_file_read_with_limit(app, bot_id, requested, max_bytes).await?;
    Ok((read.name, read.data))
}

/// 舊的 `/bots/{id}/scratchpad*`：scratchpad 不再給使用者（使用者 2026-09-16 裁示）。明確回 404，
/// 不讓它掉進 SPA fallback 回一頁 HTML 200，舊分頁也看得出「這條路沒了」。
pub async fn scratchpad_gone() -> LcError {
    LcError::NotFound("scratchpad".into())
}

// 路由用的 axum handler（`State<Arc<App>>`）住在 composition 層（`app_ports_p10`），這裡保留舊名給 `api` 的路由表與測試。
pub use crate::app_ports_p10::{file, list};

#[cfg(test)]
mod tests {
    use crate::state::App;
    use axum::extract::{Path as UrlPath, Query, State};
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
        let base = crate::testing::track(std::env::temp_dir().join(format!("am-outbox-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::canonicalize(base).unwrap()
    }

    /// 舊 `servable(root, requested) -> Option<PathBuf>` 的測試替身：現在整條路徑驗證＋open 都是
    /// fd-bound（[`open_outbox_entry`]），沒有中間的 `PathBuf` 可以比對，所以直接回讀出來的內容。
    fn servable(root: &Path, requested: &str) -> Option<Vec<u8>> {
        let (mut file, _name) = open_outbox_entry(root, &[], requested, None)?;
        use std::io::Read;
        let mut data = Vec::new();
        file.read_to_end(&mut data).ok()?;
        if content_is_withheld(&data) {
            return None;
        }
        Some(data)
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
        put("auth.json", b"{\"tokens\":{}}");
        put("application_default_credentials.json", b"{}");
        put("hosts.yml", b"github.com:\n  oauth_token: x");
        put("ui-token", b"abc");
        put("gh.token", b"abc");
        std::fs::write(root.join(".secret/plain.txt"), b"hi").unwrap();

        let fd = trusted_open::open_bound_dir(&root, &[], None).unwrap();
        let listed: Vec<String> = scan(&fd, 0).iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(listed, vec!["report.md".to_string()], "只剩一般檔案");
        assert!(servable(&root, "report.md").is_some());
        for p in ["migrate-check.sqlite3", "bak1644.db", "bak1644.db-wal", "agents-manager.sqlite3.bak-20260916", "innocent.bin", "server.pem", "notes.txt", "id_ed25519", "prod.env", ".env", ".secret/plain.txt", "auth.json", "application_default_credentials.json", "hosts.yml", "ui-token", "gh.token"] {
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
        let fd = trusted_open::open_bound_dir(&base, &[], None).unwrap();
        let files = scan(&fd, 0);
        assert_eq!(files.len(), 1, "子目錄不列：{files:?}");
        let modified = files[0]["modified"].as_u64().unwrap();
        assert!(modified > 0);
        assert_eq!(files[0]["expires_at"], json!(modified + TTL_SECS));
        assert_eq!(scan(&fd, modified + 600)[0]["remaining_secs"], json!(TTL_SECS - 600), "放了十分鐘剩五十分鐘");
        assert_eq!(scan(&fd, modified + TTL_SECS + 1)[0]["remaining_secs"], json!(0), "過期是 0，等清理");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn checked_scan_reports_directory_read_failures_instead_of_a_complete_empty_list() {
        let base = scratch("checked-scan-error");
        let file_path = base.join("not-a-directory");
        std::fs::write(&file_path, b"x").unwrap();
        let fd = std::fs::File::open(file_path).unwrap();
        assert!(scan_checked(&fd, 0).is_err(), "a failed enumeration is not a verified empty outbox");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 保留期從「搬進 outbox」起算：`mv`／`cp -p` 進來的檔 mtime 還是很久以前，ctime 才是搬入那一刻。
    /// 只看 mtime 的話，清單說「已過期」、清理（mtime 與 ctime 都過了才刪）卻還沒動，兩邊對不上。
    #[test]
    fn a_file_moved_in_with_an_old_mtime_expires_from_when_it_landed() {
        let base = scratch("landed");
        let f = std::fs::File::create(base.join("moved-in.pdf")).unwrap();
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        f.set_modified(two_hours_ago).unwrap(); // mtime 兩小時前；ctime 是現在
        drop(f);
        let fd = trusted_open::open_bound_dir(&base, &[], None).unwrap();
        let now = now_secs();
        let listed = &scan(&fd, now)[0];
        assert!(listed["modified"].as_u64().unwrap() <= now - 7000, "modified 仍是檔案內容的 mtime：{listed}");
        assert!(listed["remaining_secs"].as_u64().unwrap() > TTL_SECS - 60, "剛搬進來，剩的是整個保留期：{listed}");
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

    /// review 2026-09-16 core 11 洞 1：bot 把自己的 outbox 換成指向別處（例如 `~/.codex`）的符號連結，界線不能跟著搬過去。
    #[tokio::test]
    async fn a_symlinked_outbox_is_neither_listed_nor_served() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let elsewhere = env.dir.join("dot-codex");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("session-notes.md"), b"# not for download").unwrap();
        let outbox = dir_for(&env.app.data_dir, &bot.id).unwrap();
        std::fs::create_dir_all(outbox.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &outbox).unwrap();

        assert!(matches!(open_trusted_dir(&env.app.data_dir, &outbox), Err(())));
        let (status, bytes) = body(list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!((v["files"].clone(), v["reason"].clone()), (json!([]), json!("outbox_untrusted")));
        assert_eq!(get_file(&env.app, &bot.id, "session-notes.md").await.0, StatusCode::NOT_FOUND);

        // 真的目錄照常；還沒建也不算不安全。
        std::fs::remove_file(&outbox).unwrap();
        assert!(matches!(open_trusted_dir(&env.app.data_dir, &outbox), Ok(None)), "還沒建");
        ensure(&env.app.data_dir, &bot.id).unwrap();
        assert!(matches!(open_trusted_dir(&env.app.data_dir, &outbox), Ok(Some(_))));
    }

    /// issue #96：`list()` 舊實作是「可信檢查（[`open_trusted_dir`]，fd-bound）→ 之後再用路徑 `read_dir`
    /// 重新列一次」，檢查通過之後、真正列舉之前，這顆 bot 自己能把整個目錄換成指到界線外的符號連結，讓
    /// 清單改列出界線外的檔名／大小。現在 [`open_trusted_dir`] 拿到的目錄 fd 直接交給 [`scan`]，兩者是
    /// 同一次 [`list`] 呼叫裡同一個 `spawn_blocking` 用的同一個 fd——這裡重現「檢查通過拿到 fd 之後、
    /// 真正列舉之前，把路徑換掉」這個時序：列出來的還是拿到 fd 當下那個目錄的內容，不是換過去的目標。
    #[test]
    fn listing_follows_the_fd_from_the_trust_check_not_a_path_swapped_afterward() {
        let base = scratch("list-race");
        let data_dir = base.join("data");
        let outbox_dir = data_dir.join("outbox").join("BOT01");
        std::fs::create_dir_all(&outbox_dir).unwrap();
        std::fs::write(outbox_dir.join("report.md"), b"# safe").unwrap();

        // list() 的第一步：可信檢查，拿到已經打開的目錄 fd。
        let fd = open_trusted_dir(&data_dir, &outbox_dir).unwrap().expect("目錄存在，該給 fd");

        // 檢查通過之後、真正列舉之前：整個目錄搬到旁邊（內容不動），原本的名字換成指到界線外的符號連結。
        std::fs::rename(&outbox_dir, data_dir.join("outbox").join("moved-aside")).unwrap();
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("secret.txt"), b"host secret").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &outbox_dir).unwrap();

        // list() 的第二步：拿著同一個 fd 去列，不重新解一次路徑。
        let listed: Vec<String> = scan(&fd, 0).into_iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(listed, vec!["report.md".to_string()], "列到的是拿到 fd 當下那個目錄，不是換過去的 elsewhere");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// issue #89：舊實作是「查完路徑安全 → 再用路徑名字重新 open 一次」，兩次 open 之間，這顆 bot 自己
    /// 就能把驗證通過的檔案換成指到界線外的符號連結。現在整條鏈是 fd-bound（[`open_outbox_entry`]），
    /// 驗證用的就是真正拿去讀的那個 fd——這裡重現「先下載成功一次、換成符號連結、再下載」這個時序，
    /// 第二次一定拿不到界線外的內容。
    #[tokio::test]
    async fn a_file_swapped_for_a_symlink_between_downloads_never_leaks_the_target() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let outbox = ensure(&env.app.data_dir, &bot.id).unwrap();
        std::fs::write(outbox.join("report.md"), b"# safe").unwrap();

        let (status, bytes) = get_file(&env.app, &bot.id, "report.md").await;
        assert_eq!((status, bytes.as_slice()), (StatusCode::OK, &b"# safe"[..]), "第一次下載，正常檔案");

        let secret = env.dir.join("host-secret.txt");
        std::fs::write(&secret, b"host secret").unwrap();
        std::fs::remove_file(outbox.join("report.md")).unwrap();
        std::os::unix::fs::symlink(&secret, outbox.join("report.md")).unwrap();

        let (status, bytes) = get_file(&env.app, &bot.id, "report.md").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "換成符號連結之後不能再拿到任何內容");
        assert_ne!(bytes, b"host secret".to_vec());
    }

    /// 遠端主機的 bot 而那台連不上（這裡根本沒設定 `box`）：清單回空＋原因，下載不給。連得上的情形見 `outbox_remote`。
    #[tokio::test]
    async fn a_remote_bot_on_an_unreachable_host_lists_nothing_and_says_why() {
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
        assert_eq!((v["files"].clone(), v["reason"].clone(), v["host"].clone()), (json!([]), json!("outbox_remote_unreachable"), json!("box")));
        assert_ne!(get_file(&env.app, &bot.id, "x.txt").await.0, StatusCode::OK);
        assert_eq!(get_file(&env.app, "nope", "x.txt").await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn share_file_bytes_with_limit_rejects_oversized_file_without_reading() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let outbox = ensure(&env.app.data_dir, &bot.id).unwrap();

        // 建立 5 MiB 稀疏檔案，不佔硬碟空間
        let f = std::fs::File::create(outbox.join("sparse.bin")).unwrap();
        f.set_len(5 * 1024 * 1024).unwrap();
        drop(f);

        // 指定上限 4 MiB：stat 即擋下，回傳 TooLarge
        let res = share_file_bytes_with_limit(&env.app, &bot.id, "sparse.bin", 4 * 1024 * 1024).await;
        assert!(matches!(res, Err(ShareFileError::TooLarge)));

        // 建立 100 bytes 檔案
        std::fs::write(outbox.join("small.bin"), vec![b'a'; 100]).unwrap();
        // 指定上限 50 bytes：回傳 TooLarge
        let res_small_too_large = share_file_bytes_with_limit(&env.app, &bot.id, "small.bin", 50).await;
        assert!(matches!(res_small_too_large, Err(ShareFileError::TooLarge)));
        // 指定上限 200 bytes：回傳 Ok
        let res_small_ok = share_file_bytes_with_limit(&env.app, &bot.id, "small.bin", 200).await;
        assert!(res_small_ok.is_ok());
        assert_eq!(res_small_ok.unwrap().1.len(), 100);

        // 既有的 symlink 安全防護：指到外面的符號連結不可繞過（違反可信邊界回 Unavailable）
        let secret = env.dir.join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();
        std::os::unix::fs::symlink(&secret, outbox.join("link_to_secret.bin")).unwrap();
        let res_escape = share_file_bytes_with_limit(&env.app, &bot.id, "link_to_secret.bin", 4 * 1024 * 1024).await;
        assert!(matches!(res_escape, Err(ShareFileError::Unavailable)));
    }
}
