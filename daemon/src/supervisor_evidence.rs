//! Searchable, attributable history for AGM. Unlike the sidebar hit count, every result
//! identifies its source message and includes deleted bots so old work stays discoverable.
use crate::{lifecycle::LcError, state::App};
use axum::{extract::{Query, State}, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Debug, Default, Deserialize)]
pub struct EvidenceQuery {
    pub q: String,
    pub bot_id: Option<String>,
    pub project_id: Option<String>,
    pub before: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct Evidence {
    id: String,
    bot_id: String,
    bot_name: String,
    project_id: String,
    project_label: String,
    bot_deleted: bool,
    turn_id: Option<String>,
    role: String,
    content: String,
    source: String,
    incomplete: bool,
    created_at: String,
}

pub async fn search(State(app): State<Arc<App>>, Query(q): Query<EvidenceQuery>) -> Result<Json<Value>, LcError> {
    query(&app.db, q).await.map(Json)
}

async fn query(pool: &SqlitePool, q: EvidenceQuery) -> Result<Value, LcError> {
    let needle = q.q.trim();
    if needle.is_empty() || needle.chars().count() > 500 {
        return Err(LcError::Bad("q must contain 1–500 characters".into()));
    }
    let cursor: Option<(String, String)> = q.before.as_deref().map(|s| {
        if s.len() > 512 { return Err(LcError::Bad("invalid evidence cursor".into())); }
        serde_json::from_str(s).map_err(|_| LcError::Bad("invalid evidence cursor".into()))
    }).transpose()?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let pattern = format!("%{}%", needle.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let mut rows = sqlx::query_as::<_, Evidence>(
        r#"SELECT m.id, b.id AS bot_id, b.name AS bot_name,
                  p.id AS project_id, p.label AS project_label,
                  b.deleted_at IS NOT NULL AS bot_deleted, m.turn_id, m.role,
                  m.content, m.source, m.incomplete, m.created_at
           FROM messages m JOIN conversations c ON c.id=m.conversation_id
           JOIN bots b ON b.id=c.bot_id JOIN projects p ON p.id=b.project_id
           WHERE m.content LIKE ?1 ESCAPE '\'
             AND (?2 IS NULL OR b.id=?2) AND (?3 IS NULL OR p.id=?3)
             AND (?4 IS NULL OR m.created_at < ?4 OR (m.created_at=?4 AND m.id < ?5))
           ORDER BY m.created_at DESC, m.id DESC LIMIT ?6"#,
    )
    .bind(pattern).bind(q.bot_id).bind(q.project_id)
    .bind(cursor.as_ref().map(|c| &c.0)).bind(cursor.as_ref().map(|c| &c.1))
    .bind(limit + 1).fetch_all(pool).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next_cursor = if has_more {
        rows.last().map(|r| serde_json::to_string(&(&r.created_at, &r.id)).expect("string tuple"))
    } else { None };
    let messages: Vec<Value> = rows.into_iter().map(|r| {
        // Bound model input without hiding that the API returned only part of a source.
        let truncated = r.content.chars().count() > 16_000;
        let mut v = serde_json::to_value(&r).expect("evidence serialization");
        if truncated { v["content"] = json!(r.content.chars().take(16_000).collect::<String>()); }
        v["truncated"] = json!(truncated);
        v
    }).collect();
    Ok(json!({"messages": messages, "has_more": has_more, "next_cursor": next_cursor}))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed() -> SqlitePool {
        let db = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(crate::db::SCHEMA).execute(&db).await.unwrap();
        sqlx::raw_sql("INSERT INTO projects(id,path,label,created_at) VALUES ('p','/p','專案','now');
            INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','新名字','claude','x','now');
            INSERT INTO bots(id,project_id,name,kind,hook_token,created_at,deleted_at) VALUES ('old','p','舊bot','claude','y','now','later');
            INSERT INTO conversations(id,bot_id,created_at) VALUES ('c','b','now'),('co','old','now');
            INSERT INTO messages(id,conversation_id,role,content,source,created_at) VALUES
            ('m1','c','user','遠端登入 100% a_b','web','2026-09-09T00:00:00Z'),
            ('m2','c','assistant','遠端登入已處理','hook','2026-09-09T00:00:00Z'),
            ('m3','co','assistant','遠端登入舊決策','hook','2026-09-09T00:00:00Z');")
            .execute(&db).await.unwrap();
        db
    }

    #[tokio::test]
    async fn history_preserves_deleted_bots_and_pages_tied_timestamps() {
        let db = seed().await;
        let page = query(&db, EvidenceQuery { q:"遠端登入".into(), limit:Some(2), ..Default::default() }).await.unwrap();
        assert_eq!(page["messages"][0]["id"], "m3");
        assert_eq!(page["messages"][0]["bot_deleted"], true);
        assert_eq!(page["messages"][1]["bot_name"], "新名字");
        assert_eq!(page["has_more"], true);
        let next = query(&db, EvidenceQuery { q:"遠端登入".into(), before:Some(page["next_cursor"].as_str().unwrap().into()), limit:Some(2), ..Default::default() }).await.unwrap();
        assert_eq!(next["messages"].as_array().unwrap().len(), 1);
        assert_eq!(next["messages"][0]["id"], "m1");
        assert_eq!(next["has_more"], false);
    }

    #[tokio::test]
    async fn filters_and_literal_wildcards_do_not_broaden_search() {
        let db = seed().await;
        for term in ["%", "a_b"] {
            let page = query(&db, EvidenceQuery { q:term.into(), ..Default::default() }).await.unwrap();
            assert_eq!(page["messages"].as_array().unwrap().len(), 1);
        }
        let page = query(&db, EvidenceQuery { q:"遠端".into(), bot_id:Some("old".into()), ..Default::default() }).await.unwrap();
        assert_eq!(page["messages"].as_array().unwrap().len(), 1);
        let page = query(&db, EvidenceQuery { q:"遠端".into(), project_id:Some("missing".into()), ..Default::default() }).await.unwrap();
        assert!(page["messages"].as_array().unwrap().is_empty());
        assert!(matches!(query(&db, EvidenceQuery { q:"遠端".into(), before:Some("bad".into()), ..Default::default() }).await, Err(LcError::Bad(_))));
        assert!(matches!(query(&db, EvidenceQuery::default()).await, Err(LcError::Bad(_))));
    }

    #[tokio::test]
    async fn evidence_limits_large_sources_without_claiming_they_are_complete() {
        let db = seed().await;
        sqlx::query("UPDATE messages SET content=?, source='terminal_fallback', incomplete=1 WHERE id='m1'")
            .bind(format!("遠端{}", "長".repeat(17_000))).execute(&db).await.unwrap();
        let page = query(&db, EvidenceQuery { q:"遠端".into(), ..Default::default() }).await.unwrap();
        let m = page["messages"].as_array().unwrap().iter().find(|m| m["id"] == "m1").unwrap();
        assert_eq!(m["truncated"], true);
        assert_eq!(m["incomplete"], true);
        assert_eq!(m["source"], "terminal_fallback");
        assert_eq!(m["content"].as_str().unwrap().chars().count(), 16_000);
    }
}
