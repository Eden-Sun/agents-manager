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
use std::sync::Arc;

use serde_json::{json, Value};

use crate::config::LOCAL_HOST;
use crate::db;
use crate::lifecycle::LcError;
use crate::state::App;

/// 受限 bot 目前只做 claude、只在本機（工作目錄在這台的 data dir 底下）。
pub(crate) fn check_profile(kind: &str, host: &str) -> Result<(), LcError> {
    if kind != "claude" {
        return Err(LcError::conflict(
            "unsupported_kind",
            json!({"kind": kind, "message": "分享用的受限 bot 目前只支援 claude（codex／grok 還沒有對應的籠子）"}),
        ));
    }
    if host != LOCAL_HOST {
        return Err(LcError::conflict(
            "unsupported_host",
            json!({"host": host, "message": "分享用的受限 bot 只能建在本機專案（工作目錄在這台 daemon 的資料目錄底下）"}),
        ));
    }
    Ok(())
}

/// 啟動前：這顆是不是受限 bot。是的話回它的工作目錄（順手補建），而且 kind／host 不合就不准起來。
/// 讀不到 `shared_bots` 一律不啟動——寧可起不來，也不要把受限 bot 當一般 bot 起（fail closed）。
pub(crate) async fn prepare(app: &Arc<App>, bot: &db::Bot, host: &str) -> Result<Option<String>, LcError> {
    let ws = crate::share::store::caged_workspace(&app.db, &bot.id)
        .await
        .map_err(|e| LcError::Upstream(format!("cannot tell whether bot {} is a restricted share bot: {e}", bot.id)))?;
    let Some(ws) = ws else {
        // 信任分享（trusted）不進籠子，照一般 bot 起；但 outbox 一樣不給 gc 清、上傳的 inbox 一樣要在。
        if let Ok(Some(dir)) = crate::share::store::workspace(&app.db, &bot.id).await {
            let _ = crate::share::folder::ensure_inbox(Path::new(&dir));
            crate::outbox::mark_share_keep(&app.data_dir, &bot.id);
        }
        return Ok(None);
    };
    check_profile(&bot.kind, host)?;
    // 資料夾不見了就不起來（不替使用者重建一個空的）；inbox 不存在就建。
    crate::share::folder::ensure_inbox(Path::new(&ws)).map_err(|e| LcError::Upstream(format!("restricted bot folder {ws}: {e}")))?;
    // outbox 不給 AGM 的 gc 清（使用者 2026-10-04）：每次啟動補一次標記，bot 自己刪掉也回得來。
    crate::outbox::mark_share_keep(&app.data_dir, &bot.id);
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
pub(crate) fn cage_env(env: &mut Value, identity_env: &BTreeMap<String, String>, home: &str) {
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

/// 啟動時重算 [`cage_env`] 要的身分 env（本機）。
pub(crate) async fn identity_env(app: &Arc<App>, bot: &db::Bot) -> BTreeMap<String, String> {
    match bot.identity.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(idn) => crate::tools::identity_for_host(app, LOCAL_HOST, idn).await.map(|i| i.env).unwrap_or_default(),
        None => BTreeMap::new(),
    }
}

/// 受限 bot 拿得到的工具（`--tools`）。2026-10-03 實測：`--restricted` 加這份白名單之後工具就只剩這六個。
pub(crate) const TOOLS: &str = "Read,Edit,Write,Glob,Grep,WebSearch";

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
pub(crate) fn cage_settings(settings: &mut Value, workspace: &str, env: &Value) {
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
pub(crate) fn launch_args(env: &Value, prompt_file: &Path) -> Vec<String> {
    let mut out: Vec<String> = ["--restricted", "--tools", TOOLS, "--strict-mcp-config", "--permission-mode", "dontAsk"].map(String::from).to_vec();
    if let Some(o) = env.get("AM_OUTBOX").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        out.extend(["--add-dir".to_string(), o.to_string()]);
    }
    out.extend(["--append-system-prompt-file".to_string(), prompt_file.to_string_lossy().into_owned()]);
    out
}

/// 把受限 bot 的系統提示寫進它自己的 bot 目錄（0600；它的檔案工具碰不到 `bots/`，CLI 啟動時自己讀）。
pub(crate) fn install_prompt(app: &App, bot: &db::Bot, workspace: &str, env: &Value) -> anyhow::Result<PathBuf> {
    let outbox = env.get("AM_OUTBOX").and_then(Value::as_str).filter(|s| !s.is_empty());
    let dir = app.bot_dir(&bot.id)?;
    crate::private_files::create_private_dir(&dir)?;
    let path = dir.join("share-system-prompt.md");
    let folder_md = crate::share::folder::instructions(Path::new(workspace));
    crate::lifecycle::setup::write_private(&path, system_prompt(workspace, outbox, bot.persona.as_deref(), &folder_md).as_bytes())?;
    Ok(path)
}

/// `folder_md`＝[`crate::share::folder::instructions`]：資料夾的 CLAUDE.md／AGENTS.md 與 `.claude/memory/`，放在最後
/// （主人寫的 persona 之後），啟動時讀一次。
pub(crate) fn system_prompt(workspace: &str, outbox: Option<&str>, persona: Option<&str>, folder_md: &str) -> String {
    let mut p = format!(
        "你是透過分享連結開放給外部使用者的助理。對方的訊息開頭會有「〔分享使用者〕」；對方只看得到你的文字回覆，看不到你的工具過程。\n\
         - 你的工作目錄是 `{workspace}`，這個資料夾就是你能讀寫的全部範圍。對方上傳的檔案放在 `inbox/`，訊息裡會寫出檔名。\n\
         - 你沒有指令列工具（沒有 Bash）、不能抓網頁（沒有 WebFetch，可以用 WebSearch），也不能讀工作目錄以外的檔案；不要嘗試，也不要透露這台機器的路徑或設定。\n\
         - 你的長期記憶在 `{workspace}/memory/`：`MEMORY.md` 是索引（一行一則，連到同目錄的 md 檔）。要記住新的事就在那裡寫一個 md 檔、在 MEMORY.md 加一行；下次啟動時會自動載入（`.claude/memory/` 的舊記憶也會載入，但那裡你寫不進去）。\n"
    );
    match outbox {
        Some(o) => p.push_str(&format!("- 要交給對方的檔案寫進 `{o}`，對方的頁面會列出來讓他下載（一小時後自動清掉）。\n")),
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
pub(crate) fn local_home() -> String {
    crate::home::dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// 建受限 bot 時要擋掉的請求內容：自訂 args／env 在籠子裡都不會生效，收下來只會讓人以為有用。
pub(crate) fn check_create(kind: &str, host: &str, args: &[String], env: Option<&BTreeMap<String, String>>) -> Result<(), LcError> {
    check_profile(kind, host)?;
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
pub(crate) async fn latest_opus(app: &Arc<App>, identity: Option<&str>) -> String {
    match crate::models::list(app, LOCAL_HOST, "claude", identity, false).await {
        Ok(v) => latest_opus_in(&v).unwrap_or_else(|| "opus".into()),
        Err(e) => {
            tracing::warn!(error = %e, "cannot list claude models for the restricted bot; using the `opus` alias");
            "opus".into()
        }
    }
}

pub(crate) fn latest_opus_in(models: &Value) -> Option<String> {
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
