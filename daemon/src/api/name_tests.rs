use super::next_free_name;

#[test]
fn next_free_name_bumps_the_suffix() {
    let taken = |n: &str| ["cc1-1", "cc1-2", "review"].contains(&n);
    assert_eq!(next_free_name("cc1-1", &taken), "cc1-3");
    assert_eq!(next_free_name("cc1-9", &taken), "cc1-3");
    assert_eq!(next_free_name("review", &taken), "review-1");
    let long = "a".repeat(32);
    assert!(next_free_name(&long, &taken).len() <= 32);
}

/// issue #112：`valid_bot_name` 允許任意 Unicode（`chars().count() <= 32`，非 ASCII 也合法），
/// 但撞名時原本按**位元組**長度截斷——29 個 ASCII 字元後面接一個多位元組字元、剛好落在
/// `room`（byte）那一刀中間就會直接 panic（"byte index N is not a char boundary"）。
#[test]
fn next_free_name_does_not_panic_on_a_multibyte_boundary() {
    let name = format!("{}中", "a".repeat(29));
    assert_eq!(name.chars().count(), 30, "valid_bot_name 用字元數判，這個名字合法");
    let taken = |n: &str| n == name;
    let out = next_free_name(&name, &taken);
    assert!(out.chars().count() <= 32, "{out}");
    assert_ne!(out, name);
}

/// 中文名字整段截斷也不能把字切一半（亂碼），比照 `fork.rs::default_name`／
/// `default_session.rs::imported_name` 以字元為單位截斷——是「32 個字元」的字元預算，
/// 不是位元組，太保守（例如 3 位元組的中文只留 10 個字）也算沒修對。
#[test]
fn next_free_name_truncates_on_char_boundaries_for_cjk_names() {
    let long = "審".repeat(40);
    let taken = |n: &str| n == long;
    let out = next_free_name(&long, &taken);
    assert_eq!(out, format!("{}-1", "審".repeat(30)), "{out}");
}
