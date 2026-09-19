//! claude 的一回合是誰起頭的（issue #224）。
//!
//! `begin_external_turn` 在 pane 的 `-> working` 邊、沒有回合在飛時開一筆外部回合，並把畫面上最後一個 `❯` 回音當成使用者訊息存下來。
//! 使用者直接在 pane 裡打字時那個回音就是這一回合的 prompt；但 claude 也會**自己**起一回合——背景 shell（`Bash run_in_background`）
//! 跑完的 task notification——畫面上沒有新的回音，最後一個回音是**上一則**使用者 prompt，早就記在上一回合底下。
//! 把它存成這一回合的 user 訊息，就是對話裡「背景任務完成」那一回合掛著上一則 prompt（`source: hook`）。
//!
//! 2.1.278 的 transcript 分得出來（2026-09-19 真機實抓，`fixtures/claude_2.1.278_task_notification.jsonl`）：每一則回合起頭的
//! `type == "user"` entry 帶 `origin.kind`——使用者打的（含 daemon 經 herdr 送的）是 `human`，背景 shell 完成是 `task-notification`
//! （同一筆還有 `promptSource: "system"`、`turnOrigin: "task_notification"`，前面另有 `queue-operation` enqueue／dequeue）。
//! 回合中間的工具結果也是 `type == "user"`，但沒有 `origin`，不算起點。

use serde_json::Value;

/// transcript 尾巴（一次讀這麼多；一回合的起點一定在最後幾百 KB 裡，同 `hookrecv::last_transcript_user_text`）。
const TAIL: u64 = 512 * 1024;

/// 從後往前找第一則帶 `origin.kind` 的 `type == "user"` entry：目前這一回合的起點是誰。舊版 CLI 沒有 `origin`、或讀不到 → `None`。
pub(crate) fn starter_origin_kind(log: &str) -> Option<String> {
    log.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("type").and_then(Value::as_str) != Some("user") {
            return None;
        }
        v.get("origin")?.get("kind")?.as_str().map(str::to_string)
    })
}

