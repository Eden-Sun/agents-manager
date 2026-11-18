//! openai 的 key 來源與「key 絕不外洩」：假 key `sk-test-SENTINEL-XYZ` 當 sentinel，沿著每一條出口找它——
//! key 來源的錯誤、設定的 Debug／序列化、log、WS 事件、DB（attempt／checkpoint／receipt／usage）。不打真 API、不讀真 key。
use super::super::adapter::KeySource;
use super::super::runtime::{ConfigKeySource, OpenAiConfigKeySource};
use super::cap;
use am_base::db::api_bot::{self as db_api, Attempt, Checkpoint, Fence, Receipt};
use serde_json::json;
use std::sync::{Arc, Mutex};

const SENTINEL: &str = "sk-test-SENTINEL-XYZ";
const CLAUDE_SENTINEL: &str = "sk-ant-test-CLAUDE-KEY-123456";

fn publish_key(dir: &std::path::Path, name: &str, key: &str) -> String {
    let path = dir.join(name).to_string_lossy().into_owned();
    am_base::key_file::stage_key(&path, key).unwrap().publish().unwrap();
    path
}

async fn configure(app: &Arc<crate::state::App>, claude_key: &str, openai_key: &str, openai_on: bool) {
    let (claude_key, openai_key) = (claude_key.to_string(), openai_key.to_string());
    app.cfg
        .update(move |c| {
            c.claude_api.enabled = true;
            c.claude_api.key_file = claude_key;
            c.claude_api.api_bot.enabled = true;
            c.claude_api.api_bot.max_turn_cost_microusd = Some(500_000);
            c.openai_api.enabled = openai_on;
            c.openai_api.key_file = openai_key;
            c.openai_api.api_bot.max_turn_cost_microusd = Some(500_000);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn each_provider_reads_only_its_own_key_file_and_failures_say_nothing() {
    let env = crate::testing::env().await;
    let dir = crate::testing::scratch_dir("am-openai-keys");
    let claude_path = publish_key(&dir, "claude-key", CLAUDE_SENTINEL);
    let openai_path = publish_key(&dir, "openai-key", SENTINEL);
    configure(&env.app, &claude_path, &openai_path, true).await;
    let weak = Arc::downgrade(&env.app);

    let claude = ConfigKeySource(weak.clone()).snapshot().await.unwrap_or_else(|_| panic!("claude key"));
    let openai = OpenAiConfigKeySource(weak.clone()).snapshot().await.unwrap_or_else(|_| panic!("openai key"));
    assert_eq!(claude.expose_for_test(), CLAUDE_SENTINEL);
    assert_eq!(openai.expose_for_test(), SENTINEL, "openai 讀自己的 key 檔，不是 Claude 的");

    // 沒開、檔案不見、權限太鬆：一律 Err(())——型別本身就裝不下路徑或 key。
    configure(&env.app, &claude_path, &openai_path, false).await;
    assert!(OpenAiConfigKeySource(weak.clone()).snapshot().await.is_err(), "openai 沒開就沒有 key");
    assert!(ConfigKeySource(weak.clone()).snapshot().await.is_ok(), "Claude 的不受影響");
    configure(&env.app, &claude_path, &dir.join("nope").to_string_lossy(), true).await;
    assert!(OpenAiConfigKeySource(weak.clone()).snapshot().await.is_err());
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&openai_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    configure(&env.app, &claude_path, &openai_path, true).await;
    assert!(OpenAiConfigKeySource(weak.clone()).snapshot().await.is_err(), "world-readable 的 key 檔不用");
    // key_file 自己的錯誤文字也不含 key。
    let status = am_base::key_file::key_status(&openai_path).unwrap_err();
    assert!(!status.contains(SENTINEL) && !format!("{status:?}").contains(SENTINEL));
}

/// 收 log 的 writer。
#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;
    fn make_writer(&'a self) -> LogSink {
        self.clone()
    }
}

#[tokio::test]
async fn the_sentinel_key_appears_in_no_log_event_config_dump_or_database_row() {
    let sink = LogSink::default();
    let subscriber = tracing_subscriber::fmt().with_writer(sink.clone()).with_max_level(tracing::Level::TRACE).with_ansi(false).finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let env = crate::testing::env().await;
    let app = env.app.clone();
    let mut ws = app.subscribe();
    let dir = crate::testing::scratch_dir("am-openai-sentinel");
    let openai_path = publish_key(&dir, "openai-key", SENTINEL);
    let claude_path = publish_key(&dir, "claude-key", CLAUDE_SENTINEL);
    configure(&app, &claude_path, &openai_path, true).await;
    app.cfg
        .update(|c| {
            c.openai_api.api_bot.total_cap_usd = Some(1.0);
            c.openai_api.api_bot.total_cap_since = Some("2026-01-01T00:00:00.000Z".into());
            Ok(())
        })
        .await
        .unwrap();

    // 1. key 來源（成功與失敗）。
    let source = OpenAiConfigKeySource(Arc::downgrade(&app));
    assert!(source.snapshot().await.is_ok());

    // 2. 一次 openai 預留 ＋ 結算（走 cap::reserve 的 provider 範圍），含「usage 裡夾帶 key」被拒。
    let bot = crate::testing::claude_bot(&app, &env.project_id, "oai-sentinel").await;
    sqlx::query("UPDATE bots SET kind='claude-api' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let run = crate::testing::fake_run(&app, &bot.id).await;
    let conv = crate::db::conversation_id(&app.db, &bot.id).await.unwrap();
    let turn = crate::db::ulid();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,created_at) VALUES(?,?,?,'web','in_flight',?)")
        .bind(&turn).bind(&conv).bind(&run).bind(crate::db::now()).execute(&app.db).await.unwrap();
    let session = crate::db::ulid();
    let fence = Fence { session: &session, bot: &bot.id, run: &run, turn: &turn, generation: 1, revision: 0 };
    let chk = |pending: bool| Checkpoint::new(&json!({"messages": [], "responses": [], "api_pending": pending, "tool_pending": null, "completed_tool_ids": []})).unwrap();
    db_api::create(&app.db, &fence, &chk(false)).await.unwrap();
    let cfg = app.cfg.get().await;
    let cap = cap::total_cap(&cfg);
    assert_eq!(cap.as_ref().map(|c| c.1), Some(1_000_000));
    let attempt = Attempt { id: "oai-attempt", ordinal: 0, request_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", speed: "standard", reserved_microusd: 4_221, pricing_version: "openai:gpt-6-luna:2026-11-18" };
    cap::reserve(&app.db, &fence, &attempt, &chk(true), 500_000, 0, cap.as_ref()).await.unwrap();
    let leaky_usage = json!({"input_tokens": 10, "output_tokens": 2, "echo": SENTINEL});
    let leaky = Receipt { state: "recorded", actual_speed: None, usage: Some(&leaky_usage), estimated_microusd: Some(3) };
    let f1 = Fence { revision: 1, ..fence.clone() };
    assert!(db_api::record(&app.db, &f1, "oai-attempt", &leaky, &chk(false)).await.is_err(), "usage 夾帶 key 形的字串不收");
    let clean_usage = json!({"input_tokens": 10, "output_tokens": 2});
    let clean = Receipt { state: "recorded", actual_speed: None, usage: Some(&clean_usage), estimated_microusd: Some(3) };
    db_api::record(&app.db, &f1, "oai-attempt", &clean, &chk(false)).await.unwrap();

    // 3. 設定的 Debug／序列化只有路徑，沒有 key。
    let dump = format!("{:?}\n{}", cfg.openai_api, toml::to_string_pretty(&cfg.openai_api).unwrap());
    assert!(!dump.contains(SENTINEL) && !dump.contains(CLAUDE_SENTINEL), "{dump}");

    // 4. 整個 api_bot 帳本與 receipt、messages 的所有文字欄位。
    let mut db_text = String::new();
    for table in ["api_bot_sessions", "api_bot_attempts", "api_bot_join_receipts", "api_bot_rejected_responses", "messages", "turns"] {
        let rows = sqlx::query(&format!("SELECT * FROM {table}")).fetch_all(&app.db).await.unwrap();
        for row in rows {
            use sqlx::{Column, Row};
            for col in row.columns() {
                if let Ok(Some(text)) = row.try_get::<Option<String>, _>(col.ordinal()) {
                    db_text.push_str(&text);
                    db_text.push('\n');
                }
            }
        }
    }
    assert!(!db_text.contains(SENTINEL) && !db_text.contains(CLAUDE_SENTINEL), "DB 裡找到 key");

    // 5. WS 事件與 log。
    let mut events = String::new();
    while let Ok(ev) = ws.try_recv() {
        events.push_str(&format!("{} {}\n", ev.kind, ev.data));
    }
    assert!(!events.contains(SENTINEL), "WS 事件裡找到 key");
    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.contains(SENTINEL) && !logs.contains(CLAUDE_SENTINEL), "log 裡找到 key");
}
