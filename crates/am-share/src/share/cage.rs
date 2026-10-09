//! 受限 bot 的籠子：就算 end user 說服 agent 去做壞事，也拿不到機器上的秘密、改不到別的東西。
//!
//! 全部照 claude 2.1.288 實際支援的寫（2026-10-03 對 `claude --help` 與 binary 內的 settings schema 查過，不是猜的）：
//!
//! - `--restricted`：拿掉 Bash／PowerShell／REPL 等會跑指令或程式碼的內建工具與 WebFetch（除非 `--tools` 點名，我們不點），
//!   不讀 user／project／local settings（`--settings` 照樣生效，hook 就靠它），檔案工具只在工作目錄（含 `--add-dir`），
//!   拒絕 bypassPermissions。
//! - `--tools Read,Edit,Write,Glob,Grep,WebSearch`：工具白名單。只有 `--restricted` 時還剩 `SendMessage`／`ListAgents`
//!   （傳話給這台機器上**別的** Claude session）、`PushNotification`（推播到主人的手機）、`Agent`、`Skill`、`Cron*`、
//!   `EnterWorktree`、`ToolSearch` 等（2026-10-03 實際跑過列出來的）；白名單之後只剩這六個、也沒有 deferred tools。
//!   新版 claude 多出來的工具也進不來。settings 的 deny 另外點名那幾個危險的，當第二層。
//! - `--strict-mcp-config`：不載任何 MCP server（沒有 `--mcp-config` 就是零個）。
//! - `--permission-mode dontAsk`（settings 的 `permissions.defaultMode` 也寫一次）：「Don't prompt for permissions, deny if
//!   not pre-approved」。沒人守在 pane 前面，任何權限框都會讓它卡住；沒預先允許的一律拒絕。
//! - Bash：claude 的 sandbox 在 Linux 要 bubblewrap＋socat，這台（agm-host，2026-10-03）兩個都沒有，sandbox 起不來，
//!   照契約「做不到 sandbox 就整個 deny Bash」：`--restricted` 已經拿掉，settings 另外 deny 一次。
//! - WebFetch 契約說「可留」，但 `--restricted` 預設拿掉而且我們不加回來：它是 claude 行程自己發的 HTTP，
//!   打得到 127.0.0.1／區網上的 daemon（`/api/session` 對本機來源會回 UI token）。WebSearch（伺服器端）留著。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use am_base::hosts::HostConn;

use crate::config::LOCAL_HOST;
use crate::db;
use crate::lifecycle::LcError;
use crate::outbox::ShareStorage;
use crate::share::remote_fs::{self, PreflightError};
use crate::share::site::{self, RemoteSite, ShareSite, SiteEnv};

/// 受限 bot 的 claude 最低版本：籠子的旗標與行為是照這版驗的（R-S2 §4.1）。
pub const MIN_CLAUDE: &str = "2.1.288";

/// 受限 bot 啟動／建立要從 `App` 拿的外部事實（`App` 在 `app_ports_p10` 實作）：本機身分的 env、bot 目錄、claude 模型清單。
///
/// 身分表與模型清單的既有函式要的是 `&Arc<App>`，所以這兩個是以 `&Arc<Self>` 為參數的關聯函式（`App` 實作；`Arc<App>` 不能在 daemon 實作外來 trait）。
pub trait CageEnv: ShareStorage {
    /// 本機名叫 `identity` 的身分的 env（沒有＝空）。
    fn local_identity_env(app: &std::sync::Arc<Self>, identity: &str) -> impl std::future::Future<Output = BTreeMap<String, String>> + Send;
    /// 這顆 bot 在 daemon 資料目錄底下的目錄（id 不合法＝錯誤）。
    fn bot_dir(&self, bot_id: &str) -> anyhow::Result<PathBuf>;
    /// `/api/models?kind=claude` 當下的清單（`{"models":[{"id":…}]}`）；列不出來回錯誤字串。
    fn claude_models(app: &std::sync::Arc<Self>, identity: Option<&str>) -> impl std::future::Future<Output = Result<Value, String>> + Send;
}

