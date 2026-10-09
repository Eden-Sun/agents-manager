//! 分享入口送出 SVG 時，把 `<image href="inbox/照片.jpeg">` 這類相對路徑嵌成 data URI（使用者 2026-10-04）。
//!
//! 受限 bot 沒有 shell，只寫得出 SVG 文字；長輩上傳的照片要合成進海報，就讓 bot 用相對路徑引用資料夾裡的照片，
//! 由 daemon 在 `/s/{token}/api/files/{name}` 送出時換成縮過的 data URI。原始 SVG 檔不改。
//!
//! - 只認相對路徑：任何 scheme（`http:`、`file:`…）、`/` 開頭、`\`、`..`、`.` 開頭的段一律不解；解開時從 bot 的資料夾
//!   逐段 `O_NOFOLLOW` 打開（[`crate::trusted_open::open_bound_file`]：不跟符號連結、硬連結也擋）。
//! - 只收 JPEG／PNG／WebP／GIF（看檔頭，不看副檔名）；HEIC 這裡解不了（沒有 libheif），跳過。
//! - 解不開、不是圖、太大的：整個 href 拿掉，換成 `data-am-embed="<理由>"` 留個記號；`#id` 與 `data:` 原樣不動。
//! - 縮圖：修正 EXIF 方向、長邊最多 [`LONG_SIDE`]、JPEG 品質 [`JPEG_QUALITY`]（有透明才用 PNG）；重編碼順便拿掉 EXIF（含 GPS）。
//!   每張與整份都有上限；縮好的結果依引用檔的 inode／大小／mtime 快取。

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine as _;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, ImageReader};

/// 縮圖後的長邊上限（px）。
pub const LONG_SIDE: u32 = 1600;
/// 縮圖 JPEG 品質。
pub const JPEG_QUALITY: u8 = 85;
/// 一張引用檔最多讀這麼多（手機原圖通常 2–8MB）。
const SOURCE_MAX: u64 = 40 * 1024 * 1024;
/// 解碼的長寬上限：擋解壓縮炸彈（總像素另有 [`PIXEL_MAX`]）。
const DECODE_SIDE_MAX: u32 = 16_384;
/// 嵌進去的一張（base64 之後）最多這麼大。
const EMBED_MAX: usize = 4 * 1024 * 1024;
/// 嵌完之後整份 SVG 最多這麼大；超過的那幾張不嵌。
const OUT_MAX: usize = 24 * 1024 * 1024;
/// 一份 SVG 最多嵌幾張。
const MAX_IMAGES: usize = 16;
/// 一份 SVG 最多嘗試解幾個引用（成功與失敗都算），免得一堆壞檔把解碼預算耗光。
const MAX_ATTEMPTS: usize = 2 * MAX_IMAGES;
/// 單張的像素上限（≈50.3 MP，涵蓋 48 MP 手機原圖）；超過在解碼前就拒絕。
const PIXEL_MAX: u64 = 8192 * 6144;
/// 全站同時進行的 compose 數（解碼很吃記憶體與 CPU）。
const COMPOSE_SLOTS: usize = 1;
/// 等全站 compose 名額最久這麼久，等不到回 503。
#[cfg(not(test))]
pub(crate) const COMPOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
pub(crate) const COMPOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
/// 快取總量（data URI 的位元組數）。
const CACHE_MAX: usize = 64 * 1024 * 1024;

/// 引用檔的身分：同一個來源（本機資料夾，或「主機＋遠端資料夾」，兩台同路徑不串）、同一條相對路徑、同一個 inode／大小／mtime 才算同一張。
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    scope: String,
    rel: String,
    ino: u64,
    len: u64,
    mtime_ns: i128,
}

/// 一張的結果：data URI，或跳過的理由（理由也快取，壞檔不用每次重解）。
type Embedded = Result<Arc<str>, &'static str>;

/// 引用檔的身分資料（快取鍵用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhotoMeta {
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: i128,
}

/// 照片從哪裡來（#955 R-S3）：本機資料夾，或預先從遠端抓好的一批。`embed_with` 只透過它看照片，其餘（href 解析、縮圖、上限、快取）共用。
/// 理由字串沿用既有的：`not_found`／`source_too_large`／`read_failed`。
pub trait PhotoSource {
    /// 快取鍵的來源部分：本機是資料夾路徑；遠端要帶主機名。
    fn scope(&self) -> String;
    fn stat(&self, parts: &[&str]) -> Result<PhotoMeta, &'static str>;
    fn read(&self, parts: &[&str], max: u64) -> Result<Vec<u8>, &'static str>;
}

