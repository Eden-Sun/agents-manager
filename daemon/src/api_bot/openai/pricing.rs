//! Responses API 的價表與用量換算（整數有理數運算，最後進位到微美元）。
//!
//! 價格單位是「微美元 / 百萬 tokens」的整數（例：$0.10 / 百萬 tokens ＝ 100_000）。各分項先乘價相加成一個分子，
//! 只在最後除以 1_000_000 並**進位**：低價短回合（幾個 token）不會被算成 0，也不會因逐項進位而多收。
//! 價表帶日期（`effective_date`），並由 `fixtures/gpt-6-luna-pricing-2026-11-18.json` 釘住（測試核對）。

use serde_json::Value;

/// 一個模型的標準計價（微美元／百萬 tokens）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PriceTable {
    pub model: &'static str,
    /// 價表生效日（`YYYY-MM-DD`）；改價就新增一張，不改舊的。
    pub effective_date: &'static str,
    pub input: u64,
    pub cache_read: u64,
    /// 預設期限的快取寫入（第一版只用預設期限）。
    pub cache_write: u64,
    /// 12 小時期限的快取寫入（輸入價 ×2.5）。我們不要求這個期限，但 usage 回報了就要照價算，不能當成預設期限。
    pub cache_write_12h: u64,
    pub output: u64,
    /// 輸入 token **超過**這個數時，整個請求改用長文價：輸入與快取 ×2、輸出 ×1.5。
    pub long_context_over: u64,
}

/// gpt-6-luna，標準 tier，2026-11-18：普通輸入 $0.10、快取讀 $0.01、預設寫 $0.125、輸出 $0.50（每百萬 tokens）。
/// 12 小時寫入 $0.25（輸入價 ×2.5）；輸入超過 272K tokens 時整個請求輸入與快取 ×2、輸出 ×1.5。
/// https://developers.openai.com/api/docs/models/gpt-6-luna 、
/// https://developers.openai.com/api/docs/guides/prompt-caching — checked 2026-11-18.
pub(crate) const GPT_6_LUNA_2026_11_18: PriceTable = PriceTable {
    model: "gpt-6-luna",
    effective_date: "2026-11-18",
    input: 100_000,
    cache_read: 10_000,
    cache_write: 125_000,
    cache_write_12h: 250_000,
    output: 500_000,
    long_context_over: 272_000,
};

/// 這個 binary 認得的價表（每個模型取生效日最新的一張）。找不到＝缺價，呼叫端必須在 HTTP 前拒絕。
pub(crate) fn price_table(model: &str) -> Option<&'static PriceTable> {
    const TABLES: &[PriceTable] = &[GPT_6_LUNA_2026_11_18];
    TABLES.iter().filter(|t| t.model == model).max_by_key(|t| t.effective_date)
}

/// Responses `usage` 的分項。`reasoning_tokens` 已含在 `output_tokens`，只供顯示，**不重算**。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    /// `cache_write_tokens` 之中屬於 12 小時期限的部分（`cache_write_tokens_details.ttl_12h_tokens`）；其餘是預設期限。
    pub cache_write_12h_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UsageError {
    /// 回應沒有 usage：不能當成 0 元，這一次的費用未知。
    Missing,
    /// 欄位缺漏、型別不對或互相矛盾（快取比輸入多、reasoning 比輸出多、total 對不上）。
    Inconsistent(String),
}

fn field(obj: &Value, key: &str) -> Result<u64, UsageError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => v.as_u64().ok_or_else(|| UsageError::Inconsistent(format!("{key} is not a non-negative integer"))),
    }
}

