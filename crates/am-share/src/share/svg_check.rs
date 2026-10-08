//! 分享 bot 的 outbox 裡 `.svg` 壞掉（不是 well-formed XML）就自動告訴那顆 bot 修（SPEC §20，使用者 2026-10-04）。
//!
//! 實例：ai-cc 改圖時把 `<text x="540" y="380"font-size="100" …>` 寫壞（屬性之間少空格），手機瀏覽器整張畫不出來，
//! 長輩在分享頁只看到一張空白圖。bot 自己不知道，要等擁有者發現。所以：
//!
//! - **什麼時候查**：分享頁讀檔案清單（`GET /s/{token}/api/files`）時，背景順便查清單上的 `.svg`。不另寫 watcher：daemon 沒有
//!   檔案監看的基礎建設，而分享頁正好在「載入、SSE resync、每一輪 bot 回完」時重讀清單——就是 end user 會看到那張圖的時候。
//!   同一個檔的同一版（mtime＋大小）只查一次。
//! - **查什麼**：quick-xml 管標籤配對與重複屬性；它放過的幾種這裡自己補——屬性之間少空格、`&` 不是合法的實體（有 DOCTYPE 的不查）、
//!   標籤到檔尾沒結束、根元素之後還有東西。
//! - **怎麼講**：壞了就以 daemon 名義（`relay_from = daemon`，擁有者那一類）送一則**後台**訊息給那顆 bot：分享頁不顯示它，也不顯示
//!   bot 對它的回覆（`share_reply_visible` 沒標）。同一個檔同一版（含錯誤與壞掉世代）只送一次——`client_request_id` 由 bot＋檔名＋版本指紋＋錯誤決定，
//!   送過（有那一筆 turn）就不再送；修好後若再度改壞（新版本／新 episode）會重新提醒（SPEC §20，issue #845）；bot 沒在跑就先起來再送（`start_send`）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use quick_xml::events::Event;
use sha2::Digest as _;


/// 單檔超過這麼大就不查（bot 畫的圖不會這麼大；查不完不如不查）。
pub const MAX_CHECK_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvgError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

/// 位元組位移 → 第幾行第幾欄（都從 1 起算，欄以字元計）。
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let mut at = offset;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let before = &text[..at];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0) + 1;
    (line, col)
}

fn err(text: &str, offset: usize, message: impl Into<String>) -> SvgError {
    let (line, col) = line_col(text, offset);
    SvgError { line, col, message: message.into() }
}

/// `&` 後面要是預先定義的五個實體或數字字元參照。回傳第一個壞掉的 `&` 的位移（相對 `s`）。
fn bad_entity(s: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'&' {
            let rest = &s[i + 1..];
            let end = rest.iter().position(|&b| b == b';')?;
            let name = &rest[..end];
            let ok = matches!(name, b"lt" | b"gt" | b"amp" | b"quot" | b"apos")
                || (name.len() > 2 && name[0] == b'#' && (name[1] == b'x' || name[1] == b'X') && name[2..].iter().all(u8::is_ascii_hexdigit))
                || (name.len() > 1 && name[0] == b'#' && name[1..].iter().all(u8::is_ascii_digit));
            if !ok || end == 0 || end > 32 {
                return Some(i);
            }
            i += end + 2;
        } else {
            i += 1;
        }
    }
    None
}

/// 一個標籤的屬性區（`attributes_raw`）：每個屬性值的結尾引號之後要接空白、`/` 或結束；值裡不能有 `<`、`&` 要合法。
/// 回傳（相對 `raw` 的位移, 說明）。
fn bad_attrs(raw: &[u8], check_entities: bool) -> Option<(usize, String)> {
    let mut i = 0;
    while i < raw.len() {
        let q = raw[i];
        if q == b'"' || q == b'\'' {
            let close = raw[i + 1..].iter().position(|&b| b == q)? + i + 1;
            let value = &raw[i + 1..close];
            if let Some(p) = value.iter().position(|&b| b == b'<') {
                return Some((i + 1 + p, "屬性值裡不能有 `<`".into()));
            }
            if check_entities {
                if let Some(p) = bad_entity(value) {
                    return Some((i + 1 + p, "屬性值裡的 `&` 不是合法的實體（要寫成 `&amp;`）".into()));
                }
            }
            let next = raw.get(close + 1).copied();
            if let Some(n) = next {
                if !(n.is_ascii_whitespace() || n == b'/') {
                    return Some((close + 1, "屬性之間少了空格".into()));
                }
            }
            i = close + 1;
        } else {
            i += 1;
        }
    }
    None
}

