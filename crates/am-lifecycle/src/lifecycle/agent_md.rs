//! SPEC §6.5i（使用者 2026-10-01）：bot 讀的指示檔（agent md）只從 `[agents]` 設定來。
//!
//! 以前 claude bot 讀各帳號的 `~/.claude*/CLAUDE.md` 加上 repo 的 CLAUDE.md，codex 讀 AGENTS.md：cc0／cc1／cc2 各一份、
//! 早就不同步（cc1 是舊版、cc2 沒有），codex 讀到的又是另一份。現在三種 CLI 自己找指示檔的機制一律關掉
//! （`CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`、codex `project_doc_max_bytes=0`），內容只有一個來源：設定指到的檔，
//! 接在 AG Man 規則後面注入。子 agent 經 herdr shim 拿同一份（`AM_INSTRUCTIONS_FILE`）。


use crate::config::LOCAL_HOST;
use crate::db;
use crate::hosts::sh_quote;

/// bot 目錄裡給子 agent 讀的那份（AG Man 規則 + agent md，不含母 bot 自己的 persona）。
pub const CHILD_FILE: &str = "instructions.md";
pub const GROK_RULES_FILE: &str = "grok-rules.md";
/// claude 母 bot 的完整啟動指示（AG Man 規則 + agent md + persona）：argv 只帶 `--append-system-prompt-file <這個檔>`，
/// 因為 herdr 把整條指令壓在 900 bytes 內，整份放進 argv 會被砍成「…（後略）」。
pub const PERSONA_FILE: &str = "persona.md";

/// 讀出來的 agent md。`problems` 是讀不到或空的檔：bot 照開，但要讓人看得到少了什麼。
/// `configured`＝`[agents]` 有指定檔：只有這時才關掉 CLI 自己的指示檔——沒設定就維持 CLI 原本的行為，
/// 免得換版之後、設定還沒寫好之前，bot 兩邊都讀不到。
/// `unreadable`＝其中**讀不到**的檔（不含空檔）：只要有一份讀不到，CLI 自己的指示檔就不關（#769 方案 A，派工者決定）——
/// 遠端主機睡著、ssh 逾時的那一刻開 bot，不能變成「兩邊都沒有規則」。
#[derive(Debug, Default)]
pub struct AgentMd {
    pub configured: bool,
    pub text: String,
    pub problems: Vec<String>,
    pub unreadable: Vec<String>,
}

impl AgentMd {
    /// 這次啟動要不要關掉 CLI 自己的指示檔（claude 的 CLAUDE.md、codex 的 AGENTS.md）：設了而且每一份都讀到才關。
    pub fn cli_docs_disabled(&self) -> bool {
        self.configured && self.unreadable.is_empty()
    }
}

/// 依 `[agents]` 讀這個專案的 agent md（全域在前、專案在後）。全域那份在 daemon 這台機器上讀；
/// 專案那份跟 repo 放在一起，在專案所在的主機上讀（遠端走 ssh）。
pub async fn load(app: &(impl crate::capabilities::Cfg + crate::hosts::HostsAccess), project: &db::Project) -> AgentMd {
    let agents = app.cfg().agents_fresh().await;
    let mut reads: Vec<Result<String, String>> = Vec::new();
    if let Some(f) = agents.global_file() {
        reads.push(read_local(f).await.map(|text| source_document(f, LOCAL_HOST, text)));
    }
    let conn = if project.host == LOCAL_HOST { None } else { app.hosts().get(&project.host).await };
    for f in agents.project_files(&project.id, &project.label) {
        reads.push(if project.host == LOCAL_HOST {
            read_local(f).await.map(|text| source_document(f, LOCAL_HOST, text))
        } else {
            match &conn {
                Some(conn) => read_remote(conn, f).await.map(|text| source_document(f, &project.host, text)),
                None => Err(format!("{f}：未知主機 `{}`", project.host)),
            }
        });
    }
    collect(reads)
}


/// Stable per-source fingerprints make startup snapshots identifiable without changing the
/// prompt on every restart. Empty documents must still reach `collect` as empty documents.
fn source_document(path: &str, host: &str, text: String) -> String {
    use sha2::{Digest, Sha256};
    if text.trim().is_empty() { return text; }
    let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
    format!("[AG Man 指示來源：主機 {host}；檔案 {path}；SHA256 {digest}。此為啟動快照，檔案／主幹更新不代表這次 session 已重新載入。]\n\n{text}")
}

async fn read_local(f: &str) -> Result<String, String> {
    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
    let path = crate::config::expand_home(f, &home);
    tokio::fs::read_to_string(&path).await.map_err(|e| format!("{path} 讀不到：{e}"))
}

