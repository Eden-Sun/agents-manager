//! Project group chat (SPEC §13): one Project is one group; `@<bot>` / `@all` in the text
//! picks the recipients, the daemon fans the prompt out to each of them, and the group
//! timeline is every member bot's conversation merged by message id.
//!
//! No new conversation type: each recipient gets its own Turn + user Message through the
//! ordinary `lifecycle::prompt` path, stamped with a shared `messages.group_id` so the UI can
//! fold the copies back into one bubble.

use crate::db;
use crate::lifecycle::{self, LcError, LcResult};
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;

/// A bot as the mention parser sees it.
#[derive(Debug, Clone)]
pub struct Member {
    pub id: String,
    pub name: String,
}

/// SPEC §13.2: resolve `@all` / `@<name>` (case-insensitive, trailing punctuation tolerated)
/// against the project's bots. Returns the recipients in project order, deduplicated.
/// A `@` only counts when it starts the text or follows a non-word character, so
/// `me@example.com` is not a mention.
/// Characters that may appear inside an `@mention` token: anything but whitespace and the
/// usual sentence punctuation, so CJK nicknames work (`@小幫手，看一下`).
fn mention_char(c: char) -> bool {
    !c.is_whitespace() && !"@,:;?!。，、！？()（）[]{}<>\"'".contains(c)
}

pub fn parse_mentions(text: &str, members: &[Member]) -> Vec<Member> {
    let mut all = false;
    let mut hit: Vec<usize> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '@' {
            i += 1;
            continue;
        }
        let boundary = i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        let start = i + 1;
        let mut end = start;
        while end < chars.len() && mention_char(chars[end]) {
            end += 1;
        }
        i = end.max(i + 1);
        if !boundary || end == start {
            continue;
        }
        let raw: String = chars[start..end].iter().collect::<String>().to_ascii_lowercase();
        // `@name,` is already cut at the comma; `@name-` / `@name_` (trailing joiners) are
        // retried without them when the full token matches nobody.
        let candidates = [raw.clone(), raw.trim_end_matches(['-', '_']).to_string()];
        for cand in candidates.iter().filter(|c| !c.is_empty()) {
            if cand == "all" {
                all = true;
                break;
            }
            if let Some(idx) = members.iter().position(|m| m.name.to_ascii_lowercase() == *cand) {
                if !hit.contains(&idx) {
                    hit.push(idx);
                }
                break;
            }
        }
    }
    if all {
        return members.to_vec();
    }
    hit.sort_unstable();
    hit.into_iter().map(|i| members[i].clone()).collect()
}

/// Remove every recognised mention token (`@all`, `@<member>`, optionally followed by
/// `,` / `:` / `;`) and tidy the whitespace. Falls back to the original text when nothing
/// would be left, so a bare `@all` still delivers *something* rather than an empty prompt.
pub fn strip_mentions(text: &str, members: &[Member]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')) {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && mention_char(chars[end]) {
                end += 1;
            }
            if end > start {
                let raw: String = chars[start..end].iter().collect::<String>().to_ascii_lowercase();
                let trimmed = raw.trim_end_matches(['-', '_']).to_string();
                let known = raw == "all"
                    || trimmed == "all"
                    || members.iter().any(|m| {
                        let n = m.name.to_ascii_lowercase();
                        n == raw || n == trimmed
                    });
                if known {
                    let mut skip_to = end;
                    if skip_to < chars.len() && matches!(chars[skip_to], ',' | ':' | ';' | '，' | '：' | '；' | '、') {
                        skip_to += 1;
                    }
                    i = skip_to;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    // collapse whitespace runs left behind by removed tokens (keep newlines)
    let mut tidy = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        if c == ' ' || c == '\t' {
            if !prev_space {
                tidy.push(' ');
            }
            prev_space = true;
        } else {
            tidy.push(c);
            prev_space = c == '\n';
        }
    }
    let tidy = tidy.trim().to_string();
    if tidy.is_empty() {
        text.to_string()
    } else {
        tidy
    }
}

/// Why a recipient was skipped (SPEC §13.3). Short machine codes; the system message
/// carries the human-readable text.
fn skip_reason(app_err: &LcError) -> (&'static str, String) {
    match app_err {
        LcError::Conflict(v) => {
            let reason = v.get("reason").and_then(|r| r.as_str()).unwrap_or("conflict");
            let code = if reason.contains("no active run") {
                "not_running"
            } else if reason.contains("not running") {
                "not_running"
            } else if reason.contains("blocked") {
                "blocked"
            } else if reason.contains("in flight") {
                "in_flight"
            } else if reason.contains("unknown delivery") {
                "unknown_delivery"
            } else {
                "conflict"
            };
            (code, reason.to_string())
        }
        LcError::NotFound(w) => ("not_found", format!("not found: {w}")),
        LcError::Bad(m) => ("bad_request", m.clone()),
        LcError::BadValue(v) => ("bad_request", v.to_string()),
        LcError::Upstream(m) => ("upstream", m.clone()),
    }
}

