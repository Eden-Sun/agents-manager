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
    /// Model to run under, injected as claude `--model <m>` / codex `-m <m>`. None = the CLI's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub autostart: bool,
    /// Extension beyond SPEC v3: lets a bot run without daemon hook injection so the
    /// terminal-fallback path (§4.3) can be exercised. Defaults to true.
    #[serde(default = "default_true")]
    pub inject_hooks: bool,
    /// Grant the agent all permissions on start: claude `--dangerously-skip-permissions`,
    /// codex `--yolo` (alias of `--dangerously-bypass-approvals-and-sandbox`). Defaults to true.
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
/// same kind run under different accounts (e.g. claude's `CLAUDE_CONFIG_DIR`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityCfg {
    /// Unique id, `[a-z][a-z0-9_-]{0,31}`.
    pub name: String,
    /// `claude` | `codex`; must match the bot it is applied to.
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

pub const BOT_NAME_RE: &str = "[a-z][a-z0-9_-]{0,31}";

pub fn valid_bot_name(name: &str) -> bool {
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

/// herdr agent name for a bot: `<project slug>-<bot name>`, squeezed into herdr's
/// `[a-z][a-z0-9_-]{0,31}`. The project label is slugged (lowercase, non-name chars → `-`,
/// must start with a letter) and truncated so the bot name always survives intact; when
/// nothing of the prefix fits, the bare bot name is used.
pub fn agent_name(project_label: &str, bot_name: &str) -> String {
    const MAX: usize = 32;
    let mut slug: String = project_label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    // collapse runs of '-' and trim them at both ends
    let mut collapsed = String::with_capacity(slug.len());
    for c in slug.chars() {
        if c == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(c);
    }
    slug = collapsed.trim_matches('-').to_string();
    if let Some(first) = slug.chars().next() {
        if !first.is_ascii_lowercase() {
            slug.insert(0, 'p');
        }
    }
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
    valid_bot_name(name)
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
    valid_bot_name(name)
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
    use super::agent_name;

    #[test]
    fn prefixes_with_project_slug() {
        assert_eq!(agent_name("agents-manager", "am-claude"), "agents-manager-am-claude");
        assert_eq!(agent_name("PowerTech Hub", "pt-leader"), "powertech-hub-pt-leader");
        assert_eq!(agent_name("pt", "test"), "pt-test");
    }

    #[test]
    fn slug_must_start_with_a_letter_and_survives_symbols() {
        assert_eq!(agent_name("2026 專案!!", "bot"), "p2026-bot");
        assert_eq!(agent_name("---", "bot"), "bot");
        assert_eq!(agent_name("", "bot"), "bot");
    }

    #[test]
    fn bot_name_always_survives_the_32_char_cap() {
        let long_bot = "b".repeat(30);
        assert_eq!(agent_name("agents-manager", &long_bot), format!("a-{long_bot}"));
        let bot32 = "c".repeat(32);
        assert_eq!(agent_name("agents-manager", &bot32), bot32);
        let n = agent_name("a-very-long-project-label-indeed", "worker");
        assert!(n.len() <= 32, "{n}");
        assert!(n.ends_with("-worker"));
    }
}
