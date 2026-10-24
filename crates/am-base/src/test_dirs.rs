//! 測試用暫存目錄（不依賴 daemon 的 `testing`，讓 am-base 自己的測試也跑得起來）：登記後在行程結束時刪掉。
use std::sync::{Mutex, Once};

fn remove_at_exit(path: &std::path::Path) {
    static LIST: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());
    static HOOK: Once = Once::new();
    extern "C" fn sweep() {
        for p in LIST.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            if std::fs::remove_dir_all(&p).is_err() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    LIST.lock().unwrap_or_else(|e| e.into_inner()).push(path.to_path_buf());
    HOOK.call_once(|| {
        // SAFETY: `sweep` 是沒有參數的 `extern "C"` 函式，整個行程生命週期內都有效。
        unsafe { libc::atexit(sweep) };
    });
}

/// `$TMPDIR/<prefix>-<ulid>`，已建好並登記。基底先 canonicalize，理由同 `testing::scratch_dir`（#1964）。
pub fn scratch_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().canonicalize().unwrap().join(format!("{prefix}-{}", crate::db::ulid()));
    std::fs::create_dir_all(&dir).unwrap();
    remove_at_exit(&dir);
    dir
}

/// 把自己組的暫存路徑登記成「行程結束時刪掉」，原樣回傳、不建立。
pub fn track(path: std::path::PathBuf) -> std::path::PathBuf {
    remove_at_exit(&path);
    path
}

#[cfg(test)]
pub(crate) mod scratch_leak_guard {
    /// 檢查單一原始碼字串中是否有未受保護的 temp_dir()。
    /// 規則要跟 daemon/src/testing.rs 的 scratch_leak_guard 一起改（#1967）。
    pub fn find_violations(src: &str) -> Vec<(usize, String)> {
        let mut violations = Vec::new();
        let mut statement = String::new();
        for (i, l) in src.lines().enumerate() {
            let line_num = i + 1;
            let trimmed_line = l.trim();
            // 例外用行尾標記 // scratch-guard: allow <理由>，理由不能是空的。
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
                if segment.contains("temp_dir()")
                    && ![
                        "testing::track(",
                        "testing::scratch_dir(",
                        "tt::track(",
                        "tt::scratch_dir(",
                        "test_dirs::track(",
                        "test_dirs::scratch_dir(",
                    ]
                    .iter()
                    .any(|w| statement.contains(w))
                {
                    violations.push((line_num, segment.trim().to_string()));
                }
                if segment.ends_with(';') {
                    statement.clear();
                }
            }
        }
        violations
    }

    /// issue #1967：只掃 crates/am-base/src，排除 test_dirs.rs 本身。
    /// 規則要跟 daemon/src/testing.rs 的 scratch_leak_guard 一起改。
    #[test]
    fn no_am_base_file_builds_an_unregistered_path_under_temp_dir() {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("test_dirs.rs") {
                    let content = std::fs::read_to_string(&p).unwrap();
                    let v = find_violations(&content);
                    for (line, desc) in v {
                        out.push(format!("{}:{}: {}", p.display(), line, desc));
                    }
                }
            }
        }

        let mut bad = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut bad);
        assert!(
            bad.is_empty(),
            "wrap with crate::testing::track(…) or test_dirs::track(…) or use scratch_dir:\n{}",
            bad.join("\n")
        );
    }

    #[test]
    fn pure_guard_detects_unwrapped_temp_dir() {
        let code = "let d = std::env::temp_dir().join(\"bad\");\n";
        let v = find_violations(code);
        assert_eq!(v.len(), 1);
        assert!(v[0].1.contains("temp_dir()"));
    }

    #[test]
    fn pure_guard_rejects_empty_allow_reason() {
        let code = "let d = std::env::temp_dir().join(\"bad\"); // scratch-guard: allow\n";
        let v = find_violations(code);
        assert_eq!(v.len(), 1);
        assert!(v[0].1.contains("缺少理由"));

        let code_spaces = "let d = std::env::temp_dir().join(\"bad\"); // scratch-guard: allow    \n";
        let v_spaces = find_violations(code_spaces);
        assert_eq!(v_spaces.len(), 1);
        assert!(v_spaces[0].1.contains("缺少理由"));
    }

    #[test]
    fn pure_guard_allows_marked_with_reason() {
        let code = "let d = std::env::temp_dir().join(\"bad\"); // scratch-guard: allow some valid reason\n";
        let v = find_violations(code);
        assert!(v.is_empty());
    }

    #[test]
    fn pure_guard_allows_wrapped_with_scratch_dir_or_track() {
        let code1 = "let d = crate::test_dirs::scratch_dir(\"ok\");\n";
        assert!(find_violations(code1).is_empty());

        let code2 = "let d = crate::test_dirs::track(std::env::temp_dir().join(\"ok\"));\n";
        assert!(find_violations(code2).is_empty());

        let code3 = "let d = crate::testing::track(std::env::temp_dir().join(\"ok\"));\n";
        assert!(find_violations(code3).is_empty());

        let code4 = "let d = tt::scratch_dir(\"ok\");\n";
        assert!(find_violations(code4).is_empty());
    }
}
