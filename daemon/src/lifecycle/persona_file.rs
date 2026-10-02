//! SPEC §6.5i：母 bot 的 persona（AG Man 規則＋`[agents]` 的 agent md＋bot 自己的 persona）交給 CLI 的方式。
//!
//! 以前整段塞進 argv（`--append-system-prompt <全文>`），但 `HerdrClient::agent_start` 的 `fit_command_line` 把整條 argv 壓進
//! 900 bytes：AG Man 規則自己就比這長，全文被截成「…（後略）」，agent md 一個字都進不去，CLI 自己的指示檔卻已經關掉
//! （2026-10-02 正式環境實測，persona 628 bytes）。現在比照子 agent（`herdr_shim` 的 `AM_INSTRUCTIONS_FILE`）改走檔案，
//! argv 只帶一個短參數：
//! - claude：`--append-system-prompt-file <bot 目錄>/persona.md`
//! - codex：`-p am-parent-<bot id>`；profile 檔 `$CODEX_HOME/am-parent-<bot id>.config.toml` 的 `developer_instructions`
//!   用 TOML basic string（跳脫過、單行），內容含 `'''` 也沒事（不像 shim 的多行字面字串）
//! - grok：`--rules` 一行，指向 persona.md（grok 沒有讀檔版）
//!
//! 檔案 0600，寫在專案所在的那台主機（遠端先送過去，同 `agent_md::install`）。寫不進去、或 codex bot 自己的 args 已經帶了
//! `-p`／`--profile` 時回 `None`，呼叫端退回舊的 inline 參數（會被截斷，但至少有規則的開頭）。

use std::sync::Arc;

use serde_json::Value;

use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;

pub(crate) fn codex_profile(bot_id: &str) -> String {
    format!("am-parent-{}", bot_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>())
}

/// 把 `text` 寫成這顆 bot 的 persona 檔並回 argv 片段；`None`＝這次用不了檔案（呼叫端退回 inline）。
/// `env` 是這次 pane 的 env（codex 的 `CODEX_HOME` 在裡面）；`md_configured`＝`[agents]` 有指檔（codex 才要多關 AGENTS.md）。
pub(crate) async fn launch_args(
    app: &Arc<App>,
    bot: &db::Bot,
    project: &db::Project,
    shim_dir: Option<&str>,
    env: &Value,
    text: &str,
    md_configured: bool,
) -> Option<Vec<String>> {
    // herdr ≥0.9 拒絕含控制字元的參數（#772）：路徑帶換行之類就不用檔案（也不寫），呼叫端退回並說一聲。
    if shim_dir.is_some_and(|d| d.chars().any(char::is_control)) {
        tracing::warn!(bot = %bot.name, "the bot dir path holds a control character; the persona stays inline");
        return None;
    }
    match bot.kind.as_str() {
        "claude" => {
            let path = super::agent_md::install_in_bot_dir(app, bot, project, shim_dir, super::agent_md::PERSONA_FILE, text).await?;
            Some(vec!["--append-system-prompt-file".into(), path])
        }
        "grok" => {
            let path = super::agent_md::install_in_bot_dir(app, bot, project, shim_dir, super::agent_md::PERSONA_FILE, text).await?;
            // 同子 agent 的寫法（herdr_shim）：`--rules` 只收字串，給一行指向檔案的指示。
            // 路徑用反引號框起來（跟 herdr_shim 的子 agent 同一個寫法）：有空白時才分得出哪裡到哪裡。
            Some(vec!["--rules".into(), format!("AG Man 指示（硬規則，效力同系統指示）在 `{path}`：開始任何工作前先完整讀過並照做。")])
        }
        "codex" => {
            if has_own_profile(bot) {
                tracing::warn!(bot = %bot.name, "the bot's own codex args carry a profile; the persona stays inline");
                return None;
            }
            let dir = codex_home(app, project, env).await?;
            let name = codex_profile(&bot.id);
            let toml = format!("developer_instructions = {}\n", super::setup::toml_basic_string(text));
            if let Err(e) = super::agent_md::write_private(app, project, &format!("{dir}/{name}.config.toml"), &toml).await {
                tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not write the codex persona profile");
                return None;
            }
            if project.host == LOCAL_HOST {
                // 內容一樣時 `write_private` 不動檔案，mtime 會越來越舊：先標成「剛用過」，掃舊檔才不會掃到自己。
                let me = std::path::Path::new(&dir).join(format!("{name}.config.toml"));
                let _ = std::fs::File::options().write(true).open(&me).and_then(|f| f.set_modified(std::time::SystemTime::now()));
                sweep_stale_local(std::path::Path::new(&dir));
            }
            let mut args = vec!["-p".to_string(), name];
            if md_configured {
                args.extend(["-c".into(), "project_doc_max_bytes=0".into()]);
            }
            Some(args)
        }
        _ => None,
    }
}

