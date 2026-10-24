//! 暫存目錄守門（issue #763／#1967／#1968）：測試裡直接 `std::env::temp_dir().join(…)` 而沒登記的路徑，行程結束時沒有人會刪。
//! `find_violations` 是唯一的判斷邏輯，am-base 的守門測試與 daemon 的 `scratch_leak_guard` 都呼叫它；規則改這裡就好。
use std::path::Path;

/// 合法的包法：呼叫會登記（行程結束時刪掉）的函式。`tt::` 是各測試模組 `use crate::testing as tt;` 的慣用別名。
const WRAPPERS: [&str; 8] = [
    "testing::track(",
    "testing::scratch_dir(",
    "tt::track(",
    "tt::scratch_dir(",
    "test_dirs::track(",
    "test_dirs::scratch_dir(",
    "test_support::track(",
    "test_support::scratch_dir(",
];

/// 不掃的檔名：各 crate 的暫存輔助本身（裡面就是登記的實作），以及守門自己（裡面有 `temp_dir()` 的字面字串）。
const EXEMPT_FILES: [&str; 3] = ["test_dirs.rs", "test_support.rs", "scratch_guard.rs"];

/// 檔案是否免掃（依檔名，不看路徑）。
pub fn is_exempt(path: &Path) -> bool {
    path.file_name().is_some_and(|n| EXEMPT_FILES.iter().any(|e| n == *e))
}

/// 檢查單一原始碼字串中是否有未受保護的 temp_dir()。回傳 (行號, 內容)。
/// 例外用行尾標記 `// scratch-guard: allow <理由>`，理由不能是空的。
pub fn find_violations(src: &str) -> Vec<(usize, String)> {
    let mut violations = Vec::new();
    let mut statement = String::new();
    for (i, l) in src.lines().enumerate() {
        let line_num = i + 1;
        let trimmed_line = l.trim();
        let allow_marker = "// scratch-guard: allow";
        if let Some(pos) = trimmed_line.find(allow_marker) {
            let rest = trimmed_line[pos + allow_marker.len()..].trim();
            if rest.is_empty() {
                violations.push((line_num, "scratch-guard: allow 標記缺少理由".to_string()));
            }
            continue;
        }

        for segment in l.split_inclusive(';') {
            statement.push_str(segment);
            if segment.contains("temp_dir()") && !WRAPPERS.iter().any(|w| statement.contains(w)) {
                violations.push((line_num, segment.trim().to_string()));
            }
            if segment.ends_with(';') {
                statement.clear();
            }
        }
    }
    violations
}

/// 掃 `dir` 底下所有 .rs（跳過 `skip` 為真的檔），每筆是 `路徑:行號: 內容`。
pub fn scan_tree(dir: &Path, skip: &dyn Fn(&Path) -> bool) -> Vec<String> {
    fn walk(dir: &Path, skip: &dyn Fn(&Path) -> bool, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, skip, out);
            } else if p.extension().is_some_and(|x| x == "rs") && !skip(&p) {
                let src = std::fs::read_to_string(&p).unwrap();
                for (line, desc) in find_violations(&src) {
                    out.push(format!("{}:{}: {}", p.display(), line, desc));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, skip, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// daemon 的 `scratch_leak_guard` 測試用同一份輸入（逐字相同），預期結果也要一致。
    const SAMPLE: &str = "let a = std::env::temp_dir().join(\"bad\");\nlet b = std::env::temp_dir().join(\"x\"); // scratch-guard: allow\nlet c = crate::test_dirs::scratch_dir(\"ok\");\nlet d = std::env::temp_dir().join(\"y\"); // scratch-guard: allow 正式程式碼有 Drop\n";

    #[test]
    fn a_bare_temp_dir_call_is_red() {
        let v = find_violations("let d = std::env::temp_dir().join(\"bad\");\n");
        assert_eq!(v.len(), 1);
        assert!(v[0].1.contains("temp_dir()"));
    }

    #[test]
    fn an_allow_marker_without_a_reason_is_red() {
        let v = find_violations("let d = std::env::temp_dir().join(\"bad\"); // scratch-guard: allow\n");
        assert_eq!(v.len(), 1);
        assert!(v[0].1.contains("缺少理由"));

        let v_spaces = find_violations("let d = std::env::temp_dir().join(\"bad\"); // scratch-guard: allow    \n");
        assert_eq!(v_spaces.len(), 1);
        assert!(v_spaces[0].1.contains("缺少理由"));
    }

    #[test]
    fn an_allow_marker_with_a_reason_is_green() {
        let v = find_violations("let d = std::env::temp_dir().join(\"bad\"); // scratch-guard: allow some valid reason\n");
        assert!(v.is_empty());
    }

    #[test]
    fn every_registered_wrapper_is_green() {
        assert!(find_violations("let d = crate::test_dirs::scratch_dir(\"ok\");\n").is_empty());
        assert!(find_violations("let d = crate::test_dirs::track(std::env::temp_dir().join(\"ok\"));\n").is_empty());
        assert!(find_violations("let d = crate::testing::track(std::env::temp_dir().join(\"ok\"));\n").is_empty());
        assert!(find_violations("let d = tt::scratch_dir(\"ok\");\n").is_empty());
        assert!(find_violations("let d = super::test_support::track(std::env::temp_dir().join(\"ok\"));\n").is_empty());
        assert!(find_violations("let d = super::test_support::scratch_dir(\"ok\");\n").is_empty());
    }

    #[test]
    fn exempt_helper_files_are_skipped_by_name() {
        assert!(is_exempt(Path::new("crates/am-config/src/test_support.rs")));
        assert!(is_exempt(Path::new("crates/am-supervisor/src/test_dirs.rs")));
        assert!(is_exempt(Path::new("crates/am-base/src/scratch_guard.rs")));
        assert!(!is_exempt(Path::new("crates/am-base/src/home.rs")));
    }

    #[test]
    fn scan_tree_reports_a_bad_file_but_not_an_exempt_one() {
        let root = crate::test_dirs::scratch_dir("am-guard-scan");
        std::fs::write(root.join("bad.rs"), "let d = std::env::temp_dir().join(\"bad\");\n").unwrap();
        std::fs::write(root.join("test_support.rs"), "let d = std::env::temp_dir().join(\"bad\");\n").unwrap();
        let hits = scan_tree(&root, &|p| is_exempt(p));
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].contains("bad.rs:1:"), "{hits:?}");
    }

    /// 同一份輸入的預期結果。daemon 的 `scratch_leak_guard::same_input_gives_the_same_answer_as_am_base` 用同樣的字串與數字。
    #[test]
    fn the_shared_sample_has_two_lines_flagged_in_the_expected_order() {
        let v = find_violations(SAMPLE);
        let lines: Vec<usize> = v.iter().map(|(n, _)| *n).collect();
        assert_eq!(lines, vec![1, 2]);
        assert!(v[1].1.contains("缺少理由"));
    }

    /// 守門本體：`crates/*/src` 底下沒有未登記的 temp_dir()（issue #1968 把範圍擴到所有 crate）。
    #[test]
    fn no_crate_builds_an_unregistered_path_under_temp_dir() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut bad = Vec::new();
        for e in std::fs::read_dir(&crates).unwrap().flatten() {
            let src = e.path().join("src");
            if src.is_dir() {
                bad.extend(scan_tree(&src, &|p| is_exempt(p)));
            }
        }
        assert!(
            bad.is_empty(),
            "wrap with crate::testing::track(…) / test_dirs::scratch_dir(…) / test_support::scratch_dir(…), or add `// scratch-guard: allow <理由>`:\n{}",
            bad.join("\n")
        );
    }
}
