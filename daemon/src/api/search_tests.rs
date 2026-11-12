use super::*;

#[test]
fn like_wildcards_typed_by_the_user_are_literal() {
    assert_eq!(like_escape("100%"), r"100\%");
    assert_eq!(like_escape("a_b"), r"a\_b");
    assert_eq!(like_escape(r"c:\path"), r"c:\\path");
    assert_eq!(like_escape("plain"), "plain");
}

#[test]
fn the_snippet_is_taken_around_the_hit_not_from_the_start() {
    let long = format!("{}命中在很後面{}", "前".repeat(80), "後".repeat(80));
    let s = snippet_around(&long, "命中", 30);
    assert!(s.contains("命中"), "the hit itself must be in the window: {s}");
    assert!(s.starts_with('…'), "an elided head is marked: {s}");
    assert!(s.chars().count() <= 34, "and the window stays small: {s}");
}

#[test]
fn a_hit_at_the_start_needs_no_leading_ellipsis() {
    let s = snippet_around("資料夾選擇介面的問題", "資料夾", 90);
    assert_eq!(s, "資料夾選擇介面的問題");
}

/// `İ` lowercases to two chars; an index into the lowercased copy used to panic (500).
#[test]
fn a_hit_after_a_char_that_grows_when_lowercased_stays_in_bounds() {
    let content = format!("{}命中", "İ".repeat(40));
    let s = snippet_around(&content, "命中", 90);
    assert!(s.contains("命中"), "the hit is in the window: {s}");
    let s = snippet_around(&content, "i\u{307}", 90);
    assert!(s.starts_with('İ'), "and a lowercase needle still matches the original: {s}");
}
