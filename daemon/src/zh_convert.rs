//! 簡體 → 台灣繁體（OpenCC `s2twp`，含台灣用詞）：本地字典轉換，**不呼叫任何 API、不計費、不受保險絲限制**。
//!
//! 給「assistant 訊息是簡體中文」的自動翻譯用（使用者 2026-11-11）：`message_translate::translate` 遇到這類訊息直接在本地轉好，
//! 結果寫進同一份 `message-translate/` 快取（model 記 [`MODEL`]、費用 0），網頁與推播摘要都讀這份。
//!
//! - 偵測（[`is_mostly_simplified`]）：只看文字段（圍欄程式碼、行內 code、網址不算）。簡體專用字（[`SIMPLIFIED`]）至少 3 個、
//!   且佔漢字 5% 以上。假名佔漢字四分之一以上視為日文，不算簡體（日文的新字體如「国」「会」會被誤判）。
//! - 轉換（[`to_traditional`]）：文字段走 s2twp，程式碼區塊、行內 code、網址原樣保留。
//! - 只在「不是主要英文」時用本地轉換（[`local_applies`]）：主要英文（網頁 `isMostlyEnglish` 同一套規則）照舊送 LLM 翻成繁中。
//!
//! 網頁端的同一份偵測在 `web/src/lib/zhSimplified.ts`（字表 `SIMPLIFIED_CHARS` 與 [`SIMPLIFIED`] 同一份資料）。

use std::collections::HashSet;
use std::sync::OnceLock;

use ferrous_opencc::config::BuiltinConfig;
use ferrous_opencc::OpenCC;

/// 快取裡記的 model：本地轉換，不是 API 模型。
pub(crate) const MODEL: &str = "opencc-s2twp";

/// 簡體專用字（OpenCC STCharacters 中簡繁不同的 CJK 基本區字，來源與網頁字表相同）。
const SIMPLIFIED: &str = include_str!("zh_simplified_chars.txt");

fn simplified_set() -> &'static HashSet<char> {
    static SET: OnceLock<HashSet<char>> = OnceLock::new();
    SET.get_or_init(|| SIMPLIFIED.chars().collect())
}

fn converter() -> &'static OpenCC {
    static CC: OnceLock<OpenCC> = OnceLock::new();
    CC.get_or_init(|| OpenCC::from_config(BuiltinConfig::S2twp).expect("embedded OpenCC s2twp config"))
}

/// 切段：`(要轉換, 段落)`。圍欄程式碼區塊（未閉合則到結尾）、行內 code（同一行內成對）、`http(s)://` 網址到空白為止，都是不轉換的段。
fn segments(text: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut seg_start = 0;
    let mut i = 0;
    while i < text.len() {
        let r = &text[i..];
        let keep_end = if r.starts_with("```") {
            Some(match r[3..].find("```") {
                Some(j) => i + 3 + j + 3,
                None => text.len(),
            })
        } else if r.starts_with('`') {
            match r[1..].find(['`', '\n']) {
                Some(j) if r[1 + j..].starts_with('`') => Some(i + 1 + j + 1),
                _ => None,
            }
        } else if r.starts_with("http://") || r.starts_with("https://") {
            Some(i + r.find(char::is_whitespace).unwrap_or(r.len()))
        } else {
            None
        };
        match keep_end {
            Some(end) => {
                if seg_start < i {
                    out.push((true, &text[seg_start..i]));
                }
                out.push((false, &text[i..end]));
                i = end;
                seg_start = end;
            }
            None => i += r.chars().next().map_or(1, char::len_utf8),
        }
    }
    if seg_start < text.len() {
        out.push((true, &text[seg_start..]));
    }
    out
}

