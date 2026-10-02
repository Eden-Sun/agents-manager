//! `GET /api/bots/deleted`（issue #757）：軟刪的 user bot 清單，讓「復原」不只活在刪除當下那個分頁的 15 秒通知。
//!
//! 只列 `managed_by='user'`、所屬專案還活著的：child 由父 bot／AGM 管（它們走 `POST /bots/{id}/restore` 自己復原），
//! 專案已刪的 bot 還原不回去（config 裡沒有那個專案）。復原仍是同一支 `restore_bot`。

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::lifecycle::LcError;
use crate::state::App;

/// 最多回這麼多顆（最近刪的在前）。
const LIMIT: i64 = 200;

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/bots/deleted", get(list_deleted))
}

pub(crate) async fn list_deleted(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows: Vec<(String, String, String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT b.id, b.name, b.kind, b.project_id, p.label, b.deleted_at,
                (SELECT MAX(m.created_at) FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE c.bot_id = b.id)
         FROM bots b JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL
         WHERE b.deleted_at IS NOT NULL AND b.managed_by = 'user'
         ORDER BY b.deleted_at DESC, b.id DESC LIMIT ?",
    )
    .bind(LIMIT)
    .fetch_all(&app.db)
    .await
    .map_err(|e| LcError::Upstream(e.to_string()))?;
    let bots: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, kind, project_id, project_label, deleted_at, last_message_at)| {
            json!({"id": id, "name": name, "kind": kind, "project_id": project_id, "project_label": project_label,
                   "deleted_at": deleted_at, "last_message_at": last_message_at})
        })
        .collect();
    Ok(Json(json!({ "bots": bots })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn add_bot(e: &crate::testing::Env, project: &str, name: &str, managed_by: &str, deleted_at: Option<&str>) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, hook_token, managed_by, deleted_at, created_at)
             VALUES (?,?,?,'claude','tok',?,?,?)",
        )
        .bind(&id)
        .bind(project)
        .bind(name)
        .bind(managed_by)
        .bind(deleted_at)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        id
    }

    async fn say(e: &crate::testing::Env, bot: &str, at: &str) {
        let conv = db::conversation_id(&e.app.db, bot).await.unwrap();
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'user','hi','web',?)")
            .bind(db::ulid())
            .bind(conv)
            .bind(at)
            .execute(&e.app.db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn lists_only_deleted_user_bots_of_live_projects_newest_first() {
        let e = crate::testing::env().await;
        let p = e.project_id.clone();
        let live = add_bot(&e, &p, "live", "user", None).await;
        let old = add_bot(&e, &p, "old", "user", Some("2026-09-01T00:00:00Z")).await;
        let new = add_bot(&e, &p, "new", "user", Some("2026-09-30T00:00:00Z")).await;
        let kid = add_bot(&e, &p, "kid", "child", Some("2026-09-30T00:00:00Z")).await;
        say(&e, &new, "2026-09-29T12:00:00Z").await;
        say(&e, &new, "2026-09-29T13:00:00Z").await;
        let dead_project = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, deleted_at, created_at) VALUES (?,?,?,?,?)")
            .bind(&dead_project)
            .bind("/tmp/gone")
            .bind("gone")
            .bind("2026-09-30T00:00:00Z")
            .bind(db::now())
            .execute(&e.app.db)
            .await
            .unwrap();
        let orphan = add_bot(&e, &dead_project, "orphan", "user", Some("2026-09-30T00:00:00Z")).await;

        let Json(v) = list_deleted(State(e.app.clone())).await.unwrap();
        let bots = v["bots"].as_array().unwrap();
        let ids: Vec<&str> = bots.iter().map(|b| b["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec![new.as_str(), old.as_str()], "{v}");
        assert!(!ids.contains(&live.as_str()) && !ids.contains(&kid.as_str()) && !ids.contains(&orphan.as_str()));
        assert_eq!(bots[0]["name"], "new");
        assert_eq!(bots[0]["project_id"], p);
        assert_eq!(bots[0]["project_label"], "proj");
        assert_eq!(bots[0]["deleted_at"], "2026-09-30T00:00:00Z");
        assert_eq!(bots[0]["last_message_at"], "2026-09-29T13:00:00Z");
        assert!(bots[1]["last_message_at"].is_null());
    }
}