/// Live bots of a project, in creation order.
pub async fn members(app: &Arc<App>, project_id: &str) -> LcResult<Vec<db::Bot>> {
    let bots = db::live_bots(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok(bots.into_iter().filter(|b| b.project_id == project_id).collect())
}

/// `POST /api/projects/:id/chat` (SPEC §13.4).
///
/// The group id **is** the client request id: a retry with the same id reaches the same
/// per-bot Turns (`<crid>:<bot_id>` idempotency in `lifecycle::prompt`) and does not add a
/// second "skipped" note.
pub async fn chat(
    app: &Arc<App>,
    project_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
) -> Result<Value, Response400> {
    let project = db::project(&app.db, project_id)
        .await
        .map_err(|e| Response400::Lc(LcError::Upstream(e.to_string())))?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| Response400::Lc(LcError::NotFound("project".into())))?;
    if text.trim().is_empty() {
        return Err(Response400::Lc(LcError::Bad("text must not be empty".into())));
    }
    if client_request_id.trim().is_empty() {
        return Err(Response400::Lc(LcError::Bad("client_request_id must not be empty".into())));
    }
    let bots = members(app, &project.id).await.map_err(Response400::Lc)?;
    let member_list: Vec<Member> = bots.iter().map(|b| Member { id: b.id.clone(), name: b.name.clone() }).collect();
    let targets = parse_mentions(text, &member_list);
    if targets.is_empty() {
        return Err(Response400::NoMention(
            bots.iter().map(|b| json!({"id": b.id, "name": b.name, "kind": b.kind})).collect(),
        ));
    }

    let group_id = client_request_id.to_string();
    // Bots must not see the routing syntax: `@all echo 1` reaches each bot as `echo 1`.
    let deliver = strip_mentions(text, &member_list);
    let mut sent = Vec::new();
    let mut skipped = Vec::new();
    for t in targets {
        let crid = format!("{client_request_id}:{}", t.id);
        match lifecycle::prompt_grouped(app, &t.id, text, &crid, Some(&group_id), Some(&deliver), attachment_ids).await {
            Ok(out) => sent.push(json!({
                "bot_id": t.id, "bot_name": t.name, "turn_id": out.turn_id,
                "message_id": out.message_id, "delivery": out.delivery,
            })),
            Err(e) => {
                let (code, human) = skip_reason(&e);
                tracing::info!(bot = %t.name, code, reason = %human, "group chat: recipient skipped");
                // SPEC §13.3: never auto-start; leave a note in that bot's conversation so
                // the group timeline shows who did not get the message.
                if let Err(e2) = note_skipped(app, &t, &group_id, code, &human).await {
                    tracing::warn!(bot = %t.name, error = %e2, "could not record the skipped note");
                }
                skipped.push(json!({"bot_id": t.id, "bot_name": t.name, "reason": code, "detail": human}));
            }
        }
    }
    Ok(json!({"group_id": group_id, "project_id": project.id, "sent": sent, "skipped": skipped}))
}

async fn note_skipped(app: &Arc<App>, bot: &Member, group_id: &str, code: &str, human: &str) -> anyhow::Result<()> {
    let conv = db::conversation_id(&app.db, &bot.id).await?;
    let dup: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND group_id = ? AND role = 'system'",
    )
    .bind(&conv)
    .bind(group_id)
    .fetch_one(&app.db)
    .await?;
    if dup > 0 {
        return Ok(());
    }
    let text = match code {
        "not_running" => format!("群組訊息未送達 {}：bot 未啟動（不會自動啟動）", bot.name),
        "blocked" => format!("群組訊息未送達 {}：agent 正在等待終端回應", bot.name),
        "in_flight" => format!("群組訊息未送達 {}：上一回合仍在進行中", bot.name),
        "unknown_delivery" => format!("群組訊息未送達 {}：上一回合送達狀態未知，請先放棄該回合", bot.name),
        _ => format!("群組訊息未送達 {}：{human}", bot.name),
    };
    lifecycle::insert_message_grouped(app, &conv, None, "system", &text, "system", false, None, Some(group_id)).await?;
    Ok(())
}

