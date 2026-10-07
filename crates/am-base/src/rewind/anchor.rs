//! 倒回之後**下一次 `--resume`／`--fork-session` 接到倒回點**（SPEC §6.13「重啟後」）。
//!
//! claude 2.1.289 實測（拋棄式目錄＋真的 claude，`claude_2.1.289_rewind_then_exit.jsonl` 是那段 transcript 裁掉內容的版本）：
//! - TUI 的 `/rewind` → Restore **一行都不寫進 transcript**，只截掉記憶體裡的對話。之後送出的下一則才寫進去，
//!   `parentUuid` 指向倒回點，transcript 自然分岔。
//! - 倒回後沒送任何東西就結束（daemon 重啟就是這樣），結束時 CLI 還補一行 `{"type":"last-prompt","leafUuid":<舊分支的尾巴>}`；
//!   `--resume` 照 transcript 挑 leaf，接回的是**倒回前的舊分支**，被倒掉的問答又回到 context（2026-10-04 ai-cc）。
//! - `--resume-session-at <uuid>` 在互動模式照舊被忽略（2.1.289 實測仍是舊分支）。
//! - CLI 自己認得「倒回錨點」：`{"type":"last-prompt","leafUuid":<uuid>,"explicit":true,"rewound":true}`（`leafUuid:null`＝倒回到空對話）。
//!   transcript 最後一個 `last-prompt` 是它時，`--resume`／`--resume … --fork-session` 都從那個 uuid 接；之後新回合從它往下長，
//!   不送訊息再重啟一次也保得住。**但 CLI 還開著時寫進去沒用**：結束時補的那行舊 leaf 會蓋掉它（實測）。
//!
//! 所以：倒回成功時（還握著 bot 鎖、下一則還沒打進去）從 transcript 找出那一則的 `parentUuid`、記下當時 transcript 的長度
//! （`rewind_anchors`）；下一次照這段 session `--resume`／fork 之前，長度之後**沒有新的 user／assistant 列**（＝倒回後沒有
//! 新回合，CLI 記憶體裡那條短的鏈從來沒落地）就把錨點補在檔尾。有新回合就是 CLI 已經從倒回點長出新分支，錨點作廢。
//! 只做本機 bot（遠端的 transcript 在別台）；找不到那一則、讀不到檔都只記 log，照舊重啟（等於修之前的行為）。

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::db;
use crate::pasted_content;

use super::{same_first_line, squash};

/// 倒回點：`Some(uuid)`＝從這個 uuid 接；`None`＝倒回的是這段 session 的第一則，接回空對話。
pub type Leaf = Option<String>;

#[derive(Deserialize)]
struct Row<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<&'a str>,
    #[serde(borrow)]
    uuid: Option<&'a str>,
    #[serde(rename = "parentUuid")]
    parent: Option<String>,
    #[serde(rename = "isSidechain", default)]
    sidechain: bool,
    #[serde(rename = "isMeta", default)]
    meta: bool,
    #[serde(rename = "isCompactSummary", default)]
    compact_summary: bool,
    /// `last-prompt` 的 leaf；`null` 與沒有這個鍵要分開（`explicit` 的 `null`＝清空）。
    #[serde(rename = "leafUuid", default, deserialize_with = "present")]
    leaf: Option<Option<String>>,
    #[serde(default)]
    explicit: bool,
    message: Option<Value>,
}

fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::<String>::deserialize(d)?))
}

fn rows(jsonl: &str) -> impl Iterator<Item = Row<'_>> {
    jsonl.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str::<Row>(l).ok())
}

fn is_turn_row(r: &Row) -> bool {
    matches!(r.kind, Some("user" | "assistant")) && !r.sidechain
}

/// user 列裡使用者打的字（拆掉 `<pasted_content>` 包裝）；tool_result、meta、壓縮摘要不是。
fn prompt_text(r: &Row) -> Option<String> {
    if r.kind != Some("user") || r.meta || r.compact_summary || r.sidechain {
        return None;
    }
    let text = match r.message.as_ref()?.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            if blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")) {
                return None;
            }
            blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
        }
        _ => return None,
    };
    (!text.trim().is_empty()).then(|| pasted_content::original(&text).into_owned())
}

