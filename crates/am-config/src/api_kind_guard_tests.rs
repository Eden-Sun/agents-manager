//! API bot kind 的判斷集中在 `am_config::{is_api_kind, api_provider, CLAUDE_API_KIND, api_kinds_sql!}`（openai-api 計畫 D2a）。
//! 守門：非測試碼不准再寫字面的 `'claude-api'`／`"claude-api"`——否則 openai-api 加進來時又會漏掉一處。
use crate::{api_provider, is_api_kind, API_BOT_KINDS, API_KINDS_SQL, CLAUDE_API_KIND, OPENAI_API_KIND};
use std::path::{Path, PathBuf};

#[test]
fn is_api_kind_and_api_provider_know_both_api_kinds_and_nothing_else() {
    assert!(is_api_kind("claude-api") && is_api_kind("openai-api"));
    for other in ["claude", "codex", "grok", "agy", "", "claude-api ", "OpenAI-API", "openai", "api"] {
        assert!(!is_api_kind(other), "{other:?}");
        assert_eq!(api_provider(other), None, "{other:?}");
    }
    assert_eq!(api_provider(CLAUDE_API_KIND), Some("anthropic"));
    assert_eq!(api_provider(OPENAI_API_KIND), Some("openai"));
    assert_eq!(API_BOT_KINDS, &["claude-api", "openai-api"]);
    // SQL 片段與常數同步，而且每個 kind 都在裡面。
    assert_eq!(API_KINDS_SQL, "('claude-api','openai-api')");
    assert_eq!(API_KINDS_SQL, crate::api_kinds_sql!());
    for k in API_BOT_KINDS {
        assert!(API_KINDS_SQL.contains(&format!("'{k}'")));
    }
    // provider 與 attempt 帳本的 provider 值一致（切片 C 的 CHECK）。
    for k in API_BOT_KINDS {
        assert!(matches!(api_provider(k), Some("anthropic" | "openai")));
    }
    // D2b 之前 openai-api 還不是合法 kind：行為完全不變。
    assert!(crate::valid_bot_kind("claude-api"));
    assert!(!crate::valid_bot_kind("openai-api"), "D2b 才開放");
}

/// 允許出現字面 `claude-api` 的非測試檔（以 repo 相對路徑）：kind 的定義處，以及與 kind 無關或必須是字面值的地方。
const ALLOWED: &[(&str, &str)] = &[
    ("crates/am-config/src/lib.rs", "kind 常數與 valid_bot_kind 的定義"),
    ("crates/am-config/src/api_bot.rs", "設定的單元測試（寫在檔案內）"),
    ("crates/am-base/src/db.rs", "bots.kind 的 CHECK 約束（schema 字面值；D2b 一併改）"),
    ("daemon/src/claude_api.rs", "資料目錄 `<data_dir>/claude-api/`（不是 bot kind）"),
];

fn skip_at(t: &[u8], i: usize) -> Option<usize> {
    let n = t.len();
    let starts = |s: &[u8]| t[i..].starts_with(s);
    if starts(b"//") {
        return Some(t[i..].iter().position(|&c| c == b'\n').map_or(n, |p| i + p + 1));
    }
    if starts(b"/*") {
        return Some(t[i..].windows(2).position(|w| w == b"*/").map_or(n, |p| i + p + 2));
    }
    if t[i] == b'r' && (i == 0 || !(t[i - 1].is_ascii_alphanumeric() || t[i - 1] == b'_')) {
        let mut j = i + 1;
        while j < n && t[j] == b'#' {
            j += 1;
        }
        if j < n && t[j] == b'"' {
            let mut close = vec![b'"'];
            close.extend(std::iter::repeat_n(b'#', j - i - 1));
            let body = j + 1;
            return Some(t[body..].windows(close.len()).position(|w| w == close.as_slice()).map_or(n, |p| body + p + close.len()));
        }
    }
    if t[i] == b'"' {
        let mut j = i + 1;
        while j < n && t[j] != b'"' {
            j += if t[j] == b'\\' { 2 } else { 1 };
        }
        return Some((j + 1).min(n));
    }
    if t[i] == b'\'' {
        if t[i..].starts_with(b"'\\") {
            return Some(t[i + 3..].iter().position(|&c| c == b'\'').map_or(n, |p| i + 3 + p + 1));
        }
        if i + 2 < n && t[i + 2] == b'\'' {
            return Some(i + 3);
        }
    }
    None
}

