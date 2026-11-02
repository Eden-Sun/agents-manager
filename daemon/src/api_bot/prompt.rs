//! System prompt builder for API bots (`claude-api`).
//!
//! Unlike CLI bots running in interactive terminal sessions with herdr and browser hooks,
//! API bots operate in a restricted bwrap sandbox with only the bash and str_replace_based_edit_tool tools.
//! Their prompt describes the real sandbox environment and omits CLI-only capabilities
//! (herdr child agents, agent-browser, outbox file exchange, CLI OAuth logins, auto compact, fork/resume).

use am_base::db::Bot;

/// Instructions describing the bubblewrap sandbox environment and tool constraints.
pub fn api_bot_sandbox_instructions() -> &'static str {
    "你在 bubblewrap（bwrap）沙箱環境中執行，具備以下真實環境條件與限制：\n\
- **可用工具**：只有 `bash`（bash_20250124）與 `str_replace_based_edit_tool`（text_editor_20250728，檔案路徑使用 `/workspace/...`）。\n\
- **每次呼叫獨立**：每次 bash 呼叫都是全新的 bwrap 沙箱行程（--unshare-all --share-net --clearenv --die-with-parent），前一次的 cd、export、shell 變數與背景行程都不會延續；指令結束時行程群組即被 SIGKILL，不能留背景服務；需要多步驟請在同一次呼叫中以 `&&` 串接。\n\
- **臨時目錄無狀態**：`/tmp` 為每次呼叫獨立的 tmpfs，`HOME=/tmp`；寫入 `/tmp` 或 `~` 的檔案在該次呼叫結束後即消失，要保留的檔案必須放在 `/workspace`。\n\
- **工作目錄可讀寫**：`/workspace` 綁定至專案目錄，可讀寫且指令間會保留；預設工作目錄（cwd）為 `/workspace`。\n\
- **檔案系統唯讀限制**：根目錄僅唯讀掛載 `/usr`、`/bin`、`/lib`、`/lib64`，另有 `/proc` 與 `/dev`；`/etc` 只唯讀掛入 resolv.conf、hosts、nsswitch.conf、host.conf、gai.conf、ssl、ca-certificates、passwd、group，沒有 hostname、git/ssh 全域設定或憑證。\n\
- **可以連網**：沙箱共用主機網路，curl、git clone/fetch、套件下載可用；DNS 與 TLS 憑證已掛入。\n\
- **精簡環境變數**：環境變數僅提供 `PATH=/usr/bin:/bin`、`HOME=/tmp`、`LANG=C.UTF-8`；沒有任何 `$AM_*` 環境變數。\n\
- **無 herdr 與 CLI 週邊**：沒有 herdr、不能開子 agent、沒有 agent-browser、沒有 outbox（`$AM_OUTBOX`）、沒有 CLI 登入憑證。\n\
- **無跨對話記憶與自動 compact**：對話不會自動 compact，會持續累積至 context 上限；一旦超限回合將失敗，需由使用者在介面按「清除對話」；不支援 fork/resume 或模型查詢 API。\n\
後面的共用／專案指示若提到 herdr、開子 agent、agent-browser、outbox、CLI 登入、`$AM_*` 環境變數、auto compact、fork/resume，在這個環境一律不適用，以本段為準。"
}

/// `/workspace` 對應主機上的哪個目錄：使用者與專案指示寫的是主機路徑，bot 要自己換成 `/workspace`。
fn path_mapping_note(host_root: &str) -> String {
    let root = host_root.trim_end_matches('/');
    format!("**路徑對應**：`/workspace` 就是主機上的 `{root}`。使用者或上面的指示提到 `{root}/…`（或 `~/` 開頭、指向這個目錄的路徑）時，一律換成 `/workspace/…` 再操作；回報時可兩者並列。`{root}` 以外的主機路徑（例如其他專案、`~/.config/agents-manager`）在沙箱裡看不到，直接說明無法存取，不要猜。")
}

/// Compose the system prompt for an API bot (`claude-api`).
///
/// Combines the sandbox instructions, project agent instructions (`agent_md`),
/// the bot's custom persona, and the agent name note.
pub fn compose_api_bot_system_prompt(bot: &Bot, agent_name: &str, host_root: &str, agent_md: Option<&str>) -> String {
    let base = match agent_md.map(str::trim).filter(|s| !s.is_empty()) {
        Some(md) => format!("{}\n\n{}", api_bot_sandbox_instructions(), md),
        None => api_bot_sandbox_instructions().to_string(),
    };
    let with_persona = match bot.persona.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(persona) => format!("{base}\n\n{persona}"),
        None => base,
    };
    format!("{with_persona}\n\n你在 AG Man 裡的 agent 名稱是 `{agent_name}`。\n\n{}", path_mapping_note(host_root))
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
        let prompt = compose_api_bot_system_prompt(&bot, "claude-api-opus", "/home/u/proj/", Some("# 專案說明\n這是測試專案。"));

        // Must contain sandbox environment facts
        assert!(prompt.contains("str_replace_based_edit_tool"));
        assert!(prompt.contains("每次呼叫"));
        assert!(prompt.contains("可以連網"));
        assert!(prompt.contains("`/etc` 只唯讀掛入"));
        assert!(prompt.contains("自動 compact"));
        assert!(prompt.contains("/workspace"));
        assert!(prompt.contains("HOME=/tmp"));
        assert!(prompt.contains("不能開子 agent"));
        assert!(prompt.contains("沒有 herdr"));
        assert!(prompt.contains("沒有 agent-browser"));
        assert!(prompt.contains("沒有 outbox"));
        assert!(prompt.contains("fork/resume"));
        assert!(prompt.contains("名稱是 `claude-api-opus`"));
        assert!(prompt.contains("以本段為準"));
        assert!(prompt.contains("`/workspace` 就是主機上的 `/home/u/proj`"));
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
        let prompt = compose_api_bot_system_prompt(&bot, "api-worker", "/srv/p", None);
        assert!(prompt.contains("名稱是 `api-worker`"));
        assert!(prompt.contains("/workspace"));
        assert!(prompt.contains("str_replace_based_edit_tool"));
        assert!(prompt.contains("可以連網"));
        assert!(prompt.contains("`/etc` 只唯讀掛入"));
        assert!(prompt.contains("自動 compact"));
    }
}
