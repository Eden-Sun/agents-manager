//! Project group chat (SPEC §13). No new conversation type: each recipient gets an ordinary
//! `lifecycle::prompt` Turn stamped with a shared `messages.group_id` the UI folds into one bubble.

use crate::db;
use crate::lifecycle::{self, LcError, LcResult};
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Member {
    pub id: String,
    pub name: String,
}

/// Excludes only whitespace and sentence punctuation, so CJK nicknames work (`@小幫手，看一下`).
fn mention_char(c: char) -> bool {
    !c.is_whitespace() && !"@,:;?!。，、！？()（）[]{}<>\"'".contains(c)
}

/// 名字裡有空白的成員（2026-09-19 使用者：「bot name should be able to include space」）：`@my bot` 在第一個空白就被
/// `mention_char` 切斷，只剩 `my`。`@` 後面若正好接著某個成員的**整個**名字（不分大小寫）、而且名字後面是結尾或
/// 非名字字元，就算提到它。最長的先比，`@my bot 2` 不會被 `my bot` 搶走。回 `(成員索引, 名字結束位置)`。
fn spaced_member_at(chars: &[char], start: usize, members: &[Member]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (idx, m) in members.iter().enumerate() {
        if !m.name.contains(' ') {
            continue;
        }
        let name: Vec<char> = m.name.chars().collect();
        let end = start + name.len();
        if end > chars.len() {
            continue;
        }
        let same = chars[start..end].iter().zip(&name).all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()));
        if !same || (end < chars.len() && mention_char(chars[end]) && !matches!(chars[end], '-' | '_')) {
            continue;
        }
        if best.is_none_or(|(_, e)| end > e) {
            best = Some((idx, end));
        }
    }
    best
}

/// SPEC §13.2. A `@` only counts after a non-word character, so `me@example.com` is not a mention.
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
        if boundary {
            if let Some((idx, end)) = spaced_member_at(&chars, start, members) {
                if !hit.contains(&idx) {
                    hit.push(idx);
                }
                i = end;
                continue;
            }
        }
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

/// Falls back to the original text when nothing is left, so a bare `@all` is not an empty prompt.
pub fn strip_mentions(text: &str, members: &[Member]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')) {
            let start = i + 1;
            if let Some((_, end)) = spaced_member_at(&chars, start, members) {
                let mut skip_to = end;
                if skip_to < chars.len() && matches!(chars[skip_to], ',' | ':' | ';' | '，' | '：' | '；' | '、') {
                    skip_to += 1;
                }
                i = skip_to;
                continue;
            }
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

/// SPEC §13.3 machine codes; the system message carries the human-readable text.
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
        LcError::Unprocessable(v) => ("unprocessable", v.to_string()),
        LcError::Forbidden(v) => ("forbidden", v.to_string()),
        LcError::Unavailable(v) => ("unavailable", v.to_string()),
        LcError::Uncommitted(v) => ("uncommitted", v.to_string()),
        LcError::Upstream(m) => ("upstream", m.clone()),
    }
}

