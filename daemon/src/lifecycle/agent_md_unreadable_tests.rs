//! 母 bot 的 persona（AG Man 規則＋`[agents]` 的 agent md＋bot 自己的 persona）實際怎麼交給 CLI（SPEC §6.5i、#769）。
//!
//! 走真的 `start_bot`（herdr 是 mock）。persona 一律走**檔案**，不再塞進 argv：`HerdrClient::agent_start` 的
//! `fit_command_line` 把整條 argv 壓進 900 bytes，inline 的 `--append-system-prompt` 只剩 ~600 bytes 就被截成「…（後略）」，
//! 連讀得到的 agent md 都帶不進去（2026-10-02 正式環境實測 pid 3515064）。
//! - claude：`--append-system-prompt-file <bot 目錄>/persona.md`
//! - codex：`-p am-parent-<bot id>`＋`$CODEX_HOME/am-parent-<bot id>.config.toml` 的 `developer_instructions`
//! - grok：`--rules` 一行指向 `<bot 目錄>/persona.md`
//!
//! 專案那份讀不到的行為（#769，等使用者拍板）沒動：CLI 自己的指示檔照樣關、檔案裡沒有專案那份、對話裡一則 system 訊息。

use super::*;
use crate::testing as tt;

fn last_env(e: &tt::Env) -> Value {
    e.herdr
        .calls
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(m, _)| m == "tab.create" || m == "workspace.create")
        .and_then(|(_, p)| p.get("env").cloned())
        .expect("a pane was created with an env")
}

fn last_start_args(e: &tt::Env) -> Vec<String> {
    let p = e.herdr.calls_to("agent.start").pop().expect("agent.start was called");
    p["args"].as_array().unwrap().iter().filter_map(Value::as_str).map(String::from).collect()
}

async fn point_project_at(e: &tt::Env, file: &std::path::Path) {
    let label = db::project(&e.app.db, &e.project_id).await.unwrap().unwrap().label;
    let file = file.to_string_lossy().into_owned();
    e.app
        .cfg
        .update(move |c| {
            c.agents.projects.insert(label.clone(), crate::config::AgentMdFiles::One(file.clone()));
            Ok(())
        })
        .await
        .unwrap();
}

struct Started {
    /// env 有沒有關掉 CLI 自己的 CLAUDE.md
    disabled: bool,
    args: Vec<String>,
    system: Vec<String>,
    /// 母 bot 實際讀到的 persona 全文（從它拿到的檔案讀回來）
    persona: String,
    /// 這顆 bot 的 AG Man 規則（`child_agent_rules`）
    rules: String,
    /// persona 檔的權限（codex 的是 `CODEX_HOME` 裡的 profile 檔）
    mode: u32,
}

async fn start_as(kind: &str, md: &std::path::Path, user_persona: Option<&str>) -> Started {
    let e = tt::env().await;
    point_project_at(&e, md).await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "probe").await;
    let codex_home = tt::scratch_dir("am-769-codex-home");
    sqlx::query("UPDATE bots SET kind=?, persona=?, env_json=? WHERE id=?")
        .bind(kind)
        .bind(user_persona)
        .bind(json!({"CODEX_HOME": codex_home.to_string_lossy()}).to_string())
        .bind(&bot.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    start_bot(&e.app, &bot.id).await.unwrap();
    let args = last_start_args(&e);
    let persona_path = e.app.bot_dir(&bot.id).unwrap().join("persona.md");
    let project = db::project(&e.app.db, &e.project_id).await.unwrap().unwrap();
    let agent = crate::config::agent_name(&project.label, &bot.id);
    let after = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let mut mode_of = persona_path.clone();
    let persona = match kind {
        "claude" => {
            let f = after("--append-system-prompt-file").unwrap_or_else(|| panic!("claude: 沒有 --append-system-prompt-file：{args:?}"));
            assert_eq!(std::path::Path::new(&f), persona_path, "檔案在這顆 bot 自己的目錄");
            std::fs::read_to_string(&f).unwrap()
        }
        "codex" => {
            let profile = after("-p").unwrap_or_else(|| panic!("codex: 沒有 -p：{args:?}"));
            assert_eq!(profile, format!("am-parent-{}", bot.id));
            mode_of = codex_home.join(format!("{profile}.config.toml"));
            let toml: toml::Value = toml::from_str(&std::fs::read_to_string(&mode_of).unwrap()).unwrap();
            toml["developer_instructions"].as_str().unwrap().to_string()
        }
        "grok" => {
            let rules = after("--rules").unwrap_or_else(|| panic!("grok: 沒有 --rules：{args:?}"));
            assert!(rules.contains(persona_path.to_str().unwrap()) && !rules.contains('\n'), "一行指向檔案：{rules}");
            std::fs::read_to_string(&persona_path).unwrap()
        }
        other => unreachable!("{other}"),
    };
    let mode = {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(&mode_of).unwrap().permissions().mode() & 0o777
    };
    let disabled = last_env(&e).get("CLAUDE_CODE_DISABLE_CLAUDE_MDS") == Some(&json!("1"));
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let system: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
        .bind(&conv)
        .fetch_all(&e.app.db)
        .await
        .unwrap();
    Started { disabled, args, system, persona, rules: super::setup::child_agent_rules(&agent), mode }
}

