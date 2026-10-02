//! grok 的全域 hook 檔（`<GROK_HOME>/hooks/agents-manager.json`，SPEC §12）：自癒、保留使用者自己的 hook、原子寫入、不寫真實 HOME。
//!
//! 起因（2026-10-02 c6）：使用者真實的 `~/.grok/hooks/agents-manager.json` 指向早就刪掉的 `/tmp/am-test-…/data/grok-hook.sh`——
//! 測試啟動 grok bot 時，`dirs::home_dir()` 是**真的** HOME，把測試資料目錄的 dispatcher 路徑寫進了使用者的檔案。

use super::grok_hook::{heal_at_startup, hooks_json_merged, install_local};
use crate::testing as tt;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

fn commands(v: &Value, event: &str) -> Vec<String> {
    v["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|g| g["hooks"].as_array().into_iter().flatten())
        .filter_map(|h| h["command"].as_str().map(String::from))
        .collect()
}

fn user_hook(cmd: &str) -> Value {
    json!({"hooks": [{"type": "command", "command": cmd, "timeout": 9}]})
}

#[test]
fn the_merge_replaces_only_our_entry_and_keeps_everything_the_user_added() {
    let existing = json!({
        "hooks": {
            "SessionStart": [
                {"hooks": [{"type": "command", "command": "/tmp/am-test-GONE/data/grok-hook.sh", "timeout": 5}]},
                user_hook("/home/u/my-start.sh"),
            ],
            "Stop": [{"hooks": [{"type": "command", "command": "/tmp/am-test-GONE/data/grok-hook.sh", "timeout": 5}]}],
            "PreToolUse": [user_hook("/home/u/guard.sh")],
        },
        "note": "user key outside hooks",
    })
    .to_string();
    let merged: Value = serde_json::from_str(&hooks_json_merged(Some(&existing), "/data/grok-hook.sh")).unwrap();
    assert_eq!(commands(&merged, "SessionStart"), ["/home/u/my-start.sh", "/data/grok-hook.sh"], "舊的我們那項換掉、使用者的留著");
    assert_eq!(commands(&merged, "Stop"), ["/data/grok-hook.sh"]);
    assert_eq!(commands(&merged, "PreToolUse"), ["/home/u/guard.sh"], "別的事件原封不動");
    assert_eq!(merged["note"], "user key outside hooks", "hooks 以外的鍵也留著");
    // 冪等：再合併一次不會長出第二項。
    let again: Value = serde_json::from_str(&hooks_json_merged(Some(&merged.to_string()), "/data/grok-hook.sh")).unwrap();
    assert_eq!(again, merged);
    // 一個群組裡我們的和使用者的混在一起：只拿掉我們那一個 hook，不整個群組丟掉。
    let mixed = json!({"hooks": {"Stop": [{"hooks": [
        {"type": "command", "command": "/old/dir/grok-hook.sh", "timeout": 5},
        {"type": "command", "command": "/home/u/after-stop.sh"}
    ]}]}})
    .to_string();
    let m: Value = serde_json::from_str(&hooks_json_merged(Some(&mixed), "/data/grok-hook.sh")).unwrap();
    assert_eq!(commands(&m, "Stop"), ["/home/u/after-stop.sh", "/data/grok-hook.sh"]);
}

#[test]
fn a_missing_or_unreadable_file_gets_a_fresh_one() {
    for existing in [None, Some(""), Some("not json"), Some("[1,2]"), Some(r#"{"hooks": "nope"}"#)] {
        let v: Value = serde_json::from_str(&hooks_json_merged(existing, "/data/grok-hook.sh")).unwrap();
        assert_eq!(commands(&v, "SessionStart"), ["/data/grok-hook.sh"], "{existing:?}");
        assert_eq!(commands(&v, "Stop"), ["/data/grok-hook.sh"], "{existing:?}");
    }
}

fn hooks_file(grok_home: &Path) -> std::path::PathBuf {
    grok_home.join("hooks").join(super::setup::grok_hooks_file(None))
}

/// bot 啟動時裝：壞路徑被換成這顆 daemon 的 dispatcher、使用者的 hook 留著、權限照原檔（但不放寬到別人寫得進去）、沒有殘留暫存檔。
#[tokio::test]
async fn starting_a_grok_bot_repairs_a_stale_hook_path_without_touching_the_users_hooks() {
    let e = tt::env().await;
    let grok_home = tt::scratch_dir("am-grokhook-home");
    let path = hooks_file(&grok_home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let stale = json!({"hooks": {
        "SessionStart": [{"hooks": [{"type": "command", "command": "/tmp/am-test-GONE/data/grok-hook.sh", "timeout": 5}]}, user_hook("/home/u/mine.sh")],
        "Stop": [{"hooks": [{"type": "command", "command": "/tmp/am-test-GONE/data/grok-hook.sh", "timeout": 5}]}]}});
    std::fs::write(&path, stale.to_string()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

    install_local(&e.app, &json!({"GROK_HOME": grok_home.to_string_lossy()})).unwrap();

    let dispatcher = e.app.data_dir.join("grok-hook.sh").to_string_lossy().into_owned();
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(commands(&v, "SessionStart"), ["/home/u/mine.sh".to_string(), dispatcher.clone()]);
    assert_eq!(commands(&v, "Stop"), [dispatcher.clone()]);
    assert!(Path::new(&dispatcher).is_file(), "dispatcher 本身也在");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640, "權限照原檔");
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(leftovers.len(), 1, "原子寫入不留暫存檔：{leftovers:?}");

    // 原檔別人寫得進去（0666）：換檔時收回到 0600，不能把一個能被改寫就會執行的檔案留著。
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    std::fs::write(&path, stale.to_string()).unwrap();
    install_local(&e.app, &json!({"GROK_HOME": grok_home.to_string_lossy()})).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}

/// daemon 開機：檔案在、而且我們那一項指到不是這顆 daemon 的 dispatcher（或腳本不存在）→ 換掉；沒有檔就不建
/// （沒在用 grok 的機器不該被裝上 hook）；本來就對的不碰（mtime 不動，grok 的 hook loader 不會看到無謂的變動）。
#[tokio::test]
async fn daemon_start_repairs_a_wrong_hook_file_but_never_creates_or_touches_a_good_one() {
    let e = tt::env().await;
    let grok_home = tt::scratch_dir("am-grokhook-boot");
    let path = hooks_file(&grok_home);
    let home = grok_home.to_string_lossy().into_owned();

    assert!(!heal_at_startup(&e.app, Some(&home)).unwrap(), "沒有檔：什麼都不做");
    assert!(!path.exists(), "沒有 grok 的機器不建檔");

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let wrong = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/tmp/am-test-GONE/data/grok-hook.sh"}]}]}});
    std::fs::write(&path, wrong.to_string()).unwrap();
    assert!(heal_at_startup(&e.app, Some(&home)).unwrap(), "壞路徑被修");
    let dispatcher = e.app.data_dir.join("grok-hook.sh").to_string_lossy().into_owned();
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(commands(&v, "Stop"), [dispatcher.clone()]);
    assert_eq!(commands(&v, "SessionStart"), [dispatcher.clone()]);

    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(!heal_at_startup(&e.app, Some(&home)).unwrap(), "本來就對：不碰");
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);

    // 指到的腳本被刪掉了（路徑還是這顆 daemon 的）：腳本補回來。
    std::fs::remove_file(&dispatcher).unwrap();
    assert!(heal_at_startup(&e.app, Some(&home)).unwrap());
    assert!(Path::new(&dispatcher).is_file());
}

/// 測試不得寫真實 HOME：沒有 `GROK_HOME` 時 hook 檔落在測試自己的假 HOME，不是 `dirs::home_dir()`。
#[tokio::test]
async fn tests_never_write_the_real_home() {
    let e = tt::env().await;
    let fake = crate::home::dir().expect("fake home");
    // `test_home` 在行程開始前就把 `$HOME` 換成拋棄式目錄，所以 `dirs::home_dir()` 也指向它；這裡驗它確實是那個拋棄式目錄。
    assert_eq!(fake.as_path(), crate::test_home::dir(), "home::dir() 與換掉的 $HOME 是同一處");
    assert!(fake.to_string_lossy().contains("am-test-home-"), "測試的 HOME 不是真的 HOME：{}", fake.display());
    install_local(&e.app, &json!({})).unwrap();
    assert!(fake.join(".grok/hooks").join(super::setup::grok_hooks_file(None)).is_file(), "寫進了假 HOME");
}