/// `chat` can fail with the ordinary error set or with the §13 `no_mention` body.
pub enum Response400 {
    Lc(LcError),
    NoMention(Vec<Value>),
}

/// `GET /api/projects/:id/messages?before=&limit=` (SPEC §13.4): every live member bot's
/// messages merged, paginated backwards by message id (ULID = time-ordered), returned in
/// ascending order.
pub async fn messages(app: &Arc<App>, project_id: &str, before: Option<&str>, limit: i64) -> LcResult<Value> {
    let project = db::project(&app.db, project_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let limit = limit.clamp(1, 500);
    const BASE: &str = "SELECT m.*, b.id AS bot_id, b.name AS bot_name FROM messages m
         JOIN conversations c ON c.id = m.conversation_id
         JOIN bots b ON b.id = c.bot_id
         WHERE b.project_id = ? AND b.deleted_at IS NULL";
    let rows = match before {
        Some(b) => sqlx::query_as::<_, db::GroupMessage>(&format!("{BASE} AND m.id < ? ORDER BY m.id DESC LIMIT ?"))
            .bind(&project.id)
            .bind(b)
            .bind(limit + 1)
            .fetch_all(&app.db)
            .await,
        None => sqlx::query_as::<_, db::GroupMessage>(&format!("{BASE} ORDER BY m.id DESC LIMIT ?"))
            .bind(&project.id)
            .bind(limit + 1)
            .fetch_all(&app.db)
            .await,
    }
    .map_err(|e| LcError::Upstream(e.to_string()))?;
    let has_more = rows.len() as i64 > limit;
    let mut msgs: Vec<db::GroupMessage> = rows.into_iter().take(limit as usize).collect();
    msgs.reverse();
    Ok(json!({"project_id": project.id, "messages": msgs, "has_more": has_more}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members() -> Vec<Member> {
        vec![
            Member { id: "1".into(), name: "g-claude".into() },
            Member { id: "2".into(), name: "g-codex".into() },
            Member { id: "3".into(), name: "x_y".into() },
        ]
    }

    fn names(text: &str) -> Vec<String> {
        parse_mentions(text, &members()).into_iter().map(|m| m.name).collect()
    }

    #[test]
    fn all_expands_to_every_member() {
        assert_eq!(names("@all Reply with GROUP-OK"), vec!["g-claude", "g-codex", "x_y"]);
        assert_eq!(names("hi @ALL"), vec!["g-claude", "g-codex", "x_y"]);
    }

    #[test]
    fn single_and_multiple_mentions_in_project_order() {
        assert_eq!(names("@g-codex 只有你"), vec!["g-codex"]);
        assert_eq!(names("@g-codex and @g-claude please"), vec!["g-claude", "g-codex"]);
        assert_eq!(names("@G-Claude, @g-claude: twice"), vec!["g-claude"]);
    }

    #[test]
    fn trailing_punctuation_and_joiners() {
        assert_eq!(names("@g-claude, look"), vec!["g-claude"]);
        assert_eq!(names("@g-claude: look"), vec!["g-claude"]);
        assert_eq!(names("@g-claude- look"), vec!["g-claude"]);
        assert_eq!(names("(@x_y)"), vec!["x_y"]);
    }

    #[test]
    fn no_mention_or_unknown_names() {
        assert!(names("plain text").is_empty());
        assert!(names("@nobody here").is_empty());
        assert!(names("mail me@g-claude now").is_empty(), "an email-like @ is not a mention");
        assert!(names("@").is_empty());
    }
}


#[cfg(test)]
mod strip_tests {
    use super::*;
    fn m(n: &str) -> Member {
        Member { id: n.to_string(), name: n.to_string() }
    }
    #[test]
    fn strips_all_and_members_only() {
        let ms = [m("am-claude"), m("am-codex")];
        assert_eq!(strip_mentions("@all echo 1", &ms), "echo 1");
        assert_eq!(strip_mentions("@am-claude, @am-codex: 請看一下", &ms), "請看一下");
        assert_eq!(strip_mentions("hey @am-claude what about @bob?", &ms), "hey what about @bob?");
        assert_eq!(strip_mentions("mail me@example.com @all", &ms), "mail me@example.com");
        assert_eq!(strip_mentions("@all", &ms), "@all");
        let cjk = [m("小幫手")];
        assert_eq!(parse_mentions("@小幫手，看一下", &cjk).len(), 1);
        assert_eq!(strip_mentions("@小幫手，看一下", &cjk), "看一下");
    }
}
