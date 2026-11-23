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
        "agy" => 1_048_576,
        _ => 0,
    }
}

/// bot 覆寫與角色 policy 都沒設時，這個 kind 的預設 context 上限（自動壓縮門檻）。agy 沒有 statusLine 的視窗數字，預設 400k（同 codex）。
/// claude／codex 不設＝`None`（行為不變）。只影響門檻，不產生啟動參數（`launch_params` 對 agy 本來就不補）。
pub fn default_context_tokens(kind: &str) -> Option<u32> {
    default_context_tokens_for(kind, false)
}

pub fn default_context_tokens_for(kind: &str, is_share_bot: bool) -> Option<u32> {
    if is_share_bot || kind == "agy" {
        Some(400_000)
    } else {
        None
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
        assert_eq!(context_window("claude"), 1_000_000);
        assert_eq!(context_window("codex"), 400_000);
        assert_eq!(context_window("agy"), 1_048_576);
        assert_eq!(context_window("grok"), 0);
        assert_eq!(default_context_tokens("agy"), Some(400_000));
        assert_eq!(default_context_tokens("claude"), None);
        assert_eq!(default_context_tokens("codex"), None);
        assert_eq!(default_context_tokens_for("claude", true), Some(400_000));
        assert_eq!(default_context_tokens_for("claude", false), None);
        assert_eq!(default_context_tokens_for("codex", true), Some(400_000));
        assert_eq!(default_context_tokens_for("codex", false), None);
        assert_eq!(default_context_tokens_for("agy", false), Some(400_000));
        assert_eq!(default_context_tokens_for("agy", true), Some(400_000));

        let claude = launch_params("claude", Some("opus"), Some(512_000));
        assert_eq!(claude.model.as_deref(), Some("opus[1m]"));
        assert_eq!(claude.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str), Some("512000"));

        let codex = launch_params("codex", Some("gpt-6"), Some(256_000));
        assert_eq!(codex.args, vec!["-c".to_string(), "model_auto_compact_token_limit=256000".to_string()]);
        assert_eq!(codex.model.as_deref(), Some("gpt-6"));

        let agy = launch_params("agy", Some("gemini-4-argon-medium"), Some(400_000));
        assert_eq!(agy.model.as_deref(), Some("gemini-4-argon-medium"));
        assert!(agy.env.is_empty());
        assert!(agy.args.is_empty());
    }
}
