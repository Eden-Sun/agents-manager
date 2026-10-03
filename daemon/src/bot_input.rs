//! 建立／修改 bot 與專案的 API 輸入檢查（API 與 config.toml 之間的那一道門）。
//!
//! 這些值最後會變成別的東西：model 是 CLI 的 argv、`env` 是 pane 的環境變數、專案路徑是 bot 的工作目錄、名稱與標籤會出現在
//! herdr 的 pane 標題與終端機畫面。只擋「一定會出事」的形狀，不替使用者決定什麼名字好看。

use crate::lifecycle::LcError;
use std::collections::BTreeMap;

/// 一個 bot 的 env 最多幾個、單一值最長幾個位元組（擋住一次塞進幾 MB 的請求）。
const MAX_ENV_ENTRIES: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 8 * 1024;
const MAX_MODEL_BYTES: usize = 128;
const MAX_LABEL_CHARS: usize = 64;

/// 看不見或會改變顯示方向的字元：零寬、方向控制、BOM。放進名稱或標籤，畫面上看起來一樣、實際是另一個字串（或把後面的字倒過來）。
pub fn is_invisible_format_char(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

/// model 會原樣進 `--model <值>`／`-m <值>`：開頭是 `-` 的值會被 CLI 當成旗標，空白或控制字元會把一個值拆成好幾個 argv。
pub fn check_model(model: &str) -> Result<(), LcError> {
    let m = model.trim();
    if m.is_empty() {
        return Ok(()); // 清掉＝用 CLI 預設
    }
    if m.len() > MAX_MODEL_BYTES {
        return Err(LcError::Bad(format!("model is too long (max {MAX_MODEL_BYTES} bytes)")));
    }
    if m.starts_with('-') {
        return Err(LcError::Bad("model must not start with `-` (it would be read as a CLI flag)".into()));
    }
    if m.chars().any(|c| c.is_whitespace() || c.is_control() || is_invisible_format_char(c)) {
        return Err(LcError::Bad("model must not contain whitespace or control characters".into()));
    }
    Ok(())
}

/// `env` 會原樣設進 bot 的 pane：名字要是正常的環境變數名；`AM_*` 是 daemon 自己注入、用來認人與記帳的（`AM_BOT_TOKEN`、`AM_RUN_ID`、
/// `AM_PORT`、`AM_DAEMON_EXE`…），有的在合併後會被蓋回去、有的不會，一律不讓使用者設。
pub fn check_env(env: &BTreeMap<String, String>) -> Result<(), LcError> {
    if env.len() > MAX_ENV_ENTRIES {
        return Err(LcError::Bad(format!("env has too many entries (max {MAX_ENV_ENTRIES})")));
    }
    for (k, v) in env {
        if !crate::tools::valid_env_name(k) {
            return Err(LcError::Bad(format!("env key `{}` is not a valid variable name ([A-Za-z_][A-Za-z0-9_]*)", k.chars().take(40).collect::<String>())));
        }
        if k.starts_with("AM_") {
            return Err(LcError::Bad(format!("env key `{k}` is reserved (AM_* is set by the daemon)")));
        }
        if v.len() > MAX_ENV_VALUE_BYTES {
            return Err(LcError::Bad(format!("env `{k}` value is too long (max {MAX_ENV_VALUE_BYTES} bytes)")));
        }
        if v.chars().any(|c| c.is_control()) {
            return Err(LcError::Bad(format!("env `{k}` value must not contain control characters")));
        }
    }
    Ok(())
}

/// `args` 是使用者明講要附加的 CLI 參數，內容由使用者負責；只擋 NUL（不可能出現在 argv 裡）。
pub fn check_args(args: &[String]) -> Result<(), LcError> {
    if args.iter().any(|a| a.contains('\0')) {
        return Err(LcError::Bad("args must not contain NUL".into()));
    }
    Ok(())
}

/// 專案標籤：出現在 herdr 的 workspace 標題、側欄與 `[agents.projects]` 的 key。不得含控制／看不見的字元，最長 [`MAX_LABEL_CHARS`] 字。
pub fn check_project_label(label: &str) -> Result<(), LcError> {
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(LcError::Bad(format!("project label is too long (max {MAX_LABEL_CHARS} characters)")));
    }
    if label.chars().any(|c| c.is_control() || is_invisible_format_char(c)) {
        return Err(LcError::Bad("project label must not contain control or invisible characters".into()));
    }
    Ok(())
}

