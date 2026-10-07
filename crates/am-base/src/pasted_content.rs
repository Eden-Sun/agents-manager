//! claude 把**貼上**的 prompt 包成 `<pasted_content id="…">` 才寫進 transcript（#218）。
//!
//! 2.1.27x 的伺服器旗標 `tengu_virtual_pancake` 開著時，CLI 把貼上事件的內容包起來再送給模型（系統提示會告訴模型
//! 標籤裡的字是別處貼來的資料），transcript 的 user 列存的也是包過的字：
//!
//! ```text
//! \n\n<pasted_content id="c4ab">\n<貼上的字>\n</pasted_content id="c4ab">\n
//! ```
//!
//! 2.1.278 執行檔＋2026-09-19 真機實測（`fixtures/claude_2.1.278_pasted_content.jsonl`）：
//! - id＝`sha256(session id)` 的前 4 個 hex，同一個 session 固定；
//! - 會包的是 CLI 認得的**貼上事件**：herdr `agent.prompt`、人在終端貼上、bot 用 `herdr agent prompt` 轉的訊息。
//!   daemon 自己的打字路線（`pane.send_text`）不會被包——短句、4 行、880 字都實測過；
//! - 一般貼上先 trim，少於 20 字不包；大段貼上（超過 800 字或超過 2 個換行，框裡摺成 `[Pasted text #N]`）整段包；
//! - 整則 prompt（包括沒被包的部分）裡的 `<pasted_content`／`</pasted_content` 被跳脫成 `<\pasted_content`／`<\/pasted_content`，
//!   形近字（全形 `＜` 等）也被換成 ASCII 的 `<\`；
//! - send-now（`ctrl+x ctrl+s`）送出的那一則不包。
//!
//! 這裡照 CLI 自己拆包的規則（`zet`／`Mue`）把區塊換回裡面的字，只認格式完全對的區塊，不做模糊比對。還原不了的：
//! 原文本來就寫著 `<\pasted_content`（會被當成跳脫過的還原）、形近字被換成 ASCII 的那種（對不上，照舊當成證不出來）。

use std::borrow::Cow;

const OPEN: &str = "<pasted_content id=\"";

fn is_id(p: &str) -> bool {
    p.len() == 4 && p.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// transcript 裡的 user 文字 → 使用者當初送出的字。沒有包也沒有跳脫的原樣借出。
pub fn original(text: &str) -> Cow<'_, str> {
    let unwrapped = unwrap(text);
    let base = unwrapped.as_deref().unwrap_or(text);
    if !base.contains("<\\pasted_content") && !base.contains("<\\/pasted_content") {
        return match unwrapped {
            Some(u) => Cow::Owned(u),
            None => Cow::Borrowed(text),
        };
    }
    Cow::Owned(base.replace("<\\pasted_content", "<pasted_content").replace("<\\/pasted_content", "</pasted_content"))
}

/// transcript 這一則是不是我們送的 `sent`。一字不差的照舊；CLI 改寫過的（包起來、跳脫）比還原後的字，
/// 頭尾空白不算——包之前 CLI 先把貼上的字 trim 過，大段貼上的最後一個換行也吃在標籤裡。
pub fn is_sent(logged: &str, sent: &str) -> bool {
    logged == sent || matches!(original(logged), Cow::Owned(o) if o.trim() == sent.trim())
}

/// CLI 的 `zet`＋`Mue`：每個格式對的 `<pasted_content id="XXXX">\n…\n</pasted_content id="XXXX">` 換成裡面的字，
/// 開頭標籤前、結尾標籤後各吃掉最多兩個換行。找到開頭卻找不到對應的結尾就停（後面照原文）。一個都沒有回 `None`。
fn unwrap(e: &str) -> Option<String> {
    let (mut out, mut n, mut o, mut any) = (String::new(), 0usize, 0usize, false);
    while let Some(rel) = e[o..].find(OPEN) {
        let a = o + rel;
        let s = a + OPEN.len();
        let Some(id) = e.get(s..s + 4).filter(|p| is_id(p) && e[s + 4..].starts_with("\">\n")) else {
            o = s;
            continue;
        };
        let c = s + 4 + 3;
        let close = format!("</pasted_content id=\"{id}\">");
        // 從開頭標籤最後那個換行找起：空的區塊（`…">\n</…`）也認得。
        let Some(t) = e[c - 1..].find(&format!("\n{close}")).map(|i| c - 1 + i + 1) else { break };
        let mut l = a;
        for _ in 0..2 {
            if l > n && e.as_bytes()[l - 1] == b'\n' {
                l -= 1;
            }
        }
        out.push_str(&e[n..l]);
        out.push_str(e.get(c..t - 1).unwrap_or(""));
        n = t + close.len();
        for _ in 0..2 {
            if e.as_bytes().get(n) == Some(&b'\n') {
                n += 1;
            }
        }
        o = n;
        any = true;
    }
    any.then(|| out + &e[n..])
}