/// 分享用 bot 目前只做 claude。`host_known`：本機，或設定裡有的主機（專案的 host 在那台上）。
/// 不認得的主機 409 `unsupported_host`（找不到，不是「不支援遠端」）。
pub fn check_profile(kind: &str, host: &str, host_known: bool) -> Result<(), LcError> {
    if kind != "claude" {
        return Err(LcError::conflict(
            "unsupported_kind",
            json!({"kind": kind, "message": "分享用的受限 bot 目前只支援 claude（codex／grok 還沒有對應的籠子）"}),
        ));
    }
    if host != LOCAL_HOST && !host_known {
        return Err(LcError::conflict(
            "unsupported_host",
            json!({"host": host, "message": "找不到這台主機（請先在設定裡加上它）"}),
        ));
    }
    Ok(())
}

/// 遠端 preflight 的失敗對應到 409（R-S2 §4.1、§7）：三種都是建立或啟動時就擋下，不留半套的 bot。
pub fn preflight_conflict(e: PreflightError) -> LcError {
    match e {
        PreflightError::Unreachable => LcError::conflict(
            "share_host_unreachable",
            json!({"message": "專案所在的主機連不上（或還沒連線），先確認它連得到再試"}),
        ),
        PreflightError::ClaudeTooOld { found } => LcError::conflict(
            "share_claude_too_old",
            json!({"found": found, "min": MIN_CLAUDE, "message": format!("遠端的 claude 版本要 {MIN_CLAUDE} 以上（讀到：{}）", if found.is_empty() { "讀不到" } else { found.as_str() })}),
        ),
        PreflightError::ManagedSettings => LcError::conflict(
            "share_managed_settings",
            json!({"message": "遠端機器上有 claude 的 managed settings（它會蓋過籠子的設定），拿掉之後再試"}),
        ),
    }
}

/// 受限遠端 bot 的完整檢查（建立與每次啟動都做）。
pub async fn preflight_restricted(conn: &HostConn) -> Result<(), LcError> {
    remote_fs::preflight_restricted(conn, MIN_CLAUDE).await.map_err(preflight_conflict)
}

/// 遠端分享 bot 的位置；連不上（或這顆不是遠端的）就是 `share_host_unreachable`，fail closed。
pub async fn remote_site(app: &impl SiteEnv, bot_id: &str) -> Result<RemoteSite, LcError> {
    match site::resolve(app, bot_id).await {
        Ok(ShareSite::Remote(r)) => Ok(r),
        Ok(ShareSite::Local { .. }) => Err(LcError::Upstream(format!("share bot {bot_id} is not on a remote host"))),
        Err(_) => Err(preflight_conflict(PreflightError::Unreachable)),
    }
}

/// 啟動前：這顆是不是受限 bot。是的話回它的工作目錄（順手補建），而且 kind／host 不合就不准起來。
/// 讀不到 `shared_bots` 一律不啟動——寧可起不來，也不要把受限 bot 當一般 bot 起（fail closed）。
pub async fn prepare(app: &(impl ShareStorage + SiteEnv), bot: &db::Bot, host: &str) -> Result<Option<String>, LcError> {
    let ws = crate::share::store::caged_workspace(ShareStorage::db_pool(app), &bot.id)
        .await
        .map_err(|e| LcError::Upstream(format!("cannot tell whether bot {} is a restricted share bot: {e}", bot.id)))?;
    let Some(ws) = ws else {
        // 信任分享（trusted）不進籠子，照一般 bot 起；但 outbox 一樣不給 gc 清、上傳的 inbox 一樣要在。
        if let Ok(Some(dir)) = crate::share::store::workspace(ShareStorage::db_pool(app), &bot.id).await {
            if host == LOCAL_HOST {
                let _ = crate::share::folder::ensure_inbox(Path::new(&dir));
                crate::outbox::mark_share_keep(ShareStorage::data_dir(app), &bot.id);
            } else if let Ok(site) = remote_site(app, &bot.id).await {
                // 遠端的 inbox 與保留標記盡力而為（本機也是這樣）：失敗只記 warning，不擋啟動。
                if let Err(e) = site.ensure_inbox().await {
                    tracing::warn!(bot = %bot.id, host, error = %e, "remote trusted share folder inbox not ensured");
                }
                if let Err(e) = site.mark_share_keep(true).await {
                    tracing::warn!(bot = %bot.id, host, error = %e, "remote trusted share outbox keep mark not written");
                }
            }
        }
        return Ok(None);
    };
    check_profile(&bot.kind, host, true)?;
    if host == LOCAL_HOST {
        // 資料夾不見了就不起來（不替使用者重建一個空的）；inbox 不存在就建。
        crate::share::folder::ensure_inbox(Path::new(&ws)).map_err(|e| LcError::Upstream(format!("restricted bot folder {ws}: {e}")))?;
        // outbox 不給 AGM 的 gc 清（使用者 2026-10-04）：每次啟動補一次標記，bot 自己刪掉也回得來。
        crate::outbox::mark_share_keep(ShareStorage::data_dir(app), &bot.id);
        return Ok(Some(ws));
    }
    // 遠端：每次啟動都重跑 preflight（claude 可能被換版、managed settings 可能被加），不過就不起來。
    let site = remote_site(app, &bot.id).await?;
    preflight_restricted(&site.conn).await?;
    site.ensure_inbox().await.map_err(|e| match e {
        remote_fs::RfsError::NotFound => LcError::Upstream(format!("restricted bot folder {ws} on {host} is gone")),
        remote_fs::RfsError::Unavailable => preflight_conflict(PreflightError::Unreachable),
        other => LcError::Upstream(format!("restricted bot folder {ws} on {host}: {other}")),
    })?;
    if let Err(e) = site.mark_share_keep(true).await {
        tracing::warn!(bot = %bot.id, host, error = %e, "remote restricted share outbox keep mark not written");
    }
    Ok(Some(ws))
}