async fn read_remote(conn: &crate::hosts::HostConn, f: &str) -> Result<String, String> {
    let home = conn.home().await.map_err(|e| format!("{f}（{}）：{e}", conn.name))?;
    let path = crate::config::expand_home(f, &home);
    let script = format!("F={}\nif [ -r \"$F\" ]; then printf 'AM_MD_OK\\n'; cat \"$F\"; else printf 'AM_MD_MISSING\\n'; fi\n", sh_quote(&path));
    let out = conn.ssh_exec(&script).await.map_err(|e| format!("{path}（{}）讀不到：{e}", conn.name))?;
    match out.split_once('\n') {
        Some(("AM_MD_OK", body)) => Ok(body.to_string()),
        _ => Err(format!("{path}（{}）讀不到", conn.name)),
    }
}

fn collect(reads: Vec<Result<String, String>>) -> AgentMd {
    let mut out = AgentMd { configured: !reads.is_empty(), ..Default::default() };
    let mut parts: Vec<String> = Vec::new();
    for r in reads {
        match r {
            Ok(t) if !t.trim().is_empty() => parts.push(t.trim().to_string()),
            Ok(_) => out.problems.push("有一份 agent md 是空檔".into()),
            Err(e) => {
                out.unreadable.push(e.clone());
                out.problems.push(e);
            }
        }
    }
    out.text = parts.join("\n\n");
    out
}

/// agent md 有指示檔讀不到（#769）：除了對話裡那則 system 訊息，也推一則 `ops_alert` 進 AGM inbox（`source=daemon`）。
/// 去重鍵含 bot、讀不到的內容與「小時」：同一顆 bot 同一個問題一小時內只推一則，持續讀不到下一個小時會再提醒。推不進去只記 log。
pub async fn push_unreadable_alert(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), bot: &db::Bot, unreadable: &[String]) {
    let detail = unreadable.join("；");
    let hour = chrono::Utc::now().format("%Y%m%d%H");
    let key = format!(
        "ops_alert:daemon:agent_md_unreadable:{}:{}:{hour}",
        bot.id,
        crate::supervisor_inbox::short_hash(detail.as_bytes()),
    );
    let payload = serde_json::json!({
        "source": "daemon",
        "reason": "agent_md_unreadable",
        "subject": bot.name,
        "bot_id": bot.id,
        "bot_name": bot.name,
        "detail": format!("bot `{}`（{}）啟動時有 agent md 讀不到：{detail}", bot.name, bot.id),
        "action": "到設定的指示檔所在主機確認檔案存在、ssh／網路通（遠端專案的檔在專案那台主機上讀）。這次啟動 daemon 沒有關掉 CLI 自己的 CLAUDE.md／AGENTS.md，bot 仍讀得到 repo 的規則，但沒有帶到 [agents] 設定的那份；修好後重啟 bot 才會帶上。",
    });
    match crate::supervisor_inbox::push_inbox(app.db(), &key, "ops_alert", None, Some(&bot.id), None, &payload).await {
        Ok(Some(_)) => app.emit("supervisor_changed", serde_json::json!({ "ops_alert": key })).await,
        Ok(None) => {}
        Err(e) => tracing::warn!(bot = %bot.name, error = %e, "could not queue the agent md ops_alert"),
    }
}

/// 規則與 agent md 接成一份：bot 的 persona 參數與子 agent 的檔都用這個順序。
pub fn compose(rules: &str, md: &str) -> String {
    if md.trim().is_empty() {
        rules.to_string()
    } else {
        format!("{rules}\n\n{md}")
    }
}

/// 把子 agent 讀的那份寫進 bot 目錄（`<shim_dir>/..`），回傳路徑給 pane env 的 `AM_INSTRUCTIONS_FILE`。
/// 寫不進去只警告：子 agent 少了這份，母 bot 照開。
pub async fn install(app: &impl crate::hosts::HostsAccess, bot: &db::Bot, project: &db::Project, shim_dir: Option<&str>, text: &str) -> Option<String> {
    install_named(app, bot, project, shim_dir, CHILD_FILE, text).await
}

/// grok has no rules-file flag. Stage the full startup instructions here and pass only this short path through `--rules`.
pub async fn install_grok_rules(
    app: &impl crate::hosts::HostsAccess,
    bot: &db::Bot,
    project: &db::Project,
    shim_dir: Option<&str>,
    text: &str,
) -> Option<String> {
    install_named(app, bot, project, shim_dir, GROK_RULES_FILE, text).await
}

/// claude 母 bot 的完整 persona 寫進 bot 目錄（同 [`install_grok_rules`]）；回傳路徑給 `--append-system-prompt-file`。
pub async fn install_persona(
    app: &impl crate::hosts::HostsAccess,
    bot: &db::Bot,
    project: &db::Project,
    shim_dir: Option<&str>,
    text: &str,
) -> Option<String> {
    install_named(app, bot, project, shim_dir, PERSONA_FILE, text).await
}

/// 母 bot 的 codex profile 名：`am-bot-` 前綴跟 shim 的 `am-child-` 分開（shim 的 sweep 只清 `am-child-*`）。
/// bot id 只留 `[A-Za-z0-9_-]`，其餘換成 `_`（跟 shim 的 `tr -c 'A-Za-z0-9_-' '_'` 同一招）。
pub fn codex_profile_name(bot_id: &str) -> String {
    let id: String = bot_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    format!("am-bot-{id}")
}