/// 本機資料夾：逐段 `O_NOFOLLOW` 打開（[`crate::trusted_open::open_bound_file`]：不跟符號連結、硬連結也擋）。
struct LocalSource<'a> {
    folder: &'a Path,
}

impl PhotoSource for LocalSource<'_> {
    fn scope(&self) -> String {
        self.folder.to_string_lossy().into_owned()
    }

    fn stat(&self, parts: &[&str]) -> Result<PhotoMeta, &'static str> {
        let comps: Vec<&OsStr> = parts.iter().map(OsStr::new).collect();
        let file = crate::trusted_open::open_bound_file(self.folder, &comps, None).map_err(|_| "not_found")?;
        let meta = file.metadata().map_err(|_| "not_found")?;
        Ok(PhotoMeta { ino: meta.ino(), len: meta.len(), mtime_ns: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128 })
    }

    fn read(&self, parts: &[&str], max: u64) -> Result<Vec<u8>, &'static str> {
        let comps: Vec<&OsStr> = parts.iter().map(OsStr::new).collect();
        let file = crate::trusted_open::open_bound_file(self.folder, &comps, None).map_err(|_| "not_found")?;
        match crate::trusted_open::read_limited(file, max) {
            Ok(bytes) => Ok(bytes),
            Err(crate::trusted_open::BoundedReadError::TooLarge { .. }) => Err("source_too_large"),
            Err(crate::trusted_open::BoundedReadError::Io) => Err("read_failed"),
        }
    }
}

/// 遠端照片：先在 async 端用 [`referenced_rels`] 找出引用的路徑，經 `photo_stats`／`photo_fetch` 抓好，再交給同步的 [`embed_with`] 用
/// （`embed_with` 在 `spawn_blocking` 裡跑，不能 await）。沒抓到的一律 `not_found`（遠端任何不確定都不嵌）。
pub struct PrefetchedSource {
    scope: String,
    items: HashMap<String, (PhotoMeta, Option<Result<Vec<u8>, &'static str>>)>,
}

impl PrefetchedSource {
    /// `scope`＝主機名＋遠端資料夾；`rels`、`metas`、`data` 同長度、同順序（`data[i]` 只對沒命中快取而且要抓的才有值）。
    pub fn new(host: &str, workspace: &str, rels: &[Vec<String>], metas: &[Option<PhotoMeta>], data: Vec<Option<Result<Vec<u8>, &'static str>>>) -> Self {
        let mut items = HashMap::new();
        for ((rel, meta), data) in rels.iter().zip(metas).zip(data) {
            if let Some(meta) = meta {
                items.insert(rel.join("/"), (*meta, data));
            }
        }
        Self { scope: Self::scope_of(host, workspace), items }
    }

    /// 快取鍵的來源部分：主機名＋遠端資料夾（兩台主機同一條路徑不會串）。
    pub fn scope_of(host: &str, workspace: &str) -> String {
        format!("{host}\0{workspace}")
    }
}

impl PhotoSource for PrefetchedSource {
    fn scope(&self) -> String {
        self.scope.clone()
    }

    fn stat(&self, parts: &[&str]) -> Result<PhotoMeta, &'static str> {
        self.items.get(&parts.join("/")).map(|(m, _)| *m).ok_or("not_found")
    }

    fn read(&self, parts: &[&str], max: u64) -> Result<Vec<u8>, &'static str> {
        match self.items.get(&parts.join("/")) {
            Some((_, Some(Ok(bytes)))) if bytes.len() as u64 <= max => Ok(bytes.clone()),
            Some((_, Some(Ok(_)))) => Err("source_too_large"),
            Some((_, Some(Err(why)))) => Err(why),
            _ => Err("not_found"),
        }
    }
}

/// 記錄 `embed_with` 要看哪些照片（一律回 `not_found`，不讀不解）：用同一套 href 解析找出引用路徑，不另寫一份解析器。
struct RecordingSource {
    wanted: std::cell::RefCell<Vec<Vec<String>>>,
}

