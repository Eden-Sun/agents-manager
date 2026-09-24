//! config.toml is the authority for the *desired* Project / Bot set; SQLite holds runtime state
//! and is projected from TOML (see `projection.rs`).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
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
    /// claude only: which project instruction files the CLI reads (`agents-md` plugin's `instructionFiles`, issue #213).
    /// None = [`INSTRUCTION_FILES_DEFAULT`] — the daemon pins it, it never falls back to the CLI's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_files: Option<String>,
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
    /// issue #90：全機 cargo/rustc 併發的排程設定。
    #[serde(default)]
    pub build: BuildCfg,
    /// issue #204：上游新版分診開 GitHub issue 的設定（預設不開）。
    #[serde(default, skip_serializing_if = "ReleaseTriageCfg::is_default")]
    pub release_triage: ReleaseTriageCfg,
    /// issue #240：撞限偵測的第二意見（只記錄），預設關。
    #[serde(default, skip_serializing_if = "JudgeCfg::is_default")]
    pub judge: JudgeCfg,
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
    /// 名額 TTL（秒）：拿到之後這麼久沒 renew 就視為持有者已死，下一次 acquire 收回。
    #[serde(default = "default_build_lease_ttl_secs")]
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

fn default_build_lease_ttl_secs() -> u64 {
    180
}

/// 併發上限的下限：0 不是「停用排程」，是「誰都拿不到名額」，整台機器的受管建置會全部卡死。
pub const MIN_BUILD_MAX_CONCURRENT: usize = 1;

/// 名額租約 TTL 的下限（#322）：0 會讓每個 held 列一建立就過期，下一個 acquire 把它收掉，`max_concurrent` 形同虛設。
pub const MIN_BUILD_LEASE_TTL_SECS: u64 = 10;

