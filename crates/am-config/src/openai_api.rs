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
    #[serde(default, skip_serializing_if = "OpenAiApiBotCfg::is_default")]
    pub api_bot: OpenAiApiBotCfg,
}

impl Default for OpenAiApiCfg {
    fn default() -> Self {
        Self { enabled: false, key_file: default_openai_api_key_file(), api_bot: OpenAiApiBotCfg::default() }
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

    /// 同 `ApiBotCfg::validated`：`enabled == false` 回 `Ok(None)`；開了就檢查所有限制（含必填的 `max_turn_cost_microusd`）。
    pub fn validated(&self, enabled: bool) -> Result<Option<ValidatedLimits>, String> {
        self.as_shared().validated(enabled)
    }
}
