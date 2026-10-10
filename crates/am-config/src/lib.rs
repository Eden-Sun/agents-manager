//! config.toml is the authority for the *desired* Project / Bot set; SQLite holds runtime state
//! and is projected from TOML (see `projection.rs`).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn default_listen() -> String {
    "127.0.0.1:7788".to_string()
}
/// `[server] herdr_session` 與每台主機的 `herdr_session` 的預設值。
pub const DEFAULT_HERDR_SESSION: &str = "agents-manager";

fn default_session() -> String {
    DEFAULT_HERDR_SESSION.to_string()
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
    /// 解析時就夾到 [`MIN_NOTIFY_MAX_ATTEMPTS`] 以上（Refs #504）。
    #[serde(default = "default_notify_max_attempts", deserialize_with = "de_notify_max_attempts")]
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
    /// 解析時就夾到 [`MIN_RESPONDER_MAX_BACKOFF_SECS`] 以上（Refs #504）。
    #[serde(default = "default_responder_max_backoff_secs", deserialize_with = "de_responder_max_backoff_secs")]
    pub responder_max_backoff_secs: u64,
}

/// 補送次數的下限（Refs #504）：`0`／負數不是「不補送」，是「每一筆事件一建立就算用完」——
/// `due_for` 的 `notify_attempts < max` 從頭到尾不成立，沒有人會被叫醒。`controller` 一直在
/// 讀取端寫 `.max(1)` 補救，但 `incidents::Thresholds::from_cfg` 這類地方抄漏就破功，
/// 所以在解析設定時就夾好，結構裡永遠不會有離譜的值。
pub const MIN_NOTIFY_MAX_ATTEMPTS: i64 = 1;

/// 退避上限的下限（Refs #504）：`0` 會讓 `notify_next_at` 永遠等於「現在」，協調者每個 controller tick
/// （`controller::TICK`，10 秒）重寫一次角色列、推一次 SSE——那正是 `responder::notify` 的 `Blocked`
/// 分支註解在防的抖動。低於一個 tick 的值本來也觀測不到，所以下限就取一個 tick。
pub const MIN_RESPONDER_MAX_BACKOFF_SECS: u64 = 10;

fn de_notify_max_attempts<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let raw = i64::deserialize(d)?;
    if raw < MIN_NOTIFY_MAX_ATTEMPTS {
        tracing::warn!(raw, used = MIN_NOTIFY_MAX_ATTEMPTS, "[supervisor] notify_max_attempts 太小，改用下限（0／負數＝沒有人會被叫醒）");
    }
    Ok(raw.max(MIN_NOTIFY_MAX_ATTEMPTS))
}

fn de_responder_max_backoff_secs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let raw = u64::deserialize(d)?;
    if raw < MIN_RESPONDER_MAX_BACKOFF_SECS {
        tracing::warn!(raw, used = MIN_RESPONDER_MAX_BACKOFF_SECS, "[supervisor] responder_max_backoff_secs 太小，改用下限（低於一個 controller tick 觀測不到）");
    }
    Ok(raw.max(MIN_RESPONDER_MAX_BACKOFF_SECS))
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
    /// 建這顆 bot 的那個請求的冪等鍵與指紋（`POST /api/projects/:id/bots` 的 `client_request_id`，#352）：回應遺失後原樣重送
    /// 拿回同一顆而不是再建一顆。記在 config.toml（daemon 持久、瀏覽器重整與 daemon 重啟都在）；bot 刪掉就一起沒了。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_fingerprint: Option<String>,
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
    /// #709：另一顆 daemon 也在用這台的 `herdr_session`（SPEC §11.10）。開著時這顆 daemon 在這台只碰自己的
    /// pane／tab／workspace、絕不 `herdr server stop`、不搬遠端的 bot 目錄。執行時讀當下的設定，改了不必重連。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared_session: bool,
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
    /// #708：已移交給另一台主機的 daemon 管（值＝接手主機的顯示名）。有值時這顆 daemon 對這個專案的
    /// bot／pane 一律不動（SPEC §6.5h）；清掉＝收回，下一輪對帳照常接手。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handed_off_to: Option<String>,
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
    /// issue #90：全機 cargo/rustc 併發的排程設定。
    #[serde(default)]
    pub build: BuildCfg,
    /// issue #204：上游新版分診開 GitHub issue 的設定（預設不開）。
    #[serde(default, skip_serializing_if = "ReleaseTriageCfg::is_default")]
    pub release_triage: ReleaseTriageCfg,
    /// issue #240：撞限偵測的第二意見（只記錄），預設關。
    #[serde(default, skip_serializing_if = "JudgeCfg::is_default")]
    pub judge: JudgeCfg,
    /// 使用者 2026-10-01：bot 的指示檔（agent md）由這裡指定，CLI 自己的 CLAUDE.md／AGENTS.md 一律不讀（SPEC §6.5i）。
    #[serde(default, skip_serializing_if = "AgentsCfg::is_default")]
    pub agents: AgentsCfg,
    /// issue #748：codex 0.159 `instant_interrupt` 的 canary 旗標（預設關）。
    #[serde(default, skip_serializing_if = "CodexCfg::is_default")]
    pub codex: CodexCfg,
    /// issue #749：codex app-server 的 thread 歷史當結構化 turn evidence（SPEC §4.4a）。預設開，可關。
    #[serde(default, skip_serializing_if = "CodexHistoryCfg::is_default")]
    pub codex_history: CodexHistoryCfg,
    /// 分享 bot 的對外入口（SPEC「分享 bot」）。沒寫＝不開那個 listener、也不能開分享連結。
    #[serde(default, skip_serializing_if = "ShareCfg::is_default")]
    pub share: ShareCfg,
}

/// `[share]`：`listen` 是分享入口的獨立 listener（Tailscale Funnel 指過來的那個 port），`base_url` 是對外網址。
/// 兩個都可以不寫：沒 `listen` 就不開入口，沒 `base_url` 就不能開分享連結（409 `share_not_configured`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareCfg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// 受限 bot「新資料夾」的根目錄（可用 `~/`）；沒設＝`~/shared-bots`。要在 daemon 資料目錄之外。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folders_root: Option<String>,
}

impl ShareCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// `https://…`／`http://…`，去掉結尾的 `/`；其他形狀當沒設。
    pub fn base(&self) -> Option<String> {
        let b = self.base_url.as_deref()?.trim().trim_end_matches('/');
        let rest = b.strip_prefix("https://").or_else(|| b.strip_prefix("http://"))?;
        (!rest.is_empty() && !rest.contains(char::is_whitespace)).then(|| b.to_string())
    }
}

/// `[codex]`（issue #748，SPEC §6.3 第 9 點）：`instant_interrupt = true` 才讓 `send_now` 對 codex（>= 0.159.0）生效——
/// 把字打進正在忙的 TUI，由 codex 自己的 `instant_interrupt` 把它 steer 進進行中的回合。這個旗標**只管 daemon 這一側肯不肯送**；
/// codex 那一側要另外在它自己的設定打開 `instant_interrupt`，沒開的話那一句只是排在 codex 的輸入佇列。預設關＝codex 照舊不能插隊。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCfg {
    #[serde(default)]
    pub instant_interrupt: bool,
}

impl CodexCfg {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `[codex_history]`：`enabled = false`（**預設**）時完全不開 app-server，送達／回覆／中斷證據只走 rollout 與畫面（舊路）。
/// 預設關的原因（#749 審查）：裝著的 codex 0.159.3 的 app-server 不支援 `thread/items/list`、`thread/turns/list` 又要先載入 thread，
/// 開著只是每則 prompt 白起一個 app-server（寫 CODEX_HOME、對外跑 `git ls-remote`）卻拿不到證據；等有支援的 codex 版本再開。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CodexHistoryCfg {
    #[serde(default)]
    pub enabled: bool,
}

impl CodexHistoryCfg {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `[agents]`（SPEC §6.5i）：每顆 bot 啟動時讀這些檔，接在 AG Man 規則後面注入（claude `--append-system-prompt`、
/// codex `developer_instructions`、grok `--rules`），子 agent 經 herdr shim 拿同一份。路徑可用 `~`；讀不到只警告。
/// `projects` 的 key 是專案 id 或 label，值是那個專案的 agent md（接在全域那份後面）：一個路徑或路徑陣列（依序接起來；
/// CLAUDE.md 的 `@AGENTS.md` 匯入不會展開，要兩份就兩份都列）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentsCfg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_file: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub projects: std::collections::BTreeMap<String, AgentMdFiles>,
}

/// `[agents.projects]` 的值：一個路徑或路徑陣列。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMdFiles {
    One(String),
    Many(Vec<String>),
}

impl AgentMdFiles {
    pub fn paths(&self) -> Vec<&str> {
        let all: Vec<&str> = match self {
            Self::One(p) => vec![p.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        };
        all.into_iter().filter(|p| !p.trim().is_empty()).collect()
    }
}

impl AgentsCfg {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// 全域那份（在 daemon 這台機器上讀）。
    pub fn global_file(&self) -> Option<&str> {
        self.instructions_file.as_deref().filter(|p| !p.trim().is_empty())
    }

    /// 這個專案那幾份（在專案所在的主機上讀，跟 repo 放在一起），依序。id 優先於 label。
    pub fn project_files(&self, project_id: &str, project_label: &str) -> Vec<&str> {
        self.projects.get(project_id).or_else(|| self.projects.get(project_label)).map(AgentMdFiles::paths).unwrap_or_default()
    }
}

/// `[judge]`（SPEC §4.3c）：`enabled` 與 `projects`（專案 id 或 label）兩層都要開才會把遮罩後的畫面尾段
/// 送到 `endpoint`。`key_file` 只是路徑，key 本身不進設定檔。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
    #[serde(default = "default_judge_key_file")]
    pub key_file: String,
    #[serde(default = "default_judge_endpoint")]
    pub endpoint: String,
    /// 釘版本：`jev-latest` 會漂，shadow 的數字就不能前後比。
    #[serde(default = "default_judge_model")]
    pub model: String,
    #[serde(default = "default_judge_timeout_ms")]
    pub timeout_ms: u64,
    /// 保險絲：一小時內問過這麼多次就不再問。
    #[serde(default = "default_judge_max_per_hour")]
    pub max_per_hour: u32,
}

fn default_judge_key_file() -> String {
    "~/.config/typesafe/api-key".into()
}
fn default_judge_endpoint() -> String {
    "https://api.typesafe.ai/v1/systemone".into()
}
fn default_judge_model() -> String {
    "jev-1.13.0".into()
}
fn default_judge_timeout_ms() -> u64 {
    3000
}
fn default_judge_max_per_hour() -> u32 {
    60
}

