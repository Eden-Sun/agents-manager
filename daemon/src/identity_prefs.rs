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
    // kind × host 啟用開關（使用者 2026-11-14：grok 不訂閱）。停用＝不挑、不派、不啟動、不探測。
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS kind_prefs (
          host TEXT NOT NULL,
          kind TEXT NOT NULL,
          disabled INTEGER NOT NULL DEFAULT 0,
          updated_at TEXT NOT NULL,
          PRIMARY KEY (host, kind)
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

/// 預設帳號（每個 kind 不換設定目錄的那個）在 `identity_prefs` 裡的身份名；不是合法的身份名（身份名是小寫 slug），不會跟真的身份撞名。
/// 「停用」它＝在 AG Man 隱藏這個 kind 的預設帳號（額度條不列、設定頁標已隱藏），不動 CLI 的登入、不刪任何檔案。
pub const DEFAULT_ACCOUNT: &str = "@default";

/// 身份停用搬進 daemon（原本只在瀏覽器 localStorage），挑身份時才看得到。
pub async fn put_identity_disabled(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(b): Json<DisableIn>,
) -> Result<Json<Value>, LcError> {
    if !["claude", "codex", "grok", "agy"].contains(&b.kind.as_str()) {
        return Err(LcError::Bad(
            "kind must be one of claude | codex | grok | agy".into(),
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


/// `kind` 在 `host` 上是否被停用（沒有列＝啟用）。讀失敗當作啟用，由呼叫端自己決定要不要擋。
pub async fn kind_disabled(pool: &SqlitePool, host: &str, kind: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT disabled FROM kind_prefs WHERE host=? AND kind=?")
        .bind(host)
        .bind(kind)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
        != 0
}

#[derive(Deserialize)]
pub struct KindDisableIn {
    pub host: Option<String>,
    pub disabled: bool,
}

/// `GET /api/kind-prefs`：停用中的 `{host, kind}`。
pub async fn get_kind_prefs(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT host, kind FROM kind_prefs WHERE disabled = 1 ORDER BY host, kind")
        .fetch_all(&app.db)
        .await
        .map_err(up)?;
    Ok(Json(json!({"disabled": rows.iter().map(|(h, k)| json!({"host": h, "kind": k})).collect::<Vec<_>>()})))
}

/// `PUT /api/kinds/{kind}/disabled {host?, disabled}`：停用／啟用一個 kind 在某台主機上。跑著的 bot 不殺。
pub async fn put_kind_disabled(
    State(app): State<Arc<App>>,
    Path(kind): Path<String>,
    Json(b): Json<KindDisableIn>,
) -> Result<Json<Value>, LcError> {
    if !crate::config::valid_bot_kind(&kind) || kind == crate::config::API_BOT_KIND {
        return Err(LcError::Bad("kind must be claude | codex | grok | agy".into()));
    }
    let host = b
        .host
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or(crate::config::LOCAL_HOST)
        .to_string();
    sqlx::query(
        "INSERT INTO kind_prefs (host, kind, disabled, updated_at) VALUES (?,?,?,?)
         ON CONFLICT(host, kind) DO UPDATE SET disabled = excluded.disabled, updated_at = excluded.updated_at",
    )
    .bind(&host)
    .bind(&kind)
    .bind(b.disabled as i64)
    .bind(crate::db::now())
    .execute(&app.db)
    .await
    .map_err(up)?;
    app.emit("kind_prefs_changed", json!({"host": host, "kind": kind, "disabled": b.disabled})).await;
    let app_bg = app.clone();
    tokio::spawn(Box::pin(async move {
        crate::runners::upstream_update::refresh_now(&app_bg).await;
    }));
    Ok(Json(json!({"host": host, "kind": kind, "disabled": b.disabled})))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 預設帳號（`@default`）與 agy 都能存「隱藏」；回來看得到、能復原。
    #[tokio::test]
    async fn the_default_account_of_any_kind_can_be_hidden_and_shown_again() {
        let e = crate::testing::env().await;
        for kind in ["grok", "agy"] {
            let put = |disabled: bool| {
                put_identity_disabled(
                    State(e.app.clone()),
                    Path(DEFAULT_ACCOUNT.to_string()),
                    Json(DisableIn { kind: kind.into(), disabled, host: None }),
                )
            };
            put(true).await.unwrap();
            let got = get_identity_prefs(State(e.app.clone())).await.unwrap().0;
            assert!(
                got["disabled"].as_array().unwrap().iter().any(|r| r["kind"] == kind && r["identity"] == DEFAULT_ACCOUNT && r["host"] == "local"),
                "{got}"
            );
            put(false).await.unwrap();
            let got = get_identity_prefs(State(e.app.clone())).await.unwrap().0;
            assert!(!got["disabled"].as_array().unwrap().iter().any(|r| r["kind"] == kind), "{got}");
        }
        let bad = put_identity_disabled(State(e.app.clone()), Path("x".into()), Json(DisableIn { kind: "nope".into(), disabled: true, host: None })).await;
        assert!(matches!(bad, Err(LcError::Bad(_))));
    }

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

    /// kind 停用是 kind × host：停用 grok@local 不影響 grok@m4p，且 API 只收合法 kind。
    #[tokio::test]
    async fn kind_disable_is_per_host_and_rejects_bad_kinds() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let Json(_) = put_kind_disabled(State(app.clone()), Path("grok".into()), Json(KindDisableIn { host: Some("local".into()), disabled: true })).await.unwrap();
        assert!(kind_disabled(&app.db, "local", "grok").await);
        assert!(!kind_disabled(&app.db, "m4p", "grok").await);
        assert!(!kind_disabled(&app.db, "local", "codex").await);
        let Json(prefs) = get_kind_prefs(State(app.clone())).await.unwrap();
        assert_eq!(prefs["disabled"].as_array().unwrap().len(), 1);
        let Json(_) = put_kind_disabled(State(app.clone()), Path("grok".into()), Json(KindDisableIn { host: None, disabled: false })).await.unwrap();
        assert!(!kind_disabled(&app.db, "local", "grok").await);
        assert!(put_kind_disabled(State(app.clone()), Path("claude-api".into()), Json(KindDisableIn { host: None, disabled: true })).await.is_err());
        assert!(put_kind_disabled(State(app.clone()), Path("nope".into()), Json(KindDisableIn { host: None, disabled: true })).await.is_err());
    }

    /// put_kind_disabled 在背景執行 refresh_now，不等待 upstream refresh 就立即回應。
    #[tokio::test]
    async fn put_kind_disabled_responds_without_waiting_for_refresh() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        // 持有 tick_mutex 鎖：若 put_kind_disabled 是同步等待 refresh_now，將會卡住或死鎖。
        let guard = crate::runners::upstream_update::tick_mutex().lock().await;
        let res = put_kind_disabled(
            State(app.clone()),
            Path("grok".into()),
            Json(KindDisableIn {
                host: Some("local".into()),
                disabled: true,
            }),
        )
        .await;
        assert!(res.is_ok(), "handler 應立即回應 Ok，不等待 tick_mutex 釋放");
        drop(guard);
    }
}
