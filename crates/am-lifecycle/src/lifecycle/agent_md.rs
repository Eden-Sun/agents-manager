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
#[derive(Debug, Default)]
pub struct AgentMd {
    pub configured: bool,
    pub text: String,
    pub problems: Vec<String>,
}

/// 依 `[agents]` 讀這個專案的 agent md（全域在前、專案在後）。全域那份在 daemon 這台機器上讀；
/// 專案那份跟 repo 放在一起，在專案所在的主機上讀（遠端走 ssh）。
pub async fn load(app: &(impl crate::capabilities::Cfg + crate::hosts::HostsAccess), project: &db::Project) -> AgentMd {
    let agents = app.cfg().agents_fresh().await;
    let mut reads: Vec<Result<String, String>> = Vec::new();
    if let Some(f) = agents.global_file() {
        reads.push(read_local(f).await);
    }
    let conn = if project.host == LOCAL_HOST { None } else { app.hosts().get(&project.host).await };
    for f in agents.project_files(&project.id, &project.label) {
        reads.push(if project.host == LOCAL_HOST {
            read_local(f).await
        } else {
            match &conn {
                Some(conn) => read_remote(conn, f).await,
                None => Err(format!("{f}：未知主機 `{}`", project.host)),
            }
        });
    }
    collect(reads)
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
            Err(e) => out.problems.push(e),
        }
    }
    out.text = parts.join("\n\n");
    out
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

/// codex 沒有「從檔案讀 developer_instructions」的參數：寫成 `<codex_home>/<profile>.config.toml`（0600，內容是 AG Man 的規則），
/// argv 只帶 `-p <profile>`。跟 shim 的 `am_codex_instructions_profile` 是同一種檔，只是名字前綴不同。
/// 本機或遠端依 `project.host`；回傳 profile 名，寫不進去回 None。
pub async fn install_codex_profile(
    app: &impl crate::hosts::HostsAccess,
    bot: &db::Bot,
    project: &db::Project,
    codex_home: &str,
    text: &str,
) -> Option<String> {
    let name = codex_profile_name(&bot.id);
    let path = format!("{}/{name}.config.toml", codex_home.trim_end_matches('/'));
    let body = format!("developer_instructions = {}\n", super::setup::toml_basic_string(text));
    let result = if project.host == LOCAL_HOST {
        write_private_atomic(std::path::Path::new(&path), &body).map_err(anyhow::Error::from)
    } else {
        match app.hosts().get(&project.host).await {
            Some(conn) => install_remote_mode(&conn, &path, &body, Some("600")).await,
            None => Err(anyhow::anyhow!("unknown host `{}`", project.host)),
        }
    };
    match result {
        Ok(()) => Some(name),
        Err(e) => {
            tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not write the codex persona profile");
            None
        }
    }
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
        assert!(got.configured, "設了但讀不到也算設了：CLI 自己的檔照樣關，問題寫進對話");
        assert_eq!(got.text, "GLOBAL");
        assert_eq!(got.problems.len(), 1, "{:?}", got.problems);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 專案那份跟 repo 放在一起：遠端專案到那台主機上讀，`~` 用那台的 HOME 展開；讀不到要講出主機名。
    #[tokio::test]
    async fn a_remote_projects_file_is_read_on_its_host() {
        let host = "agent-md-box";
        let env = crate::testing::env().await;
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
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
}