impl Default for JudgeCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            projects: Vec::new(),
            key_file: default_judge_key_file(),
            endpoint: default_judge_endpoint(),
            model: default_judge_model(),
            timeout_ms: default_judge_timeout_ms(),
            max_per_hour: default_judge_max_per_hour(),
        }
    }
}

impl JudgeCfg {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `[release_triage]`：`publish = false`（預設）時只寫帳本、完全不呼叫 `gh issue create`——先乾跑幾版，
/// 看過品質再打開。`gh_bin` 省略＝PATH 上的 `gh`（daemon 會補上 Homebrew 路徑）；`repo` 是 `owner/name`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReleaseTriageCfg {
    #[serde(default)]
    pub publish: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_bin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

impl ReleaseTriageCfg {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// issue #90：build scheduler 的門檻。名額的存活期（`lease_ttl_secs`）是「持有者多久沒續約就當它死了」，
/// 不是建置本身的時限——建置跑多久都行，只要背景續約還在動。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildCfg {
    /// 全機同時最多幾個受管的 cargo 佔用（`cargo-slot.sh` 現行手動限制是 2）。
    #[serde(default = "default_build_max_concurrent")]
    pub max_concurrent: usize,
    /// 每個佔用的 `CARGO_BUILD_JOBS`：不吃 cargo 預設的「核心數」，避免兩個佔用各自吃滿全機。
    #[serde(default = "default_build_cargo_jobs")]
    pub cargo_jobs: usize,
    /// 每個佔用的 `cargo test` 測試執行緒上限（shim 注入 `RUST_TEST_THREADS`，issue #813）；`0`＝不設（libtest 預設＝核心數）。
    /// 名額本來就罩住整個 test run，但 `CARGO_BUILD_JOBS` 只限 rustc，不限測試執行緒：32 核上每支 test binary 開 32 個、各吃 8 核以上。
    /// 呼叫端自己設了 `RUST_TEST_THREADS` 或帶 `--test-threads` 就尊重呼叫端。
    #[serde(default = "default_build_test_threads")]
    pub test_threads: usize,
    /// 名額 TTL（秒）：拿到之後這麼久沒 renew 就視為持有者已死，下一次 acquire 收回。
    #[serde(default = "default_build_lease_ttl_secs", deserialize_with = "de_build_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
    /// issue #104：開發者專用的外部 Cargo verification worker。密碼不在這裡，另存 data-dir/remote-cargo-password。
    #[serde(default)]
    pub remote: BuildRemoteCfg,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildRemoteCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub user: String,
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    #[serde(default = "default_remote_build_root")]
    pub remote_root: String,
    #[serde(default = "default_remote_build_jobs")]
    pub cargo_jobs: usize,
    /// 遠端 `cargo test` 的測試執行緒上限（`RUST_TEST_THREADS`，issue #202）；`0`＝不設（用 libtest 的預設＝核心數）。
    /// 呼叫端自己帶了 `--test-threads`／`RUST_TEST_THREADS` 就尊重呼叫端。
    #[serde(default = "default_remote_test_threads")]
    pub test_threads: usize,
    /// 一次遠端編譯（同步＋編譯＋測試）的整體時間上限，秒（issue #194）；`0`＝不設上限。超過就整組砍掉、回 124，不退回本機重跑。
    #[serde(default = "default_remote_build_timeout_secs")]
    pub timeout_secs: u64,
    /// 遠端每棵 worktree 的 `shared/`（原始碼＋target，各 2～3G）閒置這麼多小時就回收（issue #196）。
    #[serde(default = "default_remote_shared_idle_hours")]
    pub shared_idle_hours: u64,
    /// 遠端 `shared/` 最多留幾份（連同正在用的）；超過就從最久沒用的開始收。`0`＝不限（issue #196）。
    #[serde(default = "default_remote_max_shared_dirs")]
    pub max_shared_dirs: usize,
    /// 同一個 `remote_root` 同時最多幾個遠端編譯（不分 worktree）；滿了就排隊。`0`（預設）＝依遠端的核數與 RAM 自動算（issue #104）。
    #[serde(default)]
    pub max_concurrent: usize,
    /// 密碼檔是 data-dir 的哪一份（issue #104）：`remote-cargo-password.<id>`。沒有這個 key＝舊版設定，讀 `remote-cargo-password`；
    /// 空字串＝沒有密碼（key/agent）。密碼本身永遠不在這裡——換主機與換密碼靠「config.toml 換成指向新檔」這一次 rename 一起生效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_id: Option<String>,
    /// 指定 ssh 金鑰檔（issue #104）：ssh／rsync 帶 `-i <path> -o IdentitiesOnly=yes`。空字串＝走 ssh 預設（agent、`~/.ssh/config`、預設金鑰名）。
    /// 只放路徑，私鑰本身不進設定；檔案不存在會明確報錯，不退回本機。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub identity_file: String,
}

impl Default for BuildRemoteCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            user: String::new(),
            ssh_port: default_ssh_port(),
            remote_root: default_remote_build_root(),
            cargo_jobs: default_remote_build_jobs(),
            test_threads: default_remote_test_threads(),
            timeout_secs: default_remote_build_timeout_secs(),
            shared_idle_hours: default_remote_shared_idle_hours(),
            max_shared_dirs: default_remote_max_shared_dirs(),
            max_concurrent: 0,
            password_id: None,
            identity_file: String::new(),
        }
    }
}

pub fn default_remote_build_root() -> String {
    ".cache/agents-manager/remote-cargo".into()
}

fn default_remote_build_jobs() -> usize {
    4
}

/// 遠端是 32 vCPU 的超賣主機：測試預設開 32 個執行緒，大多在系統呼叫與鎖上互搶（`sy` 59～64%、每秒 50 萬次 context switch）。
/// 全套 1611 條測試（issue #202 實測）：32 個執行緒 283 秒、16 個 199 秒、12 個 219 秒、**8 個 168～176 秒**——挑 8。
fn default_remote_test_threads() -> usize {
    8
}

fn default_remote_shared_idle_hours() -> u64 {
    3
}

/// 每份約 2～3G：12 份約 30～36G，不把遠端的磁碟吃光（issue #196：一天開十幾顆子 agent、每張票數個變異副本，各是一個新 hash）。
/// 8 太小（issue #417）：常態就是十來顆 child 各一棵 worktree ＝ 十來個 hash，上限卡在 8 會在每一輪把最舊的幾份收掉，
/// 下一輪那幾顆又得整棵冷編譯一次——回收是要擋磁碟，不是要製造抖動。正式設定檔自己寫死 `max_shared_dirs` 時這個預設不生效，
/// 要改線上的值得走 `PUT /api/build/remote`。
fn default_remote_max_shared_dirs() -> usize {
    12
}

/// 12 分鐘：實測 112 次遠端編譯最長 8.9 分鐘（全套 test 中位數 5.0、P90 8.7），約 1.35 倍，不誤殺正常編譯（issue #194）。
pub fn default_remote_build_timeout_secs() -> u64 {
    12 * 60
}

impl Default for BuildCfg {
    fn default() -> Self {
        Self {
            max_concurrent: default_build_max_concurrent(),
            cargo_jobs: default_build_cargo_jobs(),
            test_threads: default_build_test_threads(),
            lease_ttl_secs: default_build_lease_ttl_secs(),
            remote: BuildRemoteCfg::default(),
        }
    }
}

fn default_build_max_concurrent() -> usize {
    2
}

fn default_build_cargo_jobs() -> usize {
    2
}

/// 跟遠端同一個數字（`default_remote_test_threads`，issue #202 在 32 vCPU 上實測 8 個執行緒最快）。
fn default_build_test_threads() -> usize {
    8
}

/// `test_threads` 的上限，跟 `[build.remote] test_threads` 一樣。
pub const MAX_BUILD_TEST_THREADS: usize = 256;

fn default_build_lease_ttl_secs() -> u64 {
    180
}

/// 併發上限的下限：0 不是「停用排程」，是「誰都拿不到名額」，整台機器的受管建置會全部卡死。
pub const MIN_BUILD_MAX_CONCURRENT: usize = 1;

/// 名額租約 TTL 的下限（#322）：0 會讓每個 held 列一建立就過期，下一個 acquire 把它收掉，`max_concurrent` 形同虛設。
pub const MIN_BUILD_LEASE_TTL_SECS: u64 = 10;
/// 上限（#639）：`u64::MAX` 用 `as i64` 會變成 -1，名額立刻過期；更大的秒數讓 `Duration`／`DateTime` 加法 panic。
pub const MAX_BUILD_LEASE_TTL_SECS: u64 = 24 * 60 * 60;

fn de_build_lease_ttl_secs<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    use serde::de::Error as _;

    let secs = u64::deserialize(d)?;
    checked_build_lease_ttl(secs).map(|_| secs).map_err(D::Error::custom)
}

/// 設定檔的 TTL 轉成可以加到現在的秒數。超出 [`MIN_BUILD_LEASE_TTL_SECS`]..=[`MAX_BUILD_LEASE_TTL_SECS`] 就拒絕。
pub fn checked_build_lease_ttl(secs: u64) -> Result<i64, String> {
    if !(MIN_BUILD_LEASE_TTL_SECS..=MAX_BUILD_LEASE_TTL_SECS).contains(&secs) {
        return Err(format!(
            "[build] lease_ttl_secs must be {MIN_BUILD_LEASE_TTL_SECS}..={MAX_BUILD_LEASE_TTL_SECS}, got {secs}"
        ));
    }
    i64::try_from(secs).map_err(|_| format!("[build] lease_ttl_secs {secs} does not fit in i64"))
}

impl BuildCfg {
    /// 實際使用的租約 TTL（秒）。超出範圍是設定錯誤，呼叫端要拒絕，不能夾成負數。
    pub fn lease_ttl(&self) -> Result<i64, String> {
        checked_build_lease_ttl(self.lease_ttl_secs)
    }

    /// 實際交給 shim 的測試執行緒數：`0`＝不設，其他夾在 [`MAX_BUILD_TEST_THREADS`] 以內。
    pub fn test_threads(&self) -> usize {
        self.test_threads.min(MAX_BUILD_TEST_THREADS)
    }

