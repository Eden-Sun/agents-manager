//! issue #769：`[agents]` 指了專案指示檔、但那份讀不到（遠端主機 ssh 失敗、檔案不見）時，bot 實際開出來的 env 與參數。
//! 走真的 `start_bot`（herdr 是 mock），記下現況：**CLI 自己的指示檔照樣被關掉，注入的卻沒有專案那份**。
//!
//! **審查時順帶發現（比 #769 更嚴重）**：`HerdrClient::agent_start` 的 `fit_command_line` 把整條 argv 壓進 900 bytes，
//! 母 bot 的 `--append-system-prompt` 只剩約 600 bytes 就被截成「…（後略）」——AG Man 規則自己就比這長，
//! 全域與專案的 agent md 連**讀得到的時候**都帶不進去（正式環境 pid 3515064 實測：persona 628 bytes、結尾「…（後略）」，
//! env 卻是 `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`）。子 agent 走檔案（`--append-system-prompt-file`）不受影響。
//! 下面兩條把這個現況寫成斷言；修好（母 bot 也改走檔案）之後，把標「現況」的斷言翻成 `contains`。
//!
//! 這是 SPEC §6.5i 與 `agent_md::tests::unset_means_not_configured_and_a_missing_file_is_reported` 明講的決定；
//! 這裡的斷言是「現況的證據」，使用者若改決定（讀不到就不關／不啟動），這幾條要跟著改。

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

/// (這次啟動的 env 有沒有關掉 claude 自己的 CLAUDE.md、argv 全文、對話裡的 system 訊息)
async fn start_as(kind: &str, md: &std::path::Path) -> (bool, String, Vec<String>) {
    let e = tt::env().await;
    point_project_at(&e, md).await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "probe").await;
    sqlx::query("UPDATE bots SET kind=? WHERE id=?").bind(kind).bind(&bot.id).execute(&e.app.db).await.unwrap();
    start_bot(&e.app, &bot.id).await.unwrap();
    let disabled = last_env(&e).get("CLAUDE_CODE_DISABLE_CLAUDE_MDS") == Some(&json!("1"));
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let system: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
        .bind(&conv)
        .fetch_all(&e.app.db)
        .await
        .unwrap();
    (disabled, last_start_args(&e).join("\u{1f}"), system)
}

/// 對照組：讀得到時，CLI 自己的檔關掉；**現況：專案那份被 `fit_command_line` 截掉，沒進 argv**（見檔頭）。
#[tokio::test]
async fn a_readable_project_file_is_off_the_cli_files_but_trimmed_out_of_argv_today() {
    let dir = tt::scratch_dir("am-769-ok");
    let md = dir.join("CLAUDE.md");
    std::fs::write(&md, "PROJECT-RULES-769").unwrap();
    for kind in ["claude", "codex", "grok"] {
        let (disabled, args, system) = start_as(kind, &md).await;
        assert!(disabled, "{kind}: DISABLE_CLAUDE_MDS");
        assert!(args.contains("…（後略）"), "{kind}: 現況：persona 被截斷 {args}");
        assert!(!args.contains("PROJECT-RULES-769"), "{kind}: 現況：讀得到的專案規則也沒進 argv {args}");
        assert!(!system.iter().any(|s| s.contains("agent md 有問題")), "{kind}: {system:?}");
        assert_eq!(args.contains("project_doc_max_bytes=0"), kind == "codex", "{kind}: {args}");
    }
}

/// #769 的失敗模式：專案那份讀不到，`configured` 仍是 true。
/// claude／codex：CLI 自己的指示檔照樣關掉（env `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`、codex `project_doc_max_bytes=0`），
/// 注入的 argv 沒有專案規則，唯一的提示是對話裡一則 system 訊息。grok 沒有關自己檔案的機制，所以只少注入、沒有少讀。
#[tokio::test]
async fn an_unreadable_project_file_still_turns_the_cli_files_off_and_injects_nothing_of_it() {
    let dir = tt::scratch_dir("am-769-missing");
    let md = dir.join("no-such-CLAUDE.md");
    for kind in ["claude", "codex", "grok"] {
        let (disabled, args, system) = start_as(kind, &md).await;
        assert!(disabled, "{kind}: 現況：讀不到也照樣設 CLAUDE_CODE_DISABLE_CLAUDE_MDS=1（env 不分 kind 都設）");
        assert!(args.contains("硬規則，不是建議"), "{kind}: AG Man 規則的開頭還在 {args}");
        assert!(!args.contains("PROJECT-RULES-769"), "{kind}");
        assert_eq!(args.contains("project_doc_max_bytes=0"), kind == "codex", "{kind}: codex 的 AGENTS.md 也被關掉 {args}");
        assert!(system.iter().any(|s| s.contains("agent md 有問題") && s.contains("no-such-CLAUDE.md")), "{kind}: {system:?}");
    }
}