/// 從 transcript 找倒回點：沿目前這條鏈（跟 CLI 一樣，最後一個 user／assistant 列或 `last-prompt` 的 leaf）往回走，
/// 由新到舊第 `skip+1` 個「第一行一樣」的使用者訊息（跟選單同一套跳法）就是倒回的那一則，回它的 `parentUuid`。
/// 那一則的全文對不上（`squash` 後要相同）或走不到就是 `None`：不猜。
pub fn find(jsonl: &str, target: &str, skip: usize) -> Option<Leaf> {
    let mut nodes: std::collections::HashMap<String, (Option<String>, Option<String>)> = std::collections::HashMap::new();
    let mut leaf: Option<String> = None;
    for r in rows(jsonl) {
        if r.kind == Some("last-prompt") {
            match r.leaf {
                Some(Some(l)) => leaf = Some(l),
                Some(None) if r.explicit => leaf = None,
                _ => {}
            }
            continue;
        }
        let Some(uuid) = r.uuid else { continue };
        if is_turn_row(&r) {
            leaf = Some(uuid.to_string());
        }
        nodes.insert(uuid.to_string(), (r.parent.clone(), prompt_text(&r)));
    }
    let want = squash(target);
    let mut remaining = skip;
    let mut cur = leaf;
    let mut steps = 0usize;
    while let Some(id) = cur {
        steps += 1;
        if steps > nodes.len() + 1 {
            return None; // 環：transcript 壞了
        }
        let (parent, text) = nodes.get(&id)?;
        if let Some(text) = text.as_deref().filter(|t| same_first_line(t, target)) {
            if remaining == 0 {
                return (squash(text) == want).then(|| parent.clone());
            }
            remaining -= 1;
        }
        cur = parent.clone();
    }
    None
}

/// 錨點那一行（CLI 自己倒回時寫的形狀）。
pub fn line(session_id: &str, leaf: &Leaf) -> String {
    serde_json::json!({"type": "last-prompt", "leafUuid": leaf, "explicit": true, "rewound": true, "sessionId": session_id}).to_string()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Need {
    /// 倒回後沒有新回合，最後一個 `last-prompt` 也不是錨點：要補。
    Append,
    /// 已經補過（之後沒有別的 `last-prompt` 蓋掉）。
    AlreadyThere,
    /// 倒回後有新回合：CLI 已經從倒回點長出新分支，錨點作廢。
    Obsolete,
}

/// `tail`＝倒回當時 transcript 長度之後新長出來的部分。
pub fn need(tail: &str, leaf: &Leaf) -> Need {
    let mut last_is_anchor = false;
    for r in rows(tail) {
        if is_turn_row(&r) {
            return Need::Obsolete;
        }
        if r.kind == Some("last-prompt") && r.leaf.is_some() {
            last_is_anchor = r.explicit && r.leaf.as_ref() == Some(leaf);
        }
    }
    if last_is_anchor {
        Need::AlreadyThere
    } else {
        Need::Append
    }
}

// ───────────── 記下／補上 ─────────────

/// 倒回成功之後、還握著 bot 鎖時呼叫：找出倒回點記進 `rewind_anchors`（同一段 session 只留最新的一筆）。
/// `text`／`skip`：真的按了 Restore 的那一則與它在選單上跳過幾則。失敗只記 log，不影響倒回本身。
pub async fn record(app: &impl crate::capabilities::Db, bot: &db::Bot, run: &db::Run, text: &str, skip: usize) {
    let (Some(sid), Some(path)) = (run.native_session_id.clone(), run.transcript_path.clone()) else {
        tracing::warn!(bot = %bot.name, "rewind: the run has no session/transcript on record; the next resume may bring the rewound turns back");
        return;
    };
    match db::bot_host(app.db(), &bot.id).await {
        Ok(h) if h == crate::config::LOCAL_HOST => {}
        Ok(_) => {
            tracing::info!(bot = %bot.name, "rewind: remote bot; the rewind point is not pinned for the next resume");
            return;
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "rewind: could not tell the bot's host; the rewind point is not pinned");
            return;
        }
    }
    let (target, p) = (text.to_string(), path.clone());
    let found = tokio::task::spawn_blocking(move || -> std::io::Result<(Option<Leaf>, u64)> {
        let bytes = std::fs::read(&p)?;
        Ok((find(&String::from_utf8_lossy(&bytes), &target, skip), bytes.len() as u64))
    })
    .await;
    let (leaf, len) = match found {
        Ok(Ok((Some(leaf), len))) => (leaf, len),
        Ok(Ok((None, _))) => {
            tracing::warn!(bot = %bot.name, session = %sid, "rewind: the rewound prompt was not found in the transcript; the next resume may bring the rewound turns back");
            return;
        }
        Ok(Err(e)) => {
            tracing::warn!(bot = %bot.name, transcript = %path, error = %e, "rewind: could not read the transcript; the rewind point is not pinned");
            return;
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "rewind: reading the transcript panicked");
            return;
        }
    };
    let res = sqlx::query(
        "INSERT INTO rewind_anchors (session_id, bot_id, transcript_path, leaf_uuid, transcript_len, created_at) VALUES (?,?,?,?,?,?)
         ON CONFLICT(session_id) DO UPDATE SET bot_id=excluded.bot_id, transcript_path=excluded.transcript_path,
           leaf_uuid=excluded.leaf_uuid, transcript_len=excluded.transcript_len, created_at=excluded.created_at",
    )
    .bind(&sid)
    .bind(&bot.id)
    .bind(&path)
    .bind(leaf.as_deref())
    .bind(len as i64)
    .bind(db::now())
    .execute(app.db())
    .await;
    match res {
        Ok(_) => tracing::info!(bot = %bot.name, session = %sid, leaf = ?leaf, "rewind: pinned the rewind point for the next resume"),
        Err(e) => tracing::warn!(bot = %bot.name, error = %e, "rewind: could not record the rewind point"),
    }
}

