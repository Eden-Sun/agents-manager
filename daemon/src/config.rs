//! `~/.config/agents-manager/config.toml` — the authority for the *desired* Project / Bot set.
//!
//! SQLite holds runtime state (Run / Turn / Message / tokens). On load and after every
//! write-back we project TOML into SQLite (see `projection.rs`).

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn default_listen() -> String {
    "127.0.0.1:7788".to_string()
}
fn default_session() -> String {
    "agents-manager".to_string()
}
fn default_true() -> bool {
    true
}
fn default_ssh_port() -> u16 {
    22
}
pub fn default_host() -> String {
    LOCAL_HOST.to_string()
}

/// Reserved host name for "this machine".
pub const LOCAL_HOST: &str = "local";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_session")]
    pub herdr_session: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { listen: default_listen(), herdr_session: default_session() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotCfg {
    /// ULID. Missing on hand-written files; filled in and written back on first load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub kind: String,
    /// Model to run under, injected as claude `--model <m>` / codex `-m <m>` / grok `-m <m>`.
    /// None = the CLI's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort (grok `--reasoning-effort low|medium|high`). None = CLI default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub autostart: bool,
    /// Extension beyond SPEC v3: lets a bot run without daemon hook injection so the
    /// terminal-fallback path (§4.3) can be exercised. Defaults to true.
    #[serde(default = "default_true")]
    pub inject_hooks: bool,
    /// Grant the agent all permissions on start: claude `--dangerously-skip-permissions`,
    /// codex `--yolo` (alias of `--dangerously-bypass-approvals-and-sandbox`),
    /// grok `--always-approve` (= `--permission-mode bypassPermissions`). Defaults to true.
    #[serde(default = "default_true")]
    pub auto_approve: bool,
    /// Name of an `[[identities]]` entry, or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Per-bot pane env; overrides the identity's.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

/// A named set of env vars + args, applied to a bot at start time. Lets several bots of the
/// same kind run under different accounts (e.g. claude's `CLAUDE_CONFIG_DIR`, grok's `GROK_HOME`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityCfg {
    /// Unique id, `[a-z][a-z0-9_-]{0,31}`.
    pub name: String,
    /// `claude` | `codex` | `grok`; must match the bot it is applied to.
    pub kind: String,
    /// Extra pane env. `$HOME` / `${HOME}` / a leading `~` expand to the *host's* home.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Extra CLI args, inserted between the daemon's injected args and the bot's own.
    #[serde(default)]
    pub args: Vec<String>,
}

/// SPEC §11.2 — a remote machine reached over SSH, running its own herdr.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCfg {
    /// Unique id, `[a-z][a-z0-9_-]{0,31}`. `"local"` is reserved.
    pub name: String,
    /// ssh target: `user@host` or an ssh_config alias.
    pub ssh: String,
    /// Only emitted on the ssh command line when != 22, so ssh_config aliases keep their Port.
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    /// Extra ssh arguments appended verbatim (e.g. `["-i", "/path/to/key"]`).
    /// Beyond SPEC §11.2 — needed for the loopback dev sshd (§11.8 R5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_opts: Vec<String>,
    /// Remote named session. Never the remote default session.
    #[serde(default = "default_session")]
    pub herdr_session: String,
    /// PATH a non-interactive ssh shell is missing; prefixed to the remote PATH.
    #[serde(default)]
    pub remote_path: String,
    /// Port on the remote 127.0.0.1 that is reverse-forwarded to the daemon.
    /// Defaults to the daemon's own port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCfg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub path: String,
    pub label: String,
    /// `"local"` (default) or a `[[hosts]]` name.
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default, rename = "bots")]
    pub bots: Vec<BotCfg>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identities: Vec<IdentityCfg>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<HostCfg>,
    #[serde(default)]
    pub projects: Vec<ProjectCfg>,
}

/// Strict slug shape used by host and identity names (they end up in file paths / launchd labels).
pub const SLUG_NAME_RE: &str = "[a-z][a-z0-9_-]{0,31}";
/// Bot names are nicknames (v3.8): shown in the UI and used for `@mention`, never given to herdr.
pub const BOT_NAME_RE: &str = "1–32 個字，不可含空白或 @ , : ;";

