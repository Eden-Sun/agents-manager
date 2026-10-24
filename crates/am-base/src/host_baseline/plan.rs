//! 一致性差異的 dry-run 計畫（issue #719 階段 2，SPEC §16.7）：**純函式、只讀**。
//!
//! 輸入只有 daemon 已快取的差異（[`BaselineIssue`]：id／嚴重度／訊息）與嵌進 binary 的宣告檔，
//! 簽名裡沒有任何主機上的內容可用——預覽只會有「哪個根目錄的哪個檔」「哪個鍵」「宣告檔寫的新值」，
//! 不會有主機現有檔的內容或絕對路徑。這一階段只產生 `merge_key`（缺的鍵、宣告檔有值）與 `manual`；
//! 寫檔、裝工具（`write_file`／`install_tool`）留給階段 3。
//!
//! codex 的 model／effort／service_tier：宣告檔解析時就拒絕；這裡再擋一次（同一個判斷），
//! 命中的連 `manual` 都不列——就算有人繞過解析器手造宣告檔，也進不了計畫。

use super::manifest::{codex_forbidden, Manifest, Values};
use super::BaselineIssue;
use serde::Serialize;

pub const MERGE_KEY: &str = "merge_key";
pub const MANUAL: &str = "manual";

/// 鍵沒有宣告值：不知道該補成什麼。
pub const NO_VALUE: &str = "no_value";
/// 缺檔：還沒有模板（階段 3）。
pub const NO_TEMPLATE: &str = "no_template";
/// 缺工具：還沒有安裝方式（階段 3）。
pub const NO_INSTALLER: &str = "no_installer";
/// `settings.json` 不是合法 JSON：不能往裡面合併鍵。
pub const INVALID_JSON: &str = "invalid_json";
/// `.gitconfig` 內嵌憑證：永遠手動。
pub const SECRET: &str = "secret";
/// 鍵所在的檔本身不在：先補檔。
pub const PARENT_MISSING: &str = "parent_missing";
/// 探測給了宣告檔不認得的 id。
pub const UNKNOWN: &str = "unknown";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FixAction {
    /// 對應的 issue id。
    pub id: String,
    /// `merge_key` | `manual`
    pub action: &'static str,
    pub severity: &'static str,
    /// 顯示用的位置：`<CLI>[:<身份>]/<檔>`（例 `codex/config.toml`、`claude:cc1/settings.json`、`tool/rtk`），不是主機上的路徑。
    pub target: String,
    /// 設定鍵的點號路徑（只有鍵類的差異有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// 宣告檔寫的新值（只有 `merge_key` 有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// `manual` 的原因。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Plan {
    pub actions: Vec<FixAction>,
    /// 計畫內容的雜湊：同一份計畫恆同，任何動作變了就變（階段 3 拿來擋「套用到舊計畫」）。
    pub plan_rev: String,
}

impl Plan {
    /// 可以自動補的差異 id（`hosts[].baseline.fixable`）。
    pub fn fixable(&self) -> Vec<String> {
        self.actions.iter().filter(|a| a.action == MERGE_KEY).map(|a| a.id.clone()).collect()
    }
}

fn manual(id: &str, severity: &'static str, target: String, key: Option<String>, reason: &'static str) -> FixAction {
    FixAction { id: id.to_string(), action: MANUAL, severity, target, key, value: None, reason: Some(reason) }
}

/// 照差異的順序列出動作；同一個 id 只列一次。codex 禁止的鍵整個略過。
pub fn build(m: &Manifest, values: &Values, issues: &[BaselineIssue]) -> Plan {
    let ids: std::collections::HashSet<&str> = issues.iter().map(|i| i.id.as_str()).collect();
    let mut seen = std::collections::HashSet::new();
    let mut actions = Vec::new();
    for i in issues {
        if !seen.insert(i.id.as_str()) {
            continue;
        }
        if let Some(a) = action_for(m, values, &ids, i) {
            actions.push(a);
        }
    }
    let rev = serde_json::to_vec(&actions).unwrap_or_default();
    Plan { plan_rev: crate::supervisor_inbox::short_hash(&rev), actions }
}

