//! 對話裡 assistant 訊息的「翻譯」按鈕：`POST /api/messages/{id}/translate`（User-only）。
//!
//! 走共用的 [`crate::claude_api::complete`]（用途 `message_translate`：開關、key、保險絲、帳本都在那邊），
//! 不另寫 HTTP client。快取是 `{data_dir}/message-translate/{sha256(id + 內容)}.json`（0600，不進 SQLite）：
//! 同一則訊息、同一份內容翻過一次就不再扣費；內容變了（incomplete 補完）雜湊就變，會重翻。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxPath, State};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::claude_api::{self, ClaudeError, ClaudeRequest};
use crate::lc_error::LcError;
use crate::state::App;

const USE_CASE: &str = "message_translate";
const CACHE_DIR: &str = "message-translate";
const PROMPT_VERSION: &str = "1";
/// 訊息內容上限（字元）：再長的回覆不翻，避免一次扣太多。
const MAX_CHARS: usize = 20_000;
const MAX_OUT_TOKENS: u32 = 8_000;

const SYSTEM: &str = "You are a translation engine. Translate the user's message into Traditional Chinese (zh-TW, Taiwan usage). \
Preserve the markdown structure exactly (headings, lists, tables, emphasis, links). \
Do NOT translate fenced code blocks, inline code, file paths, shell commands, URLs, identifiers, environment variables, \
commit hashes or issue numbers: copy them verbatim. Do not add explanations, notes or quotes. Reply with the translation only.";

#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    translation: String,
    model: String,
    prompt_version: String,
    /// 翻這次實際花的美元（依 [`cost_usd`]）；舊快取沒有。快取命中時照樣回傳，前端標「快取，未扣費」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
}

pub(crate) use crate::claude_api::cost_usd;

pub(crate) fn build_request(content: &str) -> ClaudeRequest {
    ClaudeRequest {
        use_case: USE_CASE,
        system: Some(SYSTEM.to_string()),
        prompt: content.to_string(),
        max_tokens: MAX_OUT_TOKENS,
        format: None,
    }
}

fn cache_key(id: &str, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(PROMPT_VERSION.as_bytes());
    h.update(b"\0");
    h.update(id.as_bytes());
    h.update(b"\0");
    h.update(content.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn cache_path(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join(CACHE_DIR).join(format!("{key}.json"))
}

fn load(path: &Path) -> Option<CacheEntry> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok().filter(|e: &CacheEntry| !e.translation.trim().is_empty())
}

fn save(path: &Path, entry: &CacheEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
    am_base::atomic_file::write(path, &data, am_base::atomic_file::Mode::Exact(0o600))
}

fn usage_json(u: &claude_api::Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cache_creation_input_tokens": u.cache_creation_input_tokens,
        "cache_read_input_tokens": u.cache_read_input_tokens,
    })
}

fn error_of(e: ClaudeError) -> LcError {
    match e {
        ClaudeError::Disabled | ClaudeError::NoKey(_) => LcError::conflict("claude_api_not_configured", json!({"message": e.to_string()})),
        other => LcError::Upstream(format!("translate_failed: {other}")),
    }
}

/// 核心：查快取、沒有才打 API、寫快取。回 `{translation, model, cached, usage?}`。
pub(crate) async fn translate(app: &App, id: &str, content: &str) -> Result<Value, LcError> {
    let path = cache_path(&app.data_dir, &cache_key(id, content));
    if let Some(hit) = load(&path) {
        return Ok(json!({"translation": hit.translation, "model": hit.model, "cached": true, "cost_usd": hit.cost_usd}));
    }
    let reply = claude_api::complete(app, build_request(content)).await.map_err(error_of)?;
    let cost = reply.usage.as_ref().and_then(|u| cost_usd(&reply.model, u));
    let entry = CacheEntry { translation: reply.text.trim().to_string(), model: reply.model.clone(), prompt_version: PROMPT_VERSION.into(), cost_usd: cost };
    if let Err(e) = save(&path, &entry) {
        tracing::warn!("message translate cache write failed: {e}");
    }
    let mut body = json!({"translation": entry.translation, "model": entry.model, "cached": false, "cost_usd": cost});
    if let Some(u) = &reply.usage {
        body["usage"] = usage_json(u);
    }
    Ok(body)
}

/// `POST /api/messages/{id}/translate`：只限 assistant 訊息。
pub async fn post_translate(
    State(app): State<Arc<App>>,
    AxPath(id): AxPath<String>,
    Extension(principal): Extension<crate::api::RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    if principal != crate::api::RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    let row: Option<(String, String)> = sqlx::query_as("SELECT role, content FROM messages WHERE id = ?")
        .bind(&id)
        .fetch_optional(&app.db)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    let (role, content) = row.ok_or_else(|| LcError::NotFound("message".into()))?;
    if role != "assistant" {
        return Err(LcError::Bad("only assistant messages can be translated".into()));
    }
    if content.trim().is_empty() {
        return Err(LcError::Bad("empty message".into()));
    }
    if content.chars().count() > MAX_CHARS {
        return Err(LcError::Unprocessable(json!({"error": "message_too_long", "max_chars": MAX_CHARS})));
    }
    Ok(Json(translate(&app, &id, &content).await?))
}

#[cfg(test)]
mod cost_tests {
    use super::*;

