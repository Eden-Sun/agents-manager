//! `~/.config/agents-manager/config.toml` — the authority for the *desired* Project / Bot set.
//!
//! SQLite holds runtime state (Run / Turn / Message / tokens). On load and after every
//! write-back we project TOML into SQLite (see `projection.rs`).

use anyhow::{anyhow, Context, Result};
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// `[supervisor]` — knobs for the manager (AGM) that are policy, not per-bot state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisorCfg {
    /// How often, at most, the daemon is allowed to wake the manager with its pending inbox.
    ///
    /// Events still land in `supervisor_inbox` the moment they happen — nothing is dropped or
    /// delayed on the way in. This only paces the *push*: one digest per window, carrying
    /// everything that accumulated in it. `0` = wake on every controller tick (the pre-4.x
    /// behaviour).
    #[serde(default = "default_notify_interval_secs")]
    pub notify_interval_secs: u64,
}

fn default_notify_interval_secs() -> u64 {
    600
}

impl Default for SupervisorCfg {
    fn default() -> Self {
        Self { notify_interval_secs: default_notify_interval_secs() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotCfg {
    /// An ASCII id matching `ID_RE`. Missing on hand-written files; filled in and written back
    /// on first load (normally as a ULID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub kind: String,
    /// Model to run under, injected as claude `--model <m>` / codex `-m <m>` / grok `-m <m>`.
    /// None = the CLI's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort: grok `--reasoning-effort low|medium|high`, codex
    /// `-c model_reasoning_effort="<x>"` (values from `model/list`). Always None for claude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// v4.0: codex "Fast" service tier (`-c service_tier="priority"`). Ignored by other kinds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    /// v4.0: text appended to the agent's system prompt (claude `--append-system-prompt`,
    /// grok `--rules`, codex `-c developer_instructions=…`). Never touches CLAUDE.md / AGENTS.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
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
    /// Herdr session override. Set for bots imported from the local user's `default` session;
    /// ordinary configured bots inherit their project's session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_session: Option<String>,
}

/// A named set of env vars + args, applied to a bot at start time. Lets several bots of the
/// same kind run under different accounts (e.g. claude's `CLAUDE_CONFIG_DIR`, grok's `GROK_HOME`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Ignored since v4.3 (SPEC §11.4): remote hooks report through that host's own herdr and
    /// leave their payload in a spool file, so there is no reverse forward and no port to pick.
    /// Still parsed — and warned about once — so an older config keeps loading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectCfg {
    /// An ASCII id matching `ID_RE`. Missing on hand-written files; filled in and written back
    /// on first load (normally as a ULID).
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub supervisor: SupervisorCfg,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identities: Vec<IdentityCfg>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<HostCfg>,
    #[serde(default)]
    pub projects: Vec<ProjectCfg>,
}

/// Strict slug shape used by host and identity names (they end up in file paths / launchd labels).
pub const SLUG_NAME_RE: &str = "[a-z][a-z0-9_-]{0,31}";
/// Config IDs are safe to append to local and remote directories.
pub const ID_RE: &str = "[A-Za-z0-9_-]{1,64}";
/// Bot names are nicknames (v3.8): shown in the UI and used for `@mention`, never given to herdr.
pub const BOT_NAME_RE: &str = "1–32 個字，不可含空白或 @ , : ;";

/// Supported agent kinds (SPEC §2, §12). Also the herdr `agent.start` `kind` value.
pub const KINDS: [&str; 3] = ["claude", "codex", "grok"];

/// Effort values a kind accepts (v4.0, kind-dependent).
///
/// grok includes `xhigh` (grok-4.6+; verified `--reasoning-effort xhigh -m grok-4.6`). The
/// per-model list from `GET /api/models` may be narrower (e.g. grok-4.5 is low/medium/high);
/// `effort_checked` drops a stored value the chosen model rejects.
///
/// claude gained `--effort <low|medium|high|xhigh|max>` in 2.1 (verified on 2.1.263:
/// `claude -p --effort high` works, and an unknown value is only a warning — it falls back to
/// the default rather than failing the run).
pub fn efforts_for_kind(kind: &str) -> &'static [&'static str] {
    match kind {
        "claude" => &["low", "medium", "high", "xhigh", "max"],
        "grok" => &["low", "medium", "high", "xhigh"],
        "codex" => &["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"],
        _ => &[],
    }
}

/// Normalise a requested effort for `kind`: trims / lowercases, `""` → `None`.
/// `Err(msg)` when the value is not one this kind accepts.
pub fn normalize_effort(kind: &str, e: Option<&str>) -> Result<Option<String>, String> {
    let Some(e) = e.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    let v = e.to_ascii_lowercase();
    let allowed = efforts_for_kind(kind);
    if allowed.contains(&v.as_str()) {
        Ok(Some(v))
    } else {
        Err(format!("effort for {kind} must be one of {}", allowed.join(", ")))
    }
}

/// v4.0: the `herdr` command a user pastes into a terminal to attach to a host's session.
pub fn attach_command(host: Option<&HostCfg>, local_session: &str) -> String {
    match host {
        None => format!("herdr --session {local_session}"),
        Some(h) if h.ssh_port == 22 => format!("herdr --remote {} --session {}", h.ssh, h.herdr_session),
        Some(h) => format!("herdr --remote ssh://{}:{} --session {}", h.ssh, h.ssh_port, h.herdr_session),
    }
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

pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
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
    pub async fn update<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut ConfigFile) -> Result<T>,
    {
        let mut g = self.inner.lock().await;
        let on_disk = std::fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
        if self.path.exists() && on_disk != g.mtime {
            let (cfg, mtime) = read_file(&self.path)
                .context("config.toml changed on disk and could not be re-read")?;
            tracing::info!("config.toml changed on disk; reloaded before applying update");
            g.cfg = cfg;
            g.mtime = mtime;
        }
        let mut next = g.cfg.clone();
        let out = f(&mut next)?;
        // Only touch the file when the closure actually changed something: a full serde
        // rewrite drops comments / unknown keys, so a no-op update must not clobber them
        // (issue #38 — startup projection used to rewrite the file on every boot).
        if next != g.cfg {
            write_atomic(&self.path, &next)?;
            g.mtime = std::fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
        }
        g.cfg = next;
        Ok(out)
    }
}