/// 專案路徑不分主機只收絕對路徑（或 `~`、`~/…`）。相對路徑在本機會以 daemon 工作目錄為基準，遠端則會以 SSH 登入目錄為基準，
/// 兩種都不是使用者提供的專案位置。目錄存在與 canonicalize 仍由本機／遠端的 host-specific 路徑處理。
pub fn check_project_path(raw: &str) -> Result<(), LcError> {
    let p = raw.trim();
    if p.is_empty() {
        return Err(LcError::Bad("project path must not be empty".into()));
    }
    if p.contains('\0') {
        return Err(LcError::Bad("project path must not contain NUL".into()));
    }
    if !(p == "~" || p.starts_with("~/") || std::path::Path::new(p).is_absolute()) {
        return Err(LcError::Bad("project path must be absolute (or start with ~/)".into()));
    }
    Ok(())
}

/// `canonical_path` 之後呼叫：解開後必須是目錄。
pub fn check_is_dir(canonical: &str) -> Result<(), LcError> {
    if std::path::Path::new(canonical).is_dir() {
        Ok(())
    } else {
        Err(LcError::Bad(format!("project path is not a directory: {canonical}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad(r: Result<(), LcError>) -> bool {
        matches!(r, Err(LcError::Bad(_)))
    }

    #[test]
    fn a_model_that_would_be_read_as_a_flag_or_split_into_several_args_is_refused() {
        for m in ["--dangerously-skip-permissions", "-c", "opus --foo", "opus\nbar", "opus\u{1b}[0m", "a\u{202e}b", &"x".repeat(129)] {
            assert!(bad(check_model(m)), "{m:?}");
        }
        for m in ["opus", "claude-opus-5-5", "gpt-6.1-sol", "opus[1m]", "", "  sonnet  ", "openai/gpt-6:high"] {
            assert!(check_model(m).is_ok(), "{m:?}");
        }
    }

    #[test]
    fn env_may_not_set_daemon_owned_or_malformed_variables() {
        let env = |k: &str, v: &str| BTreeMap::from([(k.to_string(), v.to_string())]);
        for (k, v) in [("AM_BOT_TOKEN", "x"), ("AM_PORT", "1"), ("AM_RUN_ID", "x"), ("AM_DAEMON_EXE", "/bin/sh"), ("A-B", "x"), ("1A", "x"), ("", "x"), ("A=B", "x"), ("OK", "a\nb"), ("OK", "a\0b")] {
            assert!(bad(check_env(&env(k, v))), "{k}={v:?}");
        }
        for (k, v) in [("CLAUDE_CONFIG_DIR", "$HOME/.claude-x"), ("CODEX_HOME", "/tmp/x"), ("HTTPS_PROXY", "http://p:1"), ("PATH", "/usr/bin"), ("amx", "1")] {
            assert!(check_env(&env(k, v)).is_ok(), "{k}");
        }
        let many: BTreeMap<String, String> = (0..=MAX_ENV_ENTRIES).map(|i| (format!("K{i}"), "v".into())).collect();
        assert!(bad(check_env(&many)));
    }

    #[test]
    fn labels_and_paths_are_shaped_before_they_reach_herdr_or_the_filesystem() {
        assert!(bad(check_project_label("a\nb")));
        assert!(bad(check_project_label("a\u{1b}[31mred")));
        assert!(bad(check_project_label("evil\u{202e}txt")));
        assert!(bad(check_project_label(&"長".repeat(65))));
        assert!(check_project_label("agents-manager 專案").is_ok());
        for p in ["", "  ", ".", "..", "foo/bar", "./x", "a\0b"] {
            assert!(bad(check_project_path(p)), "{p:?}");
        }
        for p in ["/tmp", "~", "~/project/x"] {
            assert!(check_project_path(p).is_ok(), "{p:?}");
        }
        assert!(bad(check_args(&["a\0".into()])));
        assert!(check_args(&["--append-system-prompt".into(), "line1\nline2".into()]).is_ok());
    }
}