/// Only this safe diagnostic crosses the startup boundary; parser/SSH output may contain trust values.
#[derive(Debug)]
pub struct ProfileError(String);

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ProfileError {}

fn profile_error(path: &str, detail: &str) -> ProfileError {
    ProfileError(format!(
        "Codex profile {path}：{detail}；拒絕以過期指示啟動"
    ))
}

fn patch_codex_profile(
    path: &str,
    previous: &str,
    instructions: &str,
) -> Result<String, ProfileError> {
    let parsed = toml_edit::ImDocument::parse(previous).map_err(|error| {
        let byte = error
            .span()
            .map(|span| span.start)
            .unwrap_or(0)
            .min(previous.len());
        let line = previous.as_bytes()[..byte]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1;
        // TomlError's Display/Debug includes the source; only expose a safe location.
        profile_error(
            path,
            &format!("第 {line} 行無法解析；請修好這個檔或刪掉它，就會重新產生"),
        )
    })?;
    let original_span = parsed
        .get("developer_instructions")
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::span);
    let mut doc = parsed.into_mut();
    match doc.get_mut("developer_instructions") {
        Some(toml_edit::Item::Value(value)) if value.as_str().is_some() => {
            if value.as_str() == Some(instructions) {
                return Ok(previous.to_owned());
            }
            let decor = value.decor().clone();
            let mut replacement = toml_edit::Value::from(instructions);
            *replacement.decor_mut() = decor;
            *value = replacement;
        }
        Some(_) => {
            return Err(profile_error(
                path,
                "developer_instructions 必須是字串；請修好這個檔或刪掉它，就會重新產生",
            ))
        }
        None => {
            doc.insert("developer_instructions", toml_edit::value(instructions));
        }
    }
    // DocumentMut's whole-document formatter normalizes CRLF. Render only the edited value;
    // the original byte slices retain all other nodes, comments, decor, ordering and line endings.
    let mut edited = doc["developer_instructions"].as_value().unwrap().clone();
    edited.decor_mut().clear();
    let token = edited.to_string();
    if let Some(span) = original_span {
        let prefix = previous
            .get(..span.start)
            .ok_or_else(|| profile_error(path, "無法安全定位指示欄位，請檢查檔案後重試"))?;
        let suffix = previous
            .get(span.end..)
            .ok_or_else(|| profile_error(path, "無法安全定位指示欄位，請檢查檔案後重試"))?;
        Ok(format!("{prefix}{token}{suffix}"))
    } else {
        // A new root key must precede existing tables. Keep every pre-existing byte in order.
        let newline = if previous.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let (bom, body) = previous
            .strip_prefix('\u{feff}')
            .map_or(("", previous), |s| ("\u{feff}", s));
        Ok(format!(
            "{bom}developer_instructions = {token}{newline}{body}"
        ))
    }
}

fn local_profile_snapshot(path: &std::path::Path) -> Result<String, ProfileError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(_) => Err(profile_error(
            &path.display().to_string(),
            "無法讀取，請確認檔案權限與編碼後重試",
        )),
    }
}

fn remote_profile_read_script(path: &str) -> String {
    format!(
        "set -eu\nF={}\nif [ -e \"$F\" ]; then\n  if [ ! -f \"$F\" ] || [ ! -r \"$F\" ]; then printf 'AM_PROFILE_READ_ERROR\\n'; exit 0; fi\n  printf 'AM_PROFILE_PRESENT\\n'\n  base64 < \"$F\"\n  printf '\\nAM_PROFILE_END\\n'\nelif [ -L \"$F\" ]; then\n  printf 'AM_PROFILE_READ_ERROR\\n'\nelse\n  printf 'AM_PROFILE_MISSING\\n'\nfi\n",
        sh_quote(path),
    )
}

fn decode_remote_profile(path: &str, reply: &str) -> Result<String, ProfileError> {
    use base64::Engine as _;
    if reply == "AM_PROFILE_MISSING\n" {
        return Ok(String::new());
    }
    let decode_error =
        || profile_error(path, "遠端讀取或傳輸失敗，請確認連線、檔案權限與編碼後重試");
    let encoded = reply
        .strip_prefix("AM_PROFILE_PRESENT\n")
        .and_then(|body| body.strip_suffix("\nAM_PROFILE_END\n"))
        .ok_or_else(decode_error)?;
    // The ASCII envelope avoids lossy UTF-8 conversion in the SSH transport.
    let compact: String = encoded.split_whitespace().collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|_| decode_error())?;
    String::from_utf8(bytes).map_err(|_| decode_error())
}