/// Shape of the TOML we understand, used to report keys serde would otherwise drop silently.
///
/// Unknown keys are a *warning*, not an error: an older daemon must still start on a file
/// written by a newer one (forward compatibility), but a typo (`auto_start`) must not vanish
/// without a trace either.
enum Schema {
    Table(&'static [(&'static str, Schema)]),
    Array(&'static Schema),
    /// Free-form value (`env` maps, scalars) — never descended into.
    Any,
}

const BOT_SCHEMA: Schema = Schema::Table(&[
    ("id", Schema::Any),
    ("name", Schema::Any),
    ("kind", Schema::Any),
    ("model", Schema::Any),
    ("effort", Schema::Any),
    ("fast", Schema::Any),
    ("persona", Schema::Any),
    ("args", Schema::Any),
    ("autostart", Schema::Any),
    ("inject_hooks", Schema::Any),
    ("auto_approve", Schema::Any),
    ("identity", Schema::Any),
    ("env", Schema::Any),
    ("herdr_session", Schema::Any),
]);

const PROJECT_SCHEMA: Schema = Schema::Table(&[
    ("id", Schema::Any),
    ("path", Schema::Any),
    ("label", Schema::Any),
    ("host", Schema::Any),
    ("bots", Schema::Array(&BOT_SCHEMA)),
]);

const IDENTITY_SCHEMA: Schema =
    Schema::Table(&[("name", Schema::Any), ("kind", Schema::Any), ("env", Schema::Any), ("args", Schema::Any)]);

const HOST_SCHEMA: Schema = Schema::Table(&[
    ("name", Schema::Any),
    ("ssh", Schema::Any),
    ("ssh_port", Schema::Any),
    ("ssh_opts", Schema::Any),
    ("herdr_session", Schema::Any),
    ("remote_path", Schema::Any),
    ("hook_port", Schema::Any),
]);