impl PhotoSource for RecordingSource {
    fn scope(&self) -> String {
        String::new()
    }

    fn stat(&self, parts: &[&str]) -> Result<PhotoMeta, &'static str> {
        let rel: Vec<String> = parts.iter().map(|p| (*p).to_string()).collect();
        let mut wanted = self.wanted.borrow_mut();
        // 候選（原樣與 `%XX` 解碼後）合計最多 2×MAX_IMAGES 條，免得一份塞滿 `<image>` 的 SVG 讓遠端查太多。
        if wanted.len() < MAX_IMAGES * 2 && !wanted.contains(&rel) {
            wanted.push(rel);
        }
        Err("not_found")
    }

    fn read(&self, _parts: &[&str], _max: u64) -> Result<Vec<u8>, &'static str> {
        Err("not_found")
    }
}

/// SVG 引用的照片路徑（至多 [`MAX_IMAGES`] 張的候選，順序固定）。遠端先抓這些，再 `embed_with`。純函式。
pub fn referenced_rels(svg: &[u8]) -> Vec<Vec<String>> {
    let rec = RecordingSource { wanted: Default::default() };
    let _ = embed_with(svg, &rec);
    rec.wanted.into_inner()
}

/// 這張照片（依來源＋路徑＋身分）的縮圖結果已在快取裡嗎？命中的不必再從遠端抓。
pub fn is_cached(scope: &str, rel: &[String], meta: &PhotoMeta) -> bool {
    cache_get(&key_of(scope, &rel.join("/"), meta)).is_some()
}

/// 照片的原始大小上限（超過回 `source_too_large`），遠端 `photo_fetch` 的每張上限也用它。
pub const PHOTO_SOURCE_MAX: u64 = SOURCE_MAX;

fn key_of(scope: &str, rel: &str, meta: &PhotoMeta) -> Key {
    Key { scope: scope.to_string(), rel: rel.to_string(), ino: meta.ino, len: meta.len, mtime_ns: meta.mtime_ns }
}

#[derive(Default)]
struct Cache {
    map: HashMap<Key, Embedded>,
    order: VecDeque<Key>,
    bytes: usize,
}

/// 全站 compose 名額：跨分享連結共用，避免多個連結同時各解一批大圖。
#[cfg(not(test))]
pub(crate) fn slots() -> Arc<tokio::sync::Semaphore> {
    static S: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    S.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(COMPOSE_SLOTS))).clone()
}

/// 測試版：每個測試（各自一條執行緒、current_thread runtime）用自己的名額，彼此不搶；名額的行為本身不變。
#[cfg(test)]
pub(crate) fn slots() -> Arc<tokio::sync::Semaphore> {
    thread_local! {
        static S: Arc<tokio::sync::Semaphore> = Arc::new(tokio::sync::Semaphore::new(COMPOSE_SLOTS));
    }
    S.with(Arc::clone)
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn cost(e: &Embedded) -> usize {
    e.as_ref().map(|s| s.len()).unwrap_or(0) + 256
}

fn cache_get(k: &Key) -> Option<Embedded> {
    cache().lock().unwrap_or_else(|e| e.into_inner()).map.get(k).cloned()
}

fn cache_put(k: Key, v: Embedded) {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    c.bytes += cost(&v);
    if let Some(old) = c.map.insert(k.clone(), v) {
        c.bytes -= cost(&old);
    } else {
        c.order.push_back(k);
    }
    while c.bytes > CACHE_MAX {
        let Some(old) = c.order.pop_front() else { break };
        if let Some(v) = c.map.remove(&old) {
            c.bytes -= cost(&v);
        }
    }
}

// 解碼次數（測試看快取有沒有生效）；`embed` 在呼叫端的執行緒上同步跑，平行的其他測試不會算進來。
#[cfg(test)]
thread_local!(static DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });

#[cfg(test)]
fn decodes_for_test() -> usize {
    DECODES.with(|d| d.get())
}

/// SVG 文字裡值得處理的跡象：沒有 `<image` 就原樣送，不必解析。
pub fn wants_embed(svg: &[u8]) -> bool {
    svg.windows(6).any(|w| w == b"<image")
}

