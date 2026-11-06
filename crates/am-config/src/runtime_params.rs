//! Runtime context validation and launch parameters shared by role policy and bot overrides.

use std::collections::BTreeMap;

pub const CONTEXT_MIN: u32 = 200_000;
/// claude 的 `[1m]` 變體以上才用得到的門檻：超過它就把模型換成 `<model>[1m]`。
pub const CLAUDE_BASE_WINDOW: u32 = 200_000;

/// 模型視窗（token）。`/api/models` 目前沒有 context window 欄位，所以用家族預設：claude 1M、codex 400k。
pub fn context_window(kind: &str) -> u32 {
    match kind {
        "claude" => 1_000_000,
        "codex" => 400_000,
        _ => 0,
    }
}

/// 將 role policy／bot 覆寫的 context 上限轉成 CLI 啟動參數。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchParams {
    pub model: Option<String>,
    pub env: BTreeMap<String, String>,
    pub args: Vec<String>,
}

pub fn launch_params(kind: &str, model: Option<&str>, context_tokens: Option<u32>) -> LaunchParams {
    let mut out = LaunchParams { model: model.map(str::to_owned), env: BTreeMap::new(), args: Vec::new() };
    let Some(tokens) = context_tokens else { return out };
    match kind {
        "claude" => {
            out.env.insert("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), tokens.to_string());
            if tokens > CLAUDE_BASE_WINDOW {
                if let Some(m) = out.model.as_mut() {
                    if !m.ends_with("[1m]") {
                        m.push_str("[1m]");
                    }
                }
            }
        }
        "codex" => {
            out.args.push("-c".into());
            out.args.push(format!("model_auto_compact_token_limit={tokens}"));
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_overlays_preserve_kind_specific_launch_parameters() {
        let claude = launch_params("claude", Some("opus"), Some(512_000));
        assert_eq!(claude.model.as_deref(), Some("opus[1m]"));
        assert_eq!(claude.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str), Some("512000"));

        let codex = launch_params("codex", Some("gpt-6"), Some(256_000));
        assert_eq!(codex.args, vec!["-c".to_string(), "model_auto_compact_token_limit=256000".to_string()]);
        assert_eq!(codex.model.as_deref(), Some("gpt-6"));
        assert!(launch_params("agy", None, Some(256_000)).args.is_empty());
    }
}