/// 「主要是簡體」：文字段裡簡體專用字夠多、且是中文（不是日文）。見模組說明。
pub(crate) fn is_mostly_simplified(text: &str) -> bool {
    let (mut han, mut simp, mut kana) = (0usize, 0usize, 0usize);
    for (prose, seg) in segments(text) {
        if !prose {
            continue;
        }
        for c in seg.chars() {
            match c {
                '\u{4E00}'..='\u{9FFF}' => {
                    han += 1;
                    if simplified_set().contains(&c) {
                        simp += 1;
                    }
                }
                '\u{3040}'..='\u{30FF}' => kana += 1,
                _ => {}
            }
        }
    }
    if kana > 0 && kana * 4 >= han {
        return false;
    }
    simp >= 3 && simp * 20 >= han
}

/// 這則要不要走本地轉換：主要是簡體、而且不是主要英文（主要英文照舊走 LLM，它的譯文本來就是繁體）。
pub(crate) fn local_applies(text: &str) -> bool {
    is_mostly_simplified(text) && !crate::message_translate::is_mostly_english(text)
}

/// 簡體 → 台灣繁體；程式碼區塊、行內 code、網址原樣保留。
pub(crate) fn to_traditional(text: &str) -> String {
    let cc = converter();
    segments(text)
        .into_iter()
        .map(|(conv, seg)| if conv { cc.convert(seg) } else { seg.to_string() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMP: &str = "这个问题我们需要修复一下，修复之后请再运行测试，确认没有问题了再告诉我们。";
    const TRAD: &str = "這個問題我們需要修復一下，修復之後請再執行測試，確認沒有問題了再告訴我們。";
    const JA: &str = "国会の議論を見て、会議の内容を社員に共有しました。ありがとうございます。";

    #[test]
    fn simplified_is_detected_traditional_english_and_japanese_are_not() {
        assert!(is_mostly_simplified(SIMP));
        assert!(!is_mostly_simplified(TRAD), "繁體不動");
        assert!(!is_mostly_simplified(JA), "日文不動（新字體不算簡體）");
        assert!(!is_mostly_simplified("I fixed the parser bug and re-ran every test in the suite."), "英文不算");
        assert!(!is_mostly_simplified("好"), "太短、沒有簡體字");
    }

    #[test]
    fn simplified_converts_to_taiwan_traditional() {
        assert_eq!(to_traditional(SIMP), TRAD);
        assert!(local_applies(SIMP));
        assert!(!local_applies(TRAD));
        assert!(!local_applies(JA));
    }

    #[test]
    fn mixed_simplified_dominant_converts_but_english_dominant_stays_on_llm() {
        let mixed = "修好了 parser 的 bug，这个问题我们需要修复一下，请再运行测试。";
        assert!(local_applies(mixed), "中英混雜、簡體為主：只轉繁，不送英翻中");
        let english = "I fixed the parser bug and re-ran every test in the whole suite again today, all green; 这个问题。";
        assert!(is_mostly_simplified(english));
        assert!(!local_applies(english), "英文為主照舊走 LLM");
    }

    #[test]
    fn code_urls_and_inline_code_are_kept_verbatim() {
        let text = "这个问题我们需要修复：\n```rust\nlet 这个 = \"这个\";\n```\n运行 `这个命令` 然后看 https://example.com/这个 结果，这个问题就好了。";
        let out = to_traditional(text);
        assert!(out.contains("```rust\nlet 这个 = \"这个\";\n```"), "圍欄程式碼原樣: {out}");
        assert!(out.contains("`这个命令`"), "行內 code 原樣: {out}");
        assert!(out.contains("https://example.com/这个"), "網址原樣: {out}");
        assert!(out.starts_with("這個問題我們需要修復："), "{out}");
    }

    #[test]
    fn simplified_chars_list_has_the_expected_pairs() {
        for c in ['个', '们', '国', '问', '这', '为'] {
            assert!(simplified_set().contains(&c), "{c} 應在簡體字表");
        }
        for c in ['個', '們', '國', '問', '這', '為', '的'] {
            assert!(!simplified_set().contains(&c), "{c} 是繁體或兩邊同形，不應在表內");
        }
    }
}