/// pane env 裡受限 bot 留得下來的 daemon 變數：hook／statusline 指令要的就這些（`hook_cmd.rs`、`statusline_cmd.rs`），
/// 加上給它放檔案的 `AM_OUTBOX`。`AM_BOT_TOKEN`（打 API 用）、shim 與 cargo 要的、子 agent 命名要的全部不帶。
const KEEP_AM: &[&str] = &["AM_BOT_ID", "AM_HOOK_TOKEN", "AM_RUN_ID", "AM_PORT", "AM_INSTANCE", "AM_OUTBOX"];
/// daemon 自己為 claude 設的、跟權限無關的開關。
const KEEP_CLAUDE: &[&str] = &["CLAUDE_CODE_CHILD_SESSION", "CLAUDECODE", "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION"];
/// 不是我們放的、但可能從 herdr server 的環境繼承下來的（daemon 常常是在某顆 bot 的 pane 裡起的）：明確蓋成空字串。
const BLANK: &[&str] = &[
    "AM_BOT_TOKEN",
    "AM_CHILD_OF",
    "AM_AGENT_NAME",
    "AM_KIND",
    "AM_MODEL",
    "AM_EFFORT",
    "AM_PROJECT_ID",
    "AM_WORKSPACE_ID",
    "AM_DAEMON_EXE",
    "AM_CONFIG_PATH",
    "AM_DATA_DIR",
    "AM_INSTRUCTIONS_FILE",
    "AM_REAL_HERDR",
    "AM_REAL_CARGO",
];

/// 把 `pane_env` 的結果收成受限 bot 的：只留 [`KEEP_AM`]、[`KEEP_CLAUDE`] 與身分（帳號）的 env，bot 自訂 env 一律丟掉，
/// shim 的 `PATH` 也丟掉（不能開子 agent）；再把可能繼承來的憑證類變數蓋成空的。CLAUDE.md 一律不讀。
pub fn cage_env(env: &mut Value, identity_env: &BTreeMap<String, String>, home: &str) {
    let Some(map) = env.as_object_mut() else { return };
    map.retain(|k, _| KEEP_AM.contains(&k.as_str()) || KEEP_CLAUDE.contains(&k.as_str()));
    // 身分的值重算一次：bot env 可能蓋過同名的鍵，那是使用者自訂，不算數。
    for (k, v) in identity_env {
        if k.starts_with("AM_") || k == "PATH" {
            continue;
        }
        map.insert(k.clone(), json!(crate::config::expand_home(v, home)));
    }
    for k in BLANK {
        map.insert((*k).to_string(), json!(""));
    }
    map.insert("CLAUDE_CODE_DISABLE_CLAUDE_MDS".into(), json!("1"));
}