/// persona 沒能交給檔案、退回 inline 時要寫進對話的 system 訊息（inline 會被 `fit_command_line` 壓進 900 bytes）。
pub(crate) fn fallback_notice(bot: &db::Bot) -> String {
    let own_profile = bot.kind == "codex" && has_own_profile(bot);
    let why = if own_profile {
        "bot 自己的 args 帶了 -p／--profile，codex 的 profile 不能搶"
    } else {
        "persona 檔寫不進去或路徑不能用（詳見 daemon.log）"
    };
    format!("母 bot 的 persona 沒能交給檔案（{why}），退回 inline：argv 會被壓進約 900 bytes，AG Man 規則與 agent md 被截斷，這顆 bot 拿到的指示是殘缺的")
}

fn has_own_profile(bot: &db::Bot) -> bool {
    bot.args().iter().any(|a| a == "-p" || a == "--profile" || a.starts_with("--profile="))
}

/// 每顆 bot 一個 profile、只增不減：bot 刪了或換了 CODEX_HOME 就留在那裡（內容是 AG Man 規則）。寫完順手掃掉超過 30 天沒重寫的
/// `am-parent-*.config.toml`（每次啟動都重寫，所以掃掉舊的不會害到誰）與寫到一半被殺掉的暫存檔（超過 10 分鐘）；別的檔案不碰。
/// 遠端同一件事在 `agent_md::remote_script` 裡（同一趟 ssh）。
fn sweep_stale_local(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let max_age = if name.starts_with("am-parent-") && name.ends_with(".config.toml") {
            std::time::Duration::from_secs(30 * 24 * 3600)
        } else if name.starts_with(".am-parent-") && name.contains(".tmp-") {
            std::time::Duration::from_secs(10 * 60)
        } else {
            continue;
        };
        let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > max_age);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 這顆 codex bot 的 `CODEX_HOME`（在專案所在主機上的絕對路徑）：env 有就用（身分的目錄，已展開 `~`／`$HOME`），沒有是 `~/.codex`。
