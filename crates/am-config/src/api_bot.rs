use serde::{Deserialize, Serialize};

/// Claude API Bot (`[claude_api.api_bot]`) 執行限制與超額保險絲設定（SPEC §4.3d，issue #1944）。
/// 預設全關，只有在 `[claude_api] enabled = true` 且 `[claude_api.api_bot] enabled = true` 雙開時才生效。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiBotCfg {
    #[serde(default)]
    pub enabled: bool,
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
}

pub fn default_max_api_calls_per_turn() -> u32 {
    20
}

pub fn default_max_requests_per_hour() -> u32 {
    60
}

pub fn default_turn_timeout_secs() -> u64 {
    300
}

pub fn default_max_output_tokens() -> u32 {
    8192
}

pub fn default_max_checkpoint_bytes() -> usize {
    8_388_608
}

pub fn default_command_timeout_secs() -> u64 {
    60
}

pub fn default_max_tool_output_bytes() -> usize {
    65536
}

impl Default for ApiBotCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            max_api_calls_per_turn: default_max_api_calls_per_turn(),
            max_requests_per_hour: default_max_requests_per_hour(),
            turn_timeout_secs: default_turn_timeout_secs(),
            max_output_tokens: default_max_output_tokens(),
            max_checkpoint_bytes: default_max_checkpoint_bytes(),
            command_timeout_secs: default_command_timeout_secs(),
            max_tool_output_bytes: default_max_tool_output_bytes(),
            max_turn_cost_microusd: None,
        }
    }
}

impl ApiBotCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// 驗證並產生執行期結構化限制（issue #1944）。
    /// - 若 `global_enabled == false` 或 `self.enabled == false`，代表未啟用，回傳 `Ok(None)`。
    /// - 雙開時，所有限制必須在有效邊界內，且 `max_turn_cost_microusd` 必須為 `Some(1..=i64::MAX as u64)`（整數成本且不能為 0、浮點或 None）。
    pub fn validated(&self, global_enabled: bool) -> Result<Option<ValidatedLimits>, String> {
        if !global_enabled || !self.enabled {
            return Ok(None);
        }

        if !(1..=64).contains(&self.max_api_calls_per_turn) {
            return Err(format!(
                "max_api_calls_per_turn must be in 1..=64, got {}",
                self.max_api_calls_per_turn
            ));
        }

        if !(1..=10000).contains(&self.max_requests_per_hour) {
            return Err(format!(
                "max_requests_per_hour must be in 1..=10000, got {}",
                self.max_requests_per_hour
            ));
        }

        if !(1..=3600).contains(&self.turn_timeout_secs) {
            return Err(format!(
                "turn_timeout_secs must be in 1..=3600, got {}",
                self.turn_timeout_secs
            ));
        }

        if !(1..=64000).contains(&self.max_output_tokens) {
            return Err(format!(
                "max_output_tokens must be in 1..=64000, got {}",
                self.max_output_tokens
            ));
        }

        if !(1..=8_388_608).contains(&self.max_checkpoint_bytes) {
            return Err(format!(
                "max_checkpoint_bytes must be in 1..=8388608, got {}",
                self.max_checkpoint_bytes
            ));
        }

        if !(1..=300).contains(&self.command_timeout_secs) {
            return Err(format!(
                "command_timeout_secs must be in 1..=300, got {}",
                self.command_timeout_secs
            ));
        }

        if self.command_timeout_secs > self.turn_timeout_secs {
            return Err(format!(
                "command_timeout_secs ({}) cannot exceed turn_timeout_secs ({})",
                self.command_timeout_secs, self.turn_timeout_secs
            ));
        }

        if !(1..=1_048_576).contains(&self.max_tool_output_bytes) {
            return Err(format!(
                "max_tool_output_bytes must be in 1..=1048576, got {}",
                self.max_tool_output_bytes
            ));
        }

        let max_turn_cost_microusd = match self.max_turn_cost_microusd {
            Some(cost) if cost >= 1 && cost <= i64::MAX as u64 => cost as i64,
            Some(0) => return Err("max_turn_cost_microusd must be greater than 0".to_string()),
            Some(cost) => {
                return Err(format!(
                    "max_turn_cost_microusd exceeds maximum allowable limit (i64::MAX): {}",
                    cost
                ))
            }
            None => {
                return Err("max_turn_cost_microusd is required when api_bot is enabled".to_string())
            }
        };

        Ok(Some(ValidatedLimits {
            max_api_calls_per_turn: self.max_api_calls_per_turn,
            max_requests_per_hour: self.max_requests_per_hour,
            turn_timeout_secs: self.turn_timeout_secs,
            max_output_tokens: self.max_output_tokens,
            max_checkpoint_bytes: self.max_checkpoint_bytes,
            command_timeout_secs: self.command_timeout_secs,
            max_tool_output_bytes: self.max_tool_output_bytes,
            max_turn_cost_microusd,
        }))
    }
}