fn read_tail(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// 這個 claude run 目前這一回合是不是 CLI 自己起頭的（`origin.kind` 有寫、而且不是 `human`）。
/// 讀不到 transcript、舊版沒有 `origin`、不是 claude ＝ `false`：照舊當成使用者在 pane 裡打的。
pub(crate) async fn started_by_the_cli_itself(bot_kind: &str, transcript_path: Option<&str>) -> bool {
    if bot_kind != "claude" {
        return false;
    }
    let Some(path) = transcript_path.filter(|p| !p.is_empty()).map(std::path::PathBuf::from) else { return false };
    let kind = tokio::task::spawn_blocking(move || read_tail(&path).and_then(|log| starter_origin_kind(&log))).await.ok().flatten();
    matches!(kind.as_deref(), Some(k) if k != "human")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::testing as tt;

    /// 2026-09-19 真機實抓（claude 2.1.278）：使用者 prompt（`origin.kind: human`）→ 工具呼叫與結果 → 回覆 → 背景 shell 完成後
    /// claude 自己接的一回合（`queue-operation` → `origin.kind: task-notification`）→ 回覆。
    const SAMPLE: &str = include_str!("fixtures/claude_2.1.278_task_notification.jsonl");
    /// 上一則使用者 prompt 還留在畫面上（那一則已經記在上一回合底下）；自己起的那一回合沒有新的 `❯` 回音。
    const SCREEN_AFTER_A_NOTIFICATION_TURN: &str = "❯ 用 Bash 工具的 run_in_background 參數跑一個背景指令：sleep 20; echo done\n⏺ 已在背景啟動，20 秒後會完成。\n✻ Worked for 3s\n⏺ 背景任務已完成。\n──────\n❯\n";
    const SCREEN_TYPED_IN_THE_PANE: &str = "❯ 順便看一下 lint\n──────\n❯\n";

    #[test]
    fn the_sample_says_who_started_the_turn() {
        assert_eq!(starter_origin_kind(SAMPLE).as_deref(), Some("task-notification"), "最後一回合是背景 shell 完成的通知");
        // 使用者那一回合（前面幾筆）：起點是 human；中間的工具結果（`type: user`、沒有 origin）不算起點。
        let human_turn: Vec<&str> = SAMPLE.lines().take(6).collect();
        assert_eq!(starter_origin_kind(&human_turn.join("\n")).as_deref(), Some("human"));
        // 通知那一回合之後又有工具結果：起點仍是通知。
        let tool_result = SAMPLE.lines().find(|l| l.contains("tool_result")).expect("樣本裡有一筆真的工具結果");
        assert_eq!(starter_origin_kind(&format!("{SAMPLE}{tool_result}\n")).as_deref(), Some("task-notification"));
        // 舊版沒有 origin、壞行、空的：沒有證據。
        assert_eq!(starter_origin_kind("{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\nnot json\n"), None);
        assert_eq!(starter_origin_kind(""), None);
    }

    async fn with_transcript(log: &str) -> (tt::Env, db::Run, String) {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "notified").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let path = env.dir.join("session.jsonl");
        std::fs::write(&path, log).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?").bind(path.to_string_lossy().to_string()).bind(&run_id).execute(&app.db).await.unwrap();
        env.herdr.set_screen(&format!("pane-{}", bot.id), SCREEN_AFTER_A_NOTIFICATION_TURN);
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        (env, run, bot.id)
    }

    async fn user_messages(env: &tt::Env, bot_id: &str) -> Vec<String> {
        let conv = db::conversation_id(&env.app.db, bot_id).await.unwrap();
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='user' ORDER BY created_at")
            .bind(conv)
            .fetch_all(&env.app.db)
            .await
            .unwrap()
    }

    async fn external_turns(env: &tt::Env, bot_id: &str) -> Vec<(String, String)> {
        let conv = db::conversation_id(&env.app.db, bot_id).await.unwrap();
        sqlx::query_as("SELECT origin, status FROM turns WHERE conversation_id=?").bind(conv).fetch_all(&env.app.db).await.unwrap()
    }

    /// 事故（2026-09-19 真機，#224）：claude 自己接的回合（背景 shell 完成的通知）開出外部回合時，畫面上最後一個 `❯` 回音是上一則
    /// 使用者 prompt——不能存成這一回合的 user 訊息。回合照開（回覆要有地方掛）。
    #[tokio::test]
    async fn a_turn_the_cli_starts_by_itself_does_not_borrow_the_previous_prompt() {
        let (env, run, bot_id) = with_transcript(SAMPLE).await;
        crate::lifecycle::begin_external_turn(&env.app, &run).await;

        assert_eq!(external_turns(&env, &bot_id).await, vec![("external".to_string(), "in_flight".to_string())], "回合照開");
        assert_eq!(user_messages(&env, &bot_id).await, Vec::<String>::new(), "不是使用者起頭的：沒有 user 訊息，不借上一則的 prompt");
    }

    /// 對照組：使用者直接在 pane 裡打字（transcript 最新的起點是 `human`）——回音照舊存成 user 訊息。
    #[tokio::test]
    async fn a_prompt_typed_in_the_pane_keeps_its_echo() {
        // 通知那一回合之後，使用者在 pane 裡又打了一句：最新的起點換成 human。
        let typed = r#"{"type":"user","message":{"role":"user","content":"順便看一下 lint"},"origin":{"kind":"human"},"promptSource":"typed","turnOrigin":"human","timestamp":"2026-09-19T10:05:00.000Z"}"#;
        let (env, run, bot_id) = with_transcript(&format!("{SAMPLE}{typed}\n")).await;
        env.herdr.set_screen(&format!("pane-{bot_id}"), SCREEN_TYPED_IN_THE_PANE);
        crate::lifecycle::begin_external_turn(&env.app, &run).await;

        assert_eq!(user_messages(&env, &bot_id).await, vec!["順便看一下 lint".to_string()]);
    }

    /// 對照組：讀不到 transcript、或舊版沒有 `origin`——沒有證據，照舊當成使用者打的。
    #[tokio::test]
    async fn without_evidence_the_echo_is_kept_as_before() {
        let (env, run, bot_id) = with_transcript("").await;
        crate::lifecycle::begin_external_turn(&env.app, &run).await;
        assert_eq!(user_messages(&env, &bot_id).await.len(), 1, "沒有 origin：照舊");
        drop(env);

        let (env, mut run, bot_id) = with_transcript(SAMPLE).await;
        run.transcript_path = None;
        sqlx::query("UPDATE runs SET transcript_path=NULL WHERE id=?").bind(&run.id).execute(&env.app.db).await.unwrap();
        crate::lifecycle::begin_external_turn(&env.app, &run).await;
        assert_eq!(user_messages(&env, &bot_id).await.len(), 1, "run 沒有 transcript 路徑：照舊");
    }

    /// 只有 claude 的 transcript 讀得懂：codex／grok 的 run 就算有路徑也照舊。
    #[tokio::test]
    async fn only_claude_transcripts_are_read() {
        assert!(started_by_the_cli_itself("claude", Some(&write_sample())).await);
        assert!(!started_by_the_cli_itself("codex", Some(&write_sample())).await);
        assert!(!started_by_the_cli_itself("grok", Some(&write_sample())).await);
        assert!(!started_by_the_cli_itself("claude", None).await);
        assert!(!started_by_the_cli_itself("claude", Some("/nonexistent/session.jsonl")).await);
    }

    fn write_sample() -> String {
        let path = std::env::temp_dir().join(format!("am-origin-{}.jsonl", db::ulid()));
        std::fs::write(&path, SAMPLE).unwrap();
        path.to_string_lossy().to_string()
    }
}
