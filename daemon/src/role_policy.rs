//! 角色 policy（`projects.role_policy_json`）與 bot 執行期覆寫（`bots.runtime_override_json`）的驗證（B 期，User-only 寫入）。
//! 形狀與合併規則在 `am_base::runtime_policy`；這裡只做看 kind／model 的部分：context 視窗（與池條目同一套規則）、
//! 保溫分鐘要小於該 kind 的 TTL。錯誤碼：`invalid_runtime_policy`（帶 `field`），context 另有 `context_*`。

use crate::executor_pool::{context_window, CLAUDE_BASE_WINDOW, CONTEXT_MIN};
use crate::lifecycle::LcError;
use am_base::runtime_policy::RuntimeSettings;
use serde_json::{json, Value};

/// 角色 policy 不知道 kind／model，context 上限取所有 kind 的最大視窗（實際套用時再由 `build_overlay` 夾到該 bot 的視窗）。
const ROLE_CONTEXT_MAX: u32 = 1_000_000;
const KEEP_WARM_MIN: i64 = 5;
const KEEP_WARM_MAX: i64 = 59;
const HOT_COMPACT_MIN: i64 = 5;
const HOT_COMPACT_MAX: i64 = 119;

fn bad(field: &str, message: impl Into<String>) -> LcError {
    LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_runtime_policy", "field": field, "message": message.into()}))
}

/// 共用：形狀（object、未知欄位拒絕）、分鐘與百分比範圍、熱壓＞保溫（同一物件內）。
fn parse(v: &Value) -> Result<RuntimeSettings, LcError> {
    if !v.is_object() {
        return Err(bad("", "must be a JSON object"));
    }
    let s: RuntimeSettings = serde_json::from_value(v.clone()).map_err(|e| bad("", e.to_string()))?;
    if let Some(m) = s.keep_warm_after_min {
        if !(KEEP_WARM_MIN..=KEEP_WARM_MAX).contains(&m) {
            return Err(bad("keep_warm_after_min", format!("must be {KEEP_WARM_MIN}..={KEEP_WARM_MAX}")));
        }
    }
    if let Some(m) = s.hot_compact_after_min {
        if !(HOT_COMPACT_MIN..=HOT_COMPACT_MAX).contains(&m) {
            return Err(bad("hot_compact_after_min", format!("must be {HOT_COMPACT_MIN}..={HOT_COMPACT_MAX}")));
        }
    }
    if let (Some(k), Some(h)) = (s.keep_warm_after_min, s.hot_compact_after_min) {
        if h <= k {
            return Err(bad("hot_compact_after_min", "must be greater than keep_warm_after_min"));
        }
    }
    if let Some(p) = s.hot_compact_min_pct {
        if !(0..=100).contains(&p) {
            return Err(bad("hot_compact_min_pct", "must be 0..=100"));
        }
    }
    Ok(s)
}

/// 角色 policy 的一個 role（`manager`／`executor`／`advisor`／`specialist`）。回傳正規化後要存的物件。
pub fn validate_role(role: &str, v: &Value) -> Result<Value, LcError> {
    let s = parse(v).map_err(|e| with_role(e, role))?;
    if let Some(t) = s.context_tokens {
        if t < CONTEXT_MIN || t > ROLE_CONTEXT_MAX {
            return Err(with_role(
                bad("context_tokens", format!("must be {CONTEXT_MIN}..={ROLE_CONTEXT_MAX}（實際套用時會夾到該 bot 的模型視窗）")),
                role,
            ));
        }
    }
    Ok(serde_json::to_value(&s).unwrap_or(Value::Null))
}

fn with_role(e: LcError, role: &str) -> LcError {
    match e {
        LcError::BadValue(mut v) => {
            v["role"] = json!(role);
            LcError::BadValue(v)
        }
        other => other,
    }
}

