//! 專家模式執行者池的條目規則（使用者 2026-10-31）：PATCH 驗證、增聘解析、用量與 context 啟動參數。
//! 條目的型別與舊形狀轉換在 `am_config::executor_pool`；這裡只放 daemon 的規則，`api.rs` 只做接線。

use crate::config::{BotCfg, ExecutorPoolEntry};
use crate::lifecycle::LcError;
use serde_json::{json, Value};
use std::collections::BTreeMap;

// context 規則與啟動參數的實作在 am-config（role policy 與 am-lifecycle 也用同一份）。
pub use am_config::executor_pool::{context_window, launch_params, LaunchParams, CLAUDE_BASE_WINDOW, CONTEXT_MIN};

pub const MAX_ENTRIES: usize = 20;
pub const MAX_COUNT: u32 = 10;
/// PATCH `executor_pool` 的整份陣列驗證；整份覆蓋，所以這裡一次檢查全部條目。
pub fn parse_patch(raw: &Value) -> Result<Vec<ExecutorPoolEntry>, LcError> {
    let Value::Array(items) = raw else {
        return Err(LcError::Bad("executor_pool must be an array of entries".into()));
    };
    if items.len() > MAX_ENTRIES {
        return Err(LcError::Bad(format!("executor_pool holds at most {MAX_ENTRIES} entries")));
    }
    let mut out: Vec<ExecutorPoolEntry> = Vec::with_capacity(items.len());
    for item in items {
        let mut e: ExecutorPoolEntry = serde_json::from_value(item.clone())
            .map_err(|err| LcError::Bad(format!("invalid executor_pool entry: {err}")))?;
        e.effort = e.effort.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        e.model = e.model.trim().to_string();
        validate_entry(&e)?;
        if out.iter().any(|o| o.id == e.id) {
            return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "duplicate_pool_id", "id": e.id})));
        }
        if out.iter().any(|o| same_shape(o, &e)) {
            return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "duplicate_pool_entry", "id": e.id})));
        }
        out.push(e);
    }
    Ok(out)
}

fn validate_entry(e: &ExecutorPoolEntry) -> Result<(), LcError> {
    let id_ok = !e.id.is_empty() && e.id.len() <= 40 && e.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !id_ok {
        return Err(LcError::Bad(format!("pool entry id must be a short slug [a-z0-9-], got `{}`", e.id)));
    }
    if !crate::config::valid_kind(&e.kind) {
        return Err(LcError::Bad(format!("invalid kind in executor_pool: {}", e.kind)));
    }
    if !(1..=MAX_COUNT).contains(&e.count) {
        return Err(LcError::Bad(format!("executor_pool count for {} must be between 1 and {MAX_COUNT}", e.id)));
    }
    if let Some(tokens) = e.context_tokens {
        if e.kind == "agy" {
            return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "context_unsupported", "kind": "agy"})));
        }
        let window = context_window(&e.kind, &e.model);
        if tokens < CONTEXT_MIN || tokens > window {
            return Err(LcError::BadValue(json!({
                "error": "bad_request", "reason": "context_out_of_range", "min": CONTEXT_MIN, "max": window, "got": tokens,
            })));
        }
        // claude 超過 200k 要靠 `[1m]` 變體，沒有模型名就無從套用。
        if e.kind == "claude" && tokens > CLAUDE_BASE_WINDOW && e.model.is_empty() {
            return Err(LcError::Bad("a claude context above 200000 needs a model".into()));
        }
    }
    Ok(())
}

fn same_shape(a: &ExecutorPoolEntry, b: &ExecutorPoolEntry) -> bool {
    a.kind == b.kind && a.model == b.model && a.effort == b.effort && a.context_tokens == b.context_tokens
}

/// 增聘要套用的條目與（manager 才有的）上限檢查結果。`None` 表示 User 沒指定條目、不受池限制。
pub fn resolve_hire<'a>(
    entries: &'a [ExecutorPoolEntry],
    manager: bool,
    kind: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    pool_entry: Option<&str>,
) -> Result<Option<&'a ExecutorPoolEntry>, LcError> {
    let model = model.filter(|m| !m.trim().is_empty());
    let effort = effort.filter(|e| !e.trim().is_empty());
    if let Some(id) = pool_entry {
        let Some(entry) = entries.iter().find(|e| e.id == id) else {
            return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "not_in_pool", "pool_entry": id})));
        };
        // kind／model／effort 一律由條目決定：帶了不同的值就拒絕（model 空字串的條目接受任何模型）。
        let kind_bad = kind.is_some_and(|k| k != entry.kind);
        let model_bad = model.is_some_and(|m| !entry.model.is_empty() && m != entry.model);
        let effort_bad = effort.is_some_and(|e| entry.effort.as_deref() != Some(e));
        if kind_bad || model_bad || effort_bad {
            return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "pool_entry_mismatch", "pool_entry": id})));
        }
        return Ok(Some(entry));
    }
    if !manager {
        return Ok(None);
    }
    let kind = kind.ok_or_else(|| LcError::Bad("kind is required".into()))?;
    let candidates: Vec<&ExecutorPoolEntry> = entries
        .iter()
        .filter(|e| e.kind == kind && model.is_none_or(|m| e.model.is_empty() || e.model == m))
        .collect();
    match candidates.as_slice() {
        [] => Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "kind_not_in_pool", "kind": kind}))),
        [only] => Ok(Some(only)),
        many => Err(LcError::BadValue(json!({
            "error": "bad_request", "reason": "pool_entry_required", "entries": many.iter().map(|e| entry_json(e, 0)).collect::<Vec<_>>(),
        }))),
    }
}

