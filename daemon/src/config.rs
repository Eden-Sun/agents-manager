//! config.toml is the authority for the *desired* Project / Bot set; SQLite holds runtime state
//! and is projected from TOML (see `projection.rs`).

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

pub const LOCAL_HOST: &str = "local";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_session")]
    pub herdr_session: String,
    /// 資料目錄（SQLite、ui-token、spool）。留空＝跟著設定檔所在目錄，預設設定檔就是 `~/.config/agents-manager`。
    /// 相對路徑以設定檔所在目錄為準；`startup.rs` 負責解析。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { listen: default_listen(), herdr_session: default_session(), data_dir: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisorCfg {
    /// Paces only the *push* (one digest per window); events still land in the inbox immediately.
    /// `0` = wake on every controller tick.
    #[serde(default = "default_notify_interval_secs")]
    pub notify_interval_secs: u64,
    /// Re-offer unacked notifications: delivery is not an answer.
    #[serde(default = "default_notify_ack_deadline_secs")]
    pub notify_ack_deadline_secs: u64,
    /// Then raise `notify_exhausted`; the event is kept, only the quota burn on a silent manager stops.
    #[serde(default = "default_notify_max_attempts")]
    pub notify_max_attempts: i64,
    /// Reconnect blips are not a fault.
    #[serde(default = "default_host_disconnected_secs")]
    pub host_disconnected_secs: u64,
    /// Autostart bots only; a bot the user stopped is never counted.
    #[serde(default = "default_bot_stopped_secs")]
    pub bot_stopped_secs: u64,
    #[serde(default = "default_assignment_stalled_secs")]
    pub assignment_stalled_secs: u64,
    /// 派工遇到對方回合中時排進 `queued`；排超過這個時間還沒送出就把交辦標成 blocked，不要無聲排下去
    /// （AGM 2026-09-16）。
    #[serde(default = "default_assignment_queue_wait_secs")]
    pub assignment_queue_wait_secs: u64,
    /// 協調者（AGM responder）的短窗批次：第一件待辦等滿這麼久、距上次喚醒也滿這麼久才叫醒，
    /// 同一陣的申請合成一次（SPEC §18.15）。
    #[serde(default = "default_responder_batch_secs")]
    pub responder_batch_secs: u64,
    /// 協調者送不出去或等額度時，下一次重試最多隔多久。沒有次數上限。
    #[serde(default = "default_responder_max_backoff_secs")]
    pub responder_max_backoff_secs: u64,
}

fn default_responder_batch_secs() -> u64 {
    15
}

fn default_responder_max_backoff_secs() -> u64 {
    300
}

fn default_notify_interval_secs() -> u64 {
    600
}

fn default_notify_ack_deadline_secs() -> u64 {
    1800
}

fn default_notify_max_attempts() -> i64 {
    5
}

fn default_host_disconnected_secs() -> u64 {
    120
}

fn default_bot_stopped_secs() -> u64 {
    300
}

fn default_assignment_stalled_secs() -> u64 {
    7200
}

fn default_assignment_queue_wait_secs() -> u64 {
    1800
}