/// 是不是 well-formed XML。只回第一個錯誤。
pub fn check(text: &str) -> Result<(), SvgError> {
    let mut r = quick_xml::Reader::from_str(text);
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut roots = 0usize;
    let mut has_doctype = false;
    loop {
        let before = r.buffer_position() as usize;
        let ev = match r.read_event() {
            Ok(ev) => ev,
            Err(e) => return Err(err(text, r.error_position() as usize, format!("{e}"))),
        };
        match ev {
            Event::Eof => break,
            Event::DocType(_) => has_doctype = true,
            Event::Start(ref e) | Event::Empty(ref e) => {
                if stack.is_empty() {
                    roots += 1;
                    if roots > 1 {
                        return Err(err(text, before, "根元素之後還有另一個元素（整份只能有一個根）"));
                    }
                }
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let raw = e.attributes_raw();
                let raw_at = before + 1 + e.name().as_ref().len();
                if let Some((p, why)) = bad_attrs(raw, !has_doctype) {
                    return Err(err(text, raw_at + p, format!("<{name}> {why}")));
                }
                for a in e.attributes().with_checks(true) {
                    if let Err(x) = a {
                        return Err(err(text, before, format!("<{name}> 的屬性有問題：{x}")));
                    }
                }
                if matches!(ev, Event::Start(_)) {
                    stack.push((name, before));
                }
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Text(ref t) => {
                let bytes: &[u8] = t.as_ref();
                if stack.is_empty() && bytes.iter().any(|b| !b.is_ascii_whitespace()) {
                    let p = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(0);
                    return Err(err(text, before + p, "根元素外面不能有文字"));
                }
                if !has_doctype {
                    if let Some(p) = bad_entity(bytes) {
                        return Err(err(text, before + p, "文字裡的 `&` 不是合法的實體（要寫成 `&amp;`）"));
                    }
                }
            }
            // quick-xml 把文字裡的 `&名稱;` 切成獨立事件（`before` 指在 `&`）。
            Event::GeneralRef(ref g) => {
                let name: &[u8] = g.as_ref();
                let ok = matches!(name, b"lt" | b"gt" | b"amp" | b"quot" | b"apos") || name.first() == Some(&b'#');
                if !ok && !has_doctype {
                    return Err(err(text, before, format!("文字裡的 `&{};` 不是 XML 認得的實體（要寫成 `&amp;` 或數字參照）", String::from_utf8_lossy(name))));
                }
            }
            // quick-xml 把文字裡的 `&名稱;` 切成獨立事件（`before` 指在 `&`）。
            Event::GeneralRef(ref g) => {
                let name: &[u8] = g.as_ref();
                let ok = matches!(name, b"lt" | b"gt" | b"amp" | b"quot" | b"apos") || name.first() == Some(&b'#');
                if !ok && !has_doctype {
                    return Err(err(text, before, format!("文字裡的 `&{};` 不是 XML 認得的實體（要寫成 `&amp;` 或數字參照）", String::from_utf8_lossy(name))));
                }
            }
            _ => {}
        }
    }
    if let Some((name, at)) = stack.last() {
        return Err(err(text, *at, format!("<{name}> 到檔尾都沒有結束")));
    }
    if roots == 0 {
        return Err(err(text, 0, "找不到根元素"));
    }
    Ok(())
}

/// 先驗原始 bytes 是不是合法 UTF-8，再當 XML 查（issue #866）。分享頁送給瀏覽器的是原始 bytes，
/// 不能先 `from_utf8_lossy` 把壞位元組換成 `�` 再查——那樣查的是另一份檔。
pub fn check_bytes(data: &[u8]) -> Result<(), SvgError> {
    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(e) => {
            let at = e.valid_up_to();
            let valid = std::str::from_utf8(&data[..at]).unwrap_or_default();
            return Err(err(valid, valid.len(), format!("檔案不是合法 UTF-8 SVG（位元組 0x{:02X} 不是 UTF-8）", data[at])));
        }
    };
    check(text)
}