/// 條目的已用數＝同專案 executor 或 specialist 中 `pool_entry` 等於它 id 的 bot 數（只看 config）。
/// 專家與執行者共用同一個池（specialist 增聘走同一套 resolve_hire／check_capacity）。
pub fn used(bots: &[BotCfg], entry_id: &str) -> u32 {
    bots.iter()
        .filter(|b| matches!(b.role.as_deref(), Some("executor" | "specialist")) && b.pool_entry.as_deref() == Some(entry_id))
        .count() as u32
}

/// manager 增聘時，條目滿了就 409。
pub fn check_capacity(entry: &ExecutorPoolEntry, bots: &[BotCfg]) -> Result<(), LcError> {
    let current = used(bots, &entry.id);
    if current >= entry.count {
        return Err(LcError::Conflict(json!({
            "error": "conflict", "reason": "executor_pool_full", "pool_entry": entry.id, "current": current, "max": entry.count,
        })));
    }
    Ok(())
}

/// `/api/state` 與 peers 用的條目 JSON，附上已用數。
pub fn entry_json(e: &ExecutorPoolEntry, used: u32) -> Value {
    json!({
        "id": e.id, "kind": e.kind, "model": e.model, "effort": e.effort,
        "context_tokens": e.context_tokens, "count": e.count, "used": used,
    })
}

