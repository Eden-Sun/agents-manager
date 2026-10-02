//! SPEC §6.5i（使用者 2026-10-01）：bot 讀的指示檔（agent md）只從 `[agents]` 設定來。
//!
//! 以前 claude bot 讀各帳號的 `~/.claude*/CLAUDE.md` 加上 repo 的 CLAUDE.md，codex 讀 AGENTS.md：cc0／cc1／cc2 各一份、
//! 早就不同步（cc1 是舊版、cc2 沒有），codex 讀到的又是另一份。現在三種 CLI 自己找指示檔的機制一律關掉
//! （`CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`、codex `project_doc_max_bytes=0`），內容只有一個來源：設定指到的檔，
//! 接在 AG Man 規則後面注入。子 agent 經 herdr shim 拿同一份（`AM_INSTRUCTIONS_FILE`）。

use std::sync::Arc;

use crate::config::LOCAL_HOST;
use crate::db;
use crate::hosts::sh_quote;
use crate::state::App;

/// bot 目錄裡給子 agent 讀的那份（AG Man 規則 + agent md，不含母 bot 自己的 persona）。
pub const CHILD_FILE: &str = "instructions.md";

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
pub async fn load(app: &App, project: &db::Project) -> AgentMd {
    let agents = app.cfg.agents_fresh().await;
    let mut reads: Vec<Result<String, String>> = Vec::new();
    if let Some(f) = agents.global_file() {
        reads.push(read_local(f).await);
    }
    let conn = if project.host == LOCAL_HOST { None } else { app.hosts.get(&project.host).await };
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
pub async fn install(app: &Arc<App>, bot: &db::Bot, project: &db::Project, shim_dir: Option<&str>, text: &str) -> Option<String> {
    install_in_bot_dir(app, bot, project, shim_dir, CHILD_FILE, text).await
}

/// 寫 `<bot 目錄>/<file>`（0600；遠端先送過去），回傳那台主機上的絕對路徑。母 bot 的 persona 檔（[`PERSONA_FILE`]）也走這裡。
pub async fn install_in_bot_dir(app: &Arc<App>, bot: &db::Bot, project: &db::Project, shim_dir: Option<&str>, file: &str, text: &str) -> Option<String> {
    let bot_dir = std::path::Path::new(shim_dir?).parent()?.to_string_lossy().into_owned();
    let path = format!("{bot_dir}/{file}");
    match write_private(app, project, &path, text).await {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!(bot = %bot.name, host = %project.host, file, error = ?e, "could not write the bot's instructions file");
            None
        }
    }
}

/// 母 bot 的 persona 全文（AG Man 規則＋agent md＋bot 自己的 persona）：argv 放不下（`fit_command_line` 的 900 bytes），改交檔案。
pub const PERSONA_FILE: &str = "persona.md";

/// 在專案所在的主機上寫一個只有自己讀得到的檔（0600，暫存檔＋換上；遠端走 ssh）。
pub async fn write_private(app: &Arc<App>, project: &db::Project, path: &str, text: &str) -> anyhow::Result<()> {
    if project.host == LOCAL_HOST {
        write_private_local(std::path::Path::new(path), text)?;
        Ok(())
    } else {
        match app.hosts.get(&project.host).await {
            Some(conn) => install_remote(&conn, path, text).await,
            None => Err(anyhow::anyhow!("unknown host `{}`", project.host)),
        }
    }
}

/// 內容一樣就不動；否則 0600 的暫存檔寫好再 rename（讀的人看不到寫到一半的檔）。
fn write_private_local(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        return Ok(());
    }
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp-{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("md"), std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    // 寫不完（磁碟滿）或換不上：暫存檔（內容是 AG Man 規則）一律收掉，不留半份在 bot 目錄／CODEX_HOME。
    let done = o.open(&tmp).and_then(|mut f| f.write_all(text.as_bytes())).and_then(|()| std::fs::rename(&tmp, path));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

const REMOTE_EOF: &str = "AM_AGENT_MD_EOF";

async fn install_remote(conn: &crate::hosts::HostConn, path: &str, text: &str) -> anyhow::Result<()> {
    if text.lines().any(|l| l == REMOTE_EOF) {
        anyhow::bail!("instructions contain the heredoc terminator `{REMOTE_EOF}`");
    }
    let script = remote_script(path, text);
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_AGENT_MD_OK") {
        anyhow::bail!("remote instructions install did not confirm:\n{}", out.trim());
    }
    Ok(())
}

/// 暫存檔 + `cmp`：內容一樣就不動 mtime，跟 herdr skill 的遠端安裝同一招。
fn remote_script(path: &str, text: &str) -> String {
    // codex 的母 bot profile：標成剛用過（內容一樣時 mv 不會發生，mtime 會越來越舊）、掃掉很久沒重寫的別顆 bot 的 profile 與被殺掉留下的暫存檔。
    let is_profile = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("am-parent-") && n.ends_with(".config.toml"));
    let sweep = if is_profile {
        "touch \"$F\"\nfind \"$(dirname \"$F\")\" -maxdepth 1 \\( -name 'am-parent-*.config.toml' -mtime +30 -o -name '.am-parent-*.tmp-*' -mmin +10 \\) -exec rm -f {} + 2>/dev/null || true\n"
    } else {
        ""
    };
    format!(
        "set -e\nF={f}\nmkdir -p \"$(dirname \"$F\")\"\numask 077\ncat > \"$F.new\" <<'{REMOTE_EOF}'\n{text}\n{REMOTE_EOF}\nchmod 600 \"$F.new\"\nif cmp -s \"$F.new\" \"$F\" 2>/dev/null; then rm -f \"$F.new\"; chmod 600 \"$F\"; else mv \"$F.new\" \"$F\"; fi\n{sweep}printf 'AM_AGENT_MD_OK\\n'\n",
        f = sh_quote(path),
        text = text.trim_end(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 遠端送檔的 script 真的拿去 `sh` 跑：路徑與內容含引號、`$()`、反引號、換行、像結束標記的行，都只是資料——
    /// 不會執行任何東西、內容原樣落地、檔案 0600。
    #[test]
    fn the_remote_script_treats_path_and_text_as_data() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!("am-test-remote-script-{}", crate::db::ulid()));
        let evil = root.join("sp ace/'q' $(touch PWN-PATH)/`touch PWN-BQ`/line\nbreak");
        let path = evil.join("persona.md");
        let text = "a'b \"c\" \\d\n$(touch PWN-TEXT)\n`touch PWN-TEXT2`\n${HOME} $HOME\nAM_AGENT_MD_EOFX\n  AM_AGENT_MD_EOF\n\\\nend";
        std::fs::create_dir_all(&root).unwrap();
        let script = remote_script(path.to_str().unwrap(), text);
        // cwd 放進 root：萬一真的被執行，`touch PWN-*` 會落在 root 底下找得到。
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(&script).current_dir(&root).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let pwned: Vec<String> = ["PWN-PATH", "PWN-BQ", "PWN-TEXT", "PWN-TEXT2"].iter().filter(|n| root.join(n).exists()).map(|s| s.to_string()).collect();
        let written = std::fs::read_to_string(&path);
        let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777);
        let _ = std::fs::remove_dir_all(&root);
        assert!(stdout.contains("AM_AGENT_MD_OK"), "{stdout} {stderr}");
        assert!(pwned.is_empty(), "不該執行任何東西：{pwned:?}");
        assert_eq!(written.unwrap(), format!("{}\n", text.trim_end()));
        assert_eq!(mode.unwrap(), 0o600);
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
