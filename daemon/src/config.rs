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
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub autostart: bool,
    /// Extension beyond SPEC v3: lets a bot run without daemon hook injection so the
    /// terminal-fallback path (§4.3) can be exercised. Defaults to true.
    #[serde(default = "default_true")]
    pub inject_hooks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCfg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub path: String,
    pub label: String,
    #[serde(default, rename = "bots")]
    pub bots: Vec<BotCfg>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerConfig,
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