impl Default for SupervisorCfg {
    fn default() -> Self {
        Self {
            notify_interval_secs: default_notify_interval_secs(),
            notify_ack_deadline_secs: default_notify_ack_deadline_secs(),
            notify_max_attempts: default_notify_max_attempts(),
            host_disconnected_secs: default_host_disconnected_secs(),
            bot_stopped_secs: default_bot_stopped_secs(),
            assignment_stalled_secs: default_assignment_stalled_secs(),
            assignment_queue_wait_secs: default_assignment_queue_wait_secs(),
            responder_batch_secs: default_responder_batch_secs(),
            responder_max_backoff_secs: default_responder_max_backoff_secs(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotCfg {
    /// Missing on hand-written files; filled in (ULID) and written back on first load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub kind: String,
    /// None = the CLI's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// codex only (`-c service_tier="priority"`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    /// Appended to the system prompt; never touches CLAUDE.md / AGENTS.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub autostart: bool,
    /// false exercises the terminal-fallback path (SPEC §4.3).
    #[serde(default = "default_true")]
    pub inject_hooks: bool,
    /// Grants all permissions on start (claude `--dangerously-skip-permissions`, codex `--yolo`,
    /// grok `--always-approve`). Defaults to true.
    #[serde(default = "default_true")]
    pub auto_approve: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Per-bot pane env; overrides the identity's.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Set for bots imported from the local `default` session; others inherit the project's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_session: Option<String>,
}

/// Lets several bots of one kind run under different accounts (`CLAUDE_CONFIG_DIR`, `GROK_HOME`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdentityCfg {
    pub name: String,
    /// Must match the bot it is applied to.
    pub kind: String,
    /// 這個身分屬於哪一台主機（SPEC §16.2：同名的 `cc1` 在不同機器上是不同帳號）。
    /// 鍵是 `(host, name)`：同一個名字可以在不同主機各有一份。
    /// 省略＝本機照舊優先；**遠端也適用，但讓位給那台自己同名的身分**（合併規則在 `tools::merge_identities`）。
    /// 以前省略＝只適用本機，codex／grok 身分只能寫在 config 裡，升級後遠端 bot 一啟動就 409（review 2026-09-16 M6）；
    /// 再更早是省略＝每台都鋪上而且蓋掉那台的 `ccN`。只要本機的話寫 `host = "local"`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `$HOME` / `${HOME}` / leading `~` expand to the *host's* home.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Inserted between the daemon's injected args and the bot's own.
    #[serde(default)]
    pub args: Vec<String>,
}

impl IdentityCfg {
    /// 這一筆屬於哪一台。沒寫就是本機。
    pub fn host_or_local(&self) -> &str {
        self.host.as_deref().map(str::trim).filter(|h| !h.is_empty()).unwrap_or(LOCAL_HOST)
    }

    /// 有沒有明寫 host。
    pub fn is_hostless(&self) -> bool {
        self.host.as_deref().map(str::trim).filter(|h| !h.is_empty()).is_none()
    }
}

/// SPEC §11.2 — a remote machine reached over SSH, running its own herdr.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostCfg {
    /// `"local"` is reserved.
    pub name: String,
    pub ssh: String,
    /// Only emitted when != 22, so ssh_config aliases keep their Port.
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    /// Beyond SPEC §11.2 — needed for the loopback dev sshd (§11.8 R5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_opts: Vec<String>,
    /// Never the remote default session.
    #[serde(default = "default_session")]
    pub herdr_session: String,
    /// A non-interactive ssh shell's PATH is missing things; prefixed to the remote PATH.
    #[serde(default)]
    pub remote_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectCfg {
    /// Missing on hand-written files; filled in (ULID) and written back on first load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub path: String,
    pub label: String,
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
    /// §6.5e：非 agent pane 的 GC 門檻與例外。
    #[serde(default)]
    pub panes: PanesCfg,
}

/// SPEC §6.5e。閒置門檻可用 `AM_PANE_IDLE_CLOSE_SECS` 覆寫（看不懂／0／負數／低於 10 分鐘一律不採用——
/// 一個手滑的值不該把 GC 變成「立刻關」；設定檔的值同一條規矩）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PanesCfg {
    #[serde(default = "default_idle_close_secs")]
    pub idle_close_secs: u64,
    /// 連專案都對不到的那唯一一顆 shell pane 的固定名字；它永不自動關。
    #[serde(default = "default_scratch_name")]
    pub scratch_name: String,
    /// 關掉之前先記下畫面最後幾行（自動關不可逆，出事要說得出關掉的是什麼）。
    #[serde(default = "default_close_log_lines")]
    pub close_log_lines: u32,
}

impl Default for PanesCfg {
    fn default() -> Self {
        Self {
            idle_close_secs: default_idle_close_secs(),
            scratch_name: default_scratch_name(),
            close_log_lines: default_close_log_lines(),
        }
    }
}

fn default_idle_close_secs() -> u64 {
    21600
}

fn default_scratch_name() -> String {
    "scratch".into()
}

fn default_close_log_lines() -> u32 {
    20
}

/// 閒置門檻的下限。低於這個值不是「GC 關快一點」，是每一輪對帳都把閒著的 shell 關光（review 2026-09-16 core 8：
/// 以為 0＝停用，結果被當成 1 秒）。
pub const MIN_IDLE_CLOSE_SECS: u64 = 600;

impl PanesCfg {
    /// 環境變數覆寫；看不懂、0、負數、低於 [`MIN_IDLE_CLOSE_SECS`] 一律不採用。設定檔的值同一條規矩，不採用時回預設。
    pub fn idle_close_secs(&self) -> u64 {
        resolve_idle_close_secs(std::env::var("AM_PANE_IDLE_CLOSE_SECS").ok().as_deref(), self.idle_close_secs)
    }
}

fn resolve_idle_close_secs(env: Option<&str>, file: u64) -> u64 {
    let sane = |n: u64| n >= MIN_IDLE_CLOSE_SECS;
    match env.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(n) if sane(n) => n,
        _ if sane(file) => file,
        _ => default_idle_close_secs(),
    }
}

#[cfg(test)]
mod panes_cfg_tests {
    use super::*;