/// 通過驗證的限制，可安全交由 runtime 使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedLimits {
    pub max_api_calls_per_turn: u32,
    pub max_requests_per_hour: u32,
    pub turn_timeout_secs: u64,
    pub max_output_tokens: u32,
    pub max_checkpoint_bytes: usize,
    pub command_timeout_secs: u64,
    pub max_tool_output_bytes: usize,
    pub max_turn_cost_microusd: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigFile;

    #[test]
    fn default_values_and_is_default() {
        let def = ApiBotCfg::default();
        assert!(!def.enabled);
        assert_eq!(def.max_api_calls_per_turn, 20);
        assert_eq!(def.max_requests_per_hour, 60);
        assert_eq!(def.turn_timeout_secs, 300);
        assert_eq!(def.max_output_tokens, 8192);
        assert_eq!(def.max_checkpoint_bytes, 8_388_608);
        assert_eq!(def.command_timeout_secs, 60);
        assert_eq!(def.max_tool_output_bytes, 65536);
        assert_eq!(def.max_turn_cost_microusd, None);
        assert!(def.is_default());
    }

    #[test]
    fn config_file_roundtrip_omits_default() {
        let cfg = ConfigFile::default();
        let toml_str = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            !toml_str.contains("api_bot"),
            "api_bot should be omitted when default"
        );

        let roundtrip: ConfigFile = toml::from_str(&toml_str).unwrap();
        assert_eq!(roundtrip.claude_api.api_bot, ApiBotCfg::default());
    }

    #[test]
    fn parsing_from_toml_missing_fields_fills_defaults() {
        let toml_str = r#"
[claude_api]
enabled = true
[claude_api.api_bot]
enabled = true
max_turn_cost_microusd = 500000
"#;
        let parsed: ConfigFile = toml::from_str(toml_str).unwrap();
        let api_bot = parsed.claude_api.api_bot;
        assert!(api_bot.enabled);
        assert_eq!(api_bot.max_api_calls_per_turn, 20);
        assert_eq!(api_bot.max_requests_per_hour, 60);
        assert_eq!(api_bot.turn_timeout_secs, 300);
        assert_eq!(api_bot.max_output_tokens, 8192);
        assert_eq!(api_bot.max_checkpoint_bytes, 8_388_608);
        assert_eq!(api_bot.command_timeout_secs, 60);
        assert_eq!(api_bot.max_tool_output_bytes, 65536);
        assert_eq!(api_bot.max_turn_cost_microusd, Some(500000));
    }

    #[test]
    fn validated_disabled_returns_ok_none() {
        let mut cfg = ApiBotCfg::default();
        cfg.max_turn_cost_microusd = Some(100);

        // global off, api_bot off -> Ok(None)
        assert_eq!(cfg.validated(false), Ok(None));

        // global on, api_bot off -> Ok(None)
        assert_eq!(cfg.validated(true), Ok(None));

        // global off, api_bot on -> Ok(None)
        cfg.enabled = true;
        assert_eq!(cfg.validated(false), Ok(None));
    }

    #[test]
    fn validated_budget_validation() {
        let mut cfg = ApiBotCfg::default();
        cfg.enabled = true;

        // None budget is rejected when enabled
        cfg.max_turn_cost_microusd = None;
        assert!(cfg.validated(true).is_err());

        // 0 budget is rejected
        cfg.max_turn_cost_microusd = Some(0);
        assert!(cfg.validated(true).is_err());

        // Valid cost 1..=i64::MAX
        cfg.max_turn_cost_microusd = Some(1);
        let limits = cfg.validated(true).unwrap().unwrap();
        assert_eq!(limits.max_turn_cost_microusd, 1);

        cfg.max_turn_cost_microusd = Some(i64::MAX as u64);
        let limits = cfg.validated(true).unwrap().unwrap();
        assert_eq!(limits.max_turn_cost_microusd, i64::MAX);

        // Overflow i64::MAX is rejected
        cfg.max_turn_cost_microusd = Some(i64::MAX as u64 + 1);
        assert!(cfg.validated(true).is_err());

        cfg.max_turn_cost_microusd = Some(u64::MAX);
        assert!(cfg.validated(true).is_err());
    }

    #[test]
    fn validated_limits_mapping_and_boundaries() {
        let mut cfg = ApiBotCfg::default();
        cfg.enabled = true;
        cfg.max_turn_cost_microusd = Some(1000);

        // Default valid check
        let limits = cfg.validated(true).unwrap().unwrap();
        assert_eq!(limits.max_api_calls_per_turn, 20);
        assert_eq!(limits.max_requests_per_hour, 60);
        assert_eq!(limits.turn_timeout_secs, 300);
        assert_eq!(limits.max_output_tokens, 8192);
        assert_eq!(limits.max_checkpoint_bytes, 8_388_608);
        assert_eq!(limits.command_timeout_secs, 60);
        assert_eq!(limits.max_tool_output_bytes, 65536);
        assert_eq!(limits.max_turn_cost_microusd, 1000);

        // max_api_calls_per_turn boundaries (1..=64)
        cfg.max_api_calls_per_turn = 0;
        assert!(cfg.validated(true).is_err());
        cfg.max_api_calls_per_turn = 1;
        assert!(cfg.validated(true).is_ok());
        cfg.max_api_calls_per_turn = 64;
        assert!(cfg.validated(true).is_ok());
        cfg.max_api_calls_per_turn = 65;
        assert!(cfg.validated(true).is_err());
        cfg.max_api_calls_per_turn = 20;

        // max_requests_per_hour boundaries (1..=10000)
        cfg.max_requests_per_hour = 0;
        assert!(cfg.validated(true).is_err());
        cfg.max_requests_per_hour = 1;
        assert!(cfg.validated(true).is_ok());
        cfg.max_requests_per_hour = 10000;
        assert!(cfg.validated(true).is_ok());
        cfg.max_requests_per_hour = 10001;
        assert!(cfg.validated(true).is_err());
        cfg.max_requests_per_hour = 60;

        // turn_timeout_secs boundaries (1..=3600)
        cfg.turn_timeout_secs = 0;
        assert!(cfg.validated(true).is_err());
        cfg.turn_timeout_secs = 1;
        cfg.command_timeout_secs = 1; // command_timeout_secs must be <= turn_timeout_secs
        assert!(cfg.validated(true).is_ok());
        cfg.turn_timeout_secs = 3600;
        cfg.command_timeout_secs = 300;
        assert!(cfg.validated(true).is_ok());
        cfg.turn_timeout_secs = 3601;
        assert!(cfg.validated(true).is_err());
        cfg.turn_timeout_secs = 300;
        cfg.command_timeout_secs = 60;

        // command_timeout_secs boundaries (1..=300 and <= turn_timeout_secs)
        cfg.command_timeout_secs = 0;
        assert!(cfg.validated(true).is_err());
        cfg.command_timeout_secs = 1;
        assert!(cfg.validated(true).is_ok());
        cfg.command_timeout_secs = 300;
        assert!(cfg.validated(true).is_ok());
        cfg.command_timeout_secs = 301;
        assert!(cfg.validated(true).is_err());
        cfg.command_timeout_secs = 300;
        cfg.turn_timeout_secs = 100; // command_timeout_secs > turn_timeout_secs
        assert!(cfg.validated(true).is_err());
        cfg.turn_timeout_secs = 300;
        cfg.command_timeout_secs = 60;

        // max_output_tokens boundaries (1..=64000)
        cfg.max_output_tokens = 0;
        assert!(cfg.validated(true).is_err());
        cfg.max_output_tokens = 1;
        assert!(cfg.validated(true).is_ok());
        cfg.max_output_tokens = 64000;
        assert!(cfg.validated(true).is_ok());
        cfg.max_output_tokens = 64001;
        assert!(cfg.validated(true).is_err());
        cfg.max_output_tokens = 8192;

        // max_checkpoint_bytes boundaries (1..=8388608)
        cfg.max_checkpoint_bytes = 0;
        assert!(cfg.validated(true).is_err());
        cfg.max_checkpoint_bytes = 1;
        assert!(cfg.validated(true).is_ok());
        cfg.max_checkpoint_bytes = 8_388_608;
        assert!(cfg.validated(true).is_ok());
        cfg.max_checkpoint_bytes = 8_388_609;
        assert!(cfg.validated(true).is_err());
        cfg.max_checkpoint_bytes = 8_388_608;

        // max_tool_output_bytes boundaries (1..=1048576)
        cfg.max_tool_output_bytes = 0;
        assert!(cfg.validated(true).is_err());
        cfg.max_tool_output_bytes = 1;
        assert!(cfg.validated(true).is_ok());
        cfg.max_tool_output_bytes = 1_048_576;
        assert!(cfg.validated(true).is_ok());
        cfg.max_tool_output_bytes = 1_048_577;
        assert!(cfg.validated(true).is_err());
        cfg.max_tool_output_bytes = 65536;
    }

    #[test]
    fn claude_api_is_not_a_valid_kind() {
        assert!(!crate::valid_kind("claude-api"));
    }
    #[test]
    fn fractional_or_negative_money_is_not_an_integer_budget() {
        for value in ["0.25", "-1"] {
            let text =
                format!("[claude_api.api_bot]\nenabled=true\nmax_turn_cost_microusd={value}\n");
            assert!(toml::from_str::<ConfigFile>(&text).is_err());
        }
    }
}
