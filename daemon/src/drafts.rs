//! 對話輸入框的草稿（還沒送出的字），以 daemon 為準（使用者 2026-10-01）。
//!
//! 網頁以前把草稿存在 localStorage，手機與電腦、不同瀏覽器之間完全不同步。現在草稿存在 `composer_drafts`，
//! 網頁開頁 `GET /api/drafts` 載入、打字 debounce 後 `PUT /api/drafts/{key}`，別的瀏覽器靠 WS `draft_updated` 收到。
//!
//! - key 只收網頁現在用的三種：`bot:<bot id>`、`group:<project id>`、`shell:<host>/<pane id>`（host shell 的指令草稿，#758；pane 關掉時清）（[`valid_key`]）。
//! - `rev` 是**每個 key** 單調遞增的版本：寫入（內容真的變了）才加一，網頁用它丟掉晚到的舊事件。
//! - 空字串＝刪除，但列留著（`text = ''`，當墓碑）：整列刪掉的話 rev 會從 1 重來，網頁手上記著 5 就會把新的 1 當舊事件丟掉。
//!   `GET` 只列有字的。bot／專案被刪時一樣清成空字串（[`clear_keys`]）並推 `text: ""` 的事件，讓其他瀏覽器清掉。
//! - 內容沒變的 PUT 不加 rev、不推事件（自己的回音、重送都安全）。
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

/// 單份草稿的上限（位元組）。比網頁輸入框實際會打的長很多；再長多半是貼了整份檔案，這種留在本機就好。
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_KEY_LEN: usize = 128;
const MAX_CLIENT_ID_LEN: usize = 64;

#[derive(Debug, Clone, Serialize, sqlx::FromRow, PartialEq, Eq)]
pub struct Draft {
    pub key: String,
    pub text: String,
    pub rev: i64,
    pub updated_at: String,
}

/// host shell 面板的草稿 key（#758）：`shell:<host>/<pane id>`，pane id 只在單一主機內唯一。
pub fn shell_key(host: &str, pane_id: &str) -> String {
    format!("shell:{host}/{pane_id}")
}

fn shell_part(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@' | '+'))
}

