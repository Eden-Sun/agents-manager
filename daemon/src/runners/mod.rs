//! 上層 runner / handler 聚合模組（r5a9）。
//!
//! 封裝對 `App` 與 composition 的依賴，作為背景工作與 HTTP 路由入口，
//! 下層模組只保留核心邏輯。

pub mod ask_answers;
pub mod codex_live;
pub mod credential_spawn;
pub mod dangerous_rm;
pub mod default_session;
pub mod hook_inbox;
pub mod judge;
pub mod login_assist;
pub mod login_prompt;
pub mod pending_question;
pub mod primary_keep_warm;
pub mod prompt_suggestion;
pub mod quota_grok;
pub mod reconcile;
pub mod release_triage;
pub mod rewind;
pub mod session_paused;
pub mod tui_prompts;