pub async fn members(app: &Arc<App>, project_id: &str) -> LcResult<Vec<db::Bot>> {
    let bots = db::live_bots(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok(bots.into_iter().filter(|b| b.project_id == project_id).collect())
}

/// `POST /api/projects/:id/chat` (SPEC §13.4). The group id **is** the client request id, so a
/// retry reaches the same per-bot Turns and adds no second "skipped" note.
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
        // #167：字已經打進去、只是送達結果寫不進 DB（`Uncommitted`）——那是「結果不明」，不是「沒送」：放進 `sent`
        // （`delivery:"unknown"`，daemon 自己補），不插「未送達」。同一個 crid 重送走冪等分支，不會再打字。
        let sent_now = lifecycle::owed_as_unknown(
            lifecycle::prompt_grouped(app, &t.id, text, &crid, Some(&group_id), Some(&deliver), attachment_ids, None).await,
        );
        match sent_now {
            Ok(out) => {
                // 同一個 crid 之前對這顆寫過「未送達」note（當時沒在跑），這次送到了：note 已經不是事實，撤掉（#340）。
                if let Ok(conv) = db::conversation_id(&app.db, &t.id).await {
                    if let Err(e) = sqlx::query("DELETE FROM messages WHERE conversation_id = ? AND group_id = ? AND role = 'system'")
                        .bind(&conv)
                        .bind(&group_id)
                        .execute(&app.db)
                        .await
                    {
                        tracing::warn!(bot = %t.name, error = %e, "could not retire the stale skipped note");
                    }
                }
                sent.push(json!({
                "bot_id": t.id, "bot_name": t.name, "turn_id": out.turn_id,
                "message_id": out.message_id, "delivery": out.delivery,
                }))
            }
            Err(e) => {
                let (code, human) = skip_reason(&e);
                tracing::info!(bot = %t.name, code, reason = %human, "group chat: recipient skipped");
                // SPEC §13.3: never auto-start; the note shows who did not get the message.
                if let Err(e2) = note_skipped(app, &t, &group_id, code, &human).await {
                    tracing::warn!(bot = %t.name, error = %e2, "could not record the skipped note");
                }
                skipped.push(json!({"bot_id": t.id, "bot_name": t.name, "reason": code, "detail": human}));
            }
        }
    }
    // `delivered:false`＝一顆都沒送到（全被跳過）：仍是 200（各顆的「未送達」note 已記），但前端不該把它當成「送出去了」而清掉草稿（#340）。
    Ok(json!({"group_id": group_id, "project_id": project.id, "delivered": !sent.is_empty(), "sent": sent, "skipped": skipped}))
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

