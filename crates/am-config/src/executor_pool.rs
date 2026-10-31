//! 專家模式執行者池的條目（使用者 2026-10-31：條目＝kind／model／effort／context／數量）。
//!
//! 舊版形狀 `{kind: n}`（config 或 DB 的 `projects.executor_pool_json`）讀進來時轉成條目
//! `{id: "<kind>-legacy", model: "", effort: None, context_tokens: None, count: n}`；model 空字串＝該 kind 任何模型，
//! 只是過渡。下一次寫入就存成陣列。

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorPoolEntry {
    /// web 產生的短 slug，同一專案內唯一。
    pub id: String,
    pub kind: String,
    /// 空字串＝該 kind 任何模型（只給舊資料過渡用）。
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// None＝模型預設；有值＝寫進啟動參數的 context 上限（token 數）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    /// 這一條最多可增聘幾顆（1..=10）。
    pub count: u32,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PoolShape {
    Entries(Vec<ExecutorPoolEntry>),
    Legacy(BTreeMap<String, u32>),
}

/// 同時接受陣列（現行）與 `{kind: n}`（舊版）兩種形狀；舊形狀只保留數量 > 0 的 kind。
pub fn deserialize_pool<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ExecutorPoolEntry>, D::Error> {
    Ok(match PoolShape::deserialize(d)? {
        PoolShape::Entries(entries) => entries,
        PoolShape::Legacy(map) => legacy_entries(map),
    })
}

fn legacy_entries(map: BTreeMap<String, u32>) -> Vec<ExecutorPoolEntry> {
    map.into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(kind, count)| ExecutorPoolEntry {
            id: format!("{kind}-legacy"),
            kind,
            model: String::new(),
            effort: None,
            context_tokens: None,
            count,
        })
        .collect()
}

/// DB 的 `executor_pool_json` 字串 → 條目（壞掉或空就當沒有）。
pub fn parse_pool_json(raw: &str) -> Vec<ExecutorPoolEntry> {
    let mut de = serde_json::Deserializer::from_str(raw);
    deserialize_pool(&mut de).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Wrap {
        #[serde(default, deserialize_with = "deserialize_pool")]
        executor_pool: Vec<ExecutorPoolEntry>,
    }

    #[test]
    fn legacy_map_becomes_one_any_model_entry_per_kind() {
        let w: Wrap = toml::from_str("executor_pool = { codex = 2, claude = 1, agy = 0 }").unwrap();
        let ids: Vec<_> = w.executor_pool.iter().map(|e| (e.id.as_str(), e.kind.as_str(), e.model.as_str(), e.count)).collect();
        assert_eq!(ids, vec![("claude-legacy", "claude", "", 1), ("codex-legacy", "codex", "", 2)]);
        assert_eq!(parse_pool_json(r#"{"codex":2}"#)[0].id, "codex-legacy");
    }

    #[test]
    fn array_of_tables_round_trips() {
        let raw = r#"
            [[executor_pool]]
            id = "codex-terra-medium-256k"
            kind = "codex"
            model = "gpt-6.1-terra"
            effort = "medium"
            context_tokens = 256000
            count = 1
        "#;
        let w: Wrap = toml::from_str(raw).unwrap();
        assert_eq!(w.executor_pool[0].context_tokens, Some(256_000));
        let json = serde_json::to_string(&w.executor_pool).unwrap();
        assert_eq!(parse_pool_json(&json), w.executor_pool);
        assert!(parse_pool_json("{}").is_empty());
    }
}

// ---- 條目與 role policy 共用的 context 規則與啟動參數（daemon／am-lifecycle 都用）----
pub const CONTEXT_MIN: u32 = 32_000;
/// claude 的 `[1m]` 變體以上才用得到的門檻：超過它就把模型換成 `<model>[1m]`。
pub const CLAUDE_BASE_WINDOW: u32 = 200_000;

/// 模型視窗（token）。`/api/models` 目前沒有 context window 欄位，所以用家族預設：
/// claude 的 opus／sonnet 有 `[1m]` 變體→1M（超過 200k 的條目會套 `[1m]`）；haiku 與其他 claude model
/// （含空字串）→200k，因為 haiku 沒有 1M 變體，`haiku[1m]` 會啟動失敗；codex 400k；其他 kind 不支援 context（0）。
pub fn context_window(kind: &str, model: &str) -> u32 {
    match kind {
        "claude" if model.contains("opus") || model.contains("sonnet") => 1_000_000,
        "claude" => CLAUDE_BASE_WINDOW,
        "codex" => 400_000,
        _ => 0,
    }
}

/// 增聘時 context 寫進啟動參數：claude → env `CLAUDE_CODE_AUTO_COMPACT_WINDOW`（超過 200k 改用 `[1m]` 模型），
/// codex → `-c model_auto_compact_token_limit=N`，agy 不支援（驗證時已擋）。
pub struct LaunchParams {
    pub model: Option<String>,
    pub env: BTreeMap<String, String>,
    pub args: Vec<String>,
}

pub fn launch_params(entry: &ExecutorPoolEntry) -> LaunchParams {
    let model = (!entry.model.is_empty()).then(|| entry.model.clone());
    let mut out = LaunchParams { model, env: BTreeMap::new(), args: Vec::new() };
    let Some(tokens) = entry.context_tokens else { return out };
    match entry.kind.as_str() {
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
