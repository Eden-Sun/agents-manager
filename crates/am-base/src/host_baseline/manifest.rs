//! 基準宣告檔 `ops/host-baseline/baseline.toml`（issue #719）：編譯時嵌進 binary，解析並守門後給
//! [`super::baseline_sh`]（產生探測腳本的清單）與 [`super::evaluate`]（嚴重度、哪些算缺）用。
//!
//! 守門：名稱／路徑會直接拼進 sh，只收安全字元；路徑一律相對（不准絕對路徑、`..`、`$`、`~`）；
//! codex 的 model／effort／service_tier 一律拒絕（使用者規定不動）。壞的宣告檔不能 ship：
//! `the_shipped_manifest_parses` 在 `cargo test` 就紅；萬一還是編進去了，[`shipped`] 回錯誤，
//! 探測與 [`super::evaluate`] 一律回「未知」——不 panic，也不變成空清單悄悄跳過檢查。

use serde::Deserialize;

/// 嵌進 binary 的宣告檔原文。
pub const SOURCE: &str = include_str!("../../../../ops/host-baseline/baseline.toml");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// 工具名或相對路徑。
    pub name: String,
    pub severity: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeKey {
    /// issue id 的尾巴（`claude.<身份>.settings.json:<name>`）。
    pub name: String,
    /// `settings.json` 裡的點號路徑。
    pub key: String,
    pub severity: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexCheck {
    /// `config.toml` 裡有這個鍵（任何值）。
    Present,
    /// 點號鍵或 `[表]` 裡的鍵值為 true。
    True,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexKey {
    pub key: String,
    pub check: CodexCheck,
    pub severity: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub tools: Vec<Item>,
    pub claude_files: Vec<Item>,
    pub claude_keys: Vec<ClaudeKey>,
    /// 只在 Darwin 上才算該有的 claude plugin。
    pub mac_only_plugins: Vec<String>,
    pub codex_files: Vec<Item>,
    pub codex_keys: Vec<CodexKey>,
    pub grok_files: Vec<Item>,
    pub herdr_files: Vec<Item>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    version: u32,
    #[serde(default)]
    tools: Vec<RawTool>,
    #[serde(default)]
    claude: RawClaude,
    #[serde(default)]
    codex: RawCodex,
    #[serde(default)]
    grok: RawFiles,
    #[serde(default)]
    herdr: RawFiles,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTool {
    name: String,
    severity: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    path: String,
    severity: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawFiles {
    #[serde(default)]
    files: Vec<RawFile>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawClaude {
    #[serde(default)]
    files: Vec<RawFile>,
    #[serde(default)]
    keys: Vec<RawClaudeKey>,
    #[serde(default)]
    plugins: Vec<RawPlugin>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClaudeKey {
    name: String,
    key: String,
    severity: String,
    #[serde(default)]
    value: Option<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlugin {
    name: String,
    os: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawCodex {
    #[serde(default)]
    files: Vec<RawFile>,
    #[serde(default)]
    keys: Vec<RawCodexKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCodexKey {
    key: String,
    check: String,
    severity: String,
    #[serde(default)]
    value: Option<toml::Value>,
}

fn severity(s: &str, at: &str) -> Result<&'static str, String> {
    match s {
        "critical" => Ok(super::CRITICAL),
        "warn" => Ok(super::WARN),
        _ => Err(format!("{at}: severity must be `critical` or `warn`, got `{s}`")),
    }
}

/// 工具名、plugin 名、claude key 的 `name`：會進 sh 的 `for` 清單與 issue id。
fn name(s: &str, at: &str) -> Result<String, String> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) || s.starts_with(['-', '.']) {
        return Err(format!("{at}: `{s}` must be [A-Za-z0-9_.-] and not start with `-`/`.`"));
    }
    Ok(s.to_string())
}

/// 相對於根目錄的路徑：段落只收 [A-Za-z0-9_.-]，不准空段、`.`、`..`、開頭 `/`、`$`、`~`。
fn rel_path(s: &str, at: &str) -> Result<String, String> {
    let ok = !s.is_empty()
        && s.split('/').all(|seg| {
            !seg.is_empty() && seg != "." && seg != ".." && !seg.starts_with('-') && seg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        });
    if !ok {
        return Err(format!("{at}: `{s}` must be a relative path ([A-Za-z0-9_.-] segments, no `..`, no leading `/`, no `$`/`~`)"));
    }
    Ok(s.to_string())
}

/// 點號鍵：每段 [A-Za-z0-9_]。
fn dotted(s: &str, at: &str) -> Result<Vec<String>, String> {
    let segs: Vec<String> = s.split('.').map(str::to_string).collect();
    if segs.iter().any(|seg| seg.is_empty() || !seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
        return Err(format!("{at}: `{s}` must be dot-separated [A-Za-z0-9_] segments"));
    }
    Ok(segs)
}

/// 使用者規定不動的 codex 設定（model、各種 model_*、reasoning effort、service_tier）：連檢查都不列。
/// dry-run 計畫（[`super::plan`]）也拿這個再擋一次。
pub(crate) fn codex_forbidden(seg: &str) -> bool {
    seg.starts_with("model") || seg.contains("effort") || seg == "service_tier"
}

fn files(raw: Vec<RawFile>, at: &str) -> Result<Vec<Item>, String> {
    let mut out: Vec<Item> = Vec::new();
    for f in raw {
        let path = rel_path(&f.path, at)?;
        if out.iter().any(|i| i.name == path) {
            return Err(format!("{at}: `{path}` is listed twice"));
        }
        out.push(Item { severity: severity(&f.severity, at)?, name: path });
    }
    Ok(out)
}

/// 宣告檔替設定鍵寫的值（有值＝dry-run 計畫可以列成「自動補」）。key＝claude 鍵的 `name`／codex 的 `key`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Values {
    pub claude: std::collections::BTreeMap<String, serde_json::Value>,
    pub codex: std::collections::BTreeMap<String, serde_json::Value>,
}

/// 一個值最多這麼大（序列化成 JSON 的 byte 數）。
const VALUE_MAX_BYTES: usize = 1024;
const VALUE_MAX_STR: usize = 256;

/// 值只收 bool／整數／字串（≤ 256 字元）或一層小表（葉子是前面那幾種）；不收陣列、日期、浮點、巢狀表。
fn value(v: toml::Value, at: &str) -> Result<serde_json::Value, String> {
    fn scalar(v: toml::Value, at: &str) -> Result<serde_json::Value, String> {
        match v {
            toml::Value::Boolean(b) => Ok(b.into()),
            toml::Value::Integer(i) => Ok(i.into()),
            toml::Value::String(s) if s.chars().count() <= VALUE_MAX_STR => Ok(s.into()),
            toml::Value::String(_) => Err(format!("{at}: string value longer than {VALUE_MAX_STR} chars")),
            other => Err(format!("{at}: value must be a bool, integer, string or one-level table of those, got {}", other.type_str())),
        }
    }
    let out = match v {
        toml::Value::Table(t) => {
            let mut m = serde_json::Map::new();
            for (k, v) in t {
                if matches!(v, toml::Value::Table(_)) {
                    return Err(format!("{at}: nested tables are not allowed in a value"));
                }
                m.insert(k, scalar(v, at)?);
            }
            serde_json::Value::Object(m)
        }
        other => scalar(other, at)?,
    };
    if serde_json::to_vec(&out).map_or(usize::MAX, |b| b.len()) > VALUE_MAX_BYTES {
        return Err(format!("{at}: value larger than {VALUE_MAX_BYTES} bytes"));
    }
    Ok(out)
}

impl Manifest {
    pub fn parse(src: &str) -> Result<Manifest, String> {
        Self::parse_with_values(src).map(|(m, _)| m)
    }

    /// 連同 [`Values`] 一起解析（同一份守門）。
    pub fn parse_with_values(src: &str) -> Result<(Manifest, Values), String> {
        let mut values = Values::default();
        let raw: Raw = toml::from_str(src).map_err(|e| format!("baseline.toml: {e}"))?;
        if raw.version != 1 {
            return Err(format!("baseline.toml: unsupported version {}", raw.version));
        }
        let mut tools: Vec<Item> = Vec::new();
        for t in raw.tools {
            let n = name(&t.name, "tools")?;
            if tools.iter().any(|i| i.name == n) {
                return Err(format!("tools: `{n}` is listed twice"));
            }
            tools.push(Item { severity: severity(&t.severity, "tools")?, name: n });
        }
        let claude_files = files(raw.claude.files, "claude.files")?;
        let mut claude_keys: Vec<ClaudeKey> = Vec::new();
        for k in raw.claude.keys {
            let n = name(&k.name, "claude.keys")?;
            dotted(&k.key, "claude.keys")?;
            if claude_keys.iter().any(|c| c.name == n) {
                return Err(format!("claude.keys: `{n}` is listed twice"));
            }
            if let Some(v) = k.value {
                values.claude.insert(n.clone(), value(v, "claude.keys")?);
            }
            claude_keys.push(ClaudeKey { name: n, key: k.key, severity: severity(&k.severity, "claude.keys")? });
        }
        let mut mac_only_plugins = Vec::new();
        for p in raw.claude.plugins {
            // 目前只有 Darwin 專用一種；Linux 專用之類要先在探測／evaluate 裡支援。
            if p.os != "darwin" {
                return Err(format!("claude.plugins: os must be `darwin`, got `{}`", p.os));
            }
            let n = name(&p.name, "claude.plugins")?;
            if mac_only_plugins.contains(&n) {
                return Err(format!("claude.plugins: `{n}` is listed twice"));
            }
            mac_only_plugins.push(n);
        }
        let codex_files = files(raw.codex.files, "codex.files")?;
        let mut codex_keys: Vec<CodexKey> = Vec::new();
        for k in raw.codex.keys {
            let segs = dotted(&k.key, "codex.keys")?;
            if segs.iter().any(|s| codex_forbidden(s)) {
                return Err(format!("codex.keys: `{}` is a model/effort/service_tier setting — the baseline never touches those", k.key));
            }
            let check = match k.check.as_str() {
                // 原本就是整檔 grep `^\s*<key>\s*=`：只收單段鍵，點號鍵用 `true`。
                "present" if segs.len() == 1 => CodexCheck::Present,
                "true" if segs.len() == 2 => CodexCheck::True,
                c => return Err(format!("codex.keys: `{}` with check `{c}` (present needs a plain key, true needs `table.key`)", k.key)),
            };
            if codex_keys.iter().any(|c| c.key == k.key) {
                return Err(format!("codex.keys: `{}` is listed twice", k.key));
            }
            if let Some(v) = k.value {
                let v = value(v, "codex.keys")?;
                // `true` 檢查的鍵只能補成 true，補別的值下一次偵測照樣算缺。
                if check == CodexCheck::True && v != serde_json::Value::Bool(true) {
                    return Err(format!("codex.keys: `{}` is checked for `true`, so its value must be true", k.key));
                }
                values.codex.insert(k.key.clone(), v);
            }
            codex_keys.push(CodexKey { key: k.key, check, severity: severity(&k.severity, "codex.keys")? });
        }
        let grok_files = files(raw.grok.files, "grok.files")?;
        let herdr_files = files(raw.herdr.files, "herdr.files")?;
        // 探測靠 jq 查 settings.json 的鍵，鍵的檢查又以 settings.json 在不在為前提；codex 的鍵同理。
        if !claude_keys.is_empty() {
            if !tools.iter().any(|t| t.name == "jq") {
                return Err("claude.keys needs `jq` in tools (the probe reads settings.json with it)".into());
            }
            if !claude_files.iter().any(|f| f.name == "settings.json") {
                return Err("claude.keys needs `settings.json` in claude.files".into());
            }
        }
        if !codex_keys.is_empty() && !codex_files.iter().any(|f| f.name == "config.toml") {
            return Err("codex.keys needs `config.toml` in codex.files".into());
        }
        Ok((Manifest { tools, claude_files, claude_keys, mac_only_plugins, codex_files, codex_keys, grok_files, herdr_files }, values))
    }
}

fn shipped_full() -> Result<&'static (Manifest, Values), &'static str> {
    static M: std::sync::OnceLock<Result<(Manifest, Values), String>> = std::sync::OnceLock::new();
    M.get_or_init(|| Manifest::parse_with_values(SOURCE)).as_ref().map_err(String::as_str)
}

/// 嵌進 binary 的那一份（解析一次）。
pub fn shipped() -> Result<&'static Manifest, &'static str> {
    shipped_full().map(|(m, _)| m)
}

/// 嵌進 binary 的那一份宣告檔裡寫的值。
pub fn shipped_values() -> Result<&'static Values, &'static str> {
    shipped_full().map(|(_, v)| v)
}

/// 在 `table` 裡用 `name` 找嚴重度；不在表上＝`None`（不是基準項）。
pub fn severity_of(table: &[Item], name: &str) -> Option<&'static str> {
    table.iter().find(|i| i.name == name).map(|i| i.severity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_manifest_parses() {
        Manifest::parse(SOURCE).unwrap();
    }

    /// 搬家前寫死在 `host_baseline.rs` 的基準：宣告檔產生的清單必須逐項相等（行為不變）。
    #[test]
    fn the_shipped_manifest_is_the_baseline_that_used_to_be_hard_coded() {
        let m = shipped().unwrap();
        let pairs = |v: &[Item]| v.iter().map(|i| (i.name.clone(), i.severity)).collect::<Vec<_>>();
        let own = |v: &[(&str, &'static str)]| v.iter().map(|(n, s)| (n.to_string(), *s)).collect::<Vec<_>>();
        let (c, w) = (super::super::CRITICAL, super::super::WARN);
        assert_eq!(pairs(&m.tools), own(&[("herdr", c), ("rtk", c), ("zsh", c), ("bun", c), ("jq", c), ("gh", c)]));
        assert_eq!(pairs(&m.claude_files), own(&[("settings.json", c), ("statusline-command.sh", c), ("CLAUDE.md", w), ("RTK.md", w)]));
        assert_eq!(
            m.claude_keys.iter().map(|k| (k.name.as_str(), k.key.as_str(), k.severity)).collect::<Vec<_>>(),
            vec![("statusLine", "statusLine", c), ("hooks", "hooks", c), ("permissions", "permissions", w), ("defaultMode", "permissions.defaultMode", w)]
        );
        assert_eq!(m.mac_only_plugins, vec!["imessage", "discord"]);
        assert_eq!(pairs(&m.codex_files), own(&[("config.toml", w), ("hooks.json", w)]));
        assert_eq!(
            m.codex_keys.iter().map(|k| (k.key.as_str(), k.check, k.severity)).collect::<Vec<_>>(),
            vec![("approval_policy", CodexCheck::Present, w), ("features.hooks", CodexCheck::True, w)]
        );
        assert_eq!(pairs(&m.grok_files), own(&[("config.toml", w)]));
        assert_eq!(pairs(&m.herdr_files), own(&[("config.toml", w)]));
    }

    const MIN: &str = "version = 1\n[[tools]]\nname = \"jq\"\nseverity = \"critical\"\n";

    fn err(extra: &str) -> String {
        Manifest::parse(&format!("{MIN}{extra}")).expect_err(extra)
    }

    #[test]
    fn a_minimal_manifest_parses() {
        Manifest::parse(MIN).unwrap();
    }

    #[test]
    fn codex_model_effort_and_service_tier_are_refused() {
        for key in ["model", "model_reasoning_effort", "service_tier", "profiles.model", "features.model_x"] {
            let check = if key.contains('.') { "true" } else { "present" };
            let e = err(&format!(
                "[[codex.files]]\npath = \"config.toml\"\nseverity = \"warn\"\n[[codex.keys]]\nkey = \"{key}\"\ncheck = \"{check}\"\nseverity = \"warn\"\n"
            ));
            assert!(e.contains("never touches"), "{key}: {e}");
        }
    }

    #[test]
    fn paths_must_stay_relative_and_shell_safe() {
        for p in ["/Users/m4p/.claude/settings.json", "../x", "a/../b", "$HOME/x", "~/x", "a b", "a;rm", "", "a//b", "./x", "-rf"] {
            let e = err(&format!("[[grok.files]]\npath = \"{p}\"\nseverity = \"warn\"\n"));
            assert!(e.contains("relative path"), "{p}: {e}");
        }
        Manifest::parse(&format!("{MIN}[[grok.files]]\npath = \"sub/config.toml\"\nseverity = \"warn\"\n")).unwrap();
    }

    #[test]
    fn names_that_go_into_the_script_are_shell_safe() {
        for n in ["a b", "x;y", "$(id)", "", "-x", "a\\nb"] {
            let e = err(&format!("[[tools]]\nname = \"{n}\"\nseverity = \"warn\"\n"));
            assert!(e.contains("must be"), "{n}: {e}");
        }
    }

    #[test]
    fn severity_and_unknown_fields_are_checked() {
        assert!(err("[[grok.files]]\npath = \"x\"\nseverity = \"fatal\"\n").contains("severity"));
        assert!(err("[[grok.files]]\npath = \"x\"\nseverity = \"warn\"\ncontent = \"y\"\n").contains("unknown field"));
        assert!(Manifest::parse("version = 2\n").unwrap_err().contains("version"));
    }

    #[test]
    fn duplicates_are_refused() {
        assert!(err("[[tools]]\nname = \"jq\"\nseverity = \"warn\"\n").contains("twice"));
    }

    #[test]
    fn keys_need_the_file_and_jq_they_are_read_with() {
        let keys = "[[claude.keys]]\nname = \"hooks\"\nkey = \"hooks\"\nseverity = \"warn\"\n";
        assert!(err(keys).contains("settings.json"));
        let no_jq = format!("version = 1\n[[claude.files]]\npath = \"settings.json\"\nseverity = \"warn\"\n{keys}");
        assert!(Manifest::parse(&no_jq).unwrap_err().contains("jq"));
        let codex = "[[codex.keys]]\nkey = \"approval_policy\"\ncheck = \"present\"\nseverity = \"warn\"\n";
        assert!(err(codex).contains("config.toml"));
    }

    #[test]
    fn codex_checks_must_match_the_key_shape() {
        let f = "[[codex.files]]\npath = \"config.toml\"\nseverity = \"warn\"\n";
        assert!(err(&format!("{f}[[codex.keys]]\nkey = \"a.b\"\ncheck = \"present\"\nseverity = \"warn\"\n")).contains("check"));
        assert!(err(&format!("{f}[[codex.keys]]\nkey = \"a\"\ncheck = \"true\"\nseverity = \"warn\"\n")).contains("check"));
        assert!(err(&format!("{f}[[codex.keys]]\nkey = \"a.b\"\ncheck = \"equals\"\nseverity = \"warn\"\n")).contains("check"));
    }

    #[test]
    fn only_darwin_plugins_are_understood() {
        assert!(err("[[claude.plugins]]\nname = \"x\"\nos = \"linux\"\n").contains("darwin"));
    }

    /// 階段 2：出貨的宣告檔只替 codex `features.hooks` 寫了值（協調者裁示），其他鍵都沒值＝dry-run 列手動。
    #[test]
    fn the_shipped_values_are_only_codex_features_hooks() {
        let v = shipped_values().unwrap();
        assert!(v.claude.is_empty(), "{v:?}");
        assert_eq!(v.codex.len(), 1, "{v:?}");
        assert_eq!(v.codex.get("features.hooks"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn values_are_small_scalars_or_one_level_tables() {
        let key = |v: &str| {
            format!("[[claude.files]]\npath = \"settings.json\"\nseverity = \"warn\"\n[[claude.keys]]\nname = \"k\"\nkey = \"k\"\nseverity = \"warn\"\nvalue = {v}\n")
        };
        for ok in ["true", "3", "\"auto\"", "{ type = \"command\", command = \"x\" }"] {
            Manifest::parse_with_values(&format!("{MIN}{}", key(ok))).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let long = format!("\"{}\"", "a".repeat(257));
        let big = format!("{{ {} }}", (0..10).map(|i| format!("k{i} = \"{}\"", "b".repeat(200))).collect::<Vec<_>>().join(", "));
        for bad in ["[1, 2]", "1.5", "1979-05-27T07:32:00Z", "{ a = { b = 1 } }", "{ a = [1] }", long.as_str(), big.as_str()] {
            assert!(Manifest::parse_with_values(&format!("{MIN}{}", key(bad))).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn a_true_checked_codex_key_can_only_be_filled_with_true() {
        let f = "[[codex.files]]\npath = \"config.toml\"\nseverity = \"warn\"\n";
        let e = err(&format!("{f}[[codex.keys]]\nkey = \"features.hooks\"\ncheck = \"true\"\nseverity = \"warn\"\nvalue = false\n"));
        assert!(e.contains("must be true"), "{e}");
    }

    /// 白名單之外再一道：值代入腳本時整個 quote。直接造一份不經 `parse` 的宣告（繞過白名單），危險字元也不能被執行。
    #[test]
    fn values_are_quoted_in_the_script_even_past_the_whitelist() {
        let scratch = crate::hosts::remote_script_support::Scratch::new();
        let h = scratch.0.join("home");
        std::fs::create_dir_all(h.join(".claude")).unwrap();
        std::fs::create_dir_all(h.join(".codex")).unwrap();
        std::fs::write(h.join(".claude/settings.json"), "{}").unwrap();
        std::fs::write(h.join(".codex/config.toml"), "").unwrap();
        let bad = |tag: &str| format!("x';touch {tag}';'$(touch {tag}2)`touch {tag}3`\"; touch {tag}4 \"");
        let item = |tag: &str| Item { name: bad(tag), severity: super::super::WARN };
        let m = Manifest {
            tools: vec![item("T")],
            claude_files: vec![item("CF")],
            claude_keys: vec![ClaudeKey { name: bad("CKN"), key: bad("CKK"), severity: super::super::WARN }],
            mac_only_plugins: vec![bad("PL")],
            codex_files: vec![item("XF")],
            codex_keys: vec![CodexKey { key: bad("XK"), check: CodexCheck::Present, severity: super::super::WARN }],
            grok_files: vec![item("GF")],
            herdr_files: vec![item("HF")],
        };
        let script = super::super::render_baseline_sh(&m);
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .current_dir(&h)
            .env_clear()
            .env("HOME", &h)
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("AM_BL end"), "{}", String::from_utf8_lossy(&out.stderr));
        let made: Vec<_> = std::fs::read_dir(&h).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).filter(|n| !n.starts_with('.')).collect();
        assert!(made.is_empty(), "the script ran a declared value as code: {made:?}");
    }

    // ───────── 腳本等價：搬家前的腳本（凍結在這裡）與宣告檔產生的腳本，同一個假 HOME 跑出來逐 byte 相同 ─────────

    /// 搬家前 `host_baseline.rs` 的 `BASELINE_SH`，一字不改。
    #[rustfmt::skip]
    const OLD_BASELINE_SH: &str = r#"
printf 'AM_BL begin\n'
MAC_ONLY_PLUGINS="imessage discord"
printf 'AM_BL os %s\n' "$(uname -s 2>/dev/null)"
# 六個工具一次問完：每次 `$SHELL -lic` 都要讀完整個 rc（nvm／conda 動輒數秒），PROBE_SH 自己已經開了好幾次，
# 再各開六次會把整趟探測推過 ssh 的 30 秒上限，連原本的 tools 偵測都跟著失敗。只認絕對路徑（alias 的字串不算，#666）。
# HISTFILE=/dev/null：macOS /bin/sh 是 bash 3.2，-i 會把 history 寫進 $HOME。這支腳本必須只讀。
bl_paths=$( HISTFILE=/dev/null "${SHELL:-/bin/sh}" -lic 'for t in herdr rtk zsh bun jq gh; do printf "AM_BLP %s %s\n" "$t" "$(command -v "$t" 2>/dev/null | tail -1)"; done' 2>/dev/null </dev/null | grep '^AM_BLP ' )
bl_jq_path=$(printf '%s\n' "$bl_paths" | sed -n 's/^AM_BLP jq //p' | tail -1)
case "$bl_jq_path" in /*) [ -x "$bl_jq_path" ] || bl_jq_path="" ;; *) bl_jq_path=$(command -v jq 2>/dev/null || true) ;; esac
for t in herdr rtk zsh bun jq gh; do
  p=$(printf '%s\n' "$bl_paths" | sed -n "s/^AM_BLP $t //p" | tail -1)
  case "$p" in /*) ;; *) p="" ;; esac
  [ -n "$p" ] || p=$(command -v "$t" 2>/dev/null)
  case "$p" in /*) ;; *) p="" ;; esac
  printf 'AM_BL tool %s %s\n' "$t" "$p"
done
bl_has() { [ -e "$1" ] && printf 1 || printf 0; }
bl_plugin() { [ -f "$1" ] && grep -Eq "\"$2@[^\"]*\"[[:space:]]*:[[:space:]]*true" "$1" 2>/dev/null && printf 1 || printf 0; }
bl_key() {
  [ -f "$1" ] || { printf 0; return; }
  [ -n "$bl_jq_path" ] && [ -x "$bl_jq_path" ] || { printf '?'; return; }
  # jq 退出碼：0＝有、1＝沒有，其他（5＝不是合法 JSON）＝x。讀不懂不能算成沒有那個鍵。
  if [ "$2" = defaultMode ]; then
    "$bl_jq_path" -e 'type == "object" and (.permissions | type == "object" and has("defaultMode"))' "$1" >/dev/null 2>&1
  else
    "$bl_jq_path" -e --arg key "$2" 'type == "object" and has($key)' "$1" >/dev/null 2>&1
  fi
  case $? in 0) printf 1 ;; 1) printf 0 ;; *) printf x ;; esac
}
# `features.hooks = true`，或 `[features]` 表裡的 `hooks = true`。false／沒寫＝0。不開 login shell。
bl_features_hooks() {
  [ -f "$1" ] || { printf 0; return; }
  /usr/bin/awk '
    BEGIN { root = 1 }
    {
      line = $0
      sub(/\r$/, "", line)
      sub(/^[[:blank:]]*/, "", line)
      if (line == "" || line ~ /^#/) next
      if (line ~ /^\[/) {
        in_features = (line ~ /^\[features\][[:blank:]]*(#.*)?$/)
        root = 0
        next
      }
      sub(/[[:blank:]]+#.*/, "", line)
      if ((root && line ~ /^features\.hooks[[:blank:]]*=[[:blank:]]*true[[:blank:]]*$/) ||
          (in_features && line ~ /^hooks[[:blank:]]*=[[:blank:]]*true[[:blank:]]*$/)) found = 1
    }
    END { exit !found }
  ' "$1" && printf 1 || printf 0
}
# 同一個目錄只查一次。身份名只認一般字元；路徑不印出來。
bl_seen="|"
bl_claude() {
  d=$1
  n=$2
  case "$n" in *[!A-Za-z0-9_.-]*) return ;; esac
  [ -n "$d" ] && [ -d "$d" ] || return
  case "$bl_seen" in *"|$d|"*) return ;; esac
  bl_seen="${bl_seen}${d}|"
  printf 'AM_BL claude-dir %s\n' "$n"
  for f in settings.json statusline-command.sh CLAUDE.md RTK.md; do
    printf 'AM_BL claude-file %s %s %s\n' "$n" "$f" "$(bl_has "$d/$f")"
  done
  for k in statusLine hooks permissions defaultMode; do
    printf 'AM_BL claude-key %s %s %s\n' "$n" "$k" "$(bl_key "$d/settings.json" "$k")"
  done
  for pl in $MAC_ONLY_PLUGINS; do
    printf 'AM_BL claude-plugin %s %s %s\n' "$n" "$pl" "$(bl_plugin "$d/settings.json" "$pl")"
  done
}
for d in "${CLAUDE_CONFIG_DIR:-}" "$HOME/.claude" "$HOME"/.claude-cc[0-9]*; do
  [ -n "$d" ] && [ -d "$d" ] || continue
  case "$d" in "$HOME/.claude") n=default ;; "$HOME"/.claude-cc*) n=${d##*/.claude-} ;; *) n=custom ;; esac
  bl_claude "$d" "$n"
done
# 登入 shell 的 alias（PROBE_SH 留在 $al；單獨跑這段時改讀 ~/.zshrc，不再開一次 login shell）。
# cc0–cc6 若 CLAUDE_CONFIG_DIR 不在上面的 glob（例如 ~/.claude-work）也要查。
[ -n "${al:-}" ] || al=$(cat "$HOME/.zshrc" 2>/dev/null || true)
printf '%s\n' "$al" | grep -E '(^|[[:space:]])(alias[[:space:]]+)?cc[0-6]=' | while IFS= read -r line; do
  name=$(printf '%s\n' "$line" | sed -n 's/.*\(cc[0-6]\)=.*/\1/p' | head -1)
  dir=$(printf '%s\n' "$line" | sed -n 's/.*CLAUDE_CONFIG_DIR=["'\'']\{0,1\}\([^[:space:]"'\'']*\).*/\1/p' | head -1)
  case "$dir" in
    '${HOME}/'*) dir="$HOME/${dir#'${HOME}/'}" ;;
    \$HOME/*) dir="$HOME/${dir#\$HOME/}" ;;
    "~/"*) dir="$HOME/${dir#~/}" ;;
  esac
  case "$dir" in /*) ;; *) continue ;; esac
  [ -n "$name" ] || continue
  bl_claude "$dir" "$name"
done
CX="${CODEX_HOME:-$HOME/.codex}"
printf 'AM_BL codex-file config.toml %s\n' "$(bl_has "$CX/config.toml")"
printf 'AM_BL codex-file hooks.json %s\n' "$(bl_has "$CX/hooks.json")"
if [ -f "$CX/config.toml" ] && grep -Eq '^[[:space:]]*approval_policy[[:space:]]*=' "$CX/config.toml" 2>/dev/null; then
  printf 'AM_BL codex-key approval_policy 1\n'
else
  printf 'AM_BL codex-key approval_policy 0\n'
fi
printf 'AM_BL codex-key features.hooks %s\n' "$(bl_features_hooks "$CX/config.toml")"
printf 'AM_BL grok-file config.toml %s\n' "$(bl_has "${GROK_HOME:-$HOME/.grok}/config.toml")"
printf 'AM_BL herdr-file config.toml %s\n' "$(bl_has "${XDG_CONFIG_HOME:-$HOME/.config}/herdr/config.toml")"
if [ -f "$HOME/.gitconfig" ] && grep -Eq 'https?://([^/@[:space:]]+:[^/@[:space:]]+|(gh[pousr]_|github_pat_|glpat-)[A-Za-z0-9_-]+)@' "$HOME/.gitconfig" 2>/dev/null; then
  printf 'AM_BL gitconfig-token 1\n'
else
  printf 'AM_BL gitconfig-token 0\n'
fi
printf 'AM_BL end\n'
"#;

    fn write(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn run(script: &str, home: &std::path::Path, path: &str, envs: &[(&str, String)]) -> Vec<u8> {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("HOME", home)
            .env("PATH", path)
            .env("SHELL", "/bin/sh")
            .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    /// 只有 sed／grep 這類基本工具、沒有 jq 的 PATH（鍵查不了＝`?`）。
    fn path_without_jq(dir: &std::path::Path) -> String {
        let bin = dir.join("bin-nojq");
        std::fs::create_dir_all(&bin).unwrap();
        for t in ["sed", "grep", "tail", "head", "cat", "uname"] {
            for d in ["/usr/bin", "/bin"] {
                let src = std::path::Path::new(d).join(t);
                if src.exists() {
                    let _ = std::os::unix::fs::symlink(&src, bin.join(t));
                    break;
                }
            }
        }
        bin.display().to_string()
    }

    /// 測試行程的 PATH 再補上常見的 jq 位置（Mac 的 jq 多半在 Homebrew 底下）。
    fn path_with_jq() -> String {
        format!("{}:/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin", std::env::var("PATH").unwrap_or_default())
    }

    const CODEX_VARIANTS: [&str; 12] = [
        "approval_policy = \"never\"\n[features]\nhooks = true # on\n",
        "features.hooks = true\n",
        "features.hooks = false\napproval_policy=1\n",
        "[features]\nhooks = false\n",
        "[other]\nfeatures.hooks = true\n",
        "[features]\r\nhooks = true\r\n",
        "[features] # table\n  hooks=true\n",
        "[features.x]\nhooks = true\n",
        "# approval_policy = \"never\"\n# features.hooks = true\n",
        "[profiles.x]\napproval_policy = \"never\"\n[features]\n\n# c\nhooks = true\n",
        "featuresXhooks = true\n",
        "",
    ];

    const SETTINGS_VARIANTS: [&str; 9] = [
        "{\"statusLine\":{},\"hooks\":{},\"permissions\":{\"defaultMode\":\"auto\"},\"enabledPlugins\":{\"imessage@m\":true,\"discord@m\": false}}",
        "{\"permissions\":\"x\",\"statusLine\":1}",
        "[1,2]",
        "{",
        "{\"defaultMode\":1,\"permissions\":{}}",
        "{\"permissions\":{\"defaultMode\":null},\"hooks\":null}",
        "",
        "{\"statusLine\":false,\"permissions\":{\"defaultMode\":false}}",
        "\"str\"",
    ];

    #[test]
    fn the_rendered_script_prints_byte_for_byte_what_the_hard_coded_script_printed() {
        let new_sh = super::super::baseline_sh();
        assert!(!new_sh.is_empty() && !new_sh.contains("__AM_"), "placeholders left in the rendered script");
        let mut seen = std::collections::BTreeSet::new();
        let mut runs = 0;
        for (i, (settings, codex)) in SETTINGS_VARIANTS.iter().zip(CODEX_VARIANTS.iter().cycle()).chain(
            // codex 的每一種都要跑到（上面 zip 只走到 settings 的長度）
            CODEX_VARIANTS.iter().map(|c| (&SETTINGS_VARIANTS[0], c)).collect::<Vec<_>>(),
        ).enumerate() {
            let scratch = crate::hosts::remote_script_support::Scratch::new();
            let h = scratch.0.join("home");
            // 多個身份：default、cc2（settings 在不同變體）、alias 指到 glob 外的目錄、CLAUDE_CONFIG_DIR 指到別處。
            write(&h.join(".claude/settings.json"), settings);
            write(&h.join(".claude/statusline-command.sh"), "");
            if i % 2 == 0 {
                write(&h.join(".claude/CLAUDE.md"), "");
                write(&h.join(".claude/RTK.md"), "");
            }
            write(&h.join(".claude-cc2/settings.json"), SETTINGS_VARIANTS[(i + 3) % SETTINGS_VARIANTS.len()]);
            write(&h.join(".claude-work/RTK.md"), "");
            write(&h.join(".zshrc"), "alias cc1='CLAUDE_CONFIG_DIR=$HOME/.claude-work claude'\n");
            write(&h.join("custom/settings.json"), SETTINGS_VARIANTS[(i + 1) % SETTINGS_VARIANTS.len()]);
            if !codex.is_empty() {
                write(&h.join(".codex/config.toml"), codex);
            }
            if i % 3 == 0 {
                write(&h.join(".codex/hooks.json"), "{}");
                write(&h.join(".grok/config.toml"), "");
                write(&h.join(".config/herdr/config.toml"), "");
                write(&h.join(".gitconfig"), "[url \"https://ghp_abc123@github.com/\"]\n");
            }
            let envs = vec![("CLAUDE_CONFIG_DIR", h.join("custom").display().to_string())];
            for path in [path_with_jq(), path_without_jq(&scratch.0)] {
                let old = run(OLD_BASELINE_SH, &h, &path, &envs);
                let new = run(new_sh, &h, &path, &envs);
                assert_eq!(String::from_utf8_lossy(&old), String::from_utf8_lossy(&new), "variant {i}");
                assert_eq!(old, new, "variant {i}");
                runs += 1;
                for l in String::from_utf8(new).unwrap().lines() {
                    if l.starts_with("AM_BL claude-key") || l.starts_with("AM_BL codex-key") {
                        let f: Vec<&str> = l.split(' ').collect();
                        seen.insert(format!("{} {} {}", f[1], f[f.len() - 2], f[f.len() - 1]));
                    }
                }
            }
        }
        assert!(runs >= 40, "{runs}");
        // 三個改寫（通用 jq、bl_toml_true、bl_toml_has）每種結果都要真的跑到，不然「相同」可能只是都沒走到。
        let mut want = vec![
            "codex-key approval_policy 1",
            "codex-key approval_policy 0",
            "codex-key features.hooks 1",
            "codex-key features.hooks 0",
            "claude-key defaultMode ?",
        ];
        let jq = std::process::Command::new("/bin/sh").arg("-c").arg("command -v jq").env("PATH", path_with_jq()).output().unwrap();
        if jq.status.success() {
            want.extend(["claude-key defaultMode 1", "claude-key defaultMode 0", "claude-key defaultMode x", "claude-key statusLine 1", "claude-key statusLine 0", "claude-key hooks 0"]);
        }
        for w in want {
            assert!(seen.contains(w), "never exercised `{w}`; seen {seen:?}");
        }
    }
}