    #[test]
    fn cost_uses_list_prices_and_unknown_models_have_none() {
        let u = claude_api::Usage { input_tokens: Some(1_000_000), output_tokens: Some(1_000_000), cache_creation_input_tokens: Some(1_000_000), cache_read_input_tokens: Some(1_000_000) };
        // Haiku 5.5：0.10 + 0.50 + 0.125 + 0.01
        assert!((cost_usd("claude-haiku-5-5", &u).unwrap() - 0.735).abs() < 1e-9);
        // Sonnet 5.5：2 + 10 + 2.5 + 0.2
        assert!((cost_usd("claude-sonnet-5-5", &u).unwrap() - 14.7).abs() < 1e-9);
        assert_eq!(cost_usd("gpt-x", &u), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_api::{ClientError, ClientOutcome, MessagesClient, MessagesRequest, Usage};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct Fake {
        calls: AtomicUsize,
        seen: Mutex<Vec<(Option<String>, String, String)>>,
    }

    impl MessagesClient for Fake {
        fn complete<'a>(&'a self, req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push((req.system.clone(), req.prompt.clone(), req.model.clone()));
            Box::pin(async {
                Ok(ClientOutcome {
                    text: "  你好 `code`  ".into(),
                    model: Some("claude-haiku-5-5".into()),
                    stop_reason: Some("end_turn".into()),
                    usage: Some(Usage { input_tokens: Some(10), output_tokens: Some(5), cache_creation_input_tokens: None, cache_read_input_tokens: None }),
                })
            })
        }
    }

    async fn app(enabled: bool) -> (Arc<App>, PathBuf) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-msg-translate-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::app_ports_p1::open(&dir.join("t.sqlite3")).await.unwrap();
        let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let key = dir.join("key");
        crate::key_file::stage_key(&key.to_string_lossy(), "sk-ant-test-msg").unwrap().publish().unwrap();
        let key = key.to_string_lossy().into_owned();
        store
            .update(move |c| {
                c.claude_api.key_file = key;
                if enabled {
                    c.claude_api.enabled = true;
                    c.claude_api.uses.insert(USE_CASE.into(), crate::config::ClaudeApiUseCfg { enabled: Some(true), ..Default::default() });
                }
                Ok(())
            })
            .await
            .unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        (App::new(db, client.clone(), client, store, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false), dir)
    }

    fn fake(app: &Arc<App>) -> Arc<Fake> {
        let f = Arc::new(Fake { calls: AtomicUsize::new(0), seen: Mutex::new(Vec::new()) });
        app.claude_api.set_client(f.clone());
        f
    }

    #[test]
    fn prompt_keeps_code_and_targets_zh_tw() {
        let r = build_request("Run `cargo test` in /tmp");
        assert_eq!(r.use_case, "message_translate");
        let sys = r.system.unwrap();
        assert!(sys.contains("Traditional Chinese") && sys.contains("fenced code blocks") && sys.contains("commit hashes") && sys.contains("translation only"));
        assert_eq!(r.prompt, "Run `cargo test` in /tmp");
    }

    #[test]
    fn cache_key_depends_on_id_and_content() {
        assert_eq!(cache_key("a", "x"), cache_key("a", "x"));
        assert_ne!(cache_key("a", "x"), cache_key("b", "x"));
        assert_ne!(cache_key("a", "x"), cache_key("a", "y"));
    }

    #[tokio::test]
    async fn second_call_hits_the_cache_without_calling_the_api() {
        let (app, _dir) = app(true).await;
        let f = fake(&app);
        let a = translate(&app, "m1", "hello world").await.unwrap();
        assert_eq!((a["cached"].clone(), a["translation"].clone()), (json!(false), json!("你好 `code`")));
        assert_eq!(a["usage"]["output_tokens"], 5);
        let b = translate(&app, "m1", "hello world").await.unwrap();
        assert_eq!((b["cached"].clone(), b["translation"].clone(), b["model"].clone()), (json!(true), json!("你好 `code`"), json!("claude-haiku-5-5")));
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
        translate(&app, "m1", "hello world, edited").await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "changed content is translated again");
        let mode = std::fs::metadata(cache_path(&app.data_dir, &cache_key("m1", "hello world"))).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.permissions().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn successful_call_is_appended_to_spend_log_and_counts_in_local_attempts() {
        let (app, dir) = app(true).await;
        fake(&app);
        translate(&app, "m1", "hello world").await.unwrap();
        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        let rows = crate::claude_api::read_spend_since(&dir, since);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].1 > 0);
        let atts = crate::claude_api_balance::query_local_attempts_since(&app, &am_base::db::iso_at(since)).await.unwrap();
        assert_eq!(atts.iter().map(|a| a.microusd).sum::<i64>(), rows[0].1, "餘額的本機帳含這筆");
        use std::os::unix::fs::PermissionsExt;
        let f = dir.join("claude-api/spend.jsonl");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(f.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        // 快取命中不重複記
        translate(&app, "m1", "hello world").await.unwrap();
        assert_eq!(crate::claude_api::read_spend_since(&dir, since).len(), 1);
    }

    #[tokio::test]
    async fn not_enabled_is_a_clear_409_and_sends_nothing() {
        let (app, _dir) = app(false).await;
        let f = fake(&app);
        let err = translate(&app, "m1", "hello").await.unwrap_err();
        assert!(matches!(&err, LcError::Conflict(v) if v["reason"] == "claude_api_not_configured"), "{err:?}");
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn route_rejects_non_assistant_and_unknown_messages() {
        let (app, _dir) = app(true).await;
        fake(&app);
        let call = |id: &str| post_translate(State(app.clone()), AxPath(id.into()), Extension(crate::api::RequestPrincipal::User));
        assert!(matches!(call("nope").await.unwrap_err(), LcError::NotFound(_)));
        let bot = Extension(crate::api::RequestPrincipal::Bot("x".into()));
        assert!(matches!(post_translate(State(app.clone()), AxPath("nope".into()), bot).await.unwrap_err(), LcError::Forbidden(_)));
    }
}