fn action_for(m: &Manifest, values: &Values, ids: &std::collections::HashSet<&str>, i: &BaselineIssue) -> Option<FixAction> {
    let (id, sev) = (i.id.as_str(), i.severity);
    if id == "gitconfig.token" {
        return Some(manual(id, sev, "git/.gitconfig".into(), None, SECRET));
    }
    if let Some(tool) = id.strip_prefix("tool.") {
        return Some(manual(id, sev, format!("tool/{tool}"), None, NO_INSTALLER));
    }
    if let Some(key) = id.strip_prefix("codex.config.toml:") {
        if key.split('.').any(codex_forbidden) {
            return None;
        }
        let target = "codex/config.toml".to_string();
        if ids.contains("codex.config.toml") {
            return Some(manual(id, sev, target, Some(key.into()), PARENT_MISSING));
        }
        if !m.codex_keys.iter().any(|k| k.key == key) {
            return Some(manual(id, sev, target, Some(key.into()), UNKNOWN));
        }
        return Some(match values.codex.get(key) {
            Some(v) => FixAction { id: id.into(), action: MERGE_KEY, severity: sev, target, key: Some(key.into()), value: Some(v.clone()), reason: None },
            None => manual(id, sev, target, Some(key.into()), NO_VALUE),
        });
    }
    for (cli, files) in [("codex", &m.codex_files), ("grok", &m.grok_files), ("herdr", &m.herdr_files)] {
        if let Some(file) = id.strip_prefix(cli).and_then(|r| r.strip_prefix('.')) {
            let reason = if files.iter().any(|f| f.name == file) { NO_TEMPLATE } else { UNKNOWN };
            return Some(manual(id, sev, format!("{cli}/{file}"), None, reason));
        }
    }
    if let Some(rest) = id.strip_prefix("claude.") {
        return Some(claude_action(m, values, ids, i, rest));
    }
    Some(manual(id, sev, String::new(), None, UNKNOWN))
}

/// `claude.<身份>.<檔>`、`claude.<身份>.settings.json:<鍵名>`、`claude.<身份>.settings.json:json`、`claude.<身份>.plugin:<名>`。
/// 身份名可以有 `.`，所以從尾巴認。
fn claude_action(m: &Manifest, values: &Values, ids: &std::collections::HashSet<&str>, i: &BaselineIssue, rest: &str) -> FixAction {
    let (id, sev) = (i.id.as_str(), i.severity);
    if let Some((ident, name)) = rest.rsplit_once(".settings.json:") {
        let target = format!("claude:{ident}/settings.json");
        if name == "json" {
            return manual(id, sev, target, None, INVALID_JSON);
        }
        let Some(k) = m.claude_keys.iter().find(|k| k.name == name) else {
            return manual(id, sev, target, Some(name.into()), UNKNOWN);
        };
        if ids.contains(format!("claude.{ident}.settings.json").as_str()) {
            return manual(id, sev, target, Some(k.key.clone()), PARENT_MISSING);
        }
        return match values.claude.get(name) {
            Some(v) => FixAction { id: id.into(), action: MERGE_KEY, severity: sev, target, key: Some(k.key.clone()), value: Some(v.clone()), reason: None },
            None => manual(id, sev, target, Some(k.key.clone()), NO_VALUE),
        };
    }
    if let Some((ident, plugin)) = rest.rsplit_once(".plugin:") {
        return manual(id, sev, format!("claude:{ident}/settings.json"), Some(format!("enabledPlugins.{plugin}")), NO_VALUE);
    }
    for f in &m.claude_files {
        if let Some(ident) = rest.strip_suffix(&f.name).and_then(|r| r.strip_suffix('.')) {
            return manual(id, sev, format!("claude:{ident}/{}", f.name), None, NO_TEMPLATE);
        }
    }
    manual(id, sev, String::new(), None, UNKNOWN)
}

#[cfg(test)]
mod tests {
    use super::super::manifest::{CodexCheck, CodexKey};
    use super::super::{CRITICAL, WARN};
    use super::*;

    fn issue(id: &str, severity: &'static str) -> BaselineIssue {
        BaselineIssue { id: id.into(), severity, message: format!("msg {id}") }
    }

    fn shipped() -> (&'static Manifest, &'static Values) {
        (super::super::manifest::shipped().unwrap(), super::super::manifest::shipped_values().unwrap())
    }