    /// review 2026-09-16 core 8：設定檔寫 0 以前被 `.max(1)` 當成 1 秒，所有可 GC 的閒置 shell 下一輪就被關光。
    #[test]
    fn a_zero_or_tiny_idle_threshold_falls_back_to_the_default() {
        let default = default_idle_close_secs();
        assert_eq!(resolve_idle_close_secs(None, 0), default, "0 不是停用，也不是 1 秒");
        assert_eq!(resolve_idle_close_secs(None, 5), default, "低於下限");
        assert_eq!(resolve_idle_close_secs(None, 3600), 3600);
        assert_eq!(resolve_idle_close_secs(Some("7200"), 3600), 7200, "環境變數覆寫");
        for bad in ["0", "-5", "abc", "30", ""] {
            assert_eq!(resolve_idle_close_secs(Some(bad), 3600), 3600, "{bad:?} 不採用，回設定檔的值");
            assert_eq!(resolve_idle_close_secs(Some(bad), 0), default, "{bad:?}＋設定檔 0 → 預設");
        }
    }
}

/// Host / identity names end up in file paths and launchd labels.
pub const SLUG_NAME_RE: &str = "[a-z][a-z0-9_-]{0,31}";
pub const ID_RE: &str = "[A-Za-z0-9_-]{1,64}";
/// Nicknames, never given to herdr.
pub const BOT_NAME_RE: &str = "1–32 個字，不可含空白或 @ , : ;";

/// SPEC §2, §12. Also the herdr `agent.start` `kind` value.
pub const KINDS: [&str; 3] = ["claude", "codex", "grok"];

/// grok `xhigh` needs grok-4.6+ (verified); per-model lists may be narrower, `effort_checked` drops rejects.
/// claude `--effort` since 2.1 (verified 2.1.263); an unknown value only warns and falls back to default.
pub fn efforts_for_kind(kind: &str) -> &'static [&'static str] {
    match kind {
        "claude" => &["low", "medium", "high", "xhigh", "max"],
        "grok" => &["low", "medium", "high", "xhigh"],
        "codex" => &["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"],
        _ => &[],
    }
}

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

/// Into herdr's `[a-z][a-z0-9_-]*` alphabet; empty when nothing usable is left.
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

/// `<project slug>-<ULID tail>`: the nickname never reaches herdr, so renaming needs no restart.
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

pub fn valid_identity_name(name: &str) -> bool {
    valid_slug_name(name)
}

pub fn expand_home(value: &str, home: &str) -> String {
    let mut out = if value == "~" {
        home.to_string()
    } else if let Some(rest) = value.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        value.to_string()
    };
    out = out.replace("${HOME}", home);
    // Not inside a longer identifier ($HOMEBREW…).
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

pub fn valid_host_name(name: &str) -> bool {
    valid_slug_name(name)
}

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
        // A serde rewrite drops comments / unknown keys: no-op updates must not write (issue #38).
        if next != g.cfg {
            write_atomic(&self.path, &next)?;
            g.mtime = std::fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
        }
        g.cfg = next;
        Ok(out)
    }
}

/// Unknown keys warn, not error: an older daemon must start on a newer file, but typos must not vanish silently.
fn parse_config(text: &str) -> Result<(ConfigFile, Vec<String>)> {
    let mut ignored = Vec::new();
    let cfg = serde_ignored::deserialize(toml::Deserializer::new(text), |path| {
        ignored.push(path.to_string());
    })?;
    Ok((cfg, ignored))
}

fn read_file(path: &Path) -> Result<(ConfigFile, Option<SystemTime>)> {
    if !path.exists() {
        let cfg = ConfigFile::default();
        write_atomic(path, &cfg)?;
        let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        return Ok((cfg, mtime));
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (cfg, unknown) = parse_config(&text).with_context(|| format!("parse {}", path.display()))?;
    for key in unknown {
        tracing::warn!("{}: unknown key `{key}` is ignored (typo? or a newer daemon's field)", path.display());
    }
    let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    Ok((cfg, mtime))
}

/// Full serde re-serialization: comments are lost.
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
        // `ultracode` is a TUI-only slider position, not a CLI value.
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
    use super::{agent_name, valid_bot_name};

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
    use super::{parse_config, ConfigFile, ConfigStore};

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

    fn unknown_keys(text: &str) -> Vec<String> {
        parse_config(text).map(|(_, ignored)| ignored).unwrap_or_default()
    }

    #[test]
    fn unknown_keys_are_reported_with_paths() {
        let keys = unknown_keys(SAMPLE);
        assert_eq!(keys, vec!["projects.0.bots.0.auto_start"]);

        let keys = unknown_keys("[server]\nport = 1\n[[hosts]]\nname = \"m\"\nssh = \"x\"\nherdr-session = \"s\"\n");
        assert!(keys.contains(&"hosts.0.herdr-session".to_string()));
        assert!(keys.contains(&"server.port".to_string()));

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

        // Known trade-off: a real change drops the typo'd key.
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