/// 被 `#[cfg(test)]`／`#[cfg(all(test, …))]` 掛住的 item 的位元組範圍（到對應的 `}` 或 `;`）。
fn test_regions(text: &str) -> Vec<(usize, usize)> {
    let t = text.as_bytes();
    let mut regions = Vec::new();
    let mut line_start = 0;
    while line_start < t.len() {
        let line_end = t[line_start..].iter().position(|&c| c == b'\n').map_or(t.len(), |p| line_start + p);
        let line = text[line_start..line_end].trim_start();
        if line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test") {
            let mut i = line_end;
            // 後續的屬性行一併跳過。
            loop {
                let rest = text[i..].trim_start();
                let skipped = text[i..].len() - rest.len();
                if rest.starts_with("#[") {
                    i += skipped + rest.find('\n').unwrap_or(rest.len());
                } else {
                    i += skipped;
                    break;
                }
            }
            let (mut k, mut depth, mut end) = (i, 0i32, t.len());
            while k < t.len() {
                if let Some(j) = skip_at(t, k) {
                    k = j;
                    continue;
                }
                match t[k] {
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => depth -= 1,
                    b'{' => {
                        let mut d = 1;
                        k += 1;
                        while k < t.len() && d > 0 {
                            if let Some(j) = skip_at(t, k) {
                                k = j;
                                continue;
                            }
                            match t[k] {
                                b'{' => d += 1,
                                b'}' => d -= 1,
                                _ => {}
                            }
                            k += 1;
                        }
                        end = k;
                        break;
                    }
                    b';' if depth == 0 => {
                        end = k + 1;
                        break;
                    }
                    _ => {}
                }
                k += 1;
            }
            regions.push((line_start, end));
            line_start = end.max(line_end + 1);
            continue;
        }
        line_start = line_end + 1;
    }
    regions
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target" || n == "node_modules" || n == "fixtures") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn is_test_file(rel: &str) -> bool {
    rel.ends_with("_test.rs")
        || rel.ends_with("_tests.rs")
        || rel.ends_with("/tests.rs")
        || rel.ends_with("daemon-tests-tests.rs")
        || rel.ends_with("test_support.rs")
        || rel.contains("/tests/")
        || rel.contains("/supervisor_tests/")
}

#[test]
fn no_literal_claude_api_kind_outside_the_kind_definitions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("daemon/src"), &mut files);
    rust_files(&root.join("crates"), &mut files);
    assert!(files.len() > 200, "掃到的檔案太少（{}）：路徑不對？", files.len());
    let mut offenders = Vec::new();
    for path in files {
        let rel = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if is_test_file(&rel) || ALLOWED.iter().any(|(a, _)| *a == rel) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if !text.contains("claude-api") {
            continue;
        }
        let regions = test_regions(&text);
        for (at, _) in text.match_indices("claude-api") {
            let quoted = at > 0 && matches!(text.as_bytes()[at - 1], b'\'' | b'"') && text[at + 10..].starts_with(['\'', '"']);
            if !quoted || regions.iter().any(|&(a, b)| a <= at && at < b) {
                continue;
            }
            let line = text[..at].matches('\n').count() + 1;
            offenders.push(format!("{rel}:{line}: {}", text.lines().nth(line - 1).unwrap_or("").trim()));
        }
    }
    assert!(
        offenders.is_empty(),
        "非測試碼出現字面 claude-api kind（用 am_config::is_api_kind／api_provider／api_kinds_sql!()，Claude 專屬的用 CLAUDE_API_KIND）：\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_guard_scanner_sees_through_test_modules_strings_and_comments() {
    let src = "fn live() { let a = \"x\"; }\n#[cfg(test)]\nmod t { fn f() { let k = \"claude-api\"; let c = '}'; } }\nfn after() {}\n// \"claude-api\"\n";
    let regions = test_regions(src);
    assert_eq!(regions.len(), 1);
    let at = src.find("claude-api").unwrap();
    assert!(regions[0].0 <= at && at < regions[0].1);
    assert!(regions[0].1 < src.find("fn after").unwrap() + 1, "區段在 mod 的右大括號就結束（字元字面值 '}}' 不算）");
}