/// `GET /api/projects/:id/messages?before=&limit=` (SPEC §13.4), paginated by SQLite rowid.
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
    let before_rowid = match before {
        Some(message_id) => Some(
            sqlx::query_scalar::<_, i64>("SELECT rowid FROM messages WHERE id = ?")
                .bind(message_id)
                .fetch_optional(&app.db)
                .await
                .map_err(|e| LcError::Upstream(e.to_string()))?
                .ok_or_else(|| LcError::Bad(format!("before message does not exist: {message_id}")))?,
        ),
        None => None,
    };
    let rows = match before_rowid {
        Some(rowid) => sqlx::query_as::<_, db::GroupMessage>(&format!("{BASE} AND m.rowid < ? ORDER BY m.rowid DESC LIMIT ?"))
            .bind(&project.id)
            .bind(rowid)
            .bind(limit + 1)
            .fetch_all(&app.db)
            .await,
        None => sqlx::query_as::<_, db::GroupMessage>(&format!("{BASE} ORDER BY m.rowid DESC LIMIT ?"))
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
    /// 名字有空白（2026-09-19 使用者）：`@my bot` 要整個名字對上，`@my bot 2` 取最長的那顆。
    #[test]
    fn a_name_with_spaces_is_mentioned_by_its_whole_name() {
        let ms = vec![m("my bot"), m("my bot 2"), m("my")];
        let pick = |t: &str| parse_mentions(t, &ms).into_iter().map(|x| x.name).collect::<Vec<_>>();
        assert_eq!(pick("@my bot 看一下"), ["my bot"]);
        assert_eq!(pick("@My Bot，看一下"), ["my bot"], "不分大小寫、全形逗號收尾");
        assert_eq!(pick("@my bot 2 跟 @my bot"), ["my bot", "my bot 2"]);
        assert_eq!(pick("@my botanist"), ["my"], "後面還接著字就不是 my bot，照舊比到 my");
        assert_eq!(pick("@my 自己"), ["my"], "沒空白的照舊");
        assert_eq!(strip_mentions("@my bot, 看一下", &ms), "看一下");
        assert_eq!(strip_mentions("@my bot 2 跟 @my 說", &ms), "跟 說");
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

#[cfg(test)]
mod message_tests {
    use super::*;

    #[tokio::test]
    async fn messages_page_by_rowid_when_ids_are_out_of_order() {
        let dir = std::env::temp_dir().join(format!("am-group-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
        let app = App::new(
            pool,
            client.clone(),
            client,
            cfg,
            dir.clone(),
            dir.join("agents-managerd"),
            7799,
            "test-token".into(),
            "test".into(),
            false,
        );
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','bot','claude','tok',?)")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES ('c1','b1',?)")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        // Same millisecond, IDs in reverse lexical order: only rowid order is stable.
        let created_at = db::now();
        let ids = [
            "01ARZ3NDEKTSV4RRFFQ69G5F3C",
            "01ARZ3NDEKTSV4RRFFQ69G5F3B",
            "01ARZ3NDEKTSV4RRFFQ69G5F3A",
        ];
        for id in ids {
            sqlx::query(
                "INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,'c1','user',?,'web',?)",
            )
            .bind(id)
            .bind(id)
            .bind(&created_at)
            .execute(&app.db)
            .await
            .unwrap();
        }

        let mut cursor = None;
        let mut got = Vec::new();
        let mut has_more = Vec::new();
        for _ in 0..3 {
            let page = messages(&app, "p1", cursor.as_deref(), 1).await.unwrap();
            let page_messages = page["messages"].as_array().unwrap();
            assert_eq!(page_messages.len(), 1);
            got.push(page_messages[0]["id"].as_str().unwrap().to_string());
            has_more.push(page["has_more"].as_bool().unwrap());
            cursor = Some(page_messages[0]["id"].as_str().unwrap().to_string());
        }
        assert_eq!(got, ids.into_iter().rev().map(str::to_string).collect::<Vec<_>>());
        assert_eq!(has_more, vec![true, true, false]);

        let err = messages(&app, "p1", Some("missing-before"), 1).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(message) if message.contains("before") && message.contains("does not exist")));

        app.db.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// #167：收件 bot 的送達結果寫不進 DB（`LcError::Uncommitted`）＝字**已經打進 pane**，只是結果欠著。
/// 群組送出以前把它記成 `skipped`（`detail` 是整段 503 JSON）並在那顆 bot 的對話插「群組訊息未送達」——
/// 其實送到了、agent 正在做；照字面重發就是 bot 收到兩份。
#[cfg(test)]
mod uncommitted_tests {
    use super::*;
    use crate::testing as tt;

    /// 一顆閒著、打字進 pane 的 claude（同 `owed_delivery::tests::idle_bot`）。
    async fn idle_bot(env: &tt::Env, name: &str) -> (db::Bot, String) {
        let app = &env.app;
        let bot = tt::claude_bot(app, &env.project_id, name).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-api','api-bot','test',1,?)",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        (bot, conv)
    }

    async fn lose_delivery_writes(app: &Arc<App>) {
        sqlx::query("CREATE TRIGGER lost_delivery_write BEFORE UPDATE OF delivery ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();
    }

    fn typed(env: &tt::Env) -> usize {
        env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count()
    }

    async fn system_notes(app: &Arc<App>, conv: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(conv)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_recipient_whose_delivery_result_is_owed_is_sent_as_unknown_not_skipped() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (_bot, conv) = idle_bot(&env, "grp-owed").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;

        let out = chat(&app, &env.project_id, "@grp-owed 跑一下測試", "crid-grp", &[]).await.ok().expect("chat 本身要成功");
        assert_eq!(typed(&env), 1, "字已經打進 pane 了");
        assert!(out["skipped"].as_array().unwrap().is_empty(), "已送出的收件人不能算「跳過」：{out}");
        let sent = out["sent"].as_array().unwrap();
        assert_eq!(sent.len(), 1, "{out}");
        assert_eq!(sent[0]["delivery"], "unknown", "結果寫不進去＝結果不明，不假裝成功也不說失敗：{out}");
        assert!(sent[0]["turn_id"].as_str().is_some_and(|t| !t.is_empty()), "前端靠 turn_id 鎖那一回合：{out}");
        assert!(sent[0]["message_id"].as_str().is_some_and(|t| !t.is_empty()), "{out}");
        let notes = system_notes(&app, &conv).await;
        assert!(notes.iter().all(|n| !n.contains("未送達")), "對話裡不能說「未送達」：{notes:?}");
    }

    /// 同一個群組訊息重送（前端／使用者重按）：走冪等分支，DB 還壞著時照樣是結果不明、**不再打字**；
    /// DB 好了之後拿到寫好的結果。
    #[tokio::test]
    async fn retrying_the_group_message_never_types_it_again() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (_bot, conv) = idle_bot(&env, "grp-retry").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;

        let first = chat(&app, &env.project_id, "@grp-retry 跑一下測試", "crid-retry", &[]).await.ok().unwrap();
        let turn_id = first["sent"][0]["turn_id"].as_str().unwrap().to_string();
        let again = chat(&app, &env.project_id, "@grp-retry 跑一下測試", "crid-retry", &[]).await.ok().unwrap();
        assert!(again["skipped"].as_array().unwrap().is_empty(), "{again}");
        assert_eq!(again["sent"][0]["turn_id"], turn_id.as_str(), "同一個回合：{again}");
        assert_eq!(typed(&env), 1, "DB 還壞著：重送不再打字");

        sqlx::query("DROP TRIGGER lost_delivery_write").execute(&app.db).await.unwrap();
        let healed = chat(&app, &env.project_id, "@grp-retry 跑一下測試", "crid-retry", &[]).await.ok().unwrap();
        assert_eq!(healed["sent"][0]["turn_id"], turn_id.as_str(), "{healed}");
        assert_eq!(healed["sent"][0]["delivery"], "ok", "DB 好了：拿到寫好的結果：{healed}");
        assert_eq!(typed(&env), 1, "從頭到尾只打了一次");
        assert!(system_notes(&app, &conv).await.iter().all(|n| !n.contains("未送達")));
    }

    /// 只要有一個收件人欠著，其他收件人照常送、照常算 `sent`；真的沒送出的（bot 沒在跑）仍是 `skipped`＋說明。
    #[tokio::test]
    async fn one_owed_recipient_does_not_change_how_the_others_are_reported() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (_owed, _conv) = idle_bot(&env, "grp-a").await;
        let stopped = tt::claude_bot(&app, &env.project_id, "grp-b").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;

        let out = chat(&app, &env.project_id, "@all 跑一下測試", "crid-mix", &[]).await.ok().unwrap();
        assert_eq!(out["sent"].as_array().unwrap().len(), 1, "{out}");
        assert_eq!(out["sent"][0]["bot_name"], "grp-a", "{out}");
        let skipped = out["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1, "{out}");
        assert_eq!(skipped[0]["bot_id"], stopped.id.as_str(), "{out}");
        assert_eq!(skipped[0]["reason"], "not_running", "{out}");
    }

    /// #340：一顆都沒送到要明說（delivered:false），前端才不會清掉草稿；重送後那顆送到了，先前的「未送達」note 要撤掉。
    #[tokio::test]
    async fn an_all_skipped_send_says_it_delivered_nothing_and_a_retry_retires_the_stale_note() {
        let env = tt::env().await;
        let app = env.app.clone();
        let stopped = tt::claude_bot(&app, &env.project_id, "grp-only").await;
        let conv = db::conversation_id(&app.db, &stopped.id).await.unwrap();
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });

        let out = chat(&app, &env.project_id, "@all hi", "crid-none", &[]).await.ok().unwrap();
        assert_eq!(out["delivered"], false, "{out}");
        assert_eq!(out["sent"].as_array().unwrap().len(), 0);
        assert_eq!(system_notes(&app, &conv).await.len(), 1, "有一則未送達 note");

        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-api','api-bot','test',1,?)",
        )
        .bind(db::ulid())
        .bind(&stopped.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let out = chat(&app, &env.project_id, "@all hi", "crid-none", &[]).await.ok().unwrap();
        assert_eq!(out["delivered"], true, "{out}");
        assert!(system_notes(&app, &conv).await.is_empty(), "送到了，先前的未送達 note 要撤掉");
    }
}