/// 遠端受限 bot 的 PATH：那台 `remote_path` 照字面展開（`$HOME`／`~` 換成那台的家目錄）＋標準目錄，**不含** shim 目錄
/// （不能開子 agent）。ssh 起的 herdr PATH 只有 `/usr/bin:/bin…`，丟掉 PATH 遠端就找不到 `claude`（R-S2 §1 #9）。
pub fn remote_cage_path(remote_path: &str, home: &str) -> String {
    let mut dirs = am_base::hosts::remote_path_dirs(remote_path, home);
    dirs.extend(["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(String::from));
    dirs.join(":")
}

/// 啟動時重算 [`cage_env`] 要的身分 env（本機）。
pub async fn identity_env(app: &std::sync::Arc<impl CageEnv>, bot: &db::Bot) -> BTreeMap<String, String> {
    match bot.identity.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(idn) => CageEnv::local_identity_env(app, idn).await,
        None => BTreeMap::new(),
    }
}

/// 受限 bot 拿得到的工具（`--tools`）。2026-10-03 實測：`--restricted` 加這份白名單之後工具就只剩這六個。
pub const TOOLS: &str = "Read,Edit,Write,Glob,Grep,WebSearch";

/// 第二層：白名單之外、點名 deny 的（白名單哪天失效也擋得住最危險的那幾個）。
const DENY_TOOLS: &[&str] = &[
    "Bash",
    "WebFetch",
    "NotebookEdit",
    "SendMessage",
    "ListAgents",
    "PushNotification",
    "RemoteTrigger",
    "Agent",
    "Task",
    "Skill",
    "CronCreate",
    "CronDelete",
    "ScheduleWakeup",
    "EnterWorktree",
];

/// 絕對路徑的 permission rule（claude 的寫法是 `//` 開頭：`Edit(//etc/*)`）。
fn abs_rule(tool: &str, path: &str, glob: &str) -> String {
    format!("{tool}(/{}{glob})", path.trim_end_matches('/'))
}

/// 資料夾裡常見的秘密檔：白名單已經把整個資料夾給了它，這幾種再 deny 一層當保險（既有資料夾特別需要）。
const SECRET_FILES: &[&str] = &[".env", ".env.*", "*.pem", "*.key", "*.p12", "*.pfx", "id_*", ".netrc", ".npmrc", ".pypirc", ".git-credentials"];
/// 它的指示檔：讀得到（daemon 也會讀進系統提示），但不准改——不然 end user 叫它改寫自己的指示，下次啟動就生效。
const INSTRUCTION_FILES: &[&str] = &["CLAUDE.md", "AGENTS.md", ".claude/CLAUDE.md"];

/// 蓋掉 daemon 寫給每顆 claude bot 的 settings 裡跟權限有關的部分（hooks／statusLine 照舊）。
///
/// **白名單**（使用者 2026-10-03 裁示）：`dontAsk` 本來就拒絕沒列在 allow 的動作，所以 allow 只放它的資料夾與自己的 outbox，
/// 不再用 deny 去猜要擋哪些目錄。以前那串 deny 有一條 `Read(<data_dir>/*)`：claude 的比對是 gitignore 語意，`*` 比到
/// `shared-bots`／`outbox` 這兩個**目錄**就連底下全擋，deny 又優先於 allow，結果 inbox 讀不到、outbox 寫不進（線上實測）。
/// 現在的 deny 只有工具、資料夾內的秘密檔、指示檔的 Edit，都在資料夾**裡面**，不可能蓋到 inbox 或 outbox。
/// Read 規則也管 Glob／Grep，Edit 規則也管 Write（claude 的權限規則就是這樣分兩類，2.1.288 實測）。
pub fn cage_settings(settings: &mut Value, workspace: &str, env: &Value) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let Some(map) = settings.as_object_mut() else { return };
    let mut allow = vec![abs_rule("Read", workspace, "/**"), abs_rule("Edit", workspace, "/**"), "WebSearch".to_string()];
    if let Some(outbox) = env.get("AM_OUTBOX").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        allow.push(abs_rule("Read", outbox, "/**"));
        allow.push(abs_rule("Edit", outbox, "/**"));
    }
    let mut deny: Vec<String> = DENY_TOOLS.iter().map(|t| t.to_string()).collect();
    for pat in SECRET_FILES {
        deny.push(abs_rule("Read", workspace, &format!("/**/{pat}")));
        deny.push(abs_rule("Edit", workspace, &format!("/**/{pat}")));
    }
    for f in INSTRUCTION_FILES {
        deny.push(abs_rule("Edit", workspace, &format!("/{f}")));
    }
    map.insert(
        "permissions".into(),
        json!({
            "defaultMode": "dontAsk",
            "disableBypassPermissionsMode": "disable",
            "allow": allow,
            "deny": deny,
        }),
    );
    map.insert("remoteControlAtStartup".into(), json!(false));
    map.insert("enableAllProjectMcpServers".into(), json!(false));
    map.remove("skipDangerousModePermissionPrompt");
}

