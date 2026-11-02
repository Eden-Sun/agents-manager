//! System prompt builder for API bots (`claude-api`).
//!
//! Unlike CLI bots running in interactive terminal sessions with herdr and browser hooks,
//! API bots operate in a restricted bwrap sandbox with only the bash tool. Their prompt
//! describes the real sandbox environment and omits CLI-only capabilities (herdr child agents,
//! agent-browser, outbox file exchange, and CLI OAuth logins).

use am_base::db::Bot;

/// Instructions describing the bubblewrap sandbox environment and tool constraints.
pub fn api_bot_sandbox_instructions() -> &'static str {
    "你在 bubblewrap（bwrap）沙箱環境中執行，只能透過 bash 工具執行命令。沙箱環境具備以下限制：\n\
- **工作目錄與檔案**：目前工作目錄（cwd）為 `/workspace`，綁定至專案目錄；家目錄 `HOME` 設為 `/tmp`。\n\
- **無 herdr 支援**：環境中沒有 herdr 指令或 pane，不能開子 agent，也不支援多 pane 派工。\n\
- **無瀏覽器工具**：環境中沒有 agent-browser，無法直接瀏覽外部網頁。\n\
- **無 outbox 輸出目錄**：環境中沒有 `$AM_OUTBOX` 機制，檔案產物直接在 `/workspace` 內處理。\n\
- **無終端互動或 CLI 登入態**：你直接由 API 驅動，不具備終端互動介面或 CLI 登入憑證。\n\
- **終止行程**：背景行程僅能透過自己記錄的 PID 清理，禁止使用廣域 kill 指令。\n\
後面的共用／專案指示若提到 herdr、開子 agent、agent-browser、outbox、CLI 登入或 `$AM_*` 環境變數，在這個環境一律不適用，以本段為準。"
}

/// Compose the system prompt for an API bot (`claude-api`).
///
/// Combines the sandbox instructions, project agent instructions (`agent_md`),
/// the bot's custom persona, and the agent name note.
pub fn compose_api_bot_system_prompt(bot: &Bot, agent_name: &str, agent_md: Option<&str>) -> String {
    let base = match agent_md.map(str::trim).filter(|s| !s.is_empty()) {
        Some(md) => format!("{}\n\n{}", api_bot_sandbox_instructions(), md),
        None => api_bot_sandbox_instructions().to_string(),
    };
    let with_persona = match bot.persona.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(persona) => format!("{base}\n\n{persona}"),
        None => base,
    };
    format!("{with_persona}\n\n你在 AG Man 裡的 agent 名稱是 `{agent_name}`。")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bot(name: &str, persona: Option<&str>) -> Bot {
        Bot {
            id: "bot-id".into(),
            project_id: "proj-id".into(),
            name: name.into(),
            kind: "claude-api".into(),
            model: Some("claude-opus-5-5".into()),
            role: Some("worker".into()),
            effort: Some("high".into()),
            fast: 0,
            persona: persona.map(String::from),
            args_json: "[]".into(),
            autostart: 0,
            inject_hooks: 1,
            auto_approve: 1,
            identity: None,
            env_json: "{}".into(),
            managed_by: "user".into(),
            cwd: None,
            herdr_session: None,
            parent_bot_id: None,
            is_primary: 0,
            primary_position: 0,
            manager_use: 1,
            track: None,
            runtime_override_json: None,
            hook_token: "tok".into(),
            deleted_at: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn api_bot_prompt_contains_sandbox_and_omits_cli_rules() {
        let bot = test_bot("claude-api-opus", Some("你是一位專業的程式碼審查專家。"));
        let prompt = compose_api_bot_system_prompt(&bot, "claude-api-opus", Some("# 專案說明\n這是測試專案。"));

        // Must contain sandbox environment facts
        assert!(prompt.contains("/workspace"));
        assert!(prompt.contains("HOME"));
        assert!(prompt.contains("/tmp"));
        assert!(prompt.contains("不能開子 agent"));
        assert!(prompt.contains("沒有 herdr"));
        assert!(prompt.contains("沒有 agent-browser"));
        assert!(prompt.contains("沒有 `$AM_OUTBOX`"));
        assert!(prompt.contains("名稱是 `claude-api-opus`"));
        assert!(prompt.contains("以本段為準"));
        assert!(prompt.contains("你是一位專業的程式碼審查專家。"));
        assert!(prompt.contains("# 專案說明\n這是測試專案。"));

        // Must NOT contain CLI-specific instructions
        assert!(!prompt.contains("$AM_AGENT_NAME"));
        assert!(!prompt.contains("herdr agent start"));
        assert!(!prompt.contains("herdr pane split"));
        assert!(!prompt.contains("CLAUDE_CONFIG_DIR"));
        assert!(!prompt.contains("身分看 `$AM_IDENTITIES`"));
    }

    #[test]
    fn api_bot_prompt_without_optional_fields() {
        let bot = test_bot("api-worker", None);
        let prompt = compose_api_bot_system_prompt(&bot, "api-worker", None);
        assert!(prompt.contains("名稱是 `api-worker`"));
        assert!(prompt.contains("/workspace"));
    }
}