/// 把 `svg` 裡 `<image>` 的相對 href 嵌成 data URI，回新的內容；沒有要改的就回 `None`（照原檔送）。
/// 不是 UTF-8 的也回 `None`。會讀檔與解碼，呼叫端放在 `spawn_blocking` 裡。
pub fn embed(svg: &[u8], folder: &Path) -> Option<Vec<u8>> {
    embed_with(svg, &LocalSource { folder })
}

/// 同 [`embed`]，照片從 `src` 來（本機資料夾或遠端預先抓好的一批）。
pub fn embed_with(svg: &[u8], src: &dyn PhotoSource) -> Option<Vec<u8>> {
    if !wants_embed(svg) {
        return None;
    }
    let text = std::str::from_utf8(svg).ok()?;
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut embedded = 0usize;
    let mut attempts = 0usize;
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let Some(lt) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..lt]);
        let at = &rest[lt..];
        // 註解、CDATA、宣告與處理指令原樣跳過，裡面的 `<image` 不算。
        let skip = [("<!--", "-->"), ("<![CDATA[", "]]>"), ("<?", "?>"), ("<!", ">")].into_iter().find(|(open, _)| at.starts_with(open));
        if let Some((open, close)) = skip {
            let end = at[open.len()..].find(close).map(|e| open.len() + e + close.len()).unwrap_or(at.len());
            out.push_str(&at[..end]);
            i += lt + end;
            continue;
        }
        let is_image = at.starts_with("<image") && at[6..].starts_with(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>');
        let Some(tag_len) = tag_end(at) else {
            out.push_str(at);
            break;
        };
        let tag = &at[..tag_len];
        if is_image {
            let rewritten = rewrite_image_tag(tag, src, &mut embedded, &mut attempts, text.len() + out.len());
            changed |= rewritten.is_some();
            out.push_str(rewritten.as_deref().unwrap_or(tag));
        } else {
            out.push_str(tag);
        }
        i += lt + tag_len;
    }
    changed.then(|| out.into_bytes())
}

/// 從 `<` 開始的標籤到 `>`（含）的長度；引號裡的 `>` 不算。沒收尾回 `None`。
fn tag_end(at: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (idx, c) in at.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(idx + 1),
            _ => {}
        }
    }
    None
}