    /// 環境變數覆寫（`AM_BUILD_MAX_CONCURRENT`）；看不懂、0 一律不採用，回設定檔的值，設定檔也離譜才回預設。
    pub fn max_concurrent(&self) -> usize {
        resolve_build_max_concurrent(std::env::var("AM_BUILD_MAX_CONCURRENT").ok().as_deref(), self.max_concurrent)
    }
}

fn resolve_build_max_concurrent(env: Option<&str>, file: usize) -> usize {
    let sane = |n: usize| n >= MIN_BUILD_MAX_CONCURRENT;
    match env.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) if sane(n) => n,
        _ if sane(file) => file,
        _ => default_build_max_concurrent(),
    }
}

#[cfg(test)]
mod build_cfg_tests {
    use super::*;

    #[tokio::test]
    async fn loading_out_of_range_build_lease_ttl_is_rejected_with_the_valid_range() {
        let dir = super::test_support::track(std::env::temp_dir().join(format!("am-build-lease-ttl-load-{}", super::test_support::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut failures = Vec::new();
        for bad in [0_u64, u64::MAX, 10_000_000_000_000] {
            std::fs::write(&path, format!("[build]\nlease_ttl_secs = {bad}\n")).unwrap();
            match ConfigStore::load(path.clone()).await {
                Err(err) => {
                    let detail = format!("{err:#}");
                    let clear = detail.contains("lease_ttl_secs")
                        && (detail.contains("10..=86400") || (bad == u64::MAX && detail.contains("number too large")));
                    if !clear {
                        failures.push(format!("{bad}: rejected without a clear range/key error: {detail}"));
                    }
                }
                Ok(_) => failures.push(format!("{bad}: invalid TTL was accepted while loading config")),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// 稽核：`[build]` 的三個門檻只有手改 `config.toml` 一條路，而 `get()` 是記憶體快照——改了不生效。
    #[tokio::test]
    async fn a_hand_edited_build_section_is_picked_up_without_a_restart() {
        let dir = super::test_support::track(std::env::temp_dir().join(format!("am-build-fresh-{}", super::test_support::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[server]\nherdr_session = 'one'\n\n[build]\nmax_concurrent = 2\n").unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();
        assert_eq!(store.build_fresh().await.max_concurrent, 2);

        let bump = |text: &str| {
            std::fs::write(&path, text).unwrap();
            // 不靠檔案系統的 mtime 解析度：每次都往後推一秒。
            let at = std::time::SystemTime::now() + std::time::Duration::from_secs(bump_secs());
            std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(at).unwrap();
        };
        fn bump_secs() -> u64 {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        }

        bump("[server]\nherdr_session = 'two'\n\n[build]\nmax_concurrent = 4\ncargo_jobs = 3\ntest_threads = 6\n");
        let fresh = store.build_fresh().await;
        assert_eq!((fresh.max_concurrent, fresh.cargo_jobs, fresh.test_threads), (4, 3, 6), "手改的 [build] 讀得到");
        assert_eq!(store.get().await.build.max_concurrent, 4, "記憶體裡的 [build] 一起換");
        assert_eq!(store.get().await.server.herdr_session, "one", "其他段不熱載入（要連著投影一起處理）");

        // 打錯字／離譜的值：保留原本的，不退回預設。
        bump("[build]\nlease_ttl_secs = 0\n");
        assert_eq!(store.build_fresh().await.max_concurrent, 4);
        bump("[build\nmax_concurrent = 9\n");
        assert_eq!(store.build_fresh().await.max_concurrent, 4);
        // 改回來就跟上。
        bump("[build]\nmax_concurrent = 1\n");
        assert_eq!(store.build_fresh().await.max_concurrent, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 0 不是「停用排程」，是「誰都拿不到名額」——跟 `idle_close_secs` 同一條規矩：離譜的值回預設，不照單全收。
    #[test]
    fn a_zero_or_unreadable_override_falls_back_instead_of_locking_everyone_out() {
        let default = default_build_max_concurrent();
        assert_eq!(resolve_build_max_concurrent(None, 3), 3);
        assert_eq!(resolve_build_max_concurrent(Some("5"), 3), 5, "環境變數覆寫");
        for bad in ["0", "-1", "abc", ""] {
            assert_eq!(resolve_build_max_concurrent(Some(bad), 3), 3, "{bad:?} 不採用，回設定檔的值");
            assert_eq!(resolve_build_max_concurrent(Some(bad), 0), default, "{bad:?}＋設定檔 0 → 預設");
        }
    }
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
pub const BOT_NAME_RE: &str = "1–32 個字，不可含 @ , : ;，空白只能單一個、夾在中間";

/// SPEC §2, §12. Also the herdr `agent.start` `kind` value.
///
/// `agy` = Antigravity CLI（Google；2026-06-18 起取代對個人用戶停服的 Gemini CLI）。可執行檔就叫 `agy`，herdr 的 agent kind 同名。
pub const KINDS: [&str; 4] = ["claude", "codex", "grok", "agy"];

/// grok `xhigh` needs grok-4.6+ (verified); per-model lists may be narrower, `effort_checked` drops rejects.
/// claude `--effort` since 2.1 (verified 2.1.263); an unknown value only warns and falls back to default.
pub fn efforts_for_kind(kind: &str) -> &'static [&'static str] {
    match kind {
        "claude" => &["low", "medium", "high", "xhigh", "max"],
        "grok" => &["low", "medium", "high", "xhigh"],
        "codex" => &["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"],
        // agy：effort 已經包在模型 slug 裡（`gemini-3.8-flash-high`／`-low`…），`--effort` 旗標留到第二階段。
        "agy" => &[],
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

/// 名字中間可以有單一個半形空白（2026-09-19 使用者：「bot name should be able to include space」）；
/// 頭尾空白、連續空白、tab／換行照樣不行。herdr 的 agent 名字是另外從 bot id 算的（`agent_name`），不受影響；
/// 群組 `@` 靠 `group::spaced_member_at` 認整個名字。
/// 看不見或會改變顯示方向的字元：零寬、方向控制、BOM。放進名稱或標籤，畫面上看起來一樣、實際是另一個字串（或把後面的字倒過來）。
/// 純字元規則，住在 `config`（`bot_input` 的檢查與 bot 名稱規則共用這一份）。
pub fn is_invisible_format_char(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

pub fn valid_bot_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=32).contains(&n)
        && !name.starts_with(' ')
        && !name.ends_with(' ')
        && !name.contains("  ")
        && !name.chars().any(|c| (c.is_whitespace() && c != ' ') || c.is_control() || is_invisible_format_char(c) || matches!(c, '@' | ',' | ':' | ';'))
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
///
/// herdr names are at most 32 characters. Children are `<parent>-<suffix>` in the same budget
/// (#665), so a top-level name stops at 24 and leaves 8 characters (`-` plus a 7-character suffix).
pub fn agent_name(project_label: &str, bot_id: &str) -> String {
    const MAX: usize = 24;
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

/// Drop redundant project bot identities from an adopted child's nickname.
///
/// A child command can itself pass a full `<project slug>-<bot id tail>` name as its suffix.
/// Herdr still adds the actual parent's name, so the child then appears as
/// `<actual parent>-<other project bot>-<short suffix>`. The second identity is not useful as
/// the bot's display name. This only formats the nickname; it does not change herdr names.
pub fn short_child_name(project_label: &str, name: &str) -> String {
    const HERDR_NAME_MAX: usize = 24;
    const BOT_ID_TAIL: usize = 6;

    let mut slug = label_slug(project_label);
    let room = HERDR_NAME_MAX.saturating_sub(BOT_ID_TAIL + 1);
    if slug.len() > room {
        slug.truncate(room);
        slug = slug.trim_end_matches('-').to_string();
    }
    if slug.is_empty() {
        slug = "b".to_string();
    }
    let prefix = format!("{slug}-");

    let mut short = name;
    loop {
        let Some(after_slug) = short.strip_prefix(&prefix) else { break };
        let Some(id_tail) = after_slug.get(..BOT_ID_TAIL) else { break };
        if !id_tail.bytes().all(|c| c.is_ascii_alphanumeric()) {
            break;
        }
        let Some(suffix) = after_slug.get(BOT_ID_TAIL + 1..).filter(|_| after_slug.as_bytes().get(BOT_ID_TAIL) == Some(&b'-')) else {
            break;
        };
        if !valid_bot_name(suffix) || suffix.len() >= short.len() {
            break;
        }
        short = suffix;
    }
    short.to_string()
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

/// `ssh` 目標與 `herdr_session` 的形狀檢查（API 與手改 `config.toml` 共用）。`ssh` 原樣成為 `ssh <opts> <目標> …` 的一個 argv：
/// 開頭是 `-` 會被 ssh 當成選項（`-oProxyCommand=…` 在本機執行命令），空白／控制字元永遠不是合法的主機目標。
/// `herdr_session` 會被拼進遠端的 session 路徑、launchd label 與 plist，限制在 herdr session 名字該有的字元。
/// 呼叫端先 trim。
pub fn host_target_problem(ssh: &str, session: &str) -> Option<String> {
    // `user@-oProxyCommand=…`：`@` 後面以 `-` 開頭的主機名，較舊的 ssh 一樣會當成選項。
    let host_part = ssh.rsplit_once('@').map_or(ssh, |(_, h)| h);
    if ssh.is_empty() || ssh.starts_with('-') || host_part.starts_with('-') || ssh.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Some("ssh target must be a host (or user@host / ssh_config alias): no leading `-`, whitespace or control characters".into());
    }
    let first = session.chars().next();
    if session.len() > 64
        || !first.is_some_and(|c| c.is_ascii_alphanumeric())
        || !session.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Some("herdr_session must be 1-64 characters of [A-Za-z0-9._-], starting with a letter or digit".into());
    }
    None
}

/// `ssh_opts` 白名單鍵（`-o Key=Value`）。刻意只放連線／驗證／逾時這類：`ProxyCommand`、`LocalCommand`、`KnownHostsCommand`、
/// `Include`、`Match`、`*Forward`、`SetEnv` 都不在內——它們會在**跑 daemon 的這台機器**上執行命令或開通道。
const SSH_OPT_KEYS: &[&str] = &[
    "connecttimeout", "connectionattempts", "stricthostkeychecking", "userknownhostsfile", "globalknownhostsfile", "identityfile",
    "identitiesonly", "serveraliveinterval", "serveralivecountmax", "hostkeyalgorithms", "pubkeyacceptedalgorithms",
    "pubkeyauthentication", "passwordauthentication", "kbdinteractiveauthentication", "compression", "addressfamily", "loglevel",
    "user", "hostkeyalias", "checkhostip", "updatehostkeys", "tcpkeepalive",
];

/// `ssh_opts` 是「原樣附加到每個 ssh 指令」的 argv。放行的形狀只有：`-i <檔>`、`-o Key=Value`／`-oKey=Value`（Key 在白名單內）、`-4`／`-6`／`-q`。
/// 其餘（`-F`、`-J`、`-L`／`-R`、`-p`、裸字串…）一律不收——埠用 `ssh_port`；要跳板請寫進 ssh_config 別名。
/// 值不得以 `-` 開頭、不得含空白以外的控制字元、換行。回 `Some(原因)`。
pub fn ssh_opts_problem(opts: &[String]) -> Option<String> {
    let bad = |why: String| Some(format!("ssh_opts: {why}"));
    let clean = |v: &str| !v.is_empty() && !v.starts_with('-') && !v.chars().any(|c| c.is_control());
    let mut it = opts.iter().map(String::as_str);
    while let Some(a) = it.next() {
        match a {
            "-4" | "-6" | "-q" => {}
            "-i" => match it.next() {
                Some(v) if clean(v) => {}
                _ => return bad("`-i` needs a key file path (not starting with `-`)".into()),
            },
            "-o" => match it.next() {
                Some(kv) => {
                    if let Some(why) = ssh_option_problem(kv) {
                        return bad(why);
                    }
                }
                None => return bad("`-o` needs a Key=Value".into()),
            },
            _ if a.starts_with("-o") && a.len() > 2 => {
                if let Some(why) = ssh_option_problem(&a[2..]) {
                    return bad(why);
                }
            }
            other => return bad(format!("`{}` is not an allowed ssh option (allowed: -i <file>, -o <Key>=<Value>, -4, -6, -q)", other.chars().take(40).collect::<String>())),
        }
    }
    None
}

fn ssh_option_problem(kv: &str) -> Option<String> {
    // 允許 `Key=Value`；ssh 也接受 `Key Value`，一律先當成同一個東西檢查（Key 就是第一個 `=` 或空白前）。
    // 非半形空白的空白（全形空白、NBSP…）ssh 不認作分隔，放行只會得到 Bad configuration option，這裡直接拒絕。
    if kv.chars().any(|c| c.is_control() || (c.is_whitespace() && c != ' ')) {
        return Some("an -o value contains a control character, newline or non-ASCII whitespace".into());
    }
    // 分隔字元可能是多位元組（全形空白 3 bytes）：用 char_indices 取位元組位置，再跳過整個字元。
    let (key, value) = match kv.char_indices().find(|(_, c)| *c == '=' || c.is_whitespace()) {
        Some((i, c)) => (&kv[..i], kv[i + c.len_utf8()..].trim()),
        None => (kv, ""),
    };
    if !SSH_OPT_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
        return Some(format!("`-o {}` is not an allowed ssh option key (it could run a command or open a tunnel)", key.chars().take(40).collect::<String>()));
    }
    if value.is_empty() || value.starts_with('-') {
        return Some(format!("`-o {key}` needs a value that does not start with `-`"));
    }
    None
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

/// `ConfigStore` 寫入流程裡「不屬於設定本身」的那幾步：落盤前的投影驗證、與寫入／外部改動的稽核。
/// ConfigStore 只做 parse／atomic write，順序與錯誤處理在它這裡，**內容**由注入的實作決定
/// （`projection::validate`、`config_audit::log_*` 在 `app_ports_p2.rs` 組起來），所以 config 不依賴它們。
///
/// 全是同步方法：投影驗證是純函式、稽核只寫 log，都不碰 DB，也就不在持鎖期間 await 別的東西。
pub trait ConfigChangeHooks: Send + Sync {
    /// 落盤**之前**驗整份 `next` 投影出去會不會被擋（issue #73）；失敗的原因原樣往外傳。
    fn validate_projection(&self, next: &ConfigFile) -> Result<()>;
    /// 重讀時發現檔案內容跟記憶體那份不同＝別人改的（issue #406）。
    fn audit_external_change(&self, at: &'static std::panic::Location<'static>, path: &Path, old: &ConfigFile, new: &ConfigFile);
    /// 重讀時 mtime 變了、內容沒變。
    fn audit_reload_unchanged(&self, at: &'static std::panic::Location<'static>, path: &Path);
    /// daemon 自己寫了檔（`old` 是寫之前記憶體裡的那份）。
    fn audit_write(&self, at: &'static std::panic::Location<'static>, path: &Path, old: &ConfigFile, new: &ConfigFile);
}

pub struct ConfigStore {
    pub path: PathBuf,
    inner: tokio::sync::Mutex<Loaded>,
    hooks: std::sync::Arc<dyn ConfigChangeHooks>,
}

struct Loaded {
    cfg: ConfigFile,
    mtime: Option<SystemTime>,
    hot_fingerprint: Option<[u8; 32]>,
    hot_read_error: Option<(Option<SystemTime>, String)>,
}

impl ConfigStore {
    /// 帶著注入的 [`ConfigChangeHooks`] 載入。daemon 一律走 `projection::app_ports_p2::load_config(path)`（掛 daemon 自己的投影驗證與稽核）。
    pub async fn load_with_hooks(path: PathBuf, hooks: std::sync::Arc<dyn ConfigChangeHooks>) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let (cfg, mtime, hot_fingerprint) = read_file(&path)?;
        Ok(Self {
            path,
            inner: tokio::sync::Mutex::new(Loaded { cfg, mtime, hot_fingerprint: Some(hot_fingerprint), hot_read_error: None }),
            hooks,
        })
    }

    pub async fn get(&self) -> ConfigFile {
        self.inner.lock().await.cfg.clone()
    }

    /// `[build]` 的最新值。`max_concurrent`／`cargo_jobs`／`lease_ttl_secs` 沒有任何 API 寫得進去，唯一的設定方式就是手改
    /// `config.toml`；而 [`Self::get`] 回的是記憶體那份，手改要等重啟、或下一次不相干的 API 寫入順手重讀才進得來——什麼時候生效
    /// 沒有人說得準。build scheduler 每次拿名額／續約／看狀態都走這支：檔案內容變了就重讀，**只**換 `[build]` 那一段
    /// （其他段要連著 TOML→SQLite 投影一起處理，仍然只在啟動與 [`Self::update`] 時載入）。檔案讀不了或解析失敗（半寫、打錯字）
    /// 就保留記憶體裡原本的值並記一次 WARN（同一份壞內容不重複報）。
    pub async fn build_fresh(&self) -> BuildCfg {
        let mut g = self.inner.lock().await;
        self.refresh_hot_sections(&mut g);
        g.cfg.build.clone()
    }

    /// `[agents]` 的最新值（bot 啟動時讀指示檔的設定）。跟 [`Self::build_fresh`] 同一條規矩：內容變了就重讀、只換 `[agents]`，
    /// 手改 `config.toml` 的 `[agents.projects]` 不必重啟 daemon（SPEC §6.5i：改完重啟 bot 就生效）。
    pub async fn agents_fresh(&self) -> AgentsCfg {
        let mut g = self.inner.lock().await;
        self.refresh_hot_sections(&mut g);
        g.cfg.agents.clone()
    }

    /// 檔案內容變了就重讀一次，只換可以熱換的區段（`[build]`、`[agents]`）。mtime 只用來判斷讀取錯誤是否改變；
    /// 內容指紋也會偵測相同 mtime 的原子替換，以及壞檔在相同 mtime 下被修好。
    fn refresh_hot_sections(&self, g: &mut Loaded) {
        let mtime = std::fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
        if mtime.is_none() {
            return;
        }
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                g.hot_read_error = None;
                text
            }
            Err(e) => {
                let error = format!("{e:#}");
                let changed = g.hot_read_error.as_ref().is_none_or(|(old_mtime, old_error)| *old_mtime != mtime || old_error != &error);
                if changed {
                    tracing::warn!(path = %self.path.display(), error = %error, "config.toml changed on disk but could not be read; keeping the in-memory [build]/[agents]");
                }
                g.hot_read_error = Some((mtime, error));
                return;
            }
        };
        let fingerprint = config_fingerprint(&text);
        if g.hot_fingerprint == Some(fingerprint) {
            g.mtime = mtime;
            return;
        }
        // Cache even malformed content so it warns once, while a later corrected file with the
        // same mtime still has a different fingerprint and gets another parse attempt.
        g.hot_fingerprint = Some(fingerprint);
        g.mtime = mtime;
        // 空檔是合法 TOML（全部預設），卻是寫到一半或截斷後的形狀。當成解析失敗，留下記憶體裡的區段。
        if text.trim().is_empty() {
            tracing::warn!(path = %self.path.display(), "config.toml changed on disk but is blank; keeping the in-memory [build]/[agents]");
            return;
        }
        match parse_config(&text).map(|(cfg, _)| cfg) {
            Ok(cfg) => {
                if cfg.build != g.cfg.build {
                    tracing::info!(path = %self.path.display(), before = ?g.cfg.build, after = ?cfg.build, "config.toml [build] changed on disk; using the new values");
                    g.cfg.build = cfg.build;
                }
                if cfg.agents != g.cfg.agents {
                    tracing::info!(path = %self.path.display(), "config.toml [agents] changed on disk; using the new values");
                    g.cfg.agents = cfg.agents;
                }
            }
            Err(e) => tracing::warn!(path = %self.path.display(), error = %format!("{e:#}"), "config.toml changed on disk but could not be parsed; keeping the in-memory [build]/[agents]"),
        }
    }

    /// `#[track_caller]`：寫入／重讀的 log 要記是哪一段程式叫的（issue #406）。async fn 不能掛
    /// track_caller，所以是回 future 的一般函式，呼叫端照樣 `.await`。
    #[track_caller]
    pub fn update<'a, F, T>(&'a self, f: F) -> impl std::future::Future<Output = Result<T>> + 'a
    where
        F: FnOnce(&mut ConfigFile) -> Result<T> + 'a,
        T: 'a,
    {
        let at = std::panic::Location::caller();
        async move { self.update_guarded_at(at, f, |_next| Ok(())).await }
    }

    /// 跟 [`Self::update`] 一樣的「重讀 → 套用 → 驗證 → 寫入」，多一道 `guard`：驗證過的 `next`
    /// 落盤之前再跑一次額外檢查，驗不過一樣直接回錯誤、檔案一個字都不動（issue #73 reopen）。
    ///
    /// 給需要 DB 才判得出來的規則用（`projection::update_and_project` 的大量軟刪閘門）：那類檢查得先在
    /// 呼叫端把 DB 快照查出來，再用同步的 `guard` 帶進來比對——`update_guarded_at` 本身不碰 DB，也不知道
    /// 什麼時候該問誰，只負責「套用與驗證都過了才寫檔」這個順序不能亂。
    ///
    /// `at`＝要記進 log 的呼叫位置（`projection` 的公開函式把自己的呼叫端傳下來，issue #406）。
    pub async fn update_guarded_at<F, T, G>(&self, at: &'static std::panic::Location<'static>, f: F, guard: G) -> Result<T>
    where
        F: FnOnce(&mut ConfigFile) -> Result<T>,
        G: FnOnce(&ConfigFile) -> Result<()>,
    {
        let mut g = self.inner.lock().await;
        if self.path.exists() {
            // Always merge from the current file: filesystems can preserve/coarsen mtimes, so
            // comparing metadata alone can miss a completed external atomic replacement.
            let on_disk = std::fs::read_to_string(&self.path)
                .context("config.toml changed on disk and could not be re-read")?;
            if on_disk.trim().is_empty() {
                anyhow::bail!("config.toml changed on disk but is blank; keeping the in-memory config");
            }
            let (cfg, mtime, hot_fingerprint) = read_file(&self.path)
                .context("config.toml changed on disk and could not be re-read")?;
            // daemon 自己寫完會把記憶體那份換成寫出去的內容，所以內容對不上＝別人改的（issue #406）。
            if cfg != g.cfg {
                self.hooks.audit_external_change(at, &self.path, &g.cfg, &cfg);
            } else if mtime != g.mtime {
                self.hooks.audit_reload_unchanged(at, &self.path);
            }
            g.cfg = cfg;
            g.mtime = mtime;
            g.hot_fingerprint = Some(hot_fingerprint);
            g.hot_read_error = None;
        }
        let mut next = g.cfg.clone();
        let out = f(&mut next)?;
        // issue #73：**落盤之前**先問「改完之後這份 config 投影出去會不會被擋」。以前是先寫檔再投影，
        // 一筆會被擋下的修改等於把 TOML 改壞了才回錯誤——現場已經變了，daemon 下次啟動才爆。
        // 這裡失敗就直接 `?` 出去：`next` 被丟掉，`g.cfg` 沒動，檔案一個字都沒寫。
        //
        // 不論這次改了什麼都驗整份：一來每個 mutation 走的都是這支，規則只有一份；二來 config 已經壞掉時
        // 本來就不該再往上疊寫。
        // 失敗是 `ConfigInvalid`：原因留在最前面（呼叫端與測試都在看它），型別讓 API 分得出這不是上游壞掉。
        self.hooks.validate_projection(&next)?;
        // 純驗證過了才問需要 DB 的那一類（同一條理由：驗不過就不寫，guard 也不例外）。
        guard(&next)?;
        // 沒變就不寫（issue #38）；有變的話 `write_atomic` 就地改、保留註解與未知鍵。
        if next != g.cfg {
            write_atomic(&self.path, &next)?;
            g.mtime = std::fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
            g.hot_fingerprint = std::fs::read_to_string(&self.path).ok().map(|text| config_fingerprint(&text));
            g.hot_read_error = None;
            self.hooks.audit_write(at, &self.path, &g.cfg, &next);
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

/// daemon 自己以前寫進 config.toml、後來移除的鍵（舊檔裡還留著）：照樣忽略，但不是「打錯字或新版欄位」，不必每次開機都警告。
/// 目前只有 bot 層級的 `instruction_files`（2026-10-02 移除，見 `api.rs` 的 `a_leftover_instruction_files_key_in_config_toml_is_ignored`）。
fn is_retired_key(path: &str) -> bool {
    let parts: Vec<&str> = path.split('.').collect();
    matches!(parts.as_slice(), ["projects", i, "bots", j, "instruction_files"] if i.parse::<usize>().is_ok() && j.parse::<usize>().is_ok())
}

fn read_file(path: &Path) -> Result<(ConfigFile, Option<SystemTime>, [u8; 32])> {
    if !path.exists() {
        let cfg = ConfigFile::default();
        write_atomic(path, &cfg)?;
        let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        let text = std::fs::read_to_string(path).with_context(|| format!("read {} after creating it", path.display()))?;
        return Ok((cfg, mtime, config_fingerprint(&text)));
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (cfg, unknown) = parse_config(&text).with_context(|| format!("parse {}", path.display()))?;
    for key in unknown.into_iter().filter(|k| !is_retired_key(k)) {
        tracing::warn!("{}: unknown key `{key}` is ignored (typo? or a newer daemon's field)", path.display());
    }
    let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    Ok((cfg, mtime, config_fingerprint(&text)))
}

fn config_fingerprint(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

/// 寫回 `config.toml`：只改動被改的值，使用者手寫的註解、排版與（打錯字或新版才有的）未知鍵都留著（[`render_preserving`]）。
/// 檔案還不存在，或舊內容不是合法 TOML（沒辦法保留什麼），才整份重新序列化。
///
/// 寫的是 `path` **解開 symlink 之後**的那個檔（issue #506 的鄰居 #507）：`startup::normalize_config_file`
/// 刻意不 canonicalize 檔名那一段，因為 `config.toml` 常是指到 dotfiles 的 symlink，而資料目錄要留在
/// 連結所在的目錄。但 `rename(2)` 換掉的是連結本身，所以照著 `path` 寫等於第一次寫入就把連結吃掉：
/// 連結變成一般檔、dotfiles 那一份停在舊內容，而 `git status` 什麼都看不出來。
/// 暫存檔跟著搬到目標所在目錄，順便讓「目標在另一個檔案系統」不會 rename EXDEV。
///
/// 持久性：暫存檔 `fsync` 之後才 `rename`，再 `fsync` 所在目錄——否則寫到一半斷電／被砍，rename 先落地、內容還在快取裡，
/// 重開機看到的是一個空檔，而 daemon 看到空 config 會把它當成全新安裝。權限：暫存檔一開始就是 0600（不是先照 umask 寫出
/// 0644 的內容、再 chmod），再套上原檔的 mode；原檔不存在（第一次寫預設設定）就維持 0600。
pub fn write_atomic(path: &Path, cfg: &ConfigFile) -> Result<()> {
    use std::io::Write as _;
    // 檔案還不存在（`read_file` 第一次寫預設設定）時 canonicalize 會失敗：那就照原路徑建。
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = match std::fs::read_to_string(&target) {
        Ok(old) => render_preserving(&old, cfg)?,
        Err(_) => toml::to_string_pretty(cfg)?,
    };
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let tmp = target.with_extension(format!("toml.{}.{}.tmp", std::process::id(), nonce));
    let result = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        // rename 會把暫存檔的權限當成新檔的權限：原檔被 chmod 過（0600、0640…）就沿用，不能被悄悄放寬或收窄
        // （跟 `trust.rs::write_atomic_preserving_mode` 同一條規矩）。
        if let Ok(md) = std::fs::metadata(&target) {
            let _ = f.set_permissions(md.permissions());
        }
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &target)?;
        // 目錄項目的更動也要落盤；有些檔案系統不支援對目錄 fsync，那就算了（檔案本身已經 sync 過）。
        if let Some(dir) = target.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(())
}

/// 把 `next` 套進 `old_text`（使用者的檔）：以舊文件為底就地改，所以註解、空行、排版、鍵的順序都留著。
///
/// - 兩邊都有的值：一樣就不碰（連引號風格都保留），不一樣才換掉並保留前後的註解；
/// - `next` 有、舊檔沒有：加在該表格尾端；`next` 沒有、舊檔有：拿掉——**除非**它是未知鍵（`parse_config` 報告的、結構不認得的鍵：
///   打錯字或新版 daemon 的欄位），那種原樣留著。因為 `skip_serializing_if` 而被省略的已知欄位（例如回到預設的 `[codex]`）不在未知清單裡，會被拿掉；
/// - `[[projects]]`、`[[projects.bots]]`、`[[hosts]]`、`[[identities]]` 這類陣列表格依 `id`／`name`／`label` 認人而不是靠位置，
///   刪掉或重排一顆 bot 不會讓別顆 bot 的註解與未知鍵錯位。
///
/// 保險：合併後的文字必須重新解析成**完全等於** `next`，否則退回整份重新序列化（不留半吊子的檔）。
fn render_preserving(old_text: &str, next: &ConfigFile) -> Result<String> {
    let plain = toml::to_string_pretty(next)?;
    let Ok(mut doc) = old_text.parse::<toml_edit::DocumentMut>() else { return Ok(plain) };
    let want: toml_edit::DocumentMut = plain.parse().context("re-serialized config is not valid TOML")?;
    let unknown: std::collections::HashSet<String> = parse_config(old_text).map(|(_, u)| u.into_iter().collect()).unwrap_or_default();
    merge_table(doc.as_table_mut(), want.as_table(), "", &unknown);
    let merged = doc.to_string();
    match parse_config(&merged) {
        Ok((cfg, _)) if cfg == *next => Ok(merged),
        _ => {
            tracing::warn!("config.toml: could not apply the change in place; rewrote the whole file (comments and unknown keys are lost)");
            Ok(plain)
        }
    }
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() { key.to_string() } else { format!("{prefix}.{key}") }
}

fn merge_table(old: &mut toml_edit::Table, new: &toml_edit::Table, path: &str, unknown: &std::collections::HashSet<String>) {
    let stale: Vec<String> = old
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !new.contains_key(k) && !unknown.contains(&join_path(path, k)))
        .collect();
    for k in stale {
        old.remove(&k);
    }
    for (k, n) in new.iter() {
        let p = join_path(path, k);
        match old.get_mut(k) {
            Some(o) => merge_item(o, n, &p, unknown),
            None => {
                old.insert(k, without_positions(n));
            }
        }
    }
}

fn merge_item(old: &mut toml_edit::Item, new: &toml_edit::Item, path: &str, unknown: &std::collections::HashSet<String>) {
    use toml_edit::Item;
    match (&mut *old, new) {
        (Item::Table(o), Item::Table(n)) => merge_table(o, n, path, unknown),
        (Item::ArrayOfTables(o), Item::ArrayOfTables(n)) => merge_array_of_tables(o, n, path, unknown),
        (Item::Value(o), Item::Value(n)) => {
            if !same_value(o, n) {
                let decor = o.decor().clone();
                let mut v = n.clone();
                *v.decor_mut() = decor;
                *o = v;
            }
        }
        _ => {
            *old = without_positions(new);
        }
    }
}

/// 語意相同就不動它（使用者寫 `'x'` 而序列化器寫 `"x"` 不算改動）；陣列與 inline table 比去掉前後空白的文字。
fn same_value(a: &toml_edit::Value, b: &toml_edit::Value) -> bool {
    use toml_edit::Value::*;
    match (a, b) {
        (String(x), String(y)) => x.value() == y.value(),
        (Integer(x), Integer(y)) => x.value() == y.value(),
        (Boolean(x), Boolean(y)) => x.value() == y.value(),
        (Float(x), Float(y)) => x.value() == y.value(),
        _ => {
            let strip = |v: &toml_edit::Value| {
                let mut v = v.clone();
                *v.decor_mut() = toml_edit::Decor::default();
                v.to_string()
            };
            strip(a) == strip(b)
        }
    }
}

/// 依序試這幾組欄位找舊檔裡的同一個元素：`id`、（`name`＋`host`）、（`label`＋`path`）、`name`、`label`。每組要兩邊都有而且都相等。
fn match_old_element(old: &toml_edit::ArrayOfTables, used: &[bool], new: &toml_edit::Table) -> Option<usize> {
    const KEYS: &[&[&str]] = &[&["id"], &["name", "host"], &["label", "path"], &["name"], &["label"]];
    let field = |t: &toml_edit::Table, k: &str| t.get(k).and_then(|i| i.as_str()).map(str::to_owned);
    for keys in KEYS {
        let want: Option<Vec<String>> = keys.iter().map(|k| field(new, k)).collect();
        let Some(want) = want else { continue };
        for (i, t) in old.iter().enumerate() {
            if used[i] {
                continue;
            }
            let have: Option<Vec<String>> = keys.iter().map(|k| field(t, k)).collect();
            if have.as_ref() == Some(&want) {
                return Some(i);
            }
        }
    }
    None
}

fn merge_array_of_tables(old: &mut toml_edit::ArrayOfTables, new: &toml_edit::ArrayOfTables, path: &str, unknown: &std::collections::HashSet<String>) {
    let mut used = vec![false; old.len()];
    let mut merged = toml_edit::ArrayOfTables::new();
    for n in new.iter() {
        match match_old_element(old, &used, n) {
            Some(i) => {
                used[i] = true;
                let mut t = old.get(i).expect("matched index").clone();
                // 未知鍵的路徑用**舊檔**的索引（`parse_config` 就是這樣報的）。
                merge_table(&mut t, n, &format!("{path}.{i}"), unknown);
                merged.push(t);
            }
            None => {
                merged.push(rebuilt_table(n));
            }
        }
    }
    *old = merged;
}

/// 從另一份文件複製過來的表格帶著那份文件的顯示順序（`position`），toml_edit 又不給清掉：重建一份沒有 position 的（內容與
/// 表格旗標照舊），才會接在目標的最後面，不會插到別的表格中間。新加的內容來自序列化器，沒有註解可丟。
fn without_positions(item: &toml_edit::Item) -> toml_edit::Item {
    match item {
        toml_edit::Item::Table(t) => toml_edit::Item::Table(rebuilt_table(t)),
        toml_edit::Item::ArrayOfTables(a) => {
            let mut out = toml_edit::ArrayOfTables::new();
            a.iter().for_each(|t| out.push(rebuilt_table(t)));
            toml_edit::Item::ArrayOfTables(out)
        }
        other => other.clone(),
    }
}

fn rebuilt_table(t: &toml_edit::Table) -> toml_edit::Table {
    let mut out = toml_edit::Table::new();
    out.set_implicit(t.is_implicit());
    out.set_dotted(t.is_dotted());
    *out.decor_mut() = t.decor().clone();
    for (k, v) in t.iter() {
        out.insert(k, without_positions(v));
    }
    out
}

#[cfg(test)]
mod v40_tests {
    use super::{attach_command, normalize_effort, HostCfg};

    fn host(port: u16) -> HostCfg {
        HostCfg {
            shared_session: false,
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
        assert!(normalize_effort("agy", Some("high")).is_err(), "agy 的 effort 在模型 slug 裡，沒有獨立旗標");
        assert_eq!(normalize_effort("agy", None).unwrap(), None);
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
    use super::{agent_name, short_child_name, valid_bot_name};

    #[test]
    fn prefix_plus_id_tail() {
        assert_eq!(agent_name("agents-manager", "01M1S2SQPSYMQ8B1VQ50R963B9"), "agents-manager-r963b9");
        assert_eq!(agent_name("PowerTech Hub", "01M1S2SQPSYMQ8B1VQ50R963B9"), "powertech-hub-r963b9");
        assert_eq!(agent_name("2026 專案!!", "abcdef"), "p2026-abcdef");
        assert_eq!(agent_name("---", "abcdef"), "b-abcdef");
        let n = agent_name("a-very-long-project-label-indeed-and-more", "01M1S2SQPSYMQ8B1VQ50R963B9");
        assert!(n.len() <= 24, "{n}");
        assert!(n.ends_with("-r963b9"));
        assert!(32 - n.len() >= 8, "子 agent 至少留得下 `-` 與 7 字尾碼：{n}");
    }

    #[test]
    fn short_child_name_drops_redundant_project_agent_prefixes() {
        assert_eq!(short_child_name("智選hub", "hub-dgs9j9-dev"), "dev");
        assert_eq!(short_child_name("智選hub", "hub-dgs9j9-sheet"), "sheet");
        assert_eq!(short_child_name("智選hub", "hub-dgs9j9-code-review"), "code-review");
        assert_eq!(short_child_name("智選hub", "hub-dgs9j9-hub-kytpg9-sheet"), "sheet");
        assert_eq!(short_child_name("智選hub", "hub-dgs9j9"), "hub-dgs9j9", "bare bot names are not shortened");
        assert_eq!(short_child_name("智選hub", "feature-sheet"), "feature-sheet", "ordinary nicknames are unchanged");

        let long_label = "a-very-long-project-label-indeed-and-more";
        let prefix = agent_name(long_label, "01M1S2SQPSYMQ8B1VQ50R963B9");
        let suffix = format!("{prefix}-sheet");
        assert_eq!(short_child_name(long_label, &suffix), "sheet", "the generated label slug is truncated to herdr's parent-name budget");
    }

    #[test]
    fn nicknames_are_free_text_without_separators() {
        assert!(valid_bot_name("am-claude"));
        assert!(valid_bot_name("小幫手"));
        assert!(valid_bot_name("Reviewer_2"));
        assert!(!valid_bot_name(""));
        assert!(valid_bot_name("has space"));
        assert!(valid_bot_name("my bot 2"));
        assert!(!valid_bot_name(" lead"));
        assert!(!valid_bot_name("trail "));
        assert!(!valid_bot_name("two  spaces"));
        assert!(!valid_bot_name("tab\there"));
        assert!(!valid_bot_name("new\nline"));
        assert!(!valid_bot_name("esc\u{1b}[31m"), "終端機控制字元會出現在 pane 標題與畫面上");
        assert!(!valid_bot_name("bell\u{7}"));
        assert!(!valid_bot_name("rtl\u{202e}gnp"), "方向控制字元");
        assert!(!valid_bot_name("zero\u{200b}width"));
        assert!(!valid_bot_name(" "));
        assert!(!valid_bot_name("a@b"));
        assert!(!valid_bot_name(&"x".repeat(33)));
    }
}

#[cfg(test)]
mod issue38_tests {
    use super::{is_retired_key, parse_config, ConfigFile, ConfigStore};

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

    /// 自己以前寫過、後來移除的鍵不再每次開機警告成「打錯字」；真的打錯字的鍵照舊警告。
    #[test]
    fn a_retired_key_is_not_warned_about_but_a_typo_still_is() {
        let leftover = "[[projects]]\npath = \"/tmp\"\nlabel = \"p\"\nid = \"01P\"\n[[projects.bots]]\nid = \"01B\"\nname = \"a\"\nkind = \"claude\"\ninstruction_files = \"managed-only\"\nauto_start = true\n";
        let keys = unknown_keys(leftover);
        assert!(keys.contains(&"projects.0.bots.0.instruction_files".to_string()), "serde 還是回報它被忽略");
        let warned: Vec<_> = keys.iter().filter(|k| !is_retired_key(k)).collect();
        assert_eq!(warned, ["projects.0.bots.0.auto_start"]);
        assert!(!is_retired_key("projects.0.instruction_files"));
        assert!(!is_retired_key("hosts.0.bots.0.instruction_files"));
    }

    #[test]
    fn unknown_keys_are_still_parsed_leniently() {
        let cfg: ConfigFile = toml::from_str(SAMPLE).unwrap();
        assert!(!cfg.projects[0].bots[0].autostart, "typo'd key must not silently apply");
    }

    #[tokio::test]
    async fn noop_update_leaves_the_file_byte_identical() {
        let dir = super::test_support::track(std::env::temp_dir().join(format!("am-config-issue38-{}", super::test_support::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, SAMPLE).unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();

        let dirty = store.update(|_cfg| Ok(false)).await.unwrap();
        assert!(!dirty);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SAMPLE, "comments and typos survive a no-op update");

        // 真的有改：只動被改的那個值，使用者手寫的註解與（打錯字的）未知鍵原樣留著。
        store
            .update(|cfg| {
                cfg.projects[0].bots[0].autostart = true;
                Ok(true)
            })
            .await
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("autostart = true"));
        assert!(text.contains("# top comment"), "{text}");
        assert!(text.contains("auto_start = true   # typo for autostart"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod issue28_tests {
    use super::{ConfigFile, ConfigStore};
    use std::path::Path;
    use std::time::Duration;

    fn temp_config() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = super::test_support::track(std::env::temp_dir().join(format!("am-config-issue28-{}", super::test_support::ulid())));
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
    async fn update_preserves_external_bots_even_when_the_file_mtime_did_not_change() {
        let (dir, path) = temp_config();
        std::fs::write(
            &path,
            "[server]\nlisten = '127.0.0.1:7788'\n\n[[projects]]\nid = 'p1'\npath = '/tmp/one'\nlabel = 'one'\nhost = 'local'\n\n[[projects.bots]]\nid = 'b1'\nname = 'one'\nkind = 'claude'\n",
        )
        .unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();
        let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        std::fs::write(
            &path,
            "[server]\nlisten = '127.0.0.1:7788'\n\n[[projects]]\nid = 'p1'\npath = '/tmp/one'\nlabel = 'one'\nhost = 'local'\n\n[[projects.bots]]\nid = 'b1'\nname = 'one'\nkind = 'claude'\n\n[[projects.bots]]\nid = 'b2'\nname = 'build'\nkind = 'claude'\n",
        )
        .unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(original_mtime).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), original_mtime, "fixture models a stale mtime snapshot");

        store
            .update(|cfg| {
                cfg.server.herdr_session = "updated".into();
                Ok(())
            })
            .await
            .unwrap();

        let cfg = store.get().await;
        assert_eq!(cfg.server.herdr_session, "updated");
        assert_eq!(cfg.projects[0].bots.iter().map(|b| b.id.as_deref()).collect::<Vec<_>>(), vec![Some("b1"), Some("b2")]);
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

#[cfg(test)]
mod issue507_tests {
    use super::ConfigStore;

    /// issue #507：`~/.config/agents-manager/config.toml` 是指到 dotfiles 的 symlink 是本專案明示支援的
    /// 安裝方式（`startup::normalize_config_file` 的註解與 `a_symlinked_config_file_keeps_the_data_dir_where_the_link_is`），
    /// 但寫回是 `rename` 蓋過連結本身：第一次寫入連結就變成一般檔，dotfiles 那份停在舊內容。
    #[tokio::test]
    async fn writing_through_a_symlinked_config_updates_the_target_and_keeps_the_link() {
        let root = super::test_support::track(std::env::temp_dir().join(format!("am-config-symlink-{}", super::test_support::ulid())));
        let home = root.join("home");
        let dotfiles = root.join("dotfiles");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("am.toml");
        std::fs::write(&target, "[server]\nlisten = '127.0.0.1:7788'\nherdr_session = 'before'\n").unwrap();
        let link = home.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let store = ConfigStore::load(link.clone()).await.unwrap();
        store
            .update(|cfg| {
                cfg.server.herdr_session = "after".into();
                Ok(())
            })
            .await
            .unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "連結被換成一般檔了");
        assert!(std::fs::read_to_string(&target).unwrap().contains("after"), "dotfiles 那份沒被更新");
        // 暫存檔跟著目標走，而且兩邊都不准留下殘骸。
        for dir in [&home, &dotfiles] {
            let left: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".tmp"))
                .collect();
            assert!(left.is_empty(), "{}: 留下暫存檔 {left:?}", dir.display());
        }
        // 重新載入讀得到新值（連結還通）。
        assert_eq!(ConfigStore::load(link).await.unwrap().get().await.server.herdr_session, "after");
        std::fs::remove_dir_all(&root).ok();
    }

    /// i92b review 的兩條：**相對路徑**的 symlink（`ln -s ../dotfiles/am.toml config.toml`，
    /// 比絕對路徑更常見）要以連結所在的目錄解析；而 `rename` 會把暫存檔的 umask 權限當成新檔的權限，
    /// 所以 chmod 600 過的設定檔不可以在第一次寫入時被悄悄放寬。
    #[tokio::test]
    async fn a_relative_symlink_resolves_beside_the_link_and_keeps_the_targets_mode() {
        use std::os::unix::fs::PermissionsExt;
        let root = super::test_support::track(std::env::temp_dir().join(format!("am-config-relsym-{}", super::test_support::ulid())));
        let home = root.join("home");
        let dotfiles = root.join("dotfiles");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("am.toml");
        std::fs::write(&target, "[server]\nlisten = '127.0.0.1:7788'\nherdr_session = 'before'\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = home.join("config.toml");
        std::os::unix::fs::symlink("../dotfiles/am.toml", &link).unwrap();

        let store = ConfigStore::load(link.clone()).await.unwrap();
        store.update(|cfg| { cfg.server.herdr_session = "after".into(); Ok(()) }).await.unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "相對連結也不能被吃掉");
        assert_eq!(std::fs::read_link(&link).unwrap(), std::path::Path::new("../dotfiles/am.toml"), "連結內容不變");
        assert!(std::fs::read_to_string(&target).unwrap().contains("after"), "要寫到 ../dotfiles 那份");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600,
            "原本 0600 的設定檔不可以被 rename 放寬成 umask 的權限"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 設定檔還不存在時（`read_file` 會寫一份預設）照原路徑建，不因為 canonicalize 失敗就爆掉。
    #[tokio::test]
    async fn a_missing_config_is_still_created_in_place() {
        let dir = super::test_support::track(std::env::temp_dir().join(format!("am-config-missing-{}", super::test_support::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let store = ConfigStore::load(path.clone()).await.unwrap();
        assert!(path.exists(), "不存在時要寫出一份預設 config");
        assert!(!std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        store.update(|cfg| { cfg.server.herdr_session = "x".into(); Ok(()) }).await.unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("herdr_session = \"x\""));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod supervisor_cfg_tests {
    use super::*;

    /// Refs #504：`notify_max_attempts = 0` 以前照單全收，`due_for` 的 `notify_attempts < max`
    /// 從頭到尾不成立＝沒有人會被叫醒；`responder_max_backoff_secs = 0` 讓重試時間永遠等於「現在」，
    /// 每個 controller tick 重寫一次角色列。跟 `idle_close_secs`／`max_concurrent` 同一條規矩：
    /// 離譜的值不採用，而且在**解析**時就夾好，讀取端抄漏 `.max(1)` 也不會破功。
    #[test]
    fn hostile_supervisor_thresholds_are_clamped_while_parsing() {
        let parse = |text: &str| toml::from_str::<ConfigFile>(text).unwrap().supervisor;

        let d = SupervisorCfg::default();
        assert_eq!(parse("").notify_max_attempts, d.notify_max_attempts, "沒寫就是預設");
        assert_eq!(parse("").responder_max_backoff_secs, d.responder_max_backoff_secs);

        for bad in ["0", "-1", "-999"] {
            let cfg = parse(&format!("[supervisor]\nnotify_max_attempts = {bad}\n"));
            assert_eq!(cfg.notify_max_attempts, MIN_NOTIFY_MAX_ATTEMPTS, "notify_max_attempts = {bad}");
        }
        assert_eq!(parse("[supervisor]\nnotify_max_attempts = 3\n").notify_max_attempts, 3, "合理的值照用");

        for bad in ["0", "1", "9"] {
            let cfg = parse(&format!("[supervisor]\nresponder_max_backoff_secs = {bad}\n"));
            assert_eq!(cfg.responder_max_backoff_secs, MIN_RESPONDER_MAX_BACKOFF_SECS, "responder_max_backoff_secs = {bad}");
        }
        assert_eq!(parse("[supervisor]\nresponder_max_backoff_secs = 600\n").responder_max_backoff_secs, 600);

        // 夾過的值序列化出去再讀回來要穩定（不會每次開機都判成「外部改動」）。
        let once = parse("[supervisor]\nnotify_max_attempts = 0\nresponder_max_backoff_secs = 0\n");
        let twice = toml::from_str::<ConfigFile>(&toml::to_string_pretty(&ConfigFile {
            supervisor: once.clone(),
            ..Default::default()
        })
        .unwrap())
        .unwrap()
        .supervisor;
        assert_eq!(once, twice);
    }

    /// issue #748：`[codex] instant_interrupt` 預設關；沒寫就不出現在序列化結果裡（不改既有設定檔的長相）。
    #[test]
    fn codex_instant_interrupt_is_off_unless_the_config_turns_it_on() {
        let none: ConfigFile = toml::from_str("").unwrap();
        assert!(!none.codex.instant_interrupt);
        assert!(!toml::to_string_pretty(&none).unwrap().contains("[codex]"));
        let on: ConfigFile = toml::from_str("[codex]\ninstant_interrupt = true\n").unwrap();
        assert!(on.codex.instant_interrupt);
        let back: ConfigFile = toml::from_str(&toml::to_string_pretty(&on).unwrap()).unwrap();
        assert_eq!(on, back);
    }

    /// issue #749 審查：`[codex_history]` 預設關。裝著的 codex 0.159.3 的 app-server 對 `thread/items/list` 回
    /// `-32601 not supported yet`、`thread/turns/list` 回 `thread not loaded`，預設開等於每則 codex prompt 白起一個 app-server
    /// （還會寫 CODEX_HOME、對外跑 `git ls-remote`）卻永遠拿不到證據。要用請在設定檔明寫 `enabled = true`。
    #[test]
    fn codex_history_is_off_unless_the_config_turns_it_on() {
        let none: ConfigFile = toml::from_str("").unwrap();
        assert!(!none.codex_history.enabled);
        assert!(!CodexHistoryCfg::default().enabled);
        assert!(!toml::to_string_pretty(&none).unwrap().contains("codex_history"));
        let on: ConfigFile = toml::from_str("[codex_history]\nenabled = true\n").unwrap();
        assert!(on.codex_history.enabled);
        let back: ConfigFile = toml::from_str(&toml::to_string_pretty(&on).unwrap()).unwrap();
        assert_eq!(on, back, "明寫開著要留得住");
        let off: ConfigFile = toml::from_str("[codex_history]\nenabled = false\n").unwrap();
        assert!(!off.codex_history.enabled);
    }
}

#[cfg(test)]
mod write_review_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;

    const HAND_WRITTEN: &str = r#"# 我的設定：手寫的，別洗掉
[server]
listen = "127.0.0.1:7788"   # 只開本機

# 未來版本才有的區段
[future]
knob = 3

[[projects]]
# 主專案
path = "/tmp"
label = "main"
id = "01PROJ"

[[projects.bots]]
id = "01BOTA"
name = "a"
kind = "claude"
secret_note = "keep me"   # 未知鍵

[[projects.bots]]
id = "01BOTB"
name = "b"
kind = "codex"
# b 的註解
model = "gpt-6"
"#;

    async fn store_with(text: &str) -> (ConfigStore, PathBuf) {
        let dir = super::test_support::track(std::env::temp_dir().join(format!("am-config-write-review-{}", super::test_support::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        (ConfigStore::load(path.clone()).await.unwrap(), path)
    }

    /// 寫回只改動被改的值：手寫的註解、未知區段與未知鍵都留著，檔案重讀起來跟記憶體一致。
    #[tokio::test]
    async fn a_write_keeps_hand_written_comments_and_unknown_keys() {
        let (store, path) = store_with(HAND_WRITTEN).await;
        store.update(|c| { c.projects[0].bots[1].model = Some("gpt-6.1-sol".into()); Ok(()) }).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        for keep in ["# 我的設定：手寫的，別洗掉", "# 只開本機", "# 未來版本才有的區段", "[future]", "knob = 3", "# 主專案", "secret_note = \"keep me\"   # 未知鍵", "# b 的註解"] {
            assert!(text.contains(keep), "lost `{keep}`:\n{text}");
        }
        assert!(text.contains("gpt-6.1-sol") && !text.contains("\"gpt-6\""), "{text}");
        let (reread, _) = parse_config(&text).unwrap();
        assert_eq!(reread, store.get().await);
    }

    /// 刪掉一顆 bot：它那段（含註解、未知鍵）一起走，其他 bot 不受影響；新增的 bot 接在後面。
    #[tokio::test]
    async fn removing_and_adding_bots_only_touches_those_bots() {
        let (store, path) = store_with(HAND_WRITTEN).await;
        store
            .update(|c| {
                c.projects[0].bots.retain(|b| b.id.as_deref() != Some("01BOTA"));
                let mut nb = c.projects[0].bots[0].clone();
                nb.id = Some("01BOTC".into());
                nb.name = "c".into();
                nb.model = None;
                c.projects[0].bots.push(nb);
                Ok(())
            })
            .await
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("secret_note"), "被刪的 bot 的未知鍵跟著它走：\n{text}");
        assert!(text.contains("# b 的註解") && text.contains("# 未來版本才有的區段"), "{text}");
        let (reread, _) = parse_config(&text).unwrap();
        assert_eq!(reread.projects[0].bots.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(reread, store.get().await);
    }

    /// 回到預設而被省略序列化的區段（`skip_serializing_if`）要從檔案裡拿掉，不能因為「保留未知鍵」而殘留。
    #[tokio::test]
    async fn a_section_reset_to_its_default_is_removed_not_kept_as_an_unknown_key() {
        let (store, path) = store_with("[codex]\ninstant_interrupt = true\n\n[[projects]]\npath = \"/tmp\"\nlabel = \"p\"\nid = \"01P\"\n").await;
        store.update(|c| { c.codex = CodexCfg::default(); Ok(()) }).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("instant_interrupt"), "{text}");
        assert_eq!(parse_config(&text).unwrap().0, store.get().await);
    }

    /// 同時兩個更新各加一個專案：誰都不能被蓋掉（lost update）。
    #[tokio::test]
    async fn concurrent_updates_do_not_lose_each_other() {
        let (store, path) = store_with(HAND_WRITTEN).await;
        let store = Arc::new(store);
        let mut tasks = Vec::new();
        for n in 0..16 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .update(|c| {
                        c.projects.push(ProjectCfg { id: Some(format!("01CONC{n:02}")), path: "/tmp".into(), label: format!("conc-{n}"), host: LOCAL_HOST.into(), bots: vec![], handed_off_to: None });
                        Ok(())
                    })
                    .await
                    .unwrap();
            }));
        }
        for t in tasks { t.await.unwrap(); }
        let (reread, _) = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reread.projects.len(), 17, "16 個並行新增＋原本那個");
        assert!(std::fs::read_to_string(&path).unwrap().contains("# 我的設定：手寫的，別洗掉"));
    }

    /// 設定檔權限：原本 0600 的寫回後仍是 0600；新建的檔（預設設定）是 0600，不是 umask 的 0644；暫存檔不留。
    #[tokio::test]
    async fn the_config_file_keeps_or_gets_owner_only_permissions_and_leaves_no_temp_file() {
        let (store, path) = store_with(HAND_WRITTEN).await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        store.update(|c| { c.projects[0].label = "renamed".into(); Ok(()) }).await.unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

        let dir = path.parent().unwrap().to_path_buf();
        let fresh = dir.join("fresh").join("config.toml");
        std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        ConfigStore::load(fresh.clone()).await.unwrap();
        assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o600, "新檔不能吃 umask 變成 0644");
        for d in [&dir, fresh.parent().unwrap()] {
            let leftovers: Vec<_> = std::fs::read_dir(d).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
    }

    /// `[agents]` 手改了不必重啟 daemon（跟 `[build]` 同一條規矩）：mtime 變了就換，壞檔保留舊值。
    #[tokio::test]
    async fn agents_section_edited_on_disk_is_picked_up_without_a_restart() {
        let (store, path) = store_with("[agents]\ninstructions_file = \"~/old.md\"\n").await;
        assert_eq!(store.agents_fresh().await.instructions_file.as_deref(), Some("~/old.md"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "[agents]\ninstructions_file = \"~/new.md\"\n").unwrap();
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(t).unwrap();
        assert_eq!(store.agents_fresh().await.instructions_file.as_deref(), Some("~/new.md"));
        // 半寫／打錯字：保留記憶體裡的值。
        std::fs::write(&path, "[agents\ninstructions_file = ").unwrap();
        let t2 = t + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(t2).unwrap();
        assert_eq!(store.agents_fresh().await.instructions_file.as_deref(), Some("~/new.md"));
    }

    #[tokio::test]
    async fn a_same_mtime_agents_edit_is_picked_up_from_its_content() {
        let (store, path) = store_with("[agents]\ninstructions_file = \"~/old.md\"\n").await;
        let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "[agents]\ninstructions_file = \"~/new.md\"\n").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(original_mtime).unwrap();
        assert_eq!(store.agents_fresh().await.instructions_file.as_deref(), Some("~/new.md"), "原子替換或粗粒度檔案系統可能保留 mtime");
    }

    /// A malformed intermediate save must not poison the mtime cache when a same-mtime atomic
    /// replacement fixes it (coarse or preserved mtimes are possible on synced config files).
    #[tokio::test]
    async fn a_fixed_agents_file_is_reloaded_after_a_bad_same_mtime_save() {
        let (store, path) = store_with("[agents]\ninstructions_file = \"~/old.md\"\n").await;
        let same_mtime = std::time::SystemTime::now() + std::time::Duration::from_secs(120);

        std::fs::write(&path, "[agents\ninstructions_file = \"~/bad.md\"\n").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(same_mtime).unwrap();
        assert_eq!(store.agents_fresh().await.instructions_file.as_deref(), Some("~/old.md"), "壞檔期間沿用上一份設定");

        std::fs::write(&path, "[agents]\ninstructions_file = \"~/new.md\"\n").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(same_mtime).unwrap();
        assert_eq!(store.agents_fresh().await.instructions_file.as_deref(), Some("~/new.md"), "修好的設定要被重新載入，即使 mtime 沒變");
    }

    /// 寫到一半被讀到的空檔是合法 TOML（全部預設），不是解析錯誤。熱重載與 `update` 的重讀都必須留著記憶體裡的舊值。
    #[tokio::test]
    async fn a_blank_config_read_mid_write_keeps_the_previous_hot_sections() {
        let (store, path) = store_with("[agents]\ninstructions_file = \"~/old.md\"\n\n[build]\ncargo_jobs = 3\n").await;
        assert_eq!(store.build_fresh().await.cargo_jobs, 3);
        std::fs::write(&path, " \n\n").unwrap();
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(30);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(bumped).unwrap();
        assert_eq!(store.agents_fresh().await.instructions_file.as_deref(), Some("~/old.md"));
        assert_eq!(store.build_fresh().await.cargo_jobs, 3);
        let err = store.update(|cfg| { cfg.server.herdr_session = "from-blank".into(); Ok(()) }).await.unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("blank"), "{message}");
        assert_eq!(store.get().await.agents.instructions_file.as_deref(), Some("~/old.md"));
        assert_eq!(store.get().await.build.cargo_jobs, 3);
        assert_ne!(store.get().await.server.herdr_session, "from-blank");
    }
}

/// 本 crate 測試共用：暫存路徑登記（行程結束時刪）、不掛任何 hook 的 `ConfigStore::load`。
/// （daemon 的 `crate::testing`／`projection`／`config_audit` 不在這一層，帶 hook 的行為在 daemon 的 `app_ports_p2` 測試裡驗。）
#[cfg(test)]
mod test_support {
    use super::*;
    use std::sync::{Mutex, Once};

    pub fn ulid() -> String {
        ulid::Ulid::new().to_string()
    }

    pub fn track(path: PathBuf) -> PathBuf {
        static LIST: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
        static HOOK: Once = Once::new();
        extern "C" fn sweep() {
            for p in LIST.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
                if std::fs::remove_dir_all(&p).is_err() {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        LIST.lock().unwrap_or_else(|e| e.into_inner()).push(path.clone());
        HOOK.call_once(|| {
            // SAFETY: `sweep` 是沒有參數的 `extern "C"` 函式，整個行程生命週期內都有效。
            unsafe { libc::atexit(sweep) };
        });
        path
    }

    pub struct NoHooks;
    impl ConfigChangeHooks for NoHooks {
        fn validate_projection(&self, _next: &ConfigFile) -> Result<()> {
            Ok(())
        }
        fn audit_external_change(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
        fn audit_reload_unchanged(&self, _at: &'static std::panic::Location<'static>, _path: &Path) {}
        fn audit_write(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
    }
}

#[cfg(test)]
impl ConfigStore {
    /// 測試用：不掛 hook 載入（正式入口在 daemon 的 `projection::app_ports_p2::load_config`）。
    pub(crate) async fn load(path: PathBuf) -> Result<Self> {
        Self::load_with_hooks(path, std::sync::Arc::new(test_support::NoHooks)).await
    }
}

#[cfg(test)]
mod ssh_opts_tests {
    use super::*;

    #[test]
    fn non_ascii_whitespace_in_an_ssh_option_is_rejected_without_panicking() {
        for sep in ['\u{3000}', '\u{a0}', '\u{2003}'] {
            assert!(ssh_option_problem(&format!("User{sep}root")).is_some(), "sep={sep:?}");
            assert!(ssh_option_problem(&format!("ConnectTimeout{sep}5")).is_some(), "sep={sep:?}");
            assert!(ssh_opts_problem(&["-o".into(), format!("User{sep}root")]).is_some(), "sep={sep:?}");
            assert!(ssh_opts_problem(&[format!("-oConnectTimeout{sep}5")]).is_some(), "sep={sep:?}");
        }
        // 不回歸：半形分隔與白名單照舊。
        assert_eq!(ssh_opts_problem(&["-o".into(), "ConnectTimeout=5".into()]), None);
        assert_eq!(ssh_opts_problem(&["-oUser root".into()]), None);
        assert_eq!(ssh_opts_problem(&["-o".into(), "User root".into()]), None);
        assert_eq!(ssh_opts_problem(&["-i".into(), "/k".into()]), None);
        assert!(ssh_opts_problem(&["-o".into(), "ProxyCommand=x".into()]).is_some());
    }
}