/// 受限 bot 的 argv（daemon 注入的那段之後、model 之前）。不帶 bot 自訂的 args 與身分的 args：
/// 那兩個是一般 bot 用來加旗標的地方（`--dangerously-skip-permissions`、`--remote-control`…），受限 bot 一律不吃。
/// 系統提示走檔案（`--append-system-prompt-file`，2.1.288 實測可用）：herdr 把整行啟動指令壓在 900 bytes 內、
/// 超過就從最長的參數砍尾巴，主人寫的 persona 在最後面，會是被砍掉的那段。
pub fn launch_args(env: &Value, prompt_file: &Path) -> Vec<String> {
    let mut out: Vec<String> = ["--restricted", "--tools", TOOLS, "--strict-mcp-config", "--permission-mode", "dontAsk"].map(String::from).to_vec();
    if let Some(o) = env.get("AM_OUTBOX").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        out.extend(["--add-dir".to_string(), o.to_string()]);
    }
    out.extend(["--append-system-prompt-file".to_string(), prompt_file.to_string_lossy().into_owned()]);
    out
}

/// 系統提示檔的名字（bot 目錄底下）。
pub const PROMPT_FILE: &str = "share-system-prompt.md";

/// 把受限 bot 的系統提示寫進它自己的 bot 目錄（0600；它的檔案工具碰不到 `bots/`，CLI 啟動時自己讀）。
/// 回寫好的路徑字串（遠端就是遠端的路徑）。遠端寫完比對 sha256，不符就是錯誤（不啟動）。
pub async fn install_prompt<T: CageEnv + SiteEnv>(app: &std::sync::Arc<T>, bot: &db::Bot, host: &str, workspace: &str, env: &Value) -> anyhow::Result<String> {
    let outbox = env.get("AM_OUTBOX").and_then(Value::as_str).filter(|s| !s.is_empty());
    if host != LOCAL_HOST {
        let site = remote_site(app.as_ref(), &bot.id).await.map_err(|e| anyhow::anyhow!("遠端分享 bot 的位置：{}", lc_message(&e)))?;
        let folder_md = site.instructions().await.map_err(|e| anyhow::anyhow!("讀不到遠端資料夾的指示：{e}"))?;
        // 寫進遠端時結尾換行會被 shell 吃掉、sha 對不上，所以先去掉。
        let text = system_prompt(workspace, outbox, bot.persona.as_deref(), &folder_md);
        let dir = format!("{}/bots/{}", site.root, bot.id);
        RemoteSite::write_private_files(&site.conn, &dir, &[(PROMPT_FILE, text.trim_end_matches('\n').as_bytes())])
            .await
            .map_err(|e| anyhow::anyhow!("寫不進遠端的系統提示：{e}"))?;
        return Ok(format!("{dir}/{PROMPT_FILE}"));
    }
    let dir = app.bot_dir(&bot.id)?;
    crate::private_files::create_private_dir(&dir)?;
    let path = dir.join(PROMPT_FILE);
    let folder_md = crate::share::folder::instructions(Path::new(workspace));
    crate::lifecycle::setup::write_private(&path, system_prompt(workspace, outbox, bot.persona.as_deref(), &folder_md).as_bytes())?;
    Ok(path.to_string_lossy().into_owned())
}

fn lc_message(e: &LcError) -> String {
    match e {
        LcError::Conflict(v) => v.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("{e:?}")),
        other => format!("{other:?}"),
    }
}