/// `bot:<id>`／`group:<id>`，id 只收 ULID／bot id 會用到的字元；`shell:<host>/<pane id>` 見 [`shell_key`]。
pub fn valid_key(key: &str) -> bool {
    if key.len() > MAX_KEY_LEN {
        return false;
    }
    if let Some(rest) = key.strip_prefix("shell:") {
        return rest.split_once('/').is_some_and(|(host, pane)| shell_part(host) && shell_part(pane));
    }
    let Some(id) = key.strip_prefix("bot:").or_else(|| key.strip_prefix("group:")) else { return false };
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// 有字的草稿（墓碑不列），照 key 排。
pub async fn list(pool: &SqlitePool) -> sqlx::Result<Vec<Draft>> {
    sqlx::query_as("SELECT key, text, rev, updated_at FROM composer_drafts WHERE text <> '' ORDER BY key").fetch_all(pool).await
}

async fn get(pool: &SqlitePool, key: &str) -> sqlx::Result<Option<Draft>> {
    sqlx::query_as("SELECT key, text, rev, updated_at FROM composer_drafts WHERE key = ?").bind(key).fetch_optional(pool).await
}

/// 寫一份草稿。回傳（現況, 這次有沒有真的改到）；沒改到時 rev 不動。空字串＝刪除（留墓碑）。
pub async fn put(pool: &SqlitePool, key: &str, text: &str) -> sqlx::Result<(Option<Draft>, bool)> {
    let now = crate::db::now();
    let changed = if text.is_empty() {
        sqlx::query("UPDATE composer_drafts SET text = '', rev = rev + 1, updated_at = ? WHERE key = ? AND text <> ''")
            .bind(&now)
            .bind(key)
            .execute(pool)
            .await?
            .rows_affected()
            > 0
    } else {
        sqlx::query(
            "INSERT INTO composer_drafts (key, text, rev, updated_at) VALUES (?, ?, 1, ?)
             ON CONFLICT(key) DO UPDATE SET text = excluded.text, rev = composer_drafts.rev + 1, updated_at = excluded.updated_at
             WHERE composer_drafts.text <> excluded.text",
        )
        .bind(key)
        .bind(text)
        .bind(&now)
        .execute(pool)
        .await?
        .rows_affected()
            > 0
    };
    Ok((get(pool, key).await?, changed))
}

fn event(d: &Draft, client_id: &str) -> Value {
    json!({"key": d.key, "text": d.text, "rev": d.rev, "client_id": client_id, "updated_at": d.updated_at})
}

pub async fn get_http(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let drafts = list(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    Ok(Json(json!({"drafts": drafts})))
}

#[derive(Deserialize)]
pub struct PutIn {
    pub text: String,
    /// 發這次寫入的那個網頁分頁；原樣放進 `draft_updated`，它自己收到回音時據此略過。
    #[serde(default)]
    pub client_id: String,
}

pub async fn put_http(State(app): State<Arc<App>>, Path(key): Path<String>, Json(b): Json<PutIn>) -> Result<Json<Value>, LcError> {
    if !valid_key(&key) {
        return Err(LcError::BadValue(json!({"error": "bad_draft_key", "message": "key must be bot:<id>, group:<id> or shell:<host>/<pane>"})));
    }
    if b.text.len() > MAX_TEXT_BYTES {
        return Err(LcError::BadValue(json!({"error": "draft_too_large", "max_bytes": MAX_TEXT_BYTES})));
    }
    if b.client_id.len() > MAX_CLIENT_ID_LEN {
        return Err(LcError::Bad("client_id too long".into()));
    }
    let (draft, changed) = put(&app.db, &key, &b.text).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let Some(draft) = draft else {
        // 從沒寫過、也沒東西可刪：rev 0。
        return Ok(Json(json!({"key": key, "rev": 0, "updated_at": null})));
    };
    if changed {
        app.emit("draft_updated", event(&draft, &b.client_id)).await;
    }
    Ok(Json(json!({"key": draft.key, "rev": draft.rev, "updated_at": draft.updated_at})))
}

/// host shell pane 被關：清掉它的指令草稿（#758），其他瀏覽器也跟著清。
pub async fn clear_shell(app: &Arc<App>, host: &str, pane_id: &str) {
    clear_keys(app, &[shell_key(host, pane_id)]).await;
}

/// bot／專案被刪：把它的草稿清成空字串（留墓碑，rev 照加）並通知其他瀏覽器。盡力而為，失敗只記 log（刪除本身已經定案，不回頭）。
/// 不整列刪：bot 從垃圾桶復原後 key 會再出現，rev 從 1 重來的話網頁手上記著舊的大 rev，會把新草稿當晚到的舊事件丟掉。
pub async fn clear_keys(app: &Arc<App>, keys: &[String]) {
    for key in keys {
        match put(&app.db, key, "").await {
            Ok((Some(d), true)) => app.emit("draft_updated", event(&d, "")).await,
            Ok(_) => {}
            Err(e) => tracing::warn!(key = %key, error = ?e, "could not clear the draft of a deleted bot or project"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("agm-drafts-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("test.sqlite")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false)
    }

    fn put_in(text: &str, client: &str) -> Json<PutIn> {
        Json(PutIn { text: text.into(), client_id: client.into() })
    }

    /// host shell 面板的指令草稿（#758）：`shell:<host>/<pane id>`，兩段各自不含 `/`。
    #[test]
    fn shell_keys_take_host_and_pane_and_nothing_else() {
        assert_eq!(shell_key("local", "w1:p9"), "shell:local/w1:p9");
        assert!(valid_key(&shell_key("m4p", "ws-0:p1")));
        assert!(valid_key("shell:build.host_1/w1:p2"));
        for bad in ["shell:", "shell:local", "shell:/w1:p1", "shell:local/", "shell:a/b/c", "shell:a b/c", "shell:é/c", &format!("shell:h/{}", "p".repeat(200))] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }

    /// 網頁用 `encodeURIComponent` 送 key：`/` 變 `%2F`、`:` 變 `%3A`，路由不能把它切成兩段，Path 要還原成原本的 key。
    #[tokio::test]
    async fn an_encoded_shell_key_reaches_the_handler_as_one_segment() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let app = app().await;
        let router = axum::Router::new().route("/api/drafts/{key}", axum::routing::put(put_http)).with_state(app.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.ok() });
        let body = r#"{"text":"git log","client_id":"c"}"#;
        let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "PUT /api/drafts/shell%3Alocal%2Fw1%3Ap9 HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        conn.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        conn.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert_eq!(list(&app.db).await.unwrap().iter().map(|d| d.key.as_str()).collect::<Vec<_>>(), ["shell:local/w1:p9"]);
    }

    #[tokio::test]
    async fn closing_a_shell_pane_clears_only_that_panes_draft() {
        let app = app().await;
        put_http(State(app.clone()), Path("shell:local/w1:p9".into()), put_in("git log", "c")).await.unwrap();
        put_http(State(app.clone()), Path("shell:local/w1:p8".into()), put_in("ls", "c")).await.unwrap();
        let mut rx = app.subscribe();
        clear_shell(&app, "local", "w1:p9").await;
        let left = list(&app.db).await.unwrap();
        assert_eq!(left.iter().map(|d| d.key.as_str()).collect::<Vec<_>>(), ["shell:local/w1:p8"]);
        let ev = rx.try_recv().unwrap();
        assert_eq!((ev.data["key"].as_str(), ev.data["text"].as_str()), (Some("shell:local/w1:p9"), Some("")));
    }

    #[test]
    fn key_validation_only_takes_the_two_web_prefixes() {
        assert!(valid_key("bot:01HZX"));
        assert!(valid_key("group:p-1.a_b"));
        for bad in ["", "bot:", "group:", "bot", "x:1", "bot:a b", "bot:a/b", "bot:a:b", "BOT:1", "bot:é", &format!("bot:{}", "a".repeat(200))] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn put_bumps_rev_per_key_and_get_lists_only_non_empty() {
        let app = app().await;
        let r = put_http(State(app.clone()), Path("bot:b1".into()), put_in("hello", "c1")).await.unwrap().0;
        assert_eq!(r["rev"], 1);
        let r = put_http(State(app.clone()), Path("bot:b1".into()), put_in("hello world", "c1")).await.unwrap().0;
        assert_eq!(r["rev"], 2);
        let r = put_http(State(app.clone()), Path("group:p1".into()), put_in("g", "c1")).await.unwrap().0;
        assert_eq!(r["rev"], 1, "rev is per key");
        let all = get_http(State(app.clone())).await.unwrap().0;
        let all = all["drafts"].as_array().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!((all[0]["key"].as_str(), all[0]["text"].as_str(), all[0]["rev"].as_i64()), (Some("bot:b1"), Some("hello world"), Some(2)));
        assert!(all[0]["updated_at"].as_str().is_some());
    }

    #[tokio::test]
    async fn empty_text_deletes_but_keeps_rev_monotonic() {
        let app = app().await;
        put_http(State(app.clone()), Path("bot:b1".into()), put_in("abc", "c1")).await.unwrap();
        let r = put_http(State(app.clone()), Path("bot:b1".into()), put_in("", "c1")).await.unwrap().0;
        assert_eq!(r["rev"], 2);
        assert!(get_http(State(app.clone())).await.unwrap().0["drafts"].as_array().unwrap().is_empty());
        let r = put_http(State(app.clone()), Path("bot:b1".into()), put_in("again", "c1")).await.unwrap().0;
        assert_eq!(r["rev"], 3, "a recreated draft must not restart at rev 1");
        // 刪一份不存在的：rev 0、不留墓碑。
        let r = put_http(State(app.clone()), Path("bot:nope".into()), put_in("", "c1")).await.unwrap().0;
        assert_eq!(r["rev"], 0);
        assert!(get(&app.db, "bot:nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_pushes_draft_updated_with_client_id_and_skips_unchanged() {
        let app = app().await;
        let mut rx = app.subscribe();
        put_http(State(app.clone()), Path("bot:b1".into()), put_in("abc", "tab-1")).await.unwrap();
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.kind, "draft_updated");
        assert_eq!(ev.data["key"], "bot:b1");
        assert_eq!(ev.data["text"], "abc");
        assert_eq!(ev.data["rev"], 1);
        assert_eq!(ev.data["client_id"], "tab-1");
        assert!(ev.data["updated_at"].as_str().is_some());
        // 同樣的內容再送：不加 rev、不推。
        let r = put_http(State(app.clone()), Path("bot:b1".into()), put_in("abc", "tab-2")).await.unwrap().0;
        assert_eq!(r["rev"], 1);
        assert!(rx.try_recv().is_err());
        // 刪除也推，text 空。
        put_http(State(app.clone()), Path("bot:b1".into()), put_in("", "tab-2")).await.unwrap();
        let ev = rx.try_recv().unwrap();
        assert_eq!((ev.data["text"].as_str(), ev.data["rev"].as_i64(), ev.data["client_id"].as_str()), (Some(""), Some(2), Some("tab-2")));
    }

    #[tokio::test]
    async fn put_rejects_bad_key_and_oversized_text() {
        let app = app().await;
        for key in ["x:1", "bot:", "bot:a b"] {
            let err = put_http(State(app.clone()), Path(key.into()), put_in("x", "c")).await.unwrap_err();
            assert!(format!("{err:?}").contains("bad_draft_key"), "{key}: {err:?}");
        }
        let big = "a".repeat(MAX_TEXT_BYTES + 1);
        let err = put_http(State(app.clone()), Path("bot:b1".into()), put_in(&big, "c")).await.unwrap_err();
        assert!(format!("{err:?}").contains("draft_too_large"));
        let ok = "a".repeat(MAX_TEXT_BYTES);
        assert!(put_http(State(app.clone()), Path("bot:b1".into()), put_in(&ok, "c")).await.is_ok());
        let err = put_http(State(app.clone()), Path("bot:b1".into()), put_in("x", &"c".repeat(65))).await.unwrap_err();
        assert!(format!("{err:?}").contains("client_id"));
        assert_eq!(list(&app.db).await.unwrap().len(), 1, "rejected writes leave nothing behind");
    }

    #[tokio::test]
    async fn clear_keys_empties_drafts_and_tells_other_browsers() {
        let app = app().await;
        put_http(State(app.clone()), Path("bot:b1".into()), put_in("a", "c")).await.unwrap();
        put_http(State(app.clone()), Path("group:p1".into()), put_in("g", "c")).await.unwrap();
        put_http(State(app.clone()), Path("bot:keep".into()), put_in("k", "c")).await.unwrap();
        put_http(State(app.clone()), Path("bot:b2".into()), put_in("x", "c")).await.unwrap();
        put_http(State(app.clone()), Path("bot:b2".into()), put_in("", "c")).await.unwrap(); // 已經是墓碑
        let mut rx = app.subscribe();
        clear_keys(&app, &["bot:b1".into(), "group:p1".into(), "bot:b2".into(), "bot:never".into()]).await;
        let left = list(&app.db).await.unwrap();
        assert_eq!(left.iter().map(|d| d.key.as_str()).collect::<Vec<_>>(), ["bot:keep"]);
        assert!(get(&app.db, "bot:never").await.unwrap().is_none(), "nothing to clear, nothing written");
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            assert_eq!(ev.kind, "draft_updated");
            assert_eq!(ev.data["text"], "");
            seen.push((ev.data["key"].as_str().unwrap().to_string(), ev.data["rev"].as_i64().unwrap()));
        }
        assert_eq!(seen, [("bot:b1".to_string(), 2), ("group:p1".to_string(), 2)], "only drafts that had text announce");
        // 復原後 key 再出現：rev 接著加，不從 1 重來。
        let r = put_http(State(app.clone()), Path("bot:b1".into()), put_in("back", "c")).await.unwrap().0;
        assert_eq!(r["rev"], 3);
    }
}