fn assert_nothing_trimmed(kind: &str, s: &Started) {
    assert!(s.args.iter().all(|a| !a.contains("（後略）")), "{kind}: argv 不能有被截斷的痕跡 {:?}", s.args);
    assert!(!s.persona.contains("（後略）"), "{kind}");
    assert!(s.persona.starts_with(&s.rules), "{kind}: AG Man 規則完整在最前面（{} bytes）", s.rules.len());
    assert!(s.rules.len() > 900, "前提：規則本身就比 900 bytes 長，inline 一定會被截");
}

/// 讀得到：完整規則、全域與專案 agent md、bot 自己的 persona 依序都在檔案裡；CLI 自己的檔關掉。
#[tokio::test]
async fn the_parent_gets_its_whole_persona_through_a_file() {
    let dir = tt::scratch_dir("am-769-ok");
    let md = dir.join("CLAUDE.md");
    std::fs::write(&md, "PROJECT-RULES-769").unwrap();
    for kind in ["claude", "codex", "grok"] {
        let s = start_as(kind, &md, Some("MY-PERSONA-769")).await;
        assert_nothing_trimmed(kind, &s);
        assert!(s.disabled, "{kind}: DISABLE_CLAUDE_MDS");
        let (rules, md_at, mine) = (s.persona.find(&s.rules), s.persona.find("PROJECT-RULES-769"), s.persona.find("MY-PERSONA-769"));
        assert!(rules < md_at && md_at < mine && mine.is_some(), "{kind}: 規則 → agent md → persona 的順序 {}", s.persona);
        assert!(!s.system.iter().any(|m| m.contains("agent md 有問題")), "{kind}: {:?}", s.system);
        assert_eq!(s.args.iter().any(|a| a == "project_doc_max_bytes=0"), kind == "codex", "{kind}: {:?}", s.args);
        assert_eq!(s.mode, 0o600, "{kind}: persona 檔只給自己讀");
    }
}

/// #769（行為沒動，等使用者拍板）：專案那份讀不到，CLI 自己的指示檔照樣關、檔案裡沒有專案那份、對話裡有一則 system 訊息；
/// 但 AG Man 規則是完整的。
#[tokio::test]
async fn an_unreadable_project_file_still_turns_the_cli_files_off_but_the_rules_are_whole() {
    let dir = tt::scratch_dir("am-769-missing");
    let md = dir.join("no-such-CLAUDE.md");
    for kind in ["claude", "codex", "grok"] {
        let s = start_as(kind, &md, None).await;
        assert_nothing_trimmed(kind, &s);
        assert!(s.disabled, "{kind}: 現況：讀不到也照樣設 CLAUDE_CODE_DISABLE_CLAUDE_MDS=1（env 不分 kind 都設）");
        assert!(!s.persona.contains("PROJECT-RULES-769"), "{kind}");
        assert_eq!(s.args.iter().any(|a| a == "project_doc_max_bytes=0"), kind == "codex", "{kind}: codex 的 AGENTS.md 也被關掉 {:?}", s.args);
        assert!(s.system.iter().any(|m| m.contains("agent md 有問題") && m.contains("no-such-CLAUDE.md")), "{kind}: {:?}", s.system);
    }
}

/// persona 沒能交給檔案、退回 inline（argv 會被壓進 900 bytes、規則被截斷）：不能只在 daemon.log 裡——
/// 對話裡要有一則 system 訊息，使用者／AGM 才知道這顆母 bot 拿到的規則是殘缺的。
#[tokio::test]
async fn a_persona_that_falls_back_to_inline_says_so_in_the_conversation() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "fallback").await;
    // bot 自己的 args 帶了 `-p`：codex 的 profile 不能搶，persona 退回 inline。
    sqlx::query("UPDATE bots SET kind='codex', args_json=? WHERE id=?").bind(json!(["-p", "mine"]).to_string()).bind(&bot.id).execute(&e.app.db).await.unwrap();
    start_bot(&e.app, &bot.id).await.unwrap();
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let system: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'").bind(&conv).fetch_all(&e.app.db).await.unwrap();
    assert!(system.iter().any(|m| m.contains("persona") && m.contains("截斷")), "要說 persona 退回 inline、會被截斷：{system:?}");
    // 走檔案的正常情況不能多這則。
    let ok = tt::claude_bot(&e.app, &e.project_id, "fine").await;
    start_bot(&e.app, &ok.id).await.unwrap();
    let conv = db::conversation_id(&e.app.db, &ok.id).await.unwrap();
    let system: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'").bind(&conv).fetch_all(&e.app.db).await.unwrap();
    assert!(!system.iter().any(|m| m.contains("截斷")), "{system:?}");
}
