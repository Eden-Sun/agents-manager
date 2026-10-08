//! 守門（issue #907）：daemon 會推的每一種 WebSocket 事件都要寫在 `docs/API.md` §8 的事件表第一欄。
//! 掃 `daemon/src` 與 `crates/*/src` 裡 `emit("<type>", …)`／`emit_event("<type>", …)` 的字面值（跳過測試檔，
//! 以及檔案尾巴的測試模組）；新增事件忘了寫文件，這條測試就紅並列出名字。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 純測試／探針用的假事件，不是產品行為。
const NOT_DOCUMENTED: &[&str] = &["test_event", "capability_probe"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") && !path.file_name().is_some_and(|n| n.to_string_lossy().contains("test")) {
            out.push(path);
        }
    }
}

/// 去掉檔案尾巴的測試模組：從第一個「`#[cfg(test)]` 緊接著 `mod`／`#[path]`」起算。單獨掛在 `use`／`fn` 上的 `#[cfg(test)]`
/// 不算——它後面還有產品程式碼。
fn without_test_module(src: &str) -> &str {
    const MARK: &str = "#[cfg(test)]";
    let mut from = 0;
    while let Some(rel) = src[from..].find(MARK) {
        let at = from + rel;
        let after = src[at + MARK.len()..].trim_start();
        if after.starts_with("mod ") || after.starts_with("pub mod ") || after.starts_with("#[path") {
            return &src[..at];
        }
        from = at + MARK.len();
    }
    src
}

/// 一個檔案裡 `emit("x"`／`emit_event("x"` 的 x（呼叫可以跨行：`emit(\n "x"`）。
fn emitted_kinds(src: &str) -> Vec<String> {
    let src = without_test_module(src);
    let mut out = Vec::new();
    for call in ["emit(", "emit_event("] {
        let mut from = 0;
        while let Some(rel) = src[from..].find(call) {
            let at = from + rel;
            from = at + call.len();
            // 前一個字元是識別字的一部分（`fn emit_event(` 的 `emit_event`、`remit(`…）就不算呼叫。
            if src[..at].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let rest = src[from..].trim_start();
            let Some(rest) = rest.strip_prefix('"') else { continue };
            let Some(end) = rest.find('"') else { continue };
            let kind = &rest[..end];
            if !kind.is_empty() && kind.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                out.push(kind.to_string());
            }
        }
    }
    out
}

/// `docs/API.md` §8 事件表第一欄裡所有反引號包起來的 type。
fn documented_kinds(api_md: &str) -> BTreeSet<String> {
    let start = api_md.find("## 8. WebSocket").expect("API.md 要有 §8 WebSocket");
    let section = &api_md[start..];
    let end = section.find("終端畫面不走 WS").expect("§8 事件表後要有「終端畫面不走 WS」");
    let mut kinds = BTreeSet::new();
    for line in section[..end].lines().filter(|l| l.starts_with("| `")) {
        let first = line.trim_start_matches('|').split('|').next().unwrap_or("");
        for (i, part) in first.split('`').enumerate() {
            if i % 2 == 1 && !part.is_empty() {
                kinds.insert(part.to_string());
            }
        }
    }
    kinds
}

#[test]
fn the_scanner_reads_single_line_multi_line_and_event_variants() {
    let src = r#"
        app.emit("a_one", json!({})).await;
        app.emit(
            "b_two",
            json!({}),
        ).await;
        app.emit_event("c_three", json!({})).await;
        fn emit_event(x: &str) {}
        let dynamic = app.emit(kind, data);
        #[cfg(test)]
        use std::io;
        app.emit("e_after_a_cfg_test_use", json!({})).await;
        #[cfg(test)]
        mod tests { fn t() { app.emit("d_only_in_tests", json!({})); } }
    "#;
    assert_eq!(emitted_kinds(src), vec!["a_one", "b_two", "e_after_a_cfg_test_use", "c_three"]);
}

#[test]
fn the_ws_event_table_lists_every_emitted_type() {
    let root = repo_root();
    let mut files = Vec::new();
    rust_files(&root.join("daemon/src"), &mut files);
    if let Ok(rd) = std::fs::read_dir(root.join("crates")) {
        for e in rd.flatten() {
            rust_files(&e.path().join("src"), &mut files);
        }
    }
    assert!(files.len() > 100, "掃到的檔案太少（{}），路徑不對？", files.len());
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        let src = std::fs::read_to_string(f).unwrap_or_default();
        emitted.extend(emitted_kinds(&src));
    }
    for k in NOT_DOCUMENTED {
        emitted.remove(*k);
    }
    assert!(emitted.contains("bot_status") && emitted.contains("supervisor_changed"), "掃描器沒抓到已知事件：{emitted:?}");
    let documented = documented_kinds(&std::fs::read_to_string(root.join("docs/API.md")).expect("讀 docs/API.md"));
    let missing: Vec<&String> = emitted.iter().filter(|k| !documented.contains(*k)).collect();
    assert!(missing.is_empty(), "docs/API.md §8 的 WebSocket 事件表漏了這些事件（請補一列）：{missing:?}");
}