/// 照 `session_id` `--resume`／fork 之前呼叫：倒回後沒有新回合就把錨點補在 transcript 檔尾。
/// 呼叫端保證這段 session 的 CLI 已經結束（fork 例外：來源還開著也無妨，它結束時蓋掉的話下一次 resume 會再補）。
pub async fn ensure(app: &impl crate::capabilities::Db, session_id: &str) {
    let row: Option<(String, Option<String>, i64)> =
        match sqlx::query_as("SELECT transcript_path, leaf_uuid, transcript_len FROM rewind_anchors WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(app.db())
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(session = %session_id, error = %e, "rewind: could not read the pinned rewind point");
                return;
            }
        };
    let Some((path, leaf, len)) = row else { return };
    let sid = session_id.to_string();
    let outcome = tokio::task::spawn_blocking(move || apply(Path::new(&path), &sid, &leaf, len.max(0) as u64)).await;
    match outcome {
        Ok(Ok(Need::Append)) => tracing::info!(session = %session_id, "rewind: wrote the rewind point into the transcript before resuming"),
        Ok(Ok(Need::AlreadyThere)) => {}
        Ok(Ok(Need::Obsolete)) => {
            let _ = sqlx::query("DELETE FROM rewind_anchors WHERE session_id = ?").bind(session_id).execute(app.db()).await;
        }
        Ok(Err(e)) => tracing::warn!(session = %session_id, error = %e, "rewind: could not write the rewind point; resuming may bring the rewound turns back"),
        Err(e) => tracing::warn!(session = %session_id, error = %e, "rewind: writing the rewind point panicked"),
    }
}

/// 檔案那一半。檔案比倒回當時短＝被換過了，當作作廢。
pub fn apply(path: &Path, session_id: &str, leaf: &Leaf, len: u64) -> std::io::Result<Need> {
    use std::io::Write;
    let bytes = std::fs::read(path)?;
    if (bytes.len() as u64) < len {
        return Ok(Need::Obsolete);
    }
    let need = need(&String::from_utf8_lossy(&bytes[len as usize..]), leaf);
    if need == Need::Append {
        let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
        let sep = if bytes.last().is_some_and(|b| *b != b'\n') { "\n" } else { "" };
        f.write_all(format!("{sep}{}\n", line(session_id, leaf)).as_bytes())?;
    }
    Ok(need)
}