    fn by_id<'a>(p: &'a Plan, id: &str) -> &'a FixAction {
        p.actions.iter().find(|a| a.id == id).unwrap_or_else(|| panic!("no action for {id}: {p:?}"))
    }

    #[test]
    fn a_missing_key_with_a_declared_value_is_merge_key_and_everything_else_is_manual() {
        let (m, v) = shipped();
        let issues = vec![
            issue("tool.rtk", CRITICAL),
            issue("claude.cc1.statusline-command.sh", CRITICAL),
            issue("claude.cc1.settings.json:statusLine", CRITICAL),
            issue("claude.my.dir.settings.json:json", CRITICAL),
            issue("claude.default.plugin:imessage", WARN),
            issue("codex.config.toml:features.hooks", WARN),
            issue("codex.config.toml:approval_policy", WARN),
            issue("codex.hooks.json", WARN),
            issue("grok.config.toml", WARN),
            issue("herdr.config.toml", WARN),
            issue("gitconfig.token", WARN),
            issue("something.new", WARN),
        ];
        let p = build(m, v, &issues);
        assert_eq!(p.actions.len(), issues.len());
        let hooks = by_id(&p, "codex.config.toml:features.hooks");
        assert_eq!((hooks.action, hooks.key.as_deref(), hooks.value.clone()), (MERGE_KEY, Some("features.hooks"), Some(serde_json::json!(true))));
        assert_eq!(hooks.target, "codex/config.toml");
        let want = [
            ("tool.rtk", NO_INSTALLER, "tool/rtk"),
            ("claude.cc1.statusline-command.sh", NO_TEMPLATE, "claude:cc1/statusline-command.sh"),
            ("claude.cc1.settings.json:statusLine", NO_VALUE, "claude:cc1/settings.json"),
            ("claude.my.dir.settings.json:json", INVALID_JSON, "claude:my.dir/settings.json"),
            ("claude.default.plugin:imessage", NO_VALUE, "claude:default/settings.json"),
            ("codex.config.toml:approval_policy", NO_VALUE, "codex/config.toml"),
            ("codex.hooks.json", NO_TEMPLATE, "codex/hooks.json"),
            ("grok.config.toml", NO_TEMPLATE, "grok/config.toml"),
            ("herdr.config.toml", NO_TEMPLATE, "herdr/config.toml"),
            ("gitconfig.token", SECRET, "git/.gitconfig"),
            ("something.new", UNKNOWN, ""),
        ];
        for (id, reason, target) in want {
            let a = by_id(&p, id);
            assert_eq!((a.action, a.reason, a.target.as_str(), a.value.is_none()), (MANUAL, Some(reason), target, true), "{id}");
        }
        assert_eq!(by_id(&p, "claude.cc1.settings.json:statusLine").key.as_deref(), Some("statusLine"));
        assert_eq!(p.fixable(), vec!["codex.config.toml:features.hooks".to_string()]);
    }

    #[test]
    fn a_key_whose_file_is_missing_waits_for_the_file() {
        let (m, v) = shipped();
        let p = build(m, v, &[issue("codex.config.toml", WARN), issue("codex.config.toml:features.hooks", WARN)]);
        assert_eq!(by_id(&p, "codex.config.toml").reason, Some(NO_TEMPLATE));
        assert_eq!(by_id(&p, "codex.config.toml:features.hooks").reason, Some(PARENT_MISSING));
        assert!(p.fixable().is_empty());
        let p = build(m, v, &[issue("claude.cc2.settings.json", CRITICAL), issue("claude.cc2.settings.json:hooks", CRITICAL)]);
        assert_eq!(by_id(&p, "claude.cc2.settings.json:hooks").reason, Some(PARENT_MISSING));
    }

    #[test]
    fn the_plan_is_stable_and_its_rev_follows_its_content() {
        let (m, v) = shipped();
        let a = [issue("tool.rtk", CRITICAL), issue("codex.config.toml:features.hooks", WARN)];
        let b = [issue("tool.rtk", CRITICAL), issue("codex.config.toml:features.hooks", WARN), issue("tool.rtk", CRITICAL)];
        assert_eq!(build(m, v, &a), build(m, v, &a));
        assert_eq!(build(m, v, &a), build(m, v, &b), "a repeated id is listed once");
        assert_ne!(build(m, v, &a).plan_rev, build(m, v, &a[..1]).plan_rev);
        assert!(build(m, v, &[]).actions.is_empty());
    }

    /// ② 沒有宣告值的鍵一律手動：出貨的宣告檔每一個鍵都走一次，再拿一份全無值的宣告檔確認一個 merge_key 都沒有。
    #[test]
    fn every_key_without_a_value_is_manual() {
        let (m, v) = shipped();
        let mut issues: Vec<BaselineIssue> = m.claude_keys.iter().map(|k| issue(&format!("claude.cc1.settings.json:{}", k.name), k.severity)).collect();
        issues.extend(m.codex_keys.iter().map(|k| issue(&format!("codex.config.toml:{}", k.key), k.severity)));
        let p = build(m, v, &issues);
        assert_eq!(p.actions.len(), issues.len());
        for a in &p.actions {
            let name = a.id.rsplit_once(':').unwrap().1;
            let has_value = v.claude.contains_key(name) || v.codex.contains_key(name);
            assert_eq!(a.action == MERGE_KEY, has_value, "{a:?}");
            if !has_value {
                assert_eq!((a.reason, a.value.is_none()), (Some(NO_VALUE), true), "{a:?}");
            }
        }
        let none = Values::default();
        assert!(build(m, &none, &issues).actions.iter().all(|a| a.action == MANUAL), "no declared values must mean no merge_key");
    }

    /// ③ codex 的 model／effort／service_tier：繞過解析器手造宣告檔、而且帶值，也進不了計畫（連 manual 都沒有）。
    #[test]
    fn codex_model_keys_never_reach_the_plan_even_past_the_parser() {
        let (base, _) = shipped();
        let forbidden = ["model", "model_reasoning_effort", "service_tier", "profiles.x.model", "features.reasoning_effort"];
        let mut m = base.clone();
        let mut v = Values::default();
        for k in forbidden {
            m.codex_keys.push(CodexKey { key: k.into(), check: CodexCheck::Present, severity: WARN });
            v.codex.insert(k.into(), serde_json::json!("gpt-x"));
        }
        let issues: Vec<BaselineIssue> = forbidden.iter().map(|k| issue(&format!("codex.config.toml:{k}"), WARN)).collect();
        let p = build(&m, &v, &issues);
        assert!(p.actions.is_empty(), "{p:?}");
        let json = serde_json::to_string(&p).unwrap();
        for k in ["model", "effort", "service_tier", "gpt-x"] {
            assert!(!json.contains(k), "`{k}` leaked into the plan: {json}");
        }
    }

    /// ① 預覽不含主機上的內容：真跑探測（假 HOME 裡的檔塞了標記字串），evaluate → build → JSON，
    /// 找不到任何標記字串、也找不到假 HOME 的路徑；每個動作的欄位都在白名單裡。
    #[test]
    fn the_plan_never_carries_host_file_contents() {
        let scratch = crate::hosts::remote_script_support::Scratch::new();
        let h = scratch.0.join("home");
        let write = |rel: &str, body: &str| {
            let p = h.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        // settings.json 少 statusLine／hooks／defaultMode，另有一個不合法的身份；codex 少 approval_policy、features.hooks 是 false。
        write(".claude/settings.json", r#"{"permissions":{"allow":["AM-SECRET-perm"]},"env":{"TOKEN":"AM-SECRET-env"}}"#);
        write(".claude/CLAUDE.md", "AM-SECRET-md");
        write(".claude-cc3/settings.json", "{ AM-SECRET-broken");
        write(".codex/config.toml", "model = \"AM-SECRET-model\"\n[features]\nhooks = false # AM-SECRET-comment\n");
        write(".gitconfig", "[url \"https://ghp_AMSECRETtoken@github.com/\"]\n");
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(super::super::baseline_sh())
            .current_dir(&h)
            .env_clear()
            .env("HOME", &h)
            .env("PATH", format!("{}:/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin", std::env::var("PATH").unwrap_or_default()))
            .env("SHELL", "/bin/sh")
            .output()
            .unwrap();
        let out = String::from_utf8(out.stdout).unwrap();
        let issues = super::super::evaluate(&out).expect("probe finished");
        let (m, v) = shipped();
        let p = build(m, v, &issues);
        assert!(p.actions.len() >= 8, "the fixture must produce a real plan: {p:?}");
        assert!(p.actions.iter().any(|a| a.action == MERGE_KEY), "{p:?}");
        let json = serde_json::to_value(&p).unwrap();
        let text = json.to_string();
        for needle in ["AM-SECRET", "AMSECRET", h.to_str().unwrap(), scratch.0.to_str().unwrap()] {
            assert!(!text.contains(needle), "`{needle}` leaked into the plan: {text}");
        }
        let allowed = ["id", "action", "severity", "target", "key", "value", "reason"];
        for a in json["actions"].as_array().unwrap() {
            for k in a.as_object().unwrap().keys() {
                assert!(allowed.contains(&k.as_str()), "unexpected field `{k}` in {a}");
            }
            assert!(!a["target"].as_str().unwrap().starts_with('/'), "{a}");
        }
        let keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["actions", "plan_rev"]);
    }
}
