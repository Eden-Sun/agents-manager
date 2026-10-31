use crate::{lifecycle::LcError, state::App};
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

fn up(e: impl std::fmt::Display) -> LcError {
    LcError::Upstream(e.to_string())
}

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS identity_prefs (
          host TEXT NOT NULL,
          kind TEXT NOT NULL,
          identity TEXT NOT NULL,
          disabled INTEGER NOT NULL DEFAULT 0,
          updated_at TEXT NOT NULL,
          PRIMARY KEY (host, kind, identity)
        )",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("DROP TABLE IF EXISTS mission_events")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DROP TABLE IF EXISTS missions")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_identity_disabled(
    pool: &SqlitePool,
    host: &str,
    kind: &str,
    identity: &str,
    disabled: bool,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO identity_prefs (host, kind, identity, disabled, updated_at) VALUES (?,?,?,?,?)
         ON CONFLICT(host, kind, identity) DO UPDATE SET disabled = excluded.disabled, updated_at = excluded.updated_at",
    )
    .bind(host)
    .bind(kind)
    .bind(identity)
    .bind(disabled as i64)
    .bind(crate::db::now())
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct DisableIn {
    kind: String,
    disabled: bool,
    #[serde(default)]
    host: Option<String>,
}

/// 身份停用搬進 daemon（原本只在瀏覽器 localStorage），挑身份時才看得到。
pub async fn put_identity_disabled(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(b): Json<DisableIn>,
) -> Result<Json<Value>, LcError> {
    if !["claude", "codex", "grok"].contains(&b.kind.as_str()) {
        return Err(LcError::Bad(
            "kind must be one of claude | codex | grok".into(),
        ));
    }
    if name.trim().is_empty() {
        return Err(LcError::Bad("identity is empty".into()));
    }
    // 留白＝沒給（API.md `host?`）：存成空字串的停用列挑身份時永遠查不到（#1528）。
    let host = b
        .host
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or(crate::config::LOCAL_HOST)
        .to_string();
    set_identity_disabled(&app.db, &host, &b.kind, name.trim(), b.disabled)
        .await
        .map_err(up)?;
    app.emit(
        "identity_prefs_changed",
        json!({"host": host, "kind": b.kind, "identity": name, "disabled": b.disabled}),
    )
    .await;
    Ok(Json(
        json!({"host": host, "kind": b.kind, "identity": name, "disabled": b.disabled}),
    ))
}

pub async fn get_identity_prefs(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT host, kind, identity FROM identity_prefs WHERE disabled = 1 ORDER BY host, kind, identity")
            .fetch_all(&app.db)
            .await
            .map_err(up)?;
    Ok(Json(
        json!({"disabled": rows.iter().map(|(h, k, i)| json!({"host": h, "kind": k, "identity": i})).collect::<Vec<_>>()}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migration_drops_missions_and_preserves_identity_preferences() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for sql in [
            "CREATE TABLE missions (id TEXT PRIMARY KEY)",
            "CREATE INDEX missions_idx ON missions(id)",
            "CREATE TABLE mission_events (id TEXT PRIMARY KEY, mission_id TEXT REFERENCES missions(id))",
            "CREATE TRIGGER mission_events_trigger AFTER INSERT ON mission_events BEGIN SELECT 1; END",
            "CREATE TABLE identity_prefs (host TEXT NOT NULL, kind TEXT NOT NULL, identity TEXT NOT NULL, disabled INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL, PRIMARY KEY(host,kind,identity))",
            "INSERT INTO identity_prefs VALUES ('local','claude','cc1',1,'now')",
            "INSERT INTO missions VALUES ('m1')",
            "INSERT INTO mission_events VALUES ('e1','m1')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        migrate(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('missions','mission_events','missions_idx','mission_events_trigger')").fetch_one(&pool).await.unwrap();
        assert_eq!(remaining, 0);
        let disabled: i64 =
            sqlx::query_scalar("SELECT disabled FROM identity_prefs WHERE identity='cc1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(disabled, 1);
        set_identity_disabled(&pool, "local", "claude", "cc1", false)
            .await
            .unwrap();
        assert_eq!(
            crate::db::disabled_identities(&pool, "local", "claude")
                .await
                .unwrap()
                .len(),
            0
        );
    }

    /// `host` 留白（空字串或只有空白）＝沒給，照預設 `local` 存。以前存成空字串 host：挑身份讀的是 `local`，
    /// 那筆停用永遠查不到，回 200 卻沒有任何效果。
    #[tokio::test]
    async fn a_blank_identity_disable_host_falls_back_to_local() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        for (name, host) in [("cc-empty-host", ""), ("cc-space-host", "  ")] {
            let Json(out) = put_identity_disabled(
                State(app.clone()),
                Path(name.into()),
                Json(DisableIn {
                    kind: "claude".into(),
                    disabled: true,
                    host: Some(host.into()),
                }),
            )
            .await
            .unwrap();
            assert_eq!(
                out["host"],
                crate::config::LOCAL_HOST,
                "blank host {host:?} must default to local"
            );
        }
        let Json(prefs) = get_identity_prefs(State(app.clone())).await.unwrap();
        let rows = prefs["disabled"].as_array().unwrap();
        assert!(
            rows.iter().all(|r| r["host"] != ""),
            "no row may keep an empty host: {rows:?}"
        );
        for name in ["cc-empty-host", "cc-space-host"] {
            assert!(
                rows.iter()
                    .any(|r| r["host"] == crate::config::LOCAL_HOST && r["identity"] == name),
                "{name} must be disabled on local: {rows:?}"
            );
        }
    }
}