fn remote_profile_write_script(path: &str, content: &str) -> String {
    let temporary = format!("{path}.tmp-{}", db::ulid());
    format!(
        "set -eu\numask 077\nF={}\nT={}\nmkdir -p \"$(dirname \"$F\")\"\ntrap 'rm -f \"$T\"' 0\n(set -C; : > \"$T\")\nprintf '%s' {} > \"$T\"\nif cmp -s \"$T\" \"$F\" 2>/dev/null; then rm -f \"$T\"; else mv \"$T\" \"$F\"; fi\nchmod 600 \"$F\"\nprintf 'AM_PROFILE_WRITTEN\\n'\n",
        sh_quote(path), sh_quote(&temporary), sh_quote(content),
    )
}

pub async fn install_codex_profile(
    app: &impl crate::hosts::HostsAccess,
    bot: &db::Bot,
    project: &db::Project,
    codex_home: &str,
    text: &str,
) -> Result<String, ProfileError> {
    let name = codex_profile_name(&bot.id);
    let path = format!("{}/{name}.config.toml", codex_home.trim_end_matches('/'));
    let shown_path = if project.host == LOCAL_HOST {
        path.clone()
    } else {
        format!("{}:./{name}.config.toml（相對 CODEX_HOME）", project.host)
    };
    if project.host == LOCAL_HOST {
        let previous = local_profile_snapshot(std::path::Path::new(&path))?;
        let updated = patch_codex_profile(&shown_path, &previous, text)?;
        if previous != updated {
            write_private_atomic(std::path::Path::new(&path), &updated).map_err(|_| {
                profile_error(&shown_path, "無法原子寫入，請確認目錄權限與可用空間後重試")
            })?;
        }
    } else {
        let conn = app
            .hosts()
            .get(&project.host)
            .await
            .ok_or_else(|| profile_error(&shown_path, "遠端主機不可用，請確認連線後重試"))?;
        let reply = conn
            .ssh_exec(&remote_profile_read_script(&path))
            .await
            .map_err(|_| profile_error(&shown_path, "遠端讀取或傳輸失敗，請確認連線後重試"))?;
        let previous = decode_remote_profile(&shown_path, &reply)?;
        let updated = patch_codex_profile(&shown_path, &previous, text)?;
        if previous != updated {
            let reply = conn
                .ssh_exec(&remote_profile_write_script(&path, &updated))
                .await
                .map_err(|_| {
                    profile_error(&shown_path, "遠端原子寫入失敗，請確認連線與目錄權限後重試")
                })?;
            if reply != "AM_PROFILE_WRITTEN\n" {
                return Err(profile_error(
                    &shown_path,
                    "遠端寫入未確認，請檢查連線後重試",
                ));
            }
        }
    }
    Ok(name)
}

/// 同目錄暫存檔（0600 建立）＋ rename：不會有人讀到寫一半的檔，也不會有先 0644 再 chmod 的空窗。
fn write_private_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("profile");
    let tmp = dir.join(format!(".{name}.tmp-{}", db::ulid()));
    let result = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(content.as_bytes())?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

async fn install_named(
    app: &impl crate::hosts::HostsAccess,
    bot: &db::Bot,
    project: &db::Project,
    shim_dir: Option<&str>,
    filename: &str,
    text: &str,
) -> Option<String> {
    let bot_dir = std::path::Path::new(shim_dir?).parent()?.to_string_lossy().into_owned();
    let path = format!("{bot_dir}/{filename}");
    let result = if project.host == LOCAL_HOST {
        crate::shim_refresh::write_atomic(std::path::Path::new(&path), text).map(|_| ()).map_err(anyhow::Error::from)
    } else {
        match app.hosts().get(&project.host).await {
            Some(conn) => install_remote(&conn, &path, text).await,
            None => Err(anyhow::anyhow!("unknown host `{}`", project.host)),
        }
    };
    match result {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not write the child instructions file");
            None
        }
    }
}

const REMOTE_EOF: &str = "AM_AGENT_MD_EOF";

async fn install_remote(conn: &crate::hosts::HostConn, path: &str, text: &str) -> anyhow::Result<()> {
    install_remote_mode(conn, path, text, None).await
}

async fn install_remote_mode(conn: &crate::hosts::HostConn, path: &str, text: &str, mode: Option<&str>) -> anyhow::Result<()> {
    if text.lines().any(|l| l == REMOTE_EOF) {
        anyhow::bail!("instructions contain the heredoc terminator `{REMOTE_EOF}`");
    }
    let script = remote_script_mode(path, text, mode);
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_AGENT_MD_OK") {
        anyhow::bail!("remote instructions install did not confirm:\n{}", out.trim());
    }
    Ok(())
}

/// 暫存檔 + `cmp`：內容一樣就不動 mtime，跟 herdr skill 的遠端安裝同一招。
#[cfg_attr(not(test), allow(dead_code))]
fn remote_script(path: &str, text: &str) -> String {
    remote_script_mode(path, text, None)
}

