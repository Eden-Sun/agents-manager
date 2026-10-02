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
    match bot.kind.as_str() {
        "claude" => {
            let path = super::agent_md::install_in_bot_dir(app, bot, project, shim_dir, super::agent_md::PERSONA_FILE, text).await?;
            Some(vec!["--append-system-prompt-file".into(), path])
        }
        "grok" => {
            let path = super::agent_md::install_in_bot_dir(app, bot, project, shim_dir, super::agent_md::PERSONA_FILE, text).await?;
            // 同子 agent 的寫法（herdr_shim）：`--rules` 只收字串，給一行指向檔案的指示。
            Some(vec!["--rules".into(), format!("AG Man 指示（硬規則，效力同系統指示）在 {path}：開始任何工作前先完整讀過並照做。")])
        }
        "codex" => {
            if bot.args().iter().any(|a| a == "-p" || a == "--profile" || a.starts_with("--profile=")) {
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
            let mut args = vec!["-p".to_string(), name];
            if md_configured {
                args.extend(["-c".into(), "project_doc_max_bytes=0".into()]);
            }
            Some(args)
        }
        _ => None,
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
}