/// bot 覆寫：看得到 kind／model，所以 context 用該 kind 的視窗規則（與池條目相同），保溫分鐘要小於該 kind 的 TTL。
pub fn validate_bot(kind: &str, model: &str, v: &Value) -> Result<Value, LcError> {
    let s = parse(v)?;
    if let Some(t) = s.context_tokens {
        if kind == "agy" {
            return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "context_unsupported", "kind": "agy"})));
        }
        let window = context_window(kind, model);
        if t < CONTEXT_MIN || t > window {
            return Err(LcError::BadValue(json!({
                "error": "bad_request", "reason": "context_out_of_range", "field": "context_tokens", "min": CONTEXT_MIN, "max": window, "got": t,
            })));
        }
        if kind == "claude" && t > CLAUDE_BASE_WINDOW && model.trim().is_empty() {
            return Err(bad("context_tokens", "a claude context above 200000 needs a model"));
        }
    }
    if let (Some(m), Some(ttl)) = (s.keep_warm_after_min, am_base::cache_clock::ttl_secs(kind)) {
        if m * 60 >= ttl {
            return Err(bad("keep_warm_after_min", format!("must be shorter than the {kind} cache TTL ({} min)", ttl / 60)));
        }
    }
    Ok(serde_json::to_value(&s).unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(e: LcError) -> (String, String) {
        match e {
            LcError::BadValue(v) => (v["reason"].as_str().unwrap_or("").into(), v["field"].as_str().unwrap_or("").into()),
            other => (format!("{other:?}"), String::new()),
        }
    }

    #[test]
    fn role_values_are_checked_and_unknown_fields_refused() {
        assert!(validate_role("manager", &json!({"keep_warm": true, "keep_warm_after_min": 30})).is_ok());
        assert_eq!(reason(validate_role("manager", &json!({"bogus": 1})).unwrap_err()).0, "invalid_runtime_policy");
        assert_eq!(reason(validate_role("manager", &json!("x")).unwrap_err()).0, "invalid_runtime_policy");
        assert_eq!(reason(validate_role("manager", &json!({"context_tokens": 1000})).unwrap_err()), ("invalid_runtime_policy".into(), "context_tokens".into()));
        assert_eq!(reason(validate_role("manager", &json!({"keep_warm_after_min": 60})).unwrap_err()).1, "keep_warm_after_min");
        assert_eq!(reason(validate_role("manager", &json!({"keep_warm_after_min": 30, "hot_compact_after_min": 30})).unwrap_err()).1, "hot_compact_after_min");
        assert_eq!(reason(validate_role("manager", &json!({"hot_compact_min_pct": 101})).unwrap_err()).1, "hot_compact_min_pct");
        let err = validate_role("advisor", &json!({"keep_warm_after_min": 99})).unwrap_err();
        assert_eq!(match &err { LcError::BadValue(v) => v["role"].clone(), _ => Value::Null }, json!("advisor"), "錯誤要帶上是哪個角色");
    }

    #[test]
    fn bot_override_uses_the_kind_window_and_ttl() {
        // 不認得的 claude 模型視窗 200k：512k 超出；opus／haiku 有 1M 視窗。
        assert_eq!(reason(validate_bot("claude", "claude-x", &json!({"context_tokens": 512000})).unwrap_err()).0, "context_out_of_range");
        assert!(validate_bot("claude", "haiku", &json!({"context_tokens": 512000})).is_ok());
        assert!(validate_bot("claude", "opus", &json!({"context_tokens": 512000})).is_ok());
        // claude 超過 200k 必須有模型。
        assert!(validate_bot("claude", "", &json!({"context_tokens": 300000})).is_err());
        assert_eq!(reason(validate_bot("agy", "", &json!({"context_tokens": 300000})).unwrap_err()).0, "context_unsupported");
        assert!(validate_bot("codex", "gpt-6", &json!({"context_tokens": 400000})).is_ok());
        // 保溫分鐘 < claude TTL（60 分）；codex 沒有 TTL 限制。
        assert!(validate_bot("claude", "opus", &json!({"keep_warm_after_min": 59})).is_ok());
        assert_eq!(reason(validate_bot("claude", "opus", &json!({"keep_warm_after_min": 60})).unwrap_err()).1, "keep_warm_after_min");
    }
}
