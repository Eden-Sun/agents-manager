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

#[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
use crate::capabilities::Db;
use serde_json::Value;

/// transcript 尾巴（一次讀這麼多；一回合的起點一定在最後幾百 KB 裡，同 `hookrecv::last_transcript_user_text`）。
const TAIL: u64 = 512 * 1024;

/// 從後往前找第一則帶 `origin.kind` 的 `type == "user"` entry：目前這一回合的起點是誰、那筆條目的 `timestamp`（原樣，沒有就 `None`）。
/// 舊版 CLI 沒有 `origin`、或讀不到 → `None`。
pub fn starter_origin(log: &str) -> Option<(String, Option<String>)> {
    log.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("type").and_then(Value::as_str) != Some("user") {
            return None;
        }
        let kind = v.get("origin")?.get("kind")?.as_str()?.to_string();
        Some((kind, v.get("timestamp").and_then(Value::as_str).map(str::to_string)))
    })
}

pub fn starter_origin_kind(log: &str) -> Option<String> {
    starter_origin(log).map(|(kind, _)| kind)
}

/// [`starter_origin`] 對一個本機 transcript 檔（同步，呼叫端放進 `spawn_blocking`）。讀不到、沒有 `origin` ＝ `None`。
pub fn starter_origin_at(path: &std::path::Path) -> Option<(String, Option<String>)> {
    read_tail(path).and_then(|log| starter_origin(&log))
}

pub fn read_tail(path: &std::path::Path) -> Option<String> {
    crate::transcript_read::read_tail(path, TAIL)
}

/// 這個 claude run 目前這一回合是不是 CLI 自己起頭的（`origin.kind` 有寫、而且不是 `human`）。
/// 讀不到 transcript、舊版沒有 `origin`、不是 claude ＝ `false`：照舊當成使用者在 pane 裡打的。
pub async fn started_by_the_cli_itself_with(
    roots: &impl crate::transcript_read::TranscriptRoots,
    kind: &str,
    transcript_path: Option<&str>,
) -> bool {
    if kind != "claude" {
        return false;
    }
    let Some(path) = transcript_path.filter(|p| !p.is_empty()) else { return false };
    if !crate::transcript_read::local_transcript_allowed_for(roots, path).await {
        return false;
    }
    let path = std::path::PathBuf::from(path);
    let origin_kind = tokio::task::spawn_blocking(move || read_tail(&path).and_then(|log| starter_origin_kind(&log))).await.ok().flatten();
    matches!(origin_kind.as_deref(), Some(k) if k != "human")
}

pub async fn started_by_the_cli_itself(app: &impl super::s6_ports::TranscriptOriginServices, bot: &crate::db::Bot, transcript_path: Option<&str>) -> bool {
    app.started_by_the_cli_itself(bot, transcript_path).await
}


#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod origin_timestamp_tests {
    use super::*;

    #[test]
    fn the_starter_carries_the_timestamp_of_the_entry_that_gave_its_kind() {
        let log = concat!(
            r#"{"type":"user","origin":{"kind":"human"},"timestamp":"2026-11-22T01:21:00.000Z","message":{"role":"user","content":"A"}}"#, "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#, "\n",
            r#"{"type":"user","timestamp":"2026-11-22T01:30:09.000Z","message":{"role":"user","content":"wake"}}"#, "\n",
        );
        assert_eq!(starter_origin(log), Some(("human".to_string(), Some("2026-11-22T01:21:00.000Z".to_string()))));
        assert_eq!(starter_origin_kind(log).as_deref(), Some("human"));
    }

    #[test]
    fn a_kind_without_a_timestamp_has_no_timestamp() {
        let log = r#"{"type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"A"}}"#;
        assert_eq!(starter_origin(log), Some(("human".to_string(), None)));
        assert_eq!(starter_origin(r#"{"type":"user","message":{"role":"user","content":"old"}}"#), None);
    }
}