/// 一個 `<image …>` 標籤：`href`／`xlink:href` 是相對路徑的就換成 data URI 或拿掉；沒動到回 `None`。
/// `size_so_far` 是目前輸出的估計大小，用來守 [`OUT_MAX`]；`attempts` 是整份 SVG 已嘗試解的引用數，用來守 [`MAX_ATTEMPTS`]。
fn rewrite_image_tag(tag: &str, src: &dyn PhotoSource, embedded: &mut usize, attempts: &mut usize, size_so_far: usize) -> Option<String> {
    let mut out = String::with_capacity(tag.len());
    let mut changed = false;
    let bytes = tag.as_bytes();
    let mut i = "<image".len();
    out.push_str(&tag[..i]);
    loop {
        // 屬性之間的空白
        let ws_start = i;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let ws = &tag[ws_start..i];
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let name = &tag[name_start..i];
        if name.is_empty() {
            out.push_str(&tag[ws_start..]);
            break;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'=' {
            out.push_str(&tag[ws_start..i]);
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let Some(&q) = bytes.get(j).filter(|b| matches!(b, b'"' | b'\'')) else {
            // 沒加引號的屬性值不是合法 XML：剩下的原樣照抄。
            out.push_str(&tag[ws_start..]);
            break;
        };
        let Some(vlen) = tag[j + 1..].find(q as char) else {
            out.push_str(&tag[ws_start..]);
            break;
        };
        let value = &tag[j + 1..j + 1 + vlen];
        let end = j + 1 + vlen + 1;
        let is_href = name == "href" || name.rsplit_once(':').is_some_and(|(_, local)| local == "href");
        if !is_href {
            out.push_str(&tag[ws_start..end]);
            i = end;
            continue;
        }
        let v = unescape(value.trim());
        if v.starts_with('#') || v.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("data:")) {
            out.push_str(&tag[ws_start..end]);
        } else {
            changed = true;
            *attempts += 1;
            let outcome = if *embedded >= MAX_IMAGES || *attempts > MAX_ATTEMPTS { Err("too_many_images") } else { resolve(src, &v) };
            match outcome {
                Ok(uri) if size_so_far + out.len() + uri.len() <= OUT_MAX => {
                    *embedded += 1;
                    out.push_str(ws);
                    out.push_str(name);
                    out.push_str("=\"");
                    out.push_str(&uri);
                    out.push('"');
                }
                other => {
                    let why = other.err().unwrap_or("svg_too_large");
                    tracing::info!(href = %v, reason = why, "share svg: image not embedded");
                    out.push_str(ws);
                    out.push_str("data-am-embed=\"");
                    out.push_str(why);
                    out.push('"');
                }
            }
        }
        i = end;
    }
    changed.then_some(out)
}

/// XML 屬性值裡的實體（`&amp;`、`&#x41;`…）。認不得的原樣留著。
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p..];
        let Some(semi) = after.find(';').filter(|&n| n <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let ent = &after[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `%XX` 解碼；解出來不是 UTF-8 或格式不對回 `None`。
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 相對路徑拆成段：scheme（第一段有 `:`）、`/` 開頭、`\`、控制字元、空段、`.`／`..`、`.` 開頭的段一律 `None`。
/// 開頭的 `./` 可以。
pub fn safe_rel(href: &str) -> Option<Vec<&str>> {
    let href = href.strip_prefix("./").unwrap_or(href);
    if href.is_empty() || href.starts_with('/') || href.contains('\\') || href.contains(['?', '#']) || href.chars().any(char::is_control) {
        return None;
    }
    let parts: Vec<&str> = href.split('/').collect();
    if parts[0].contains(':') {
        return None;
    }
    parts.iter().all(|p| !p.is_empty() && !p.starts_with('.')).then_some(parts)
}

/// 解一個 href：找得到（原樣或 `%XX` 解碼後）就回 data URI，不然回理由。
fn resolve(src: &dyn PhotoSource, href: &str) -> Embedded {
    let mut candidates = vec![href.to_string()];
    if href.contains('%') {
        candidates.extend(percent_decode(href));
    }
    let mut why = "not_relative";
    for c in &candidates {
        let Some(parts) = safe_rel(c) else { continue };
        match load(src, &parts) {
            Err("not_found") => why = "not_found",
            other => return other,
        }
    }
    Err(why)
}

/// 查身分（不跟連結）、查快取、不在快取就讀檔縮圖。
fn load(src: &dyn PhotoSource, parts: &[&str]) -> Embedded {
    let meta = src.stat(parts)?;
    let key = key_of(&src.scope(), &parts.join("/"), &meta);
    if let Some(hit) = cache_get(&key) {
        return hit;
    }
    let result = if meta.len > SOURCE_MAX { Err("source_too_large") } else { src.read(parts, SOURCE_MAX).and_then(|bytes| to_data_uri(&bytes)) };
    cache_put(key, result.clone());
    result
}

/// 看檔頭判斷是不是收的圖；HEIC/HEIF 另外認出來，理由寫清楚。
fn sniff(bytes: &[u8]) -> Result<ImageFormat, &'static str> {
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && matches!(&bytes[8..12], b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"avif") {
        // HEIC 要 libheif（C 函式庫）才解得開，daemon 不帶；iPhone 從瀏覽器上傳時通常已經轉成 JPEG。
        return Err("heic_unsupported");
    }
    match image::guess_format(bytes) {
        Ok(f @ (ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP | ImageFormat::Gif)) => Ok(f),
        _ => Err("not_an_image"),
    }
}

/// 解碼 → 修正方向 → 縮到長邊 [`LONG_SIDE`] → 編碼成 data URI。
fn to_data_uri(bytes: &[u8]) -> Embedded {
    #[cfg(test)]
    DECODES.with(|d| d.set(d.get() + 1));
    let format = sniff(bytes)?;
    let mut reader = ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(DECODE_SIDE_MAX);
    limits.max_image_height = Some(DECODE_SIDE_MAX);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| "decode_failed")?;
    let (w, h) = decoder.dimensions();
    if w as u64 * h as u64 > PIXEL_MAX {
        return Err("image_too_large");
    }
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|_| "decode_failed")?;
    img.apply_orientation(orientation);
    for side in [LONG_SIDE, LONG_SIDE * 3 / 4] {
        let img = fit(&img, side);
        let (mime, data) = encode(&img)?;
        let uri = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&data));
        if uri.len() <= EMBED_MAX {
            return Ok(uri.into());
        }
    }
    Err("image_too_large")
}

fn fit(img: &DynamicImage, side: u32) -> DynamicImage {
    if img.width().max(img.height()) <= side {
        return img.clone();
    }
    img.resize(side, side, image::imageops::FilterType::CatmullRom)
}

/// 有半透明像素才用 PNG，其餘一律 JPEG。
fn encode(img: &DynamicImage) -> Result<(&'static str, Vec<u8>), &'static str> {
    let mut data = Vec::new();
    let translucent = img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p.0[3] < 255);
    if translucent {
        img.write_to(&mut std::io::Cursor::new(&mut data), ImageFormat::Png).map_err(|_| "encode_failed")?;
        Ok(("image/png", data))
    } else {
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, JPEG_QUALITY);
        img.to_rgb8().write_with_encoder(enc).map_err(|_| "encode_failed")?;
        Ok(("image/jpeg", data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let base = crate::share::test_dirs::track(std::env::temp_dir().join(format!("am-compose-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(base.join("inbox")).unwrap();
        std::fs::canonicalize(base).unwrap()
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Jpeg).unwrap();
        out
    }

    fn svg(body: &str) -> String {
        format!(r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 800 600">{body}</svg>"#)
    }

    /// 嵌完之後一定要是解得開的 XML，回所有 `<image>` 的 (href, data-am-embed)。
    fn images(out: &[u8]) -> Vec<(Option<String>, Option<String>)> {
        let mut r = quick_xml::Reader::from_reader(out);
        let mut buf = Vec::new();
        let mut found = Vec::new();
        loop {
            match r.read_event_into(&mut buf).expect("嵌完要是合法 XML") {
                quick_xml::events::Event::Eof => break,
                quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e) if e.name().as_ref() == b"image" => {
                    let mut href = None;
                    let mut mark = None;
                    for a in e.attributes() {
                        let a = a.expect("屬性要合法");
                        let v = String::from_utf8(a.value.to_vec()).unwrap();
                        match a.key.as_ref() {
                            b"href" | b"xlink:href" => href = Some(v),
                            b"data-am-embed" => mark = Some(v),
                            _ => {}
                        }
                    }
                    found.push((href, mark));
                }
                _ => {}
            }
            buf.clear();
        }
        found
    }

    fn decode_uri(uri: &str) -> DynamicImage {
        let b64 = uri.split_once(";base64,").unwrap().1;
        image::load_from_memory(&base64::engine::general_purpose::STANDARD.decode(b64).unwrap()).unwrap()
    }

    #[test]
    fn inbox_photos_are_embedded_and_shrunk() {
        let dir = scratch("embed");
        std::fs::write(dir.join("inbox/01X-IMG_3801.jpeg"), jpeg(3200, 2400)).unwrap();
        std::fs::create_dir_all(dir.join("pics/a")).unwrap();
        std::fs::write(dir.join("pics/a/small.jpg"), jpeg(400, 300)).unwrap();
        let src = svg(
            r#"<defs><clipPath id="c"><circle cx="200" cy="200" r="150"/></clipPath></defs>
               <image href="inbox/01X-IMG_3801.jpeg" x="0" y="0" width="400" height="300" preserveAspectRatio="xMidYMid slice" clip-path="url(#c)"/>
               <image xlink:href='./pics/a/small.jpg' width="100" height="75"></image>
               <!-- <image href="inbox/01X-IMG_3801.jpeg"/> 註解裡的不算 -->"#,
        );
        let out = embed(src.as_bytes(), &dir).expect("有照片要嵌");
        let imgs = images(&out);
        assert_eq!(imgs.len(), 2, "註解裡的 <image> 不算");
        for (href, mark) in &imgs {
            assert!(href.as_deref().unwrap().starts_with("data:image/jpeg;base64,"), "{href:?}");
            assert_eq!(mark, &None);
        }
        let big = decode_uri(imgs[0].0.as_deref().unwrap());
        assert_eq!((big.width(), big.height()), (LONG_SIDE, 1200), "長邊縮到上限、比例不變");
        let small = decode_uri(imgs[1].0.as_deref().unwrap());
        assert_eq!((small.width(), small.height()), (400, 300), "小圖不放大");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(r#"preserveAspectRatio="xMidYMid slice" clip-path="url(#c)""#), "其他屬性原樣");
        assert!(text.contains("<!-- <image href=\"inbox/01X-IMG_3801.jpeg\"/>"), "註解原樣");
        assert!(std::fs::read(dir.join("inbox/01X-IMG_3801.jpeg")).unwrap().len() > 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn escapes_schemes_and_links_are_refused() {
        let dir = scratch("escape");
        let outside = scratch("outside");
        std::fs::write(outside.join("secret.jpg"), jpeg(10, 10)).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.jpg"), dir.join("inbox/link.jpg")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("linkdir")).unwrap();
        std::fs::write(dir.join("inbox/hard-src.jpg"), jpeg(10, 10)).unwrap();
        std::fs::hard_link(dir.join("inbox/hard-src.jpg"), dir.join("inbox/hard.jpg")).unwrap();
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/x.jpg"), jpeg(10, 10)).unwrap();
        let rel_out = format!("../{}/secret.jpg", outside.file_name().unwrap().to_str().unwrap());
        let abs_out = outside.join("secret.jpg").to_string_lossy().into_owned();
        let bad = [
            rel_out.as_str(),
            "inbox/../../x.jpg",
            abs_out.as_str(),
            "file:///etc/passwd",
            "http://example.com/a.jpg",
            "https://example.com/a.jpg",
            "//example.com/a.jpg",
            "inbox%2F..%2F..%2Fx.jpg",
            "inbox\\link.jpg",
            "inbox/link.jpg",
            "linkdir/secret.jpg",
            "inbox/hard.jpg",
            ".claude/x.jpg",
            "javascript:alert(1)",
        ];
        let body: String = bad.iter().map(|h| format!(r#"<image href="{}" width="1" height="1"/>"#, h.replace('&', "&amp;"))).collect();
        let out = embed(svg(&body).as_bytes(), &dir).expect("要拿掉 href");
        let imgs = images(&out);
        assert_eq!(imgs.len(), bad.len());
        for ((href, mark), h) in imgs.iter().zip(bad) {
            assert_eq!(href, &None, "{h} 的 href 要拿掉");
            assert!(mark.is_some(), "{h} 要留理由");
        }
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn non_images_are_refused_and_fragments_data_stay() {
        let dir = scratch("nonimg");
        std::fs::write(dir.join("inbox/notes.jpg"), b"<html>not a photo</html>").unwrap();
        std::fs::write(dir.join("inbox/photo.heic"), [&[0, 0, 0, 24][..], b"ftypheic", &[0; 16]].concat()).unwrap();
        std::fs::write(dir.join("inbox/CLAUDE.md"), b"# hi").unwrap();
        let src = svg(
            r##"<image href="inbox/notes.jpg"/><image href="inbox/photo.heic"/><image href="inbox/CLAUDE.md"/>
                <image href="inbox/missing.jpg"/><image href="#sym"/><image href="data:image/png;base64,AAAA"/>"##,
        );
        let out = embed(src.as_bytes(), &dir).unwrap();
        let marks: Vec<_> = images(&out).into_iter().map(|(h, m)| (h, m)).collect();
        assert_eq!(marks[0], (None, Some("not_an_image".into())));
        assert_eq!(marks[1], (None, Some("heic_unsupported".into())));
        assert_eq!(marks[2], (None, Some("not_an_image".into())));
        assert_eq!(marks[3], (None, Some("not_found".into())));
        assert_eq!(marks[4], (Some("#sym".into()), None));
        assert_eq!(marks[5], (Some("data:image/png;base64,AAAA".into()), None));
        // 沒有 <image>、或只有 #id／data: 的：原樣送。
        assert!(embed(svg("<rect/>").as_bytes(), &dir).is_none());
        assert!(embed(svg(r##"<image href="#a"/>"##).as_bytes(), &dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exif_orientation_is_applied() {
        let dir = scratch("exif");
        // 200x100 的 JPEG，EXIF Orientation=6（順時針轉 90°）：嵌進去的要是 100x200。
        let raw = jpeg(200, 100);
        let exif: &[u8] = &[
            b'E', b'x', b'i', b'f', 0, 0, b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0,
        ];
        let mut withexif = raw[..2].to_vec();
        withexif.extend([0xFF, 0xE1]);
        withexif.extend(((exif.len() + 2) as u16).to_be_bytes());
        withexif.extend(exif);
        withexif.extend(&raw[2..]);
        std::fs::write(dir.join("inbox/rot.jpg"), withexif).unwrap();
        let out = embed(svg(r#"<image href="inbox/rot.jpg"/>"#).as_bytes(), &dir).unwrap();
        let img = decode_uri(images(&out)[0].0.as_deref().unwrap());
        assert_eq!((img.width(), img.height()), (100, 200));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn results_are_cached_until_the_photo_changes() {
        let dir = scratch("cache");
        std::fs::write(dir.join("inbox/p.jpg"), jpeg(300, 200)).unwrap();
        let src = svg(r#"<image href="inbox/p.jpg"/>"#);
        let before = decodes_for_test();
        let a = embed(src.as_bytes(), &dir).unwrap();
        let b = embed(src.as_bytes(), &dir).unwrap();
        assert_eq!(a, b);
        assert_eq!(decodes_for_test() - before, 1, "第二次走快取");
        // 換一張（大小不同）就重算。
        std::fs::remove_file(dir.join("inbox/p.jpg")).unwrap();
        std::fs::write(dir.join("inbox/p.jpg"), jpeg(600, 200)).unwrap();
        let c = embed(src.as_bytes(), &dir).unwrap();
        assert_ne!(a, c);
        assert!(decodes_for_test() - before >= 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_image_over_the_pixel_budget_is_refused_before_decode() {
        let dir = scratch("pixels");
        let img = image::GrayImage::from_pixel(8200, 6200, image::Luma([7u8]));
        let mut png = Vec::new();
        DynamicImage::ImageLuma8(img).write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png).unwrap();
        std::fs::write(dir.join("inbox/huge.png"), png).unwrap();
        let out = embed(svg(r#"<image href="inbox/huge.png"/>"#).as_bytes(), &dir).unwrap();
        let found = images(&out);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.as_deref(), Some("image_too_large"));
        assert!(!String::from_utf8(out).unwrap().contains("data:image"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_refs_count_against_the_attempt_cap() {
        let dir = scratch("attempts");
        let mut body = String::new();
        for i in 0..40 {
            let mut junk = b"\x89PNG\r\n\x1a\n".to_vec();
            junk.extend_from_slice(format!("garbage-{i}").as_bytes());
            std::fs::write(dir.join(format!("inbox/bad{i}.png")), junk).unwrap();
            body.push_str(&format!(r#"<image href="inbox/bad{i}.png"/>"#));
        }
        let before = decodes_for_test();
        let out = embed(svg(&body).as_bytes(), &dir).unwrap();
        assert!(decodes_for_test() - before <= MAX_ATTEMPTS, "解碼次數不得超過嘗試上限");
        let found = images(&out);
        assert_eq!(found.len(), 40);
        assert_eq!(found[MAX_ATTEMPTS - 1].1.as_deref(), Some("decode_failed"));
        for (_, mark) in &found[MAX_ATTEMPTS..] {
            assert_eq!(mark.as_deref(), Some("too_many_images"));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_normal_phone_photo_still_embeds() {
        let dir = scratch("phone");
        std::fs::write(dir.join("inbox/p.jpg"), jpeg(3200, 2400)).unwrap();
        let out = embed(svg(r#"<image href="inbox/p.jpg"/>"#).as_bytes(), &dir).unwrap();
        let found = images(&out);
        assert!(found[0].0.as_deref().is_some_and(|h| h.starts_with("data:image/jpeg;base64,")));
        assert_eq!(found[0].1, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn safe_rel_only_accepts_plain_relative_paths() {
        assert_eq!(safe_rel("inbox/a.jpg"), Some(vec!["inbox", "a.jpg"]));
        assert_eq!(safe_rel("./inbox/a b.jpg"), Some(vec!["inbox", "a b.jpg"]));
        for bad in ["", "/a.jpg", "../a.jpg", "inbox//a.jpg", "inbox/./a.jpg", "C:/a.jpg", "a:b", "inbox/a.jpg?x=1", "inbox/a.jpg#f", ".hidden/a.jpg"] {
            assert_eq!(safe_rel(bad), None, "{bad:?}");
        }
    }
}
