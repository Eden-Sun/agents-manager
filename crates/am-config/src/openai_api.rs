//! `[openai_api]`（openai-api bot kind 的設定，切片 C）：**預設關閉**，沒寫這段設定檔的長相不變。
//!
//! - `enabled`（總開關，預設 false）與 `key_file`（只是路徑，key 本身不進設定檔；跟 Claude 的 key 分開）。
//! - `[openai_api.api_bot]`：執行限制與總花費上限，**共用** `api_bot::ApiBotCfg` 的驗證（同一套邊界），
//!   但 Claude 專屬的 compaction／clear_tool_uses／auto-lower-effort／fast 不開放給 openai（沒有對應欄位）。
//! - 本片只定義設定與驗證，不接生命週期（不新增 bot kind，那是切片 D）。

use crate::api_bot::{
    default_command_timeout_secs, default_max_api_calls_per_turn, default_max_checkpoint_bytes, default_max_output_tokens, default_max_requests_per_hour,
    default_max_tool_output_bytes, default_turn_timeout_secs, ApiBotCfg, ValidatedLimits,
};
use serde::{Deserialize, Serialize};

pub fn default_openai_api_key_file() -> String {
    "~/.config/agents-manager/openai-api-key".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAiApiCfg {
    #[serde(default)]
    pub enabled: bool,
    /// key 檔路徑（`key_file::read_key` 讀；權限規則同 Claude 的）。**不是** Claude 的那一把。
    #[serde(default = "default_openai_api_key_file")]
    pub key_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance_anchor_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance_anchor_at: Option<String>,
    /// 用 platform 登入 token 讀 billing credit_grants 的實際餘額（`openai_console_balance`）。
    #[serde(default = "default_true")]
    pub console_balance_enabled: bool,
    #[serde(default = "default_console_balance_interval_minutes")]
    pub console_balance_interval_minutes: u64,
    /// platform localStorage `oai-auth:onboard` 匯出的 `{access_token, refresh_token, id_token}`（**是憑證**，0600）。
    #[serde(default = "default_openai_console_state_file")]
    pub console_state_file: String,
    /// 指定要讀哪個 org；沒設就用唯一的非 personal org，否則第一個。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_org_id: Option<String>,
    #[serde(default, skip_serializing_if = "OpenAiApiBotCfg::is_default")]
    pub api_bot: OpenAiApiBotCfg,
}

fn default_true() -> bool {
    true
}
fn default_console_balance_interval_minutes() -> u64 {
    30
}
pub fn default_openai_console_state_file() -> String {
    "~/.config/agents-manager/openai-console-state.json".into()
}

impl Default for OpenAiApiCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            key_file: default_openai_api_key_file(),
            balance_anchor_usd: None,
            balance_anchor_at: None,
            console_balance_enabled: true,
            console_balance_interval_minutes: default_console_balance_interval_minutes(),
            console_state_file: default_openai_console_state_file(),
            console_org_id: None,
            api_bot: OpenAiApiBotCfg::default(),
        }
    }
}

impl OpenAiApiCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `[openai_api.api_bot]`：Claude 的 `ApiBotCfg` 去掉 Claude 專屬欄位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAiApiBotCfg {
    #[serde(default = "default_max_api_calls_per_turn")]
    pub max_api_calls_per_turn: u32,
    #[serde(default = "default_max_requests_per_hour")]
    pub max_requests_per_hour: u32,
    #[serde(default = "default_turn_timeout_secs")]
    pub turn_timeout_secs: u64,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "default_max_checkpoint_bytes")]
    pub max_checkpoint_bytes: usize,
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u64,
    #[serde(default = "default_max_tool_output_bytes")]
    pub max_tool_output_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turn_cost_microusd: Option<u64>,
    /// 總花費上限（美元），從 `total_cap_since` 起算，**只算 openai 的 attempt**（不佔 Claude 的額）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cap_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cap_since: Option<String>,
}

impl Default for OpenAiApiBotCfg {
    fn default() -> Self {
        Self {
            max_api_calls_per_turn: default_max_api_calls_per_turn(),
            max_requests_per_hour: default_max_requests_per_hour(),
            turn_timeout_secs: default_turn_timeout_secs(),
            max_output_tokens: default_max_output_tokens(),
            max_checkpoint_bytes: default_max_checkpoint_bytes(),
            command_timeout_secs: default_command_timeout_secs(),
            max_tool_output_bytes: default_max_tool_output_bytes(),
            max_turn_cost_microusd: None,
            total_cap_usd: None,
            total_cap_since: None,
        }
    }
}

impl OpenAiApiBotCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// 轉成共用驗證用的 `ApiBotCfg`（Claude 專屬欄位取預設，驗證不看它們以外的值）。
    fn as_shared(&self) -> ApiBotCfg {
        ApiBotCfg {
            enabled: true,
            max_api_calls_per_turn: self.max_api_calls_per_turn,
            max_requests_per_hour: self.max_requests_per_hour,
            turn_timeout_secs: self.turn_timeout_secs,
            max_output_tokens: self.max_output_tokens,
            max_checkpoint_bytes: self.max_checkpoint_bytes,
            command_timeout_secs: self.command_timeout_secs,
            max_tool_output_bytes: self.max_tool_output_bytes,
            max_turn_cost_microusd: self.max_turn_cost_microusd,
            total_cap_usd: self.total_cap_usd,
            total_cap_since: self.total_cap_since.clone(),
            ..ApiBotCfg::default()
        }
    }

    /// 同 `ApiBotCfg::validated`：`enabled == false` 回 `Ok(None)`；開了就檢查所有限制。
    /// 單回合上限**可選**（使用者 2026-11-19：留空＝不設上限）：沒設就用 `i64::MAX` 當上限，預留與結算照常記帳，只是不會因上限擋下。
    /// 設了的話仍必須是 1 以上的整數（0 是無效值，不是「不限」）。
    pub fn validated(&self, enabled: bool) -> Result<Option<ValidatedLimits>, String> {
        let mut shared = self.as_shared();
        if shared.max_turn_cost_microusd.is_none() {
            shared.max_turn_cost_microusd = Some(i64::MAX as u64);
        }
        shared.validated(enabled)
    }
}