impl BuildCfg {
    /// 實際使用的租約 TTL（秒）：設定檔的值夾到 [`MIN_BUILD_LEASE_TTL_SECS`] 以上。
    pub fn lease_ttl(&self) -> u64 {
        self.lease_ttl_secs.max(MIN_BUILD_LEASE_TTL_SECS)
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

/// The `instructionFiles` values claude 2.1.277+ accepts (issue #213, read off the 2.1.277／2.1.278 binary's option list).
/// A value outside this list makes the CLI read it as *its* default (`claude-md-or-agents-md`) — silently unpinned — so nothing
/// the daemon writes into `--settings` may come from anywhere but this list.
pub const INSTRUCTION_FILES: [&str; 4] = ["claude-md", "claude-md-or-agents-md", "claude-md-and-agents-md", "managed-only"];
/// What a claude bot reads when nothing is set: CLAUDE.md only, the behaviour before claude 2.1.277 (issue #206).
pub const INSTRUCTION_FILES_DEFAULT: &str = "claude-md";

/// `bots.instruction_files` → the value the daemon writes. Anything unset, blank or not in [`INSTRUCTION_FILES`]
/// (a hand-edited TOML) gives the default, never the CLI's.
pub fn effective_instruction_files(v: Option<&str>) -> &'static str {
    let v = v.map(str::trim).unwrap_or_default();
    INSTRUCTION_FILES.iter().copied().find(|k| *k == v).unwrap_or(INSTRUCTION_FILES_DEFAULT)
}

/// Blank / null = unset (`Ok(None)`); only claude has this switch, and only the values in [`INSTRUCTION_FILES`].
pub fn normalize_instruction_files(kind: &str, v: Option<&str>) -> Result<Option<String>, String> {
    let Some(v) = v.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    if kind != "claude" {
        return Err(format!("instruction_files is only for claude bots (this one is {kind})"));
    }
    if INSTRUCTION_FILES.contains(&v) {
        Ok(Some(v.to_string()))
    } else {
        Err(format!("instruction_files must be one of {}", INSTRUCTION_FILES.join(", ")))
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
pub fn valid_bot_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=32).contains(&n)
        && !name.starts_with(' ')
        && !name.ends_with(' ')
        && !name.contains("  ")
        && !name.chars().any(|c| (c.is_whitespace() && c != ' ') || matches!(c, '@' | ',' | ':' | ';'))
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
            let (cfg, mtime) = read_file(&self.path)
                .context("config.toml changed on disk and could not be re-read")?;
            // daemon 自己寫完會把記憶體那份換成寫出去的內容，所以內容對不上＝別人改的（issue #406）。
            if cfg != g.cfg {
                crate::config_audit::log_external_change(at, &self.path, &g.cfg, &cfg);
            } else if mtime != g.mtime {
                crate::config_audit::log_reload_unchanged(at, &self.path);
            }
            g.cfg = cfg;
            g.mtime = mtime;
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
        crate::projection::validate(&next)?;
        // 純驗證過了才問需要 DB 的那一類（同一條理由：驗不過就不寫，guard 也不例外）。
        guard(&next)?;
        // A serde rewrite drops comments / unknown keys: no-op updates must not write (issue #38).
        if next != g.cfg {
            write_atomic(&self.path, &next)?;
            g.mtime = std::fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
            crate::config_audit::log_write(at, &self.path, &g.cfg, &next);
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
///
/// 寫的是 `path` **解開 symlink 之後**的那個檔（issue #506 的鄰居 #507）：`startup::normalize_config_file`
/// 刻意不 canonicalize 檔名那一段，因為 `config.toml` 常是指到 dotfiles 的 symlink，而資料目錄要留在
/// 連結所在的目錄。但 `rename(2)` 換掉的是連結本身，所以照著 `path` 寫等於第一次寫入就把連結吃掉：
/// 連結變成一般檔、dotfiles 那一份停在舊內容，而 `git status` 什麼都看不出來。
/// 暫存檔跟著搬到目標所在目錄，順便讓「目標在另一個檔案系統」不會 rename EXDEV。
pub fn write_atomic(path: &Path, cfg: &ConfigFile) -> Result<()> {
    let text = toml::to_string_pretty(cfg)?;
    // 檔案還不存在（`read_file` 第一次寫預設設定）時 canonicalize 會失敗：那就照原路徑建。
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let tmp = target.with_extension(format!("toml.{}.{}.tmp", std::process::id(), nonce));
    let result = (|| -> Result<()> {
        std::fs::write(&tmp, text)?;
        // rename 會把暫存檔的權限（umask，通常 0644）當成新檔的權限。設定檔被 chmod 600 過的話，
        // 第一次寫入就會被悄悄放寬——跟 `trust.rs::write_atomic_preserving_mode` 同一條規矩：
        // 先把原檔的 mode 套到暫存檔上。原檔不存在（第一次寫預設設定）就照 umask。
        if let Ok(md) = std::fs::metadata(&target) {
            let _ = std::fs::set_permissions(&tmp, md.permissions());
        }
        std::fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
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
mod instruction_files_tests {
    use super::{effective_instruction_files, normalize_instruction_files, INSTRUCTION_FILES, INSTRUCTION_FILES_DEFAULT};

    /// claude 2.1.277／2.1.278 binary 裡 `agents-md` plugin 的 `instructionFiles` options（`strings` 讀出來的那個陣列）。
    /// CLI 換名或增減值時要照 binary 改這一份，再改 `INSTRUCTION_FILES`。
    const BINARY_OPTIONS: [&str; 4] = ["claude-md", "claude-md-or-agents-md", "claude-md-and-agents-md", "managed-only"];

    #[test]
    fn the_allowed_values_are_the_binarys_options() {
        assert_eq!(INSTRUCTION_FILES, BINARY_OPTIONS);
        assert!(BINARY_OPTIONS.contains(&INSTRUCTION_FILES_DEFAULT));
    }

    #[test]
    fn unset_or_unknown_is_the_pinned_default_never_the_clis() {
        assert_eq!(effective_instruction_files(None), "claude-md");
        assert_eq!(effective_instruction_files(Some("")), "claude-md");
        assert_eq!(effective_instruction_files(Some("  ")), "claude-md");
        // 手改 TOML 寫錯：寫進 --settings 的話 CLI 會退回它自己的預設（改讀 AGENTS.md），所以在這裡就擋回釘住的值。
        assert_eq!(effective_instruction_files(Some("agents-md")), "claude-md");
        assert_eq!(effective_instruction_files(Some("Claude-MD-And-Agents-MD")), "claude-md");
        for v in BINARY_OPTIONS {
            assert_eq!(effective_instruction_files(Some(v)), v);
            assert_eq!(effective_instruction_files(Some(&format!(" {v} "))), v);
        }
    }

    #[test]
    fn only_claude_takes_a_value_and_only_a_known_one() {
        for v in BINARY_OPTIONS {
            assert_eq!(normalize_instruction_files("claude", Some(v)).unwrap(), Some(v.to_string()));
        }
        assert_eq!(normalize_instruction_files("claude", Some(" managed-only ")).unwrap(), Some("managed-only".into()));
        assert_eq!(normalize_instruction_files("claude", None).unwrap(), None);
        assert_eq!(normalize_instruction_files("claude", Some(" ")).unwrap(), None);
        assert!(normalize_instruction_files("claude", Some("agents-md")).is_err());
        assert!(normalize_instruction_files("claude", Some("claude")).is_err(), "2.1.276 的舊值不能寫進新選項");
        // 沒有這個 plugin 的 kind：帶值就拒，清成空才放行。
        for kind in ["codex", "grok"] {
            assert!(normalize_instruction_files(kind, Some("claude-md")).is_err(), "{kind}");
            assert_eq!(normalize_instruction_files(kind, None).unwrap(), None);
            assert_eq!(normalize_instruction_files(kind, Some("")).unwrap(), None);
        }
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
        assert!(valid_bot_name("has space"));
        assert!(valid_bot_name("my bot 2"));
        assert!(!valid_bot_name(" lead"));
        assert!(!valid_bot_name("trail "));
        assert!(!valid_bot_name("two  spaces"));
        assert!(!valid_bot_name("tab\there"));
        assert!(!valid_bot_name("new\nline"));
        assert!(!valid_bot_name(" "));
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
        let root = std::env::temp_dir().join(format!("am-config-symlink-{}", crate::db::ulid()));
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
        let root = std::env::temp_dir().join(format!("am-config-relsym-{}", crate::db::ulid()));
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
        let dir = std::env::temp_dir().join(format!("am-config-missing-{}", crate::db::ulid()));
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
}