async fn codex_home(app: &Arc<App>, project: &db::Project, env: &Value) -> Option<String> {
    if let Some(dir) = env.get("CODEX_HOME").and_then(Value::as_str).filter(|d| d.starts_with('/')) {
        return Some(dir.trim_end_matches('/').to_string());
    }
    let home = if project.host == LOCAL_HOST {
        dirs::home_dir()?.to_string_lossy().into_owned()
    } else {
        app.hosts.get(&project.host).await?.home().await.ok()?
    };
    Some(format!("{}/.codex", home.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use serde_json::json;

    async fn fixture(kind: &str) -> (tt::Env, db::Bot, db::Project) {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "pf").await;
        sqlx::query("UPDATE bots SET kind=? WHERE id=?").bind(kind).bind(&bot.id).execute(&e.app.db).await.unwrap();
        let bot = db::bot(&e.app.db, &bot.id).await.unwrap().unwrap();
        let project = db::project(&e.app.db, &e.project_id).await.unwrap().unwrap();
        (e, bot, project)
    }

    async fn remote(e: &tt::Env, project: &mut db::Project, host: &str) {
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = e.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some("/Users/x".into());
        project.host = host.into();
    }

    /// 遠端主機：檔案先經 ssh 送過去（0600、暫存檔＋換上），路徑是那台上的 bot 目錄；codex 的 `CODEX_HOME` 預設用那台的 `~/.codex`。
    #[tokio::test]
    async fn a_remote_hosts_files_are_shipped_over_ssh_with_mode_0600() {
        for (kind, host) in [("claude", "persona-box-c"), ("codex", "persona-box-x"), ("grok", "persona-box-g")] {
            let (e, bot, mut project) = fixture(kind).await;
            remote(&e, &mut project, host).await;
            let scripts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let s2 = scripts.clone();
            crate::hosts::set_ssh_fake(host, move |script| {
                s2.lock().unwrap().push(script.to_string());
                Ok("AM_AGENT_MD_OK\n".into())
            });
            let shim = "/Users/x/.config/agents-manager/bots/B1/bin";
            let args = launch_args(&e.app, &bot, &project, Some(shim), &json!({}), "RULES\nline two", true).await.unwrap();
            let scripts = scripts.lock().unwrap();
            assert_eq!(scripts.len(), 1, "{kind}: 一次 ssh");
            assert!(scripts[0].contains("chmod 600") && scripts[0].contains("umask 077"), "{kind}: {}", scripts[0]);
            if kind != "codex" {
                assert!(scripts[0].contains("RULES\nline two"), "{kind}: {}", scripts[0]);
            }
            match kind {
                "claude" => assert_eq!(args, ["--append-system-prompt-file", "/Users/x/.config/agents-manager/bots/B1/persona.md"]),
                "grok" => assert!(args[1].contains("/Users/x/.config/agents-manager/bots/B1/persona.md"), "{args:?}"),
                _ => {
                    assert_eq!(args, vec!["-p".to_string(), codex_profile(&bot.id), "-c".into(), "project_doc_max_bytes=0".into()]);
                    assert!(scripts[0].contains(&format!("/Users/x/.codex/{}.config.toml", codex_profile(&bot.id))), "{}", scripts[0]);
                    assert!(scripts[0].contains("chmod 600") && scripts[0].contains("developer_instructions = \"RULES\\nline two\""), "{}", scripts[0]);
                }
            }
        }
    }

    /// 送不過去（ssh 失敗）→ `None`，呼叫端退回 inline；不是靜默沒有 persona。
    #[tokio::test]
    async fn a_failed_upload_falls_back_to_the_inline_args() {
        let (e, bot, mut project) = fixture("claude").await;
        remote(&e, &mut project, "persona-box-down").await;
        crate::hosts::set_ssh_fake("persona-box-down", |_| Err(anyhow::anyhow!("ssh: connect timed out")));
        assert!(launch_args(&e.app, &bot, &project, Some("/Users/x/b/bin"), &json!({}), "R", false).await.is_none());
        let (e, bot, project) = fixture("claude").await;
        assert!(launch_args(&e.app, &bot, &project, None, &json!({}), "R", false).await.is_none(), "沒有 bot 目錄（shim 沒裝成）也退回");
    }

    /// codex：persona 含換行、引號、反斜線、三個連續單引號都原樣讀回（basic string 跳脫，不像 shim 的多行字面字串）；
    /// bot 自己的 args 帶了 `-p`／`--profile` 就不搶（退回 inline）。沒有 agent md 設定就不關 AGENTS.md。
    #[tokio::test]
    async fn codex_profile_round_trips_awkward_text_and_respects_the_bots_own_profile() {
        let (e, bot, project) = fixture("codex").await;
        let home = tt::scratch_dir("am-persona-codex");
        let text = "a'''b\n\"q\" \\ done\t末行";
        let env = json!({"CODEX_HOME": home.to_string_lossy()});
        let args = launch_args(&e.app, &bot, &project, Some("/x/bin"), &env, text, false).await.unwrap();
        assert_eq!(args, vec!["-p".to_string(), codex_profile(&bot.id)], "沒設 [agents]：不關 AGENTS.md");
        let t: toml::Value = toml::from_str(&std::fs::read_to_string(home.join(format!("{}.config.toml", codex_profile(&bot.id)))).unwrap()).unwrap();
        assert_eq!(t["developer_instructions"].as_str().unwrap(), text);

        sqlx::query("UPDATE bots SET args_json=? WHERE id=?").bind(json!(["--profile", "mine"]).to_string()).bind(&bot.id).execute(&e.app.db).await.unwrap();
        let bot = db::bot(&e.app.db, &bot.id).await.unwrap().unwrap();
        assert!(launch_args(&e.app, &bot, &project, Some("/x/bin"), &env, text, true).await.is_none());
    }

    /// 暫存目錄自己清（#763：測試不能把目錄留在 /tmp）。
    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("am-test-{tag}-{}", crate::db::ulid()));
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 控制字元（ESC、DEL、單獨的 CR…）在 basic string 裡一律跳脫，真 TOML 解析讀回原文——跟 shim 的 profile 同一個結論
    /// （shim 用字面字串所以要濾；這裡跳脫，所以不用濾也不會壞）。
    #[tokio::test]
    async fn codex_profile_survives_control_characters_too() {
        let (e, bot, project) = fixture("codex").await;
        let home = TmpDir::new("persona-ctl");
        let text = "esc\u{1b}[31m del\u{7f} ff\u{0c} cr\rlone crlf\r\nend ''' \"\"\" \\u0041";
        let env = json!({"CODEX_HOME": home.path().to_string_lossy()});
        launch_args(&e.app, &bot, &project, Some("/x/bin"), &env, text, false).await.unwrap();
        let t: toml::Value = toml::from_str(&std::fs::read_to_string(home.path().join(format!("{}.config.toml", codex_profile(&bot.id)))).unwrap()).unwrap();
        assert_eq!(t["developer_instructions"].as_str().unwrap(), text);
    }

    /// grok 的那一行跟子 agent 的 `--rules`（herdr_shim）同一個寫法：路徑用反引號框起來（有空白才分得出來）。
    #[tokio::test]
    async fn the_grok_rules_line_frames_the_path_in_backticks() {
        let (e, bot, project) = fixture("grok").await;
        let dir = TmpDir::new("persona grok 全形");
        let shim = format!("{}/bots/B1/bin", dir.path().display());
        let args = launch_args(&e.app, &bot, &project, Some(&shim), &json!({}), "RULES", false).await.unwrap();
        let path = format!("{}/bots/B1/persona.md", dir.path().display());
        assert!(args[1].contains(&format!("`{path}`")), "{args:?}");
        assert!(!args[1].chars().any(char::is_control), "herdr 不收控制字元：{args:?}");
    }

    /// 路徑含控制字元（換行）：herdr 會拒絕整個 `agent start`（#772）→ 不帶那個參數、也不寫檔，退回（呼叫端會說一聲）。
    #[tokio::test]
    async fn a_path_with_a_control_character_is_never_put_in_argv_or_written() {
        for kind in ["claude", "grok"] {
            let (e, bot, project) = fixture(kind).await;
            let dir = TmpDir::new("persona-nl");
            let shim = format!("{}/line\nbreak/bin", dir.path().display());
            let args = launch_args(&e.app, &bot, &project, Some(&shim), &json!({}), "RULES", false).await;
            assert!(args.is_none(), "{kind}: {args:?}");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "{kind}: 什麼都沒寫");
        }
    }

    /// codex 的 profile 一顆 bot 一個檔，永遠只增不減：bot 刪了、換了 CODEX_HOME 就留在那裡（內容是 AG Man 規則）。
    /// 每次寫完順手掃掉很久沒重寫的 `am-parent-*` 與寫到一半被殺掉的暫存檔；別的檔案（含別人的 profile）一個都不碰。
    #[tokio::test]
    async fn stale_parent_profiles_and_temp_files_are_swept() {
        let (e, bot, project) = fixture("codex").await;
        let home = TmpDir::new("persona-sweep");
        let home = home.path();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 3600);
        let age = |p: &std::path::Path| std::fs::File::options().write(true).open(p).unwrap().set_modified(old).unwrap();
        let gone = home.join("am-parent-GONE.config.toml");
        let live = home.join("am-parent-LIVE.config.toml");
        let tmp = home.join(".am-parent-X.config.toml.tmp-4242");
        let child = home.join("am-child-x.config.toml");
        let mine = home.join("mine.config.toml");
        let main_cfg = home.join("config.toml");
        for f in [&gone, &live, &tmp, &child, &mine, &main_cfg] {
            std::fs::write(f, "x = 1\n").unwrap();
        }
        for f in [&gone, &tmp, &child, &mine, &main_cfg] {
            age(f);
        }
        let env = json!({"CODEX_HOME": home.to_string_lossy()});
        launch_args(&e.app, &bot, &project, Some("/x/bin"), &env, "R", false).await.unwrap();
        assert!(!gone.exists(), "很久沒重寫的別顆 bot 的 am-parent profile 要掃掉");
        assert!(!tmp.exists(), "寫到一半被殺掉的暫存檔要掃掉");
        assert!(live.exists(), "新的別顆 bot 的 profile 不能動");
        assert!(child.exists() && mine.exists() && main_cfg.exists(), "不是 am-parent-* 的檔案一個都不能動（am-child-* 歸 herdr shim 掃）");
        assert!(home.join(format!("{}.config.toml", codex_profile(&bot.id))).exists());
    }

    /// 遠端：送過去的 script 也順手掃（同一趟 ssh）；只掃 `am-parent-*`。
    #[tokio::test]
    async fn the_remote_script_sweeps_stale_parent_profiles_in_the_same_ssh() {
        let (e, bot, mut project) = fixture("codex").await;
        remote(&e, &mut project, "persona-box-sweep").await;
        let scripts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let s2 = scripts.clone();
        crate::hosts::set_ssh_fake("persona-box-sweep", move |script| {
            s2.lock().unwrap().push(script.to_string());
            Ok("AM_AGENT_MD_OK\n".into())
        });
        launch_args(&e.app, &bot, &project, Some("/Users/x/b/bin"), &json!({}), "R", false).await.unwrap();
        let scripts = scripts.lock().unwrap();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("am-parent-*.config.toml") && scripts[0].contains("-mtime +30"), "{}", scripts[0]);
    }
}