/// `folder_md`＝[`crate::share::folder::instructions`]：資料夾的 CLAUDE.md／AGENTS.md 與 `.claude/memory/`，放在最後
/// （主人寫的 persona 之後），啟動時讀一次。
pub fn system_prompt(workspace: &str, outbox: Option<&str>, persona: Option<&str>, folder_md: &str) -> String {
    let mut p = format!(
        "你是透過分享連結開放給外部使用者的助理。對方的訊息開頭會有「〔分享使用者〕」；對方只看得到你的文字回覆，看不到你的工具過程。\n\
         - 你的工作目錄是 `{workspace}`，這個資料夾就是你能讀寫的全部範圍。對方上傳的檔案放在 `inbox/`，訊息裡會寫出檔名。\n\
         - 你沒有指令列工具（沒有 Bash）、不能抓網頁（沒有 WebFetch，可以用 WebSearch），也不能讀工作目錄以外的檔案；不要嘗試，也不要透露這台機器的路徑或設定。\n\
         - 你的長期記憶在 `{workspace}/memory/`：`MEMORY.md` 是索引（一行一則，連到同目錄的 md 檔）。要記住新的事就在那裡寫一個 md 檔、在 MEMORY.md 加一行；下次啟動時會自動載入（`.claude/memory/` 的舊記憶也會載入，但那裡你寫不進去）。\n"
    );
    match outbox {
        Some(o) => {
            p.push_str(&format!("- 要交給對方的檔案寫進 `{o}`，對方的頁面會列出來讓他下載（一小時後自動清掉）。\n"));
            // share/compose.rs：入口送出 SVG 時把相對路徑的 <image> 嵌成 data URI。
            p.push_str(
                "- 要把對方的照片放進你做的圖（SVG）：用相對於工作目錄的路徑引用，例如 `<image href=\"inbox/檔名.jpeg\" x=\"40\" y=\"40\" width=\"400\" height=\"300\" preserveAspectRatio=\"xMidYMid slice\"/>`，\
                 對方的頁面會自動把照片合成進去（只認工作目錄裡的 JPEG／PNG／WebP／GIF；網址、絕對路徑、`..` 都不會載入）。\
                 先用 Read 看過照片的內容與長寬比再決定框的大小與裁切；要圓角或圓形框就用 `<clipPath>` 包一個 `<rect rx=…>` 或 `<circle>`，再在 `<image>` 加 `clip-path=\"url(#id)\"`。\n",
            );
        }
        None => p.push_str("- 這次沒有可以交檔案給對方的目錄，只能用文字回覆。\n"),
    }
    if let Some(u) = persona.map(str::trim).filter(|s| !s.is_empty()) {
        p.push('\n');
        p.push_str(u);
    }
    if !folder_md.trim().is_empty() {
        p.push_str("\n\n# 這個資料夾的指示與記憶（啟動時從資料夾讀進來的）");
        p.push_str(folder_md);
    }
    p
}

/// 本機的家目錄字串（拿來展開身分 env）。
pub fn local_home() -> String {
    crate::home::dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// 建受限 bot 時要擋掉的請求內容：自訂 args／env 在籠子裡都不會生效，收下來只會讓人以為有用。
pub fn check_create(kind: &str, host: &str, host_known: bool, args: &[String], env: Option<&BTreeMap<String, String>>) -> Result<(), LcError> {
    check_profile(kind, host, host_known)?;
    if !args.is_empty() || env.is_some_and(|e| !e.is_empty()) {
        return Err(LcError::BadValue(json!({
            "error": "bad_request",
            "reason": "restricted_no_custom",
            "message": "分享用的受限 bot 不吃自訂 args／env（啟動時一律由 daemon 決定）",
        })));
    }
    Ok(())
}


/// 受限 bot 沒指定模型時用的：`/api/models?kind=claude` 當下列出的最新 Opus。清單有完整 id（`claude-opus-X-Y`）就挑版本最大的，
/// 只有別名就用 `opus`（claude 自己把它解析成當版最新的 Opus，2.1.288 實測是 claude-opus-5-5）。清單抓不到也退回 `opus`：
/// 不寫死版本號，也不落回帳號預設（線上那顆就是這樣跑成舊版的）。
pub async fn latest_opus(app: &std::sync::Arc<impl CageEnv>, identity: Option<&str>) -> String {
    match CageEnv::claude_models(app, identity).await {
        Ok(v) => latest_opus_in(&v).unwrap_or_else(|| "opus".into()),
        Err(e) => {
            tracing::warn!(error = %e, "cannot list claude models for the restricted bot; using the `opus` alias");
            "opus".into()
        }
    }
}

pub fn latest_opus_in(models: &Value) -> Option<String> {
    let ids = models.get("models")?.as_array()?.iter().filter_map(|m| m.get("id").and_then(Value::as_str));
    let mut best: Option<(Vec<u32>, String)> = None;
    let mut alias = None;
    for id in ids {
        if id == "opus" {
            alias = Some(id.to_string());
            continue;
        }
        let Some(rest) = id.strip_prefix("claude-opus-") else { continue };
        // `claude-opus-5-5`、`claude-opus-4-1-20250805`：日期（8 位數）不算版本。
        let ver: Vec<u32> = rest.split('-').take_while(|p| p.len() < 8).map_while(|p| p.parse().ok()).collect();
        if ver.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(v, _)| ver > *v) {
            best = Some((ver, id.to_string()));
        }
    }
    best.map(|(_, id)| id).or(alias)
}