/// 給 bot 的那一則（後台，分享頁看不到）。
pub fn reminder(name: &str, e: &SvgError) -> String {
    format!(
        "〔系統〕圖檔 {name} 第 {} 行第 {} 欄格式壞了：{}。手機瀏覽器會整張畫不出來，請修好這個檔（存回 outbox 同一個檔名），修好就好，不必跟分享使用者解釋。",
        e.line, e.col, e.message
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileHealth {
    Healthy,
    Broken,
}

#[derive(Default)]
struct EpisodeTracker {
    generation: u64,
    health: Option<FileHealth>,
}

fn file_episodes() -> &'static Mutex<HashMap<(String, String), EpisodeTracker>> {
    static M: OnceLock<Mutex<HashMap<(String, String), EpisodeTracker>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn mark_broken(bot_id: &str, name: &str) -> u64 {
    let mut m = file_episodes().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tracker = m.entry((bot_id.to_string(), name.to_string())).or_default();
    tracker.health = Some(FileHealth::Broken);
    tracker.generation
}

fn mark_healthy(bot_id: &str, name: &str) {
    let mut m = file_episodes().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tracker = m.entry((bot_id.to_string(), name.to_string())).or_default();
    if tracker.health == Some(FileHealth::Broken) {
        tracker.generation = tracker.generation.wrapping_add(1);
    }
    tracker.health = Some(FileHealth::Healthy);
}

pub fn version_fingerprint(ino: u64, mtime_ns: i128, ctime_ns: i128, gen: u64, data: &[u8]) -> String {
    let mut h = sha2::Sha256::new();
    h.update(ino.to_le_bytes());
    h.update(mtime_ns.to_le_bytes());
    h.update(ctime_ns.to_le_bytes());
    h.update(gen.to_le_bytes());
    h.update(data);
    let digest = h.finalize();
    digest.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

/// 同一個 bot、同一個檔、同一版本（含壞掉世代）、同一個錯誤 → 同一個 `client_request_id`：同版本送過就不再送。
pub fn request_id(bot_id: &str, name: &str, version: &str, e: &SvgError) -> String {
    let h = sha2::Sha256::digest(format!("{bot_id}\n{name}\n{version}\n{}:{}:{}", e.line, e.col, e.message).as_bytes());
    let hex: String = h.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("share-svg-check-{hex}")
}

/// 查過的版本：(bot, 檔名) → 檔案版本（`scan_checked` 的 `version`：inode／大小／奈秒 mtime／ctime）。同一版不重查。
fn checked() -> &'static Mutex<HashMap<(String, String), String>> {
    static M: OnceLock<Mutex<HashMap<(String, String), String>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 這一版要不要查（沒查過、或版本變了）。要查就先記下，免得同時兩個清單請求各查一次。
pub(super) fn claim(bot_id: &str, name: &str, version: &str) -> bool {
    let mut m = checked().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (bot_id.to_string(), name.to_string());
    if m.get(&key).map(String::as_str) == Some(version) {
        return false;
    }
    m.insert(key, version.to_string());
    true
}

fn forget(bot_id: &str, name: &str) {
    let key = (bot_id.to_string(), name.to_string());
    checked().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&key);
    file_episodes().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&key);
}

/// SVG 檢查要從 `App` 拿的：能讀分享檔（[`crate::outbox::OutboxEnv`]），以及「提醒 bot 修檔案」這一步（走一般的 prompt／排隊路徑）。
pub trait SvgCheckEnv: crate::outbox::OutboxEnv {
    /// 以 daemon 的名義把 `text` 送給 bot（bot 起不來就排隊）；`crid` 是冪等鍵。失敗回錯誤說明（記 log 用）。
    fn remind_bot(app: &Arc<Self>, bot_id: &str, text: &str, crid: &str) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// 這一則送過了嗎（有那一筆 turn）。讀不到就當送過——寧可少提醒一次，不要每次讀清單都送。
async fn already_sent(app: &impl crate::outbox::ShareStorage, crid: &str) -> bool {
    !matches!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns WHERE client_request_id = ?").bind(crid).fetch_one(app.db_pool()).await,
        Ok(0)
    )
}

/// 查一個檔；壞了就提醒 bot（同一版本同一個錯誤只一次）。回傳查到的錯誤（測試用）。
pub async fn check_file<H: SvgCheckEnv>(app: &Arc<H>, bot_id: &str, name: &str) -> Option<SvgError> {
    let read = match crate::outbox::share_file_read_with_limit(app, bot_id, name, MAX_CHECK_BYTES as u64).await {
        Ok(r) => r,
        Err(crate::outbox::ShareFileError::TooLarge) => {
            // 超過 4 MiB 直接略過不查；不叫 forget，同一版本不重複排查。
            return None;
        }
        // 讀不到（剛被刪、暫時失敗）：下次讀清單再查。
        Err(_) => {
            forget(bot_id, name);
            return None;
        }
    };
    if read.data.len() > MAX_CHECK_BYTES {
        return None;
    }
    let e = match check_bytes(&read.data) {
        Ok(()) => {
            mark_healthy(bot_id, name);
            return None;
        }
        Err(e) => e,
    };
    let gen = mark_broken(bot_id, name);
    let vfp = version_fingerprint(read.ino, read.mtime_ns, read.ctime_ns, gen, &read.data);
    let crid = request_id(bot_id, name, &vfp, &e);
    if already_sent(app, &crid).await {
        return Some(e);
    }
    match H::remind_bot(app, bot_id, &reminder(name, &e), &crid).await {
        Ok(_) => tracing::info!(bot = bot_id, file = name, line = e.line, col = e.col, error = %e.message, "share bot svg is broken; told the bot to fix it"),
        Err(err) => {
            // 沒送成（例如分享使用者的訊息正排著）：下次讀清單再試。
            forget(bot_id, name);
            tracing::info!(bot = bot_id, file = name, error = ?err, "share bot svg is broken; could not tell the bot yet, will retry");
        }
    }
    Some(e)
}

/// 分享頁讀清單時叫：清單上每個 `.svg`（`files` 是 portal 回的 `{name, size, modified_at, version}`），沒查過的這一版在背景查。
pub fn spawn_check<H: SvgCheckEnv>(app: &Arc<H>, bot_id: &str, files: &[serde_json::Value]) {
    let todo: Vec<String> = files
        .iter()
        .filter_map(|f| {
            let name = f.get("name")?.as_str()?;
            if !name.to_ascii_lowercase().ends_with(".svg") {
                return None;
            }
            let size = f.get("size").and_then(|v| v.as_i64()).unwrap_or(-1);
            // 沒有 `version`（舊資料）就退回 modified_at-size。
            let version = match f.get("version").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
                Some(v) => v.to_string(),
                None => format!("{}-{size}", f.get("modified_at").and_then(|v| v.as_str()).unwrap_or("")),
            };
            if size > MAX_CHECK_BYTES as i64 {
                claim(bot_id, name, &version);
                return None;
            }
            claim(bot_id, name, &version).then(|| name.to_string())
        })
        .collect();
    if todo.is_empty() {
        return;
    }
    let (app, bot_id) = (app.clone(), bot_id.to_string());
    tokio::spawn(async move {
        for name in todo {
            check_file(&app, &bot_id, &name).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad(s: &str) -> SvgError {
        check(s).expect_err(s)
    }

    #[test]
    fn the_real_case_missing_space_between_attributes_is_caught_with_line_and_column() {
        let s = "<svg xmlns=\"http://www.w3.org/2000/svg\">\n  <text x=\"540\" y=\"380\"font-size=\"100\">嗨</text>\n</svg>";
        let e = bad(s);
        assert_eq!((e.line, e.col), (2, 24), "指在少了空格的那個位置（f）：{e:?}");
        assert!(e.message.contains("屬性之間少了空格"), "{e:?}");
        assert!(reminder("card.svg", &e).contains("圖檔 card.svg 第 2 行第 24 欄格式壞了"));
    }

    #[test]
    fn well_formed_svgs_pass() {
        for s in [
            "<svg/>",
            "<?xml version=\"1.0\"?>\n<!-- c -->\n<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 10\"><text x='1' y=\"2\">a &amp; b &#x4E2D; &#20013;</text><path d=\"M0 0\"/></svg>\n",
            "<svg><text\n  x=\"1\"\n  y=\"2\"\n>多行屬性</text></svg>",
            "<!DOCTYPE svg [<!ENTITY nbsp \"&#160;\">]><svg><text>a&nbsp;b</text></svg>",
        ] {
            assert_eq!(check(s), Ok(()), "{s}");
        }
    }

    #[test]
    fn invalid_utf8_bytes_are_caught_even_when_the_lossy_text_is_well_formed() {
        // 0xFF 不是合法 UTF-8；lossy 轉成 `�` 後結構完全正確，所以要在 bytes 層擋下來（issue #866）。
        let prefix = "<svg xmlns=\"http://www.w3.org/2000/svg\"><text>";
        let mut data = prefix.as_bytes().to_vec();
        data.extend_from_slice(b"\xFF</text></svg>");
        assert!(check(&String::from_utf8_lossy(&data)).is_ok(), "lossy 版本本身是 well-formed");
        let e = check_bytes(&data).expect_err("原始 bytes 不是合法 UTF-8");
        assert_eq!((e.line, e.col), (1, prefix.chars().count() + 1), "指在 0xFF 那個位置");
        assert!(e.message.contains("不是合法 UTF-8 SVG") && e.message.contains("0xFF"), "{e:?}");
    }

    #[test]
    fn invalid_utf8_with_utf8_encoding_declaration_is_still_caught() {
        let mut data = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg><text>".to_vec();
        data.extend_from_slice(b"\xC3</text></svg>"); // 截斷的多位元組序列
        let e = check_bytes(&data).expect_err("宣告 UTF-8 也不能含非法位元組");
        assert_eq!(e.line, 2, "{e:?}");
    }

    #[test]
    fn valid_utf8_bytes_go_through_the_xml_check_unchanged() {
        assert_eq!(check_bytes("<svg><text>嗨 &amp; ÿ</text></svg>".as_bytes()), Ok(()));
        assert!(check_bytes(b"<svg><g></svg>").unwrap_err().message.contains("</g>"), "結構錯誤照樣抓");
    }

    #[test]
    fn other_shapes_quick_xml_lets_through_are_caught() {
        assert!(bad("<svg><g></svg>").message.contains("</g>"), "標籤配錯");
        assert!(bad("<svg><g>").message.contains("沒有結束"), "到檔尾沒結束");
        assert!(bad("<svg/><svg/>").message.contains("根元素"), "兩個根");
        assert!(bad("<svg><text>a &nbsp; b</text></svg>").message.contains("實體"), "未定義的實體");
        assert!(check("<svg><text>AT&T</text></svg>").is_err(), "裸 &（quick-xml 自己報）");
        assert!(bad("<svg a=\"1\" a=\"2\"/>").message.contains("屬性"), "重複屬性");
        assert!(bad("").message.contains("根元素"), "空檔");
    }

    #[test]
    fn the_same_error_on_the_same_file_has_one_request_id() {
        let e = bad("<svg><g></svg>");
        assert_eq!(request_id("B1", "a.svg", "v1", &e), request_id("B1", "a.svg", "v1", &e));
        assert_ne!(request_id("B1", "a.svg", "v1", &e), request_id("B1", "b.svg", "v1", &e), "不同檔分開提醒");
        let other = bad("<svg><g>");
        assert_ne!(request_id("B1", "a.svg", "v1", &e), request_id("B1", "a.svg", "v1", &other), "同一個檔換了錯誤要再提醒");
        assert_ne!(request_id("B1", "a.svg", "v1", &e), request_id("B1", "a.svg", "v2", &e), "同一個檔換了版本要再提醒");
    }

    #[test]
    fn version_fingerprint_tracks_file_metadata_episode_and_contents() {
        let base = version_fingerprint(1, 2, 3, 4, b"broken");
        assert_eq!(base, version_fingerprint(1, 2, 3, 4, b"broken"));
        assert_ne!(base, version_fingerprint(1, 2, 4, 4, b"broken"), "ctime 改變表示檔案曾被改寫");
        assert_ne!(base, version_fingerprint(1, 2, 3, 5, b"broken"), "修好後的新壞掉 episode 要有新指紋");
        assert_ne!(base, version_fingerprint(1, 2, 3, 4, b"other"), "內容改變要有新指紋");
    }
}