/// Supported agent kinds (SPEC §2, §12). Also the herdr `agent.start` `kind` value.
pub const KINDS: [&str; 3] = ["claude", "codex", "grok"];

pub fn valid_effort(e: &str) -> bool {
    matches!(e.to_ascii_lowercase().as_str(), "low" | "medium" | "high")
}

pub fn valid_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// Human-readable list for error messages: `claude, codex or grok`.
pub fn kinds_list() -> String {
    let (last, rest) = KINDS.split_last().unwrap();
    format!("{} or {last}", rest.join(", "))
}

pub fn valid_slug_name(name: &str) -> bool {
    let mut it = name.chars();
    match it.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    if name.len() > 32 {
        return false;
    }
    it.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn valid_bot_name(name: &str) -> bool {
    let n = name.chars().count();
    n >= 1 && n <= 32 && !name.chars().any(|c| c.is_whitespace() || matches!(c, '@' | ',' | ':' | ';'))
}

/// Slug a project label into herdr's `[a-z][a-z0-9_-]*` alphabet (lowercase, other chars → `-`,
/// runs collapsed, must start with a letter). Empty when nothing usable is left.
fn label_slug(project_label: &str) -> String {
    let raw: String = project_label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    let mut collapsed = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(c);
    }
    let mut slug = collapsed.trim_matches('-').to_string();
    if let Some(first) = slug.chars().next() {
        if !first.is_ascii_lowercase() {
            slug.insert(0, 'p');
        }
    }
    slug
}

/// herdr agent name for a bot (v3.8): `<project slug>-<hash>`, where the hash is the tail of
/// the bot's ULID. The bot's own `name` is a free nickname that never reaches herdr, so it can
/// be changed at any time without a restart. Fits herdr's `[a-z][a-z0-9_-]{0,31}`.
pub fn agent_name(project_label: &str, bot_id: &str) -> String {
    const MAX: usize = 32;
    let tail: String = bot_id.to_ascii_lowercase().chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
    let hash: String = tail.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let hash = if hash.is_empty() { "bot".to_string() } else { hash };
    let mut slug = label_slug(project_label);
    let room = MAX.saturating_sub(hash.len() + 1);
    if slug.len() > room {
        slug.truncate(room);
        slug = slug.trim_end_matches('-').to_string();
    }
    if slug.is_empty() {
        format!("b-{hash}")
    } else {
        format!("{slug}-{hash}")
    }
}

/// The v3.5 scheme (`<project slug>-<bot name>`); still recognised by reconcile so runs started
/// under it keep working until they restart.
pub fn agent_name_legacy(project_label: &str, bot_name: &str) -> String {
    const MAX: usize = 32;
    let mut slug = label_slug(project_label);
    let room = MAX.saturating_sub(bot_name.len() + 1);
    if slug.is_empty() || room == 0 {
        return bot_name.to_string();
    }
    if slug.len() > room {
        slug.truncate(room);
        slug = slug.trim_end_matches('-').to_string();
        if slug.is_empty() {
            return bot_name.to_string();
        }
    }
    format!("{slug}-{bot_name}")
}

/// Identity names use the same shape as bot names.
pub fn valid_identity_name(name: &str) -> bool {
    valid_slug_name(name)
}

/// Expand `$HOME`, `${HOME}` and a leading `~` against a specific host's home directory.
pub fn expand_home(value: &str, home: &str) -> String {
    let mut out = if value == "~" {
        home.to_string()
    } else if let Some(rest) = value.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        value.to_string()
    };
    out = out.replace("${HOME}", home);
    // Replace `$HOME` only when it is not part of a longer identifier ($HOMEBREW…).
    let mut res = String::with_capacity(out.len());
    let bytes: Vec<char> = out.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '$' && bytes[i + 1..].starts_with(&['H', 'O', 'M', 'E']) {
            let after = bytes.get(i + 5);
            let boundary = match after {
                None => true,
                Some(c) => !(c.is_ascii_alphanumeric() || *c == '_'),
            };
            if boundary {
                res.push_str(home);
                i += 5;
                continue;
            }
        }
        res.push(bytes[i]);
        i += 1;
    }
    res
}