impl Usage {
    /// 解析 `response.usage`。`None`／`null` → [`UsageError::Missing`]。
    pub(crate) fn from_response(usage: Option<&Value>) -> Result<Usage, UsageError> {
        let u = match usage {
            None | Some(Value::Null) => return Err(UsageError::Missing),
            Some(u) if u.is_object() => u,
            Some(_) => return Err(UsageError::Inconsistent("usage is not an object".into())),
        };
        let input_tokens = u.get("input_tokens").and_then(Value::as_u64).ok_or_else(|| UsageError::Inconsistent("input_tokens missing".into()))?;
        let output_tokens = u.get("output_tokens").and_then(Value::as_u64).ok_or_else(|| UsageError::Inconsistent("output_tokens missing".into()))?;
        let in_details = u.get("input_tokens_details").cloned().unwrap_or(Value::Null);
        let out_details = u.get("output_tokens_details").cloned().unwrap_or(Value::Null);
        let cache_write_tokens = field(&in_details, "cache_write_tokens")?;
        // 寫入期限分項（有給才核對）：兩種期限相加必須等於 cache_write_tokens；不認得的期限沒有價可算。
        let cache_write_12h_tokens = match in_details.get("cache_write_tokens_details") {
            None | Some(Value::Null) => 0,
            Some(Value::Object(by_ttl)) => {
                if by_ttl.keys().any(|k| k != "ttl_30m_tokens" && k != "ttl_12h_tokens") {
                    return Err(UsageError::Inconsistent("unknown cache-write lifetime".into()));
                }
                let by_ttl = Value::Object(by_ttl.clone());
                let (w30, w12) = (field(&by_ttl, "ttl_30m_tokens")?, field(&by_ttl, "ttl_12h_tokens")?);
                if w30.checked_add(w12) != Some(cache_write_tokens) {
                    return Err(UsageError::Inconsistent("cache-write lifetimes do not sum to cache_write_tokens".into()));
                }
                w12
            }
            Some(_) => return Err(UsageError::Inconsistent("cache_write_tokens_details is not an object".into())),
        };
        let usage = Usage {
            input_tokens,
            cached_tokens: field(&in_details, "cached_tokens")?,
            cache_write_tokens,
            cache_write_12h_tokens,
            output_tokens,
            reasoning_tokens: field(&out_details, "reasoning_tokens")?,
        };
        if usage.cached_tokens.checked_add(usage.cache_write_tokens).is_none_or(|n| n > usage.input_tokens) {
            return Err(UsageError::Inconsistent("cached + cache-write tokens exceed input tokens".into()));
        }
        if usage.reasoning_tokens > usage.output_tokens {
            return Err(UsageError::Inconsistent("reasoning tokens exceed output tokens".into()));
        }
        if let Some(total) = u.get("total_tokens").and_then(Value::as_u64) {
            if input_tokens.checked_add(output_tokens) != Some(total) {
                return Err(UsageError::Inconsistent("total_tokens does not equal input + output".into()));
            }
        }
        Ok(usage)
    }

    /// 普通輸入＝input − 快取讀 − 快取寫（建構時已驗證不會下溢）。
    pub(crate) fn ordinary_input(&self) -> u64 {
        self.input_tokens - self.cached_tokens - self.cache_write_tokens
    }
}

/// 估計費用（微美元，進位）。溢位或分項矛盾回 `None`（呼叫端當作未知，不當作 0）。
/// 輸入超過 `long_context_over` 時整個請求用長文價（輸入與快取 ×2、輸出 ×1.5）。
pub(crate) fn cost_microusd(table: &PriceTable, usage: &Usage) -> Option<u64> {
    let part = |tokens: u64, price: u64| u128::from(tokens) * u128::from(price);
    let write_default = usage.cache_write_tokens.checked_sub(usage.cache_write_12h_tokens)?;
    let ordinary = usage.input_tokens.checked_sub(usage.cached_tokens)?.checked_sub(usage.cache_write_tokens)?;
    let input = part(ordinary, table.input)
        + part(usage.cached_tokens, table.cache_read)
        + part(write_default, table.cache_write)
        + part(usage.cache_write_12h_tokens, table.cache_write_12h);
    let output = part(usage.output_tokens, table.output); // reasoning_tokens 已含在 output_tokens
    round_up(input, output, usage.input_tokens > table.long_context_over)
}

/// 預留用的上界（微美元，進位）：`input_tokens_bound` 個輸入 token 全部以最高輸入單價計、輸出用滿 `max_output_tokens`；
/// 上界超過長文門檻就用長文價。任何真實 usage（輸入 ≤ 上界、輸出 ≤ `max_output_tokens`）的 [`cost_microusd`] 都不會超過它。
pub(crate) fn quote_upper_bound_microusd(table: &PriceTable, input_tokens_bound: u64, max_output_tokens: u64) -> Option<u64> {
    let top = table.input.max(table.cache_read).max(table.cache_write).max(table.cache_write_12h);
    let input = u128::from(input_tokens_bound) * u128::from(top);
    let output = u128::from(max_output_tokens) * u128::from(table.output);
    round_up(input, output, input_tokens_bound > table.long_context_over)
}

/// 價表只涵蓋標準 tier：回應回報別的 tier（flex／priority…）就沒有核准的價，呼叫端要當作費用未知。
pub(crate) fn is_standard_tier(service_tier: Option<&str>) -> bool {
    matches!(service_tier, None | Some("default"))
}

/// 分子以「半個單位」相加（長文價的輸出 ×1.5 才能保持整數），最後只進位一次。
fn round_up(input: u128, output: u128, long_context: bool) -> Option<u64> {
    let (input_halves, output_halves) = if long_context { (4, 3) } else { (2, 2) };
    let halves = input.checked_mul(input_halves)?.checked_add(output.checked_mul(output_halves)?)?;
    u64::try_from(halves.div_ceil(2_000_000)).ok()
}
