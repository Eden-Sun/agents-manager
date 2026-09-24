//! 第一層：決定性規則（issue #204 §2）。規則本體在 `rules.toml`（`include_str!`），這裡只有解讀它的引擎。
//!
//! 沒有用 regex crate：規則只需要幾種固定形狀（前綴、子字串、詞首邊界、旗標、環境變數、反引號 camelCase 鍵），
//! 各寫成一個小函式，比引入依賴好審。

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

const RULES_TOML: &str = include_str!("rules.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bucket {
    Kept,
    Dropped,
    Unmatched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub bucket: Bucket,
    /// 只有 kept 有值：所有命中的類別（去重、保持規則檔的順序）。
    pub categories: Vec<String>,
    /// 命中的規則名（dropped＝命中的 drop 規則、kept＝命中的 kept／verb 規則、unmatched＝空）。
    pub rules: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Rule {
    name: String,
    #[serde(default)]
    category: String,
    kind: String,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    case_sensitive: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct Profile {
    #[serde(default)]
    hard_drop: Vec<Rule>,
    #[serde(default)]
    soft_drop: Vec<Rule>,
    #[serde(default)]
    verbs: Vec<Rule>,
    #[serde(default)]
    kept: Vec<Rule>,
}

#[derive(Debug, Deserialize)]
struct RulesFile {
    claude: Profile,
    codex: Profile,
}

fn rules() -> &'static RulesFile {
    static RULES: OnceLock<RulesFile> = OnceLock::new();
    RULES.get_or_init(|| toml::from_str(RULES_TOML).expect("release_triage/rules.toml 解析失敗"))
}

fn profile(kind: &str) -> Option<&'static Profile> {
    match kind {
        "claude" => Some(&rules().claude),
        "codex" => Some(&rules().codex),
        _ => None,
    }
}

/// 目前有規則 profile 的上游（`herdr` 第二階段才補）。`upstream:<kind>` 標籤照這份清單要求。
pub const KINDS: &[&str] = &["claude", "codex"];

/// 有沒有這個 kind 的規則 profile（`herdr` 第二階段才補）。
pub fn supported(kind: &str) -> bool {
    profile(kind).is_some()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `needle` 在 `hay` 裡出現，且（要求時）前／後一個字元不是英數底線。
fn find_bounded(hay: &str, needle: &str, before: bool, after: bool, case_sensitive: bool) -> bool {
    if needle.is_empty() {
        return false;
    }
    let (h, n) = if case_sensitive { (hay.to_string(), needle.to_string()) } else { (hay.to_lowercase(), needle.to_lowercase()) };
    h.match_indices(&n).any(|(i, m)| {
        let ok_before = !before || !h[..i].chars().next_back().is_some_and(is_word_char);
        let ok_after = !after || !h[i + m.len()..].chars().next().is_some_and(is_word_char);
        ok_before && ok_after
    })
}

/// `--` 後接小寫字母開頭、由小寫／數字／連字號組成的 CLI 旗標；前一個字元不能是英數或連字號
/// （否則 `foo--bar`、`---` 都會誤中）。
fn has_flag(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(pos) = text[i..].find("--") {
        let at = i + pos;
        let prev_ok = at == 0 || {
            let p = b[at - 1] as char;
            !(p.is_ascii_alphanumeric() || p == '-')
        };
        let rest = &text[at + 2..];
        let name_len = rest.chars().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-').count();
        if prev_ok && name_len >= 2 && rest.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            return true;
        }
        i = at + 2;
    }
    false
}

/// 全大寫底線的環境變數名，以任一前綴開頭且前綴之後還有東西。
fn has_env(text: &str, prefixes: &[String]) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|tok| {
        prefixes.iter().any(|p| tok.len() > p.len() && tok.starts_with(p.as_str()))
            && tok.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    })
}

/// 反引號裡的 camelCase 設定鍵：小寫開頭、後面至少出現一個大寫，沒有空白或標點。
fn has_camel_tick(text: &str) -> bool {
    text.split('`').skip(1).step_by(2).any(|span| {
        let mut chars = span.chars();
        chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && span.chars().all(|c| c.is_ascii_alphanumeric())
            && span.chars().any(|c| c.is_ascii_uppercase())
    })
}

fn rule_matches(rule: &Rule, text: &str) -> bool {
    let cs = rule.case_sensitive.unwrap_or(rule.kind == "exact");
    match rule.kind.as_str() {
        "prefix" => rule.patterns.iter().any(|p| {
            if cs {
                text.starts_with(p.as_str())
            } else {
                text.to_lowercase().starts_with(&p.to_lowercase())
            }
        }),
        "contains" => rule.patterns.iter().any(|p| find_bounded(text, p, false, false, cs)),
        "word" => rule.patterns.iter().any(|p| find_bounded(text, p, true, false, cs)),
        "exact" => rule.patterns.iter().any(|p| find_bounded(text, p, true, true, cs)),
        "flag" => has_flag(text),
        "env" => has_env(text, &rule.patterns),
        "camel" => has_camel_tick(text),
        other => panic!("rules.toml：不認得的規則種類 `{other}`（規則 {}）", rule.name),
    }
}

fn hits<'a>(rules: &'a [Rule], text: &str) -> Vec<&'a Rule> {
    rules.iter().filter(|r| rule_matches(r, text)).collect()
}