pub fn list_json(entries: &[ExecutorPoolEntry], bots: &[BotCfg]) -> Value {
    Value::Array(entries.iter().map(|e| entry_json(e, used(bots, &e.id))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: &str, model: &str, effort: Option<&str>, ctx: Option<u32>, count: u32) -> ExecutorPoolEntry {
        ExecutorPoolEntry { id: id.into(), kind: kind.into(), model: model.into(), effort: effort.map(Into::into), context_tokens: ctx, count }
    }

    fn reason(e: LcError) -> String {
        match e {
            LcError::BadValue(v) | LcError::Forbidden(v) | LcError::Conflict(v) => v["reason"].as_str().unwrap_or("").to_string(),
            LcError::Bad(m) => format!("bad:{m}"),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn patch_accepts_a_valid_array_and_rejects_the_bad_ones() {
        let ok = parse_patch(&json!([
            {"id": "codex-terra-medium-256k", "kind": "codex", "model": "gpt-6.1-terra", "effort": "medium", "context_tokens": 256000, "count": 1},
            {"id": "agy-flash", "kind": "agy", "model": "gemini-3.8-flash", "effort": "low", "count": 2},
        ])).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(reason(parse_patch(&json!({"claude": 1})).unwrap_err()), "bad:executor_pool must be an array of entries");
        assert_eq!(reason(parse_patch(&json!([{"id": "a", "kind": "claude", "model": "m", "count": 0}])).unwrap_err()), "bad:executor_pool count for a must be between 1 and 10");
        assert_eq!(reason(parse_patch(&json!([{"id": "A B", "kind": "claude", "model": "m", "count": 1}])).unwrap_err()), "bad:pool entry id must be a short slug [a-z0-9-], got `A B`");
        assert_eq!(reason(parse_patch(&json!([{"id": "x", "kind": "nope", "model": "m", "count": 1}])).unwrap_err()), "bad:invalid kind in executor_pool: nope");
    }

    #[test]
    fn duplicates_and_full_pool_are_rejected() {
        let same = json!({"id": "a", "kind": "codex", "model": "t", "effort": "low", "context_tokens": 300000, "count": 1});
        let twin = json!({"id": "b", "kind": "codex", "model": "t", "effort": "low", "context_tokens": 300000, "count": 2});
        assert_eq!(reason(parse_patch(&json!([same.clone(), twin])).unwrap_err()), "duplicate_pool_entry");
        let dup_id = json!({"id": "a", "kind": "claude", "model": "t", "count": 1});
        assert_eq!(reason(parse_patch(&json!([same, dup_id])).unwrap_err()), "duplicate_pool_id");
        let many: Vec<Value> = (0..21).map(|i| json!({"id": format!("e{i}"), "kind": "claude", "model": format!("m{i}"), "count": 1})).collect();
        assert!(parse_patch(&Value::Array(many)).is_err());
    }

    #[test]
    fn haiku_has_no_1m_window() {
        let haiku = json!([{"id": "h", "kind": "claude", "model": "haiku", "context_tokens": 300000, "count": 1}]);
        assert_eq!(reason(parse_patch(&haiku).unwrap_err()), "context_out_of_range");
        assert!(parse_patch(&json!([{"id": "h", "kind": "claude", "model": "haiku", "context_tokens": 200000, "count": 1}])).is_ok());
        assert!(parse_patch(&json!([{"id": "o", "kind": "claude", "model": "opus", "context_tokens": 900000, "count": 1}])).is_ok());
        assert_eq!(context_window("claude", "haiku"), 200_000);
        assert_eq!(context_window("claude", "sonnet"), 1_000_000);
    }

    #[test]
    fn context_bounds_and_agy_are_checked() {
        let bad_agy = json!([{"id": "f", "kind": "agy", "model": "g", "context_tokens": 300000, "count": 1}]);
        assert_eq!(reason(parse_patch(&bad_agy).unwrap_err()), "context_unsupported");
        let low = json!([{"id": "c", "kind": "codex", "model": "t", "context_tokens": 199999, "count": 1}]);
        assert_eq!(reason(parse_patch(&low).unwrap_err()), "context_out_of_range");
        let high = json!([{"id": "c", "kind": "codex", "model": "t", "context_tokens": 400001, "count": 1}]);
        assert_eq!(reason(parse_patch(&high).unwrap_err()), "context_out_of_range");
        let no_model = json!([{"id": "c", "kind": "claude", "model": "", "context_tokens": 300000, "count": 1}]);
        assert!(parse_patch(&no_model).is_err());
        assert!(parse_patch(&json!([{"id": "c", "kind": "claude", "model": "sonnet", "context_tokens": 300000, "count": 1}])).is_ok());
    }

    #[test]
    fn manager_hire_needs_a_single_matching_entry() {
        let entries = vec![
            entry("codex-terra", "codex", "terra", Some("medium"), Some(256_000), 1),
            entry("codex-luna", "codex", "luna", Some("xhigh"), Some(256_000), 1),
            entry("claude-any", "claude", "", None, None, 2),
        ];
        assert_eq!(reason(resolve_hire(&entries, true, Some("codex"), None, None, None).unwrap_err()), "pool_entry_required");
        assert_eq!(resolve_hire(&entries, true, Some("codex"), Some("luna"), None, None).unwrap().unwrap().id, "codex-luna");
        assert_eq!(resolve_hire(&entries, true, None, None, None, Some("codex-terra")).unwrap().unwrap().id, "codex-terra");
        assert_eq!(reason(resolve_hire(&entries, true, Some("agy"), None, None, None).unwrap_err()), "kind_not_in_pool");
        assert_eq!(reason(resolve_hire(&entries, true, None, None, None, Some("nope")).unwrap_err()), "not_in_pool");
        // 條目說了算：kind／model／effort 帶了不同的值就 mismatch；claude-any 的 model 空字串接受任何模型
        assert_eq!(reason(resolve_hire(&entries, true, Some("claude"), None, None, Some("codex-terra")).unwrap_err()), "pool_entry_mismatch");
        assert_eq!(reason(resolve_hire(&entries, true, None, Some("terra"), Some("high"), Some("codex-terra")).unwrap_err()), "pool_entry_mismatch");
        assert!(resolve_hire(&entries, true, None, Some("any-model"), None, Some("claude-any")).is_ok());
        // User：不受池限制；沒指定條目就是 None
        assert!(resolve_hire(&entries, false, Some("agy"), None, None, None).unwrap().is_none());
    }

    #[test]
    fn capacity_counts_only_executors_on_that_entry() {
        let e = entry("codex-terra", "codex", "terra", None, None, 1);
        let mut bot = BotCfg { id: Some("b1".into()), name: "w".into(), kind: "codex".into(), role: Some("executor".into()), ..Default::default() };
        bot.pool_entry = Some("codex-terra".into());
        let untagged = BotCfg { role: Some("executor".into()), ..Default::default() };
        assert_eq!(used(&[untagged.clone(), bot.clone()], "codex-terra"), 1);
        assert_eq!(reason(check_capacity(&e, &[bot]).unwrap_err()), "executor_pool_full");
        assert!(check_capacity(&e, &[untagged]).is_ok(), "bots without a pool entry do not use the entry");
    }

    #[test]
    fn context_becomes_launch_parameters() {
        let claude = launch_params(&entry("c", "claude", "sonnet", None, Some(300_000), 1));
        assert_eq!(claude.model.as_deref(), Some("sonnet[1m]"));
        assert_eq!(claude.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str), Some("300000"));
        let small = launch_params(&entry("c", "claude", "haiku", Some("low"), Some(200_000), 2));
        assert_eq!(small.model.as_deref(), Some("haiku"));
        let codex = launch_params(&entry("x", "codex", "terra", Some("medium"), Some(256_000), 1));
        assert_eq!(codex.args, vec!["-c".to_string(), "model_auto_compact_token_limit=256000".to_string()]);
        assert!(codex.env.is_empty());
        let agy = launch_params(&entry("a", "agy", "gemini", None, None, 1));
        assert!(agy.args.is_empty() && agy.env.is_empty() && agy.model.as_deref() == Some("gemini"));
    }
}