/// `mode` 有值（例如 `600`）時用 `umask 077` 建暫存檔，並在換上之後把目標也 chmod 成該權限（內容一樣不換檔時照樣修權限）。
fn remote_script_mode(path: &str, text: &str, mode: Option<&str>) -> String {
    let (umask, chmod) = match mode {
        Some(m) => ("umask 077\n".to_string(), format!("chmod {} \"$F\"\n", sh_quote(m))),
        None => (String::new(), String::new()),
    };
    format!(
        "set -e\n{umask}F={f}\nmkdir -p \"$(dirname \"$F\")\"\ncat > \"$F.new\" <<'{REMOTE_EOF}'\n{text}\n{REMOTE_EOF}\nif cmp -s \"$F.new\" \"$F\" 2>/dev/null; then rm -f \"$F.new\"; else mv \"$F.new\" \"$F\"; fi\n{chmod}printf 'AM_AGENT_MD_OK\\n'\n",
        f = sh_quote(path),
        text = text.trim_end(),
    )
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;

    #[test]
    fn source_fingerprints_identify_the_loaded_snapshot_without_volatile_timestamps() {
        let text = source_document("~/repo/AGENTS.md", "m4p", "rules v1".into());
        assert!(text.contains("主機 m4p；檔案 ~/repo/AGENTS.md；SHA256 "));
        assert!(text.ends_with("\n\nrules v1"));
        assert_eq!(text, source_document("~/repo/AGENTS.md", "m4p", "rules v1".into()));
        assert_ne!(text, source_document("~/repo/AGENTS.md", "m4p", "rules v2".into()));
        assert!(text.contains("不代表這次 session 已重新載入"));
    }

    #[test]
    fn source_annotations_do_not_hide_empty_or_missing_instruction_files() {
        let md = collect(vec![Ok(source_document("empty.md", "local", " \n".into())), Err("missing.md".into())]);
        assert!(md.text.is_empty());
        assert_eq!(md.problems.len(), 2);
        assert_eq!(md.unreadable, vec!["missing.md"]);
        assert!(!md.cli_docs_disabled());
    }

    #[test]
    fn the_project_file_is_found_by_id_before_label() {
        let mut cfg = crate::config::AgentsCfg { instructions_file: Some("~/g.md".into()), ..Default::default() };
        assert_eq!(cfg.global_file(), Some("~/g.md"));
        use crate::config::AgentMdFiles;
        cfg.projects.insert("proj".into(), AgentMdFiles::One("/by-label.md".into()));
        assert_eq!(cfg.project_files("01P", "proj"), vec!["/by-label.md"]);
        cfg.projects.insert("01P".into(), AgentMdFiles::Many(vec!["/a.md".into(), " ".into(), "/b.md".into()]));
        assert_eq!(cfg.project_files("01P", "proj"), vec!["/a.md", "/b.md"], "id 優先、空白項略過、順序照寫");
        assert_eq!(crate::config::AgentsCfg::default().global_file(), None);
        assert!(crate::config::AgentsCfg::default().project_files("01P", "proj").is_empty());
    }

    #[test]
    fn the_section_round_trips_and_is_omitted_when_unset() {
        let text = "[agents]\ninstructions_file = \"~/.config/agents-manager/agents/global.md\"\n\n[agents.projects]\nagents-manager = \"/repo/CLAUDE.md\"\npt = [\"~/pt/CLAUDE.md\", \"~/pt/AGENTS.md\"]\n";
        let cfg: crate::config::ConfigFile = toml::from_str(text).unwrap();
        assert_eq!(cfg.agents.global_file(), Some("~/.config/agents-manager/agents/global.md"));
        assert_eq!(cfg.agents.project_files("x", "agents-manager"), vec!["/repo/CLAUDE.md"]);
        assert_eq!(cfg.agents.project_files("x", "pt"), vec!["~/pt/CLAUDE.md", "~/pt/AGENTS.md"]);
        let back = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(toml::from_str::<crate::config::ConfigFile>(&back).unwrap().agents, cfg.agents);
        assert!(!toml::to_string_pretty(&crate::config::ConfigFile::default()).unwrap().contains("[agents]"));
    }

    #[tokio::test]
    async fn unset_means_not_configured_and_a_missing_file_is_reported() {
        let none = collect(vec![]);
        assert!(!none.configured && none.text.is_empty() && none.problems.is_empty());
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-agent-md-{}", std::process::id())));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("g.md");
        std::fs::write(&ok, "GLOBAL\n").unwrap();
        let got = collect(vec![read_local(ok.to_str().unwrap()).await, read_local(dir.join("nope.md").to_str().unwrap()).await]);
        assert!(got.configured, "設了但讀不到也算設了：讀得到的照常注入，問題寫進對話");
        assert_eq!(got.text, "GLOBAL");
        assert_eq!(got.problems.len(), 1, "{:?}", got.problems);
        assert_eq!(got.unreadable.len(), 1, "{:?}", got.unreadable);
        assert!(!got.cli_docs_disabled(), "有一份讀不到：CLI 自己的指示檔不關（#769 方案 A），不然 bot 兩邊都沒規則");
        // 每一份都讀到才關；空檔不是「讀不到」（檔在、只是沒內容），照舊關。
        let all = collect(vec![read_local(ok.to_str().unwrap()).await]);
        assert!(all.cli_docs_disabled());
        let empty = collect(vec![Ok("GLOBAL".into()), Ok("  \n".into())]);
        assert!(empty.problems.len() == 1 && empty.unreadable.is_empty() && empty.cli_docs_disabled(), "{empty:?}");
        assert!(!collect(vec![]).cli_docs_disabled(), "沒設定本來就不關");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 專案那份跟 repo 放在一起：遠端專案到那台主機上讀，`~` 用那台的 HOME 展開；讀不到要講出主機名。
    #[tokio::test]
    async fn a_remote_projects_file_is_read_on_its_host() {
        let host = "agent-md-box";
        let env = crate::testing::env().await;
        let cfg = crate::config::HostCfg { daemon_url: None, name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some("/Users/x".into());
        crate::hosts::set_ssh_fake(host, |script| {
            Ok(if script.contains("F='/Users/x/repo/CLAUDE.md'") { "AM_MD_OK\nREMOTE RULES\n".into() } else { "AM_MD_MISSING\n".into() })
        });
        assert_eq!(read_remote(&conn, "~/repo/CLAUDE.md").await.unwrap().trim(), "REMOTE RULES");
        let err = read_remote(&conn, "/nope.md").await.unwrap_err();
        assert!(err.contains("/nope.md") && err.contains(host), "{err}");
    }

    #[test]
    fn compose_puts_rules_first_and_skips_an_empty_md() {
        assert_eq!(compose("R", ""), "R");
        assert_eq!(compose("R", "M"), "R\n\nM");
    }

    #[test]
    fn the_remote_script_writes_atomically_and_quotes_the_path() {
        let s = remote_script("/home/u/bots/b 1/instructions.md", "line 'one'\n");
        assert!(s.contains("F='/home/u/bots/b 1/instructions.md'"), "{s}");
        assert!(s.contains("<<'AM_AGENT_MD_EOF'\nline 'one'\nAM_AGENT_MD_EOF\n"), "{s}");
        assert!(s.contains("mv \"$F.new\" \"$F\""));
    }

    mod profile_tests {
        use super::*;
        use crate::capabilities::Db as _;
        use base64::Engine as _;
        use std::sync::{Arc, Mutex};

        const INITIAL: &str = "# retain header\ndeveloper_instructions = \"old\" # retain inline\nmodel = \"fixture-model\"\n\n# retain table\n[hooks.state.\"file:fixture:session_start:0:0\"]\ntrusted_hash = \"fixture-trust-no-secret\"\nenabled = true\n# retain trailer\n\n";
        const PLACEHOLDER: &str = "/__AM_PROFILE_FIXTURE__/profile.toml";

        fn framed(text: &[u8]) -> String {
            format!(
                "AM_PROFILE_PRESENT\n{}\nAM_PROFILE_END\n",
                base64::engine::general_purpose::STANDARD.encode(text)
            )
        }
        fn shell(script: &str) -> String {
            let out = std::process::Command::new("sh")
                .args(["-c", script])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "profile script failed with {:?}",
                out.status.code()
            );
            String::from_utf8(out.stdout).unwrap()
        }

        #[test]
        fn profile_patch_changes_only_the_instruction_value_preserving_all_other_bytes() {
            let updated = patch_codex_profile("fixture.toml", INITIAL, "new").unwrap();
            assert_eq!(
                updated,
                INITIAL.replace(
                    "developer_instructions = \"old\"",
                    "developer_instructions = \"new\""
                )
            );
            assert_eq!(
                patch_codex_profile("fixture.toml", INITIAL, "old").unwrap(),
                INITIAL
            );
            for original in [INITIAL.replace('\n', "\r\n"), INITIAL.trim_end().to_owned()] {
                let expected = original.replace(
                    "developer_instructions = \"old\"",
                    "developer_instructions = \"new\"",
                );
                assert_eq!(
                    patch_codex_profile("fixture.toml", &original, "new").unwrap(),
                    expected
                );
            }

            let new = patch_codex_profile(
                "fixture.toml",
                "# header\nmodel = \"fixture\"\n",
                "line 'one'\nline \\\"two\"",
            )
            .unwrap();
            let doc: toml_edit::DocumentMut = new.parse().unwrap();
            assert_eq!(
                doc["developer_instructions"].as_str(),
                Some("line 'one'\nline \\\"two\"")
            );
            assert!(new.contains("# header\nmodel = \"fixture\"\n"));
            assert!(
                doc.get("hooks").is_none(),
                "daemon must not generate hooks.state"
            );
            assert!(!new.contains("trusted_hash"));
        }

        #[test]
        fn profile_parse_errors_name_the_path_and_line_but_never_echo_source() {
            let bad = "developer_instructions = \"old\"\n# trusted_hash = \"FIXTURE_DO_NOT_ECHO\"\n[broken =\n";
            let error = patch_codex_profile("/fixture/profile.toml", bad, "new").unwrap_err();
            let display = error.to_string();
            assert!(display.contains("/fixture/profile.toml"));
            assert!(display.contains("第 3 行"), "{display}");
            assert!(display.contains("修好這個檔或刪掉它，就會重新產生"));
            assert!(!format!("{error:?} {error}").contains("FIXTURE_DO_NOT_ECHO"));
            for bad in [
                "developer_instructions = 7\n",
                "[developer_instructions]\nold = true\n",
            ] {
                assert!(patch_codex_profile("fixture.toml", bad, "new").is_err());
            }
        }

        #[tokio::test]
        async fn profile_local_install_preserves_trust_and_keeps_invalid_files_unchanged() {
            let env = crate::testing::env().await;
            let mut bot = crate::testing::claude_bot(&env.app, &env.project_id, "profile-local").await;
            bot.kind = "codex".into();
            let project = db::project(env.app.db(), &env.project_id)
                .await
                .unwrap()
                .unwrap();
            let home = env.dir.join("codex-profile");
            std::fs::create_dir_all(&home).unwrap();
            let path = home.join(format!("{}.config.toml", codex_profile_name(&bot.id)));
            std::fs::write(&path, INITIAL).unwrap();
            let name = install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                .await
                .unwrap();
            assert_eq!(name, codex_profile_name(&bot.id));
            let expected = patch_codex_profile(path.to_str().unwrap(), INITIAL, "new").unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                .await
                .unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                modified
            );
            let broken = "# FIXTURE_DO_NOT_ECHO\n[broken =\n";
            std::fs::write(&path, broken).unwrap();
            let error = install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("FIXTURE_DO_NOT_ECHO"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
            std::fs::write(&path, [0xff, 0xfe]).unwrap();
            assert!(
                install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), vec![0xff, 0xfe]);
        }

        #[tokio::test]
        async fn profile_local_missing_is_created_but_other_read_errors_are_not_missing() {
            let env = crate::testing::env().await;
            let bot = crate::testing::claude_bot(&env.app, &env.project_id, "profile-missing").await;
            let project = db::project(env.app.db(), &env.project_id)
                .await
                .unwrap()
                .unwrap();
            let home = env.dir.join("new-codex");
            install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                .await
                .unwrap();
            let path = home.join(format!("{}.config.toml", codex_profile_name(&bot.id)));
            let contents = std::fs::read_to_string(&path).unwrap();
            assert_eq!(contents, patch_codex_profile("fresh", "", "new").unwrap());
            assert!(!contents.contains("hooks") && !contents.contains("trusted_hash"));
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(
                local_profile_snapshot(&home).is_err(),
                "directory read error is not NotFound"
            );
        }

        #[test]
        fn profile_remote_envelope_requires_explicit_missing_or_complete_valid_utf8() {
            assert_eq!(
                decode_remote_profile("fixture", "AM_PROFILE_MISSING\n").unwrap(),
                ""
            );
            assert_eq!(
                decode_remote_profile("fixture", &framed(INITIAL.as_bytes())).unwrap(),
                INITIAL
            );
            for reply in [
                "",
                "AM_PROFILE_MISSING",
                "AM_PROFILE_MISSING\nextra",
                "AM_PROFILE_READ_ERROR\n",
                "AM_PROFILE_PRESENT\n",
                "AM_PROFILE_PRESENT\nnot-base64\nAM_PROFILE_END\n",
            ] {
                assert!(decode_remote_profile("fixture", reply).is_err());
            }
            assert!(decode_remote_profile("fixture", &framed(&[0xff, 0xfe])).is_err());
        }

        #[tokio::test]
        async fn profile_remote_failures_never_publish_or_become_missing() {
            let env = crate::testing::env().await;
            let bot =
                crate::testing::claude_bot(&env.app, &env.project_id, "profile-remote-failure").await;
            let mut project = db::project(env.app.db(), &env.project_id)
                .await
                .unwrap()
                .unwrap();
            let host = "profile-remote-failure";
            project.host = host.into();
            let cfg = crate::config::HostCfg { daemon_url: None,
                name: host.into(),
                ssh: host.into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
                shared_session: false,
            };
            env.app.hosts.insert_remote_for_test(cfg).await;
            for failure in [
                None,
                Some("AM_PROFILE_READ_ERROR\n".into()),
                Some("AM_PROFILE_PRESENT\nYWJj".into()),
                Some(framed(b"# FIXTURE_DO_NOT_ECHO\n[broken =\n")),
            ] {
                let writes = Arc::new(Mutex::new(0));
                let observed = writes.clone();
                crate::hosts::set_ssh_fake(host, move |script| {
                    if script.contains("AM_PROFILE_WRITTEN") {
                        *observed.lock().unwrap() += 1;
                        return Ok("AM_PROFILE_WRITTEN\n".into());
                    }
                    match &failure {
                        Some(reply) => Ok(reply.clone()),
                        None => Err(anyhow::anyhow!("FIXTURE_DO_NOT_ECHO transport")),
                    }
                });
                let error = install_codex_profile(&*env.app, &bot, &project, "/fixture/codex", "new")
                    .await
                    .unwrap_err();
                assert!(!format!("{error:?} {error}").contains("FIXTURE_DO_NOT_ECHO"));
                assert!(error
                    .to_string()
                    .contains("profile-remote-failure:./am-bot-"));
                assert!(!error.to_string().contains("/fixture/codex"));
                assert!(!error.to_string().contains("trusted_hash"));
                assert!(!error.to_string().contains("hooks.state"));
                assert_eq!(*writes.lock().unwrap(), 0);
            }
        }

        #[tokio::test]
        async fn profile_remote_existing_and_explicit_missing_take_distinct_safe_paths() {
            let env = crate::testing::env().await;
            let bot =
                crate::testing::claude_bot(&env.app, &env.project_id, "profile-remote-success").await;
            let mut project = db::project(env.app.db(), &env.project_id)
                .await
                .unwrap()
                .unwrap();
            let host = "profile-remote-success";
            project.host = host.into();
            let cfg = crate::config::HostCfg { daemon_url: None,
                name: host.into(),
                ssh: host.into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "agents-manager".into(),
                remote_path: String::new(),
                shared_session: false,
            };
            env.app.hosts.insert_remote_for_test(cfg).await;
            for previous in [None, Some(INITIAL)] {
                let scripts = Arc::new(Mutex::new(Vec::new()));
                let observed = scripts.clone();
                crate::hosts::set_ssh_fake(host, move |script| {
                    observed.lock().unwrap().push(script.to_owned());
                    if script.contains("AM_PROFILE_WRITTEN") {
                        Ok("AM_PROFILE_WRITTEN\n".into())
                    } else {
                        Ok(previous
                            .map(|s| framed(s.as_bytes()))
                            .unwrap_or_else(|| "AM_PROFILE_MISSING\n".into()))
                    }
                });
                install_codex_profile(&*env.app, &bot, &project, "/fixture/codex", "new")
                    .await
                    .unwrap();
                let calls = scripts.lock().unwrap();
                assert_eq!(calls.len(), 2);
                assert!(calls[0].contains("AM_PROFILE_PRESENT"));
                assert!(calls[1].contains("AM_PROFILE_WRITTEN"));
                assert_eq!(
                    calls[1].contains("fixture-trust-no-secret"),
                    previous.is_some()
                );
                if previous.is_none() {
                    assert!(!calls[1].contains("hooks.state") && !calls[1].contains("trusted_hash"));
                }
            }
        }

        #[tokio::test]
        async fn macos_local_profile_scripts_preserve_exact_bytes_and_use_portable_tools() {
            let env = crate::testing::env().await;
            let path = env.dir.join("profile ' spaced.toml");
            let path = path.to_str().unwrap();
            assert_eq!(
                shell(&remote_profile_read_script(path)),
                "AM_PROFILE_MISSING\n"
            );
            let updated = patch_codex_profile(path, INITIAL, "new\nAM_AGENT_MD_EOF\nend").unwrap();
            assert_eq!(
                shell(&remote_profile_write_script(path, &updated)),
                "AM_PROFILE_WRITTEN\n"
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), updated);
            assert_eq!(
                decode_remote_profile(path, &shell(&remote_profile_read_script(path))).unwrap(),
                updated
            );
            let modified = std::fs::metadata(path).unwrap().modified().unwrap();
            shell(&remote_profile_write_script(path, &updated));
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                modified
            );
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let blocked = env.dir.join("directory.toml");
            std::fs::create_dir(&blocked).unwrap();
            assert_eq!(
                shell(&remote_profile_read_script(blocked.to_str().unwrap())),
                "AM_PROFILE_READ_ERROR\n"
            );
            assert!(std::fs::read_dir(&env.dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")));
        }

        #[test]
        fn profile_portable_verification_fixture_is_from_the_production_helpers() {
            let updated = patch_codex_profile(PLACEHOLDER, INITIAL, "new").unwrap();
            let fresh = patch_codex_profile(PLACEHOLDER, "", "new").unwrap();
            let fixture = serde_json::json!({
                "placeholder": PLACEHOLDER, "initial": INITIAL, "updated": updated, "fresh": fresh,
                "read_script": remote_profile_read_script(PLACEHOLDER),
                "write_script": remote_profile_write_script(PLACEHOLDER, &updated),
                "fresh_script": remote_profile_write_script(PLACEHOLDER, &fresh),
            });
            println!("PROFILE_VERIFY_FIXTURE={fixture}");
            assert!(!fixture["fresh"].as_str().unwrap().contains("trusted_hash"));
        }
    }
}