/// Host names use the same shape as bot names; `local` is reserved for this machine.
pub fn valid_host_name(name: &str) -> bool {
    valid_slug_name(name)
}

/// Canonicalize a project path; the directory must exist.
pub fn canonical_path(p: &str) -> Result<String> {
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?.join(rest)
    } else {
        PathBuf::from(p)
    };
    let c = std::fs::canonicalize(&expanded)
        .with_context(|| format!("project path does not exist: {}", expanded.display()))?;
    Ok(c.to_string_lossy().to_string())
}

pub struct ConfigStore {
    pub path: PathBuf,
    inner: tokio::sync::Mutex<Loaded>,
}

struct Loaded {
    cfg: ConfigFile,
    mtime: Option<SystemTime>,
}

impl ConfigStore {
    pub async fn load(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let (cfg, mtime) = read_file(&path)?;
        Ok(Self { path, inner: tokio::sync::Mutex::new(Loaded { cfg, mtime }) })
    }

    pub async fn get(&self) -> ConfigFile {
        self.inner.lock().await.cfg.clone()
    }

    /// Mutate the in-memory config and atomically write it back.
    ///
    /// Refuses (409-ish) when the file changed underneath us since the last read.
    pub async fn update<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut ConfigFile) -> Result<T>,
    {
        let mut g = self.inner.lock().await;
        let on_disk = std::fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
        if self.path.exists() && on_disk != g.mtime {
            bail!("config.toml changed on disk since it was loaded; reload required");
        }
        let mut next = g.cfg.clone();
        let out = f(&mut next)?;
        write_atomic(&self.path, &next)?;
        g.cfg = next;
        g.mtime = std::fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
        Ok(out)
    }
}

fn read_file(path: &Path) -> Result<(ConfigFile, Option<SystemTime>)> {
    if !path.exists() {
        let cfg = ConfigFile::default();
        write_atomic(path, &cfg)?;
        let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        return Ok((cfg, mtime));
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg: ConfigFile = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    Ok((cfg, mtime))
}

/// Full serde re-serialization (comments are lost — `toml_edit` preservation is stage two).
pub fn write_atomic(path: &Path, cfg: &ConfigFile) -> Result<()> {
    let text = toml::to_string_pretty(cfg)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod agent_name_tests {
    use super::{agent_name, agent_name_legacy, valid_bot_name};

    #[test]
    fn prefix_plus_id_tail() {
        assert_eq!(agent_name("agents-manager", "01M1S2SQPSYMQ8B1VQ50R963B9"), "agents-manager-r963b9");
        assert_eq!(agent_name("PowerTech Hub", "01M1S2SQPSYMQ8B1VQ50R963B9"), "powertech-hub-r963b9");
        assert_eq!(agent_name("2026 專案!!", "abcdef"), "p2026-abcdef");
        assert_eq!(agent_name("---", "abcdef"), "b-abcdef");
        let n = agent_name("a-very-long-project-label-indeed-and-more", "01M1S2SQPSYMQ8B1VQ50R963B9");
        assert!(n.len() <= 32, "{n}");
        assert!(n.ends_with("-r963b9"));
    }

    #[test]
    fn legacy_scheme_still_computable() {
        assert_eq!(agent_name_legacy("agents-manager", "am-claude"), "agents-manager-am-claude");
    }

    #[test]
    fn nicknames_are_free_text_without_separators() {
        assert!(valid_bot_name("am-claude"));
        assert!(valid_bot_name("小幫手"));
        assert!(valid_bot_name("Reviewer_2"));
        assert!(!valid_bot_name(""));
        assert!(!valid_bot_name("has space"));
        assert!(!valid_bot_name("a@b"));
        assert!(!valid_bot_name(&"x".repeat(33)));
    }
}