/// 把一條 entry 分桶。`kind` 不認得回 `None`。
pub fn classify(kind: &str, text: &str) -> Option<Classification> {
    let p = profile(kind)?;
    let hard = hits(&p.hard_drop, text);
    if !hard.is_empty() {
        return Some(Classification { bucket: Bucket::Dropped, categories: Vec::new(), rules: hard.iter().map(|r| r.name.clone()).collect() });
    }
    let mut kept: Vec<&Rule> = hits(&p.verbs, text);
    kept.extend(hits(&p.kept, text));
    if !kept.is_empty() {
        let mut categories: Vec<String> = Vec::new();
        for r in &kept {
            if !r.category.is_empty() && !categories.contains(&r.category) {
                categories.push(r.category.clone());
            }
        }
        return Some(Classification { bucket: Bucket::Kept, categories, rules: kept.iter().map(|r| r.name.clone()).collect() });
    }
    let soft = hits(&p.soft_drop, text);
    if !soft.is_empty() {
        return Some(Classification { bucket: Bucket::Dropped, categories: Vec::new(), rules: soft.iter().map(|r| r.name.clone()).collect() });
    }
    Some(Classification { bucket: Bucket::Unmatched, categories: Vec::new(), rules: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(kind: &str, t: &str) -> Classification {
        classify(kind, t).unwrap()
    }

    #[test]
    fn rules_toml_parses_and_every_kind_is_known() {
        for kind in ["claude", "codex"] {
            let p = profile(kind).unwrap();
            for r in p.hard_drop.iter().chain(&p.soft_drop).chain(&p.verbs).chain(&p.kept) {
                assert!(["prefix", "contains", "word", "exact", "flag", "env", "camel"].contains(&r.kind.as_str()), "{}", r.name);
                assert!(!r.name.is_empty());
            }
            assert!(p.kept.iter().all(|r| !r.category.is_empty()), "kept 規則都要有類別");
        }
        assert!(classify("herdr", "x").is_none());
    }

    #[test]
    fn hard_drop_beats_kept_but_soft_drop_loses_to_kept() {
        let hard = c("claude", "[VSCode] Added a `/config` option for hooks");
        assert_eq!(hard.bucket, Bucket::Dropped);
        assert_eq!(hard.rules, ["tag-surface"]);
        let soft = c("claude", "Added AGENTS.md support: change it under \"Project instructions\" in `/config` (not yet on Bedrock, Vertex or Foundry)");
        assert_eq!(soft.bucket, Bucket::Kept, "供應商名字只是附註，不能丟整條");
        assert!(soft.categories.contains(&"instructions".to_string()) && soft.categories.contains(&"settings".to_string()));
        let only_soft = c("claude", "Improved Bedrock startup time");
        assert_eq!(only_soft.bucket, Bucket::Dropped);
        assert_eq!(only_soft.rules, ["cloud-provider"]);
    }

    #[test]
    fn verb_rule_loses_only_to_hard_drop() {
        assert_eq!(c("claude", "Removed the old thing").bucket, Bucket::Kept);
        assert_eq!(c("claude", "Removed the old thing").categories, ["behavior-change"]);
        assert_eq!(c("claude", "Changed Bedrock defaults").bucket, Bucket::Kept, "動詞規則贏軟 drop");
        assert_eq!(c("claude", "Deprecated the gateway flag").bucket, Bucket::Dropped, "動詞規則輸硬 drop");
    }

    #[test]
    fn matchers_respect_boundaries() {
        assert_eq!(c("claude", "Fixed a webhook typo").bucket, Bucket::Unmatched, "hook 只認詞首");
        assert_eq!(c("claude", "Fixed hooks firing twice").bucket, Bucket::Kept);
        assert_eq!(c("claude", "Improved the Stopped banner").bucket, Bucket::Unmatched, "Stop 要整字");
        assert!(has_flag("Added `--bg` flag") && !has_flag("a --- b") && !has_flag("well-known--x"));
        assert!(has_env("set `CLAUDE_CODE_FOO=1`", &["CLAUDE_".into()]) && !has_env("CLAUDE_ alone", &["CLAUDE_".into()]));
        assert!(has_camel_tick("the `customApiKeyResponses` value") && !has_camel_tick("the `settings` value"));
        assert_eq!(c("claude", "Improved rendering of markdown tables").bucket, Bucket::Unmatched);
    }

    #[test]
    fn codex_profile_mirrors_claude() {
        let k = c("codex", "The TUI now shows live reasoning summaries in the status row and completion timestamps after successful turns. (#43558)");
        assert_eq!(k.bucket, Bucket::Kept);
        assert!(k.categories.contains(&"tui".to_string()));
        for t in [
            "Added experimental `/voice` conversations with live transcripts",
            "Added Touch ID verification for MCP requests",
            "Aligned Python SDK and runtime publishing with stable CLI releases",
            "Blocked Windows-process escapes from restricted WSL sandboxes",
        ] {
            assert_eq!(c("codex", t).bucket, Bucket::Dropped, "{t}");
        }
        assert_eq!(c("codex", "Amazon Bedrock can now obtain AWS credentials from configured commands").bucket, Bucket::Dropped);
    }
}
