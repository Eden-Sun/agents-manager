//! 守衛測試共用：把原始碼裡只在測試建置存在的區塊（`#[cfg(test)]`、`#[cfg(all(test, …))]`、test-hooks gate）標成遮罩，讓掃描只看正式碼。
//! 各 crate 的 ports 守衛測試（send／session／ingress）都用這一份。

/// 每一行是否落在 `#[cfg(test)]` 區塊（含 `#[cfg(all(test, …))]`）裡。
#[cfg(any(test, feature = "test-hooks"))]
pub fn test_mask(src: &str) -> Vec<bool> {
    let lines: Vec<&str> = src.lines().collect();
    let mut mask = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let head = lines[i].trim_start();
        if head.starts_with("#[cfg(test)]")
            || head.starts_with("#[cfg(all(test,")
            || head.starts_with("#[cfg(any(test, feature = \"test-hooks\"")
            || head.starts_with("#[cfg(any(feature = \"test-hooks\"")
            || head.starts_with("#[cfg(feature = \"test-hooks\")]")
        {
            let (mut depth, mut seen, mut j) = (0i32, false, i);
            while j < lines.len() {
                mask[j] = true;
                depth += lines[j].matches('{').count() as i32 - lines[j].matches('}').count() as i32;
                seen |= lines[j].contains('{');
                if seen && depth <= 0 {
                    break;
                }
                if !seen && lines[j].trim_end().ends_with(';') && !lines[j].trim_start().starts_with("#[") {
                    break;
                }
                j += 1;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::test_mask;

    #[test]
    fn test_mask_skips_cfg_test_items() {
        assert!(test_mask("fn a() {}\n#[cfg(test)]\nmod t {\n    fn b() {}\n}\nfn c() {}\n") == vec![false, true, true, true, true, false]);
        assert!(test_mask("fn a() {}\n#[cfg(all(test, feature = \"some-feature\"))]\nmod t {\n    fn b() {}\n}\nfn c() {}\n") == vec![false, true, true, true, true, false]);
        assert!(test_mask("fn a() {}\n#[cfg(any(feature = \"test-hooks\", all(test, feature = \"some-feature\")))]\nfn b() {\n}\nfn c() {}\n") == vec![false, true, true, true, false]);
        assert!(test_mask("#[cfg(feature = \"test-hooks\")]\nfn b() {}\nfn c() {}\n") == vec![true, true, false]);
        assert!(test_mask("#[cfg(any(test, feature = \"test-hooks\"))]\nfn b() {}\nfn c() {}\n") == vec![true, true, false]);
        assert!(test_mask("#[cfg(not(any(feature = \"test-hooks\", all(test, feature = \"some-feature\"))))]\nfn b() {}\n") == vec![false, false]);
    }
}