const CONFIG_SCHEMA: Schema = Schema::Table(&[
    ("server", Schema::Table(&[("listen", Schema::Any), ("herdr_session", Schema::Any)])),
    ("identities", Schema::Array(&IDENTITY_SCHEMA)),
    ("hosts", Schema::Array(&HOST_SCHEMA)),
    ("projects", Schema::Array(&PROJECT_SCHEMA)),
]);

/// Dotted paths (`projects[0].bots[1].auto_start`) of every key in `text` that `ConfigFile`
/// does not know about. Empty when the text is not valid TOML — `toml::from_str` reports that.
pub fn unknown_keys(text: &str) -> Vec<String> {
    let Ok(value) = text.parse::<toml::Value>() else { return Vec::new() };
    let mut out = Vec::new();
    walk_unknown(&value, &CONFIG_SCHEMA, String::new(), &mut out);
    out
}

fn walk_unknown(value: &toml::Value, schema: &Schema, path: String, out: &mut Vec<String>) {
    match (schema, value) {
        (Schema::Table(fields), toml::Value::Table(t)) => {
            for (k, v) in t {
                let child = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                match fields.iter().find(|(name, _)| *name == k) {
                    Some((_, sub)) => walk_unknown(v, sub, child, out),
                    None => out.push(child),
                }
            }
        }
        (Schema::Array(item), toml::Value::Array(items)) => {
            for (i, v) in items.iter().enumerate() {
                walk_unknown(v, item, format!("{path}[{i}]"), out);
            }
        }
        // Wrong value type (e.g. `projects = 1`): serde reports it as a parse error.
        _ => {}
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
    for key in unknown_keys(&text) {
        tracing::warn!("{}: unknown key `{key}` is ignored (typo? or a newer daemon's field)", path.display());
    }
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
mod v40_tests {
    use super::{attach_command, normalize_effort, HostCfg};

    fn host(port: u16) -> HostCfg {
        HostCfg {
            name: "m4p".into(),
            ssh: "m4p@100.112.229.82".into(),
            ssh_port: port,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
            hook_port: None,
        }
    }

    #[test]
    fn attach_commands() {
        assert_eq!(attach_command(None, "am-v40"), "herdr --session am-v40");
        assert_eq!(attach_command(Some(&host(22)), "x"), "herdr --remote m4p@100.112.229.82 --session agents-manager");
        assert_eq!(
            attach_command(Some(&host(2222)), "x"),
            "herdr --remote ssh://m4p@100.112.229.82:2222 --session agents-manager"
        );
    }

    #[test]
    fn effort_is_kind_dependent() {
        assert_eq!(normalize_effort("grok", Some(" High ")).unwrap(), Some("high".into()));
        assert_eq!(normalize_effort("grok", Some("xhigh")).unwrap(), Some("xhigh".into()));
        assert!(normalize_effort("grok", Some("max")).is_err());
        assert_eq!(normalize_effort("codex", Some("xhigh")).unwrap(), Some("xhigh".into()));
        assert_eq!(normalize_effort("codex", Some("none")).unwrap(), Some("none".into()));
        assert!(normalize_effort("codex", Some("turbo")).is_err());
        // claude gained `--effort` in 2.1: low…max, and `ultracode` is a TUI-only slider
        // position, not a CLI value.
        assert_eq!(normalize_effort("claude", Some("High")).unwrap(), Some("high".into()));
        assert_eq!(normalize_effort("claude", Some("max")).unwrap(), Some("max".into()));
        assert!(normalize_effort("claude", Some("none")).is_err());
        assert!(normalize_effort("claude", Some("ultracode")).is_err());
        assert_eq!(normalize_effort("codex", Some("")).unwrap(), None);
        assert_eq!(normalize_effort("codex", None).unwrap(), None);
    }
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

#[cfg(test)]
mod issue38_tests {
    use super::{unknown_keys, ConfigFile, ConfigStore};

    const SAMPLE: &str = r#"# top comment
[[projects]]
path = "/tmp"
label = "proj"
id = "01PROJ"

[[projects.bots]]
id = "01BOT"
name = "a"
kind = "claude"
auto_start = true   # typo for autostart
"#;

    #[test]
    fn unknown_keys_are_reported_with_paths() {
        let keys = unknown_keys(SAMPLE);
        assert_eq!(keys, vec!["projects[0].bots[0].auto_start"]);

        let keys = unknown_keys("[server]\nport = 1\n[[hosts]]\nname = \"m\"\nssh = \"x\"\nherdr-session = \"s\"\n");
        // toml::Table is a sorted map, so the order is alphabetical rather than file order.
        assert_eq!(keys, vec!["hosts[0].herdr-session", "server.port"]);

        // Free-form maps are never descended into.
        assert!(unknown_keys("[[identities]]\nname = \"i\"\nkind = \"claude\"\n[identities.env]\nFOO = \"1\"\n").is_empty());
        assert!(unknown_keys("").is_empty());
    }

    #[test]
    fn unknown_keys_are_still_parsed_leniently() {
        let cfg: ConfigFile = toml::from_str(SAMPLE).unwrap();
        assert!(!cfg.projects[0].bots[0].autostart, "typo'd key must not silently apply");
    }

    #[tokio::test]
    async fn noop_update_leaves_the_file_byte_identical() {
        let dir = std::env::temp_dir().join(format!("am-config-issue38-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, SAMPLE).unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();

        let dirty = store.update(|_cfg| Ok(false)).await.unwrap();
        assert!(!dirty);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SAMPLE, "comments and typos survive a no-op update");

        // A real change still writes back (and the typo'd key is then gone — known trade-off).
        store
            .update(|cfg| {
                cfg.projects[0].bots[0].autostart = true;
                Ok(true)
            })
            .await
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("autostart = true"));
        assert!(!text.contains("# top comment"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod issue28_tests {
    use super::{ConfigFile, ConfigStore};
    use std::path::Path;
    use std::time::Duration;

    fn temp_config() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("am-config-issue28-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), dir.join("config.toml"))
    }

    fn rewrite_externally(path: &Path, text: &str) {
        let before = std::fs::metadata(path).unwrap().modified().unwrap();
        std::fs::write(path, text).unwrap();
        if std::fs::metadata(path).unwrap().modified().unwrap() == before {
            std::thread::sleep(Duration::from_secs(1));
            std::fs::write(path, text).unwrap();
        }
        assert_ne!(std::fs::metadata(path).unwrap().modified().unwrap(), before);
    }

    #[tokio::test]
    async fn update_reloads_external_changes_before_applying_closure() {
        let (dir, path) = temp_config();
        std::fs::write(
            &path,
            r#"[server]
listen = "127.0.0.1:7788"
herdr_session = "initial"

[[projects]]
path = "/tmp/initial"
label = "initial"
"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();

        rewrite_externally(
            &path,
            r#"[server]
listen = "127.0.0.1:8899"
herdr_session = "external"

[[projects]]
path = "/tmp/external"
label = "external"
"#,
        );
        store
            .update(|cfg| {
                cfg.server.herdr_session = "closure".to_string();
                Ok(())
            })
            .await
            .unwrap();

        let cfg: ConfigFile = store.get().await;
        assert_eq!(cfg.server.listen, "127.0.0.1:8899");
        assert_eq!(cfg.server.herdr_session, "closure");
        assert_eq!(cfg.projects[0].label, "external");
        assert_eq!(cfg.projects[0].path, "/tmp/external");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn update_reports_external_toml_parse_errors() {
        let (dir, path) = temp_config();
        std::fs::write(&path, "[server]\nlisten = \"127.0.0.1:7788\"\n").unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();

        rewrite_externally(&path, "[server\nlisten = \"127.0.0.1:8899\"\n");
        let err = store
            .update(|cfg| {
                cfg.server.herdr_session = "closure".to_string();
                Ok(())
            })
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("config.toml changed on disk and could not be re-read"), "{message}");
        assert!(message.contains("parse "), "{message}");
        assert!(!message.contains("reload required"), "{message}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
