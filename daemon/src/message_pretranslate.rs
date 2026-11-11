//! 自動翻譯的「預先翻」（使用者 2026-11-10：自動翻譯開著，訊息發出就直接翻好，不等使用者打開頁面）。
//!
//! `[claude_api.uses.message_translate] auto` 開著時，每則 `message_added`（訊息落地、或同一 id 內容被換掉重推）都由這裡檢查，
//! 符合條件的 assistant 英文訊息就呼叫 [`crate::message_translate::translate`]，寫進**同一份**快取（`message-translate/`）。
//! 之後網頁開頁、手動按，都是快取命中（`cached: true`、不扣費）；預先翻跟網頁同時撞上，single-flight 只打一次 API。
//!
//! - 條件跟網頁 `useMessageTranslation` 的 eligible／fresh 一致：assistant、未撤回、非 `terminal_fallback`、主要是英文或簡體中文（2026-11-11，本地轉繁，不送 API）、
//!   `incomplete = 0`、非空、不超過 [`crate::message_translate::MAX_CHARS`]。網頁的「最近 N 則」是顯示規則，預先翻不看它。
//! - 系統觸發的回合不另外排除：網頁本來就對所有符合條件的新訊息自動翻，這裡照同一條規則。
//! - 失敗：一次失敗後 [`FAILURE_COOLDOWN`] 內不再預先翻（落地的訊息直接略過，網頁開頁時仍照舊的自動翻規則、失敗停手）。
//!   同時最多 [`MAX_IN_FLIGHT`] 則在飛。
//! - 「關」：auto、總開關、用途開關任一關掉，這裡什麼都不做，行為跟原來一樣。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast::error::RecvError, Semaphore};

use crate::message_translate as mt;
use crate::state::App;

/// 同時在飛的預先翻上限（每則一次上游呼叫）。
const MAX_IN_FLIGHT: usize = 2;
/// 預先翻失敗後的冷卻時間。
const FAILURE_COOLDOWN: Duration = Duration::from_secs(120);

static SLOTS: Semaphore = Semaphore::const_new(MAX_IN_FLIGHT);
/// 冷卻到幾時（unix 毫秒）；0＝沒有冷卻。
static COOLDOWN_UNTIL_MS: AtomicI64 = AtomicI64::new(0);

/// 啟動：訂閱事件流，每則 `message_added` 各開一個工作去檢查（不卡住事件流）。
pub fn spawn(app: Arc<App>) {
    let mut rx = app.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.kind == "message_added" => {
                    let Some(id) = ev.data["message"]["id"].as_str().map(str::to_string) else { continue };
                    let app = app.clone();
                    tokio::spawn(async move { pretranslate(&app, &id).await });
                }
                Ok(_) => {}
                // 落後的事件丟掉：網頁開頁時仍會自動翻（有快取就不扣費），不會漏翻到無法補救。
                Err(RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

/// 這則訊息要不要預先翻（跟網頁 eligible＋fresh 的條件一致，去掉只看畫面的「最近 N 則」）。
pub(crate) fn eligible(role: &str, content: &str, incomplete: bool, rewound: bool, source: &str) -> bool {
    role == "assistant"
        && !incomplete
        && !rewound
        && source != "terminal_fallback"
        && !content.trim().is_empty()
        && content.chars().count() <= mt::MAX_CHARS
        && (mt::is_mostly_english(content) || crate::zh_convert::local_applies(content))
}

fn cooling_down() -> bool {
    chrono::Utc::now().timestamp_millis() < COOLDOWN_UNTIL_MS.load(Ordering::Relaxed)
}

/// 查訊息、符合條件就翻（走快取與 single-flight）。失敗進冷卻；成功或命中快取都不動冷卻。
pub(crate) async fn pretranslate(app: &App, id: &str) {
    if !mt::auto_on(app).await || cooling_down() {
        return;
    }
    let row: Option<(String, String, i64, Option<String>, String)> =
        sqlx::query_as("SELECT role, content, incomplete, rewound_at, source FROM messages WHERE id = ?")
            .bind(id)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
    let Some((role, content, incomplete, rewound_at, source)) = row else { return };
    if !eligible(&role, &content, incomplete != 0, rewound_at.is_some(), &source) {
        return;
    }
    let Ok(_permit) = SLOTS.acquire().await else { return };
    // 排隊期間可能已經失敗過或設定被關掉：拿到名額後再看一次。
    if cooling_down() || !mt::auto_on(app).await {
        return;
    }
    if let Err(e) = mt::translate(app, id, &content).await {
        COOLDOWN_UNTIL_MS.store(chrono::Utc::now().timestamp_millis() + FAILURE_COOLDOWN.as_millis() as i64, Ordering::Relaxed);
        tracing::warn!(message = %id, error = ?e, "message pretranslate failed; pausing auto pretranslate");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_api::{ClientError, ClientOutcome, MessagesClient, MessagesRequest, Usage};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;

    /// 這個模組的測試共用行程內的名額與冷卻狀態：序列執行。
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct Fake {
        calls: AtomicUsize,
        fail: std::sync::atomic::AtomicBool,
        delay_ms: u64,
    }

    impl MessagesClient for Fake {
        fn complete<'a>(&'a self, _req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let fail = self.fail.load(Ordering::SeqCst);
            let delay = self.delay_ms;
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if fail {
                    return Err(ClientError::Http { status: 500, usage: None });
                }
                Ok(ClientOutcome {
                    text: "你好世界".into(),
                    model: Some("claude-haiku-5-5".into()),
                    stop_reason: Some("end_turn".into()),
                    usage: Some(Usage { input_tokens: Some(10), output_tokens: Some(5), cache_creation_input_tokens: None, cache_read_input_tokens: None }),
                })
            })
        }
    }

    const EN: &str = "Deployed the new daemon build and all of the tests passed on the first try.";

    async fn setup(auto: bool, delay_ms: u64) -> (crate::testing::Env, String, Arc<Fake>) {
        COOLDOWN_UNTIL_MS.store(0, Ordering::Relaxed);
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "預翻測試").await;
        let key = env.app.data_dir.join("key");
        crate::key_file::stage_key(&key.to_string_lossy(), "sk-ant-test-pre").unwrap().publish().unwrap();
        let key = key.to_string_lossy().into_owned();
        env.app
            .cfg
            .update(move |c| {
                c.claude_api.key_file = key;
                c.claude_api.enabled = true;
                c.claude_api.uses.insert("message_translate".into(), crate::config::ClaudeApiUseCfg { enabled: Some(true), auto: Some(auto), ..Default::default() });
                Ok(())
            })
            .await
            .unwrap();
        let f = Arc::new(Fake { calls: AtomicUsize::new(0), fail: std::sync::atomic::AtomicBool::new(false), delay_ms });
        env.app.claude_api.set_client(f.clone());
        (env, bot.id, f)
    }

    async fn insert(env: &crate::testing::Env, bot_id: &str, id: &str, role: &str, content: &str, incomplete: i64, rewound: bool, source: &str) {
        let conv = crate::db::conversation_id(&env.app.db, bot_id).await.unwrap();
        let rewound_at = rewound.then(crate::db::now);
        sqlx::query("INSERT INTO messages(id,conversation_id,role,content,source,incomplete,rewound_at,created_at) VALUES(?,?,?,?,?,?,?,?)")
            .bind(id)
            .bind(&conv)
            .bind(role)
            .bind(content)
            .bind(source)
            .bind(incomplete)
            .bind(rewound_at)
            .bind(crate::db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
    }

    #[test]
    fn eligibility_mirrors_the_web_rules() {
        assert!(eligible("assistant", EN, false, false, "transcript"));
        assert!(!eligible("user", EN, false, false, "web"), "使用者自己的訊息不翻");
        assert!(!eligible("assistant", EN, true, false, "hook"), "未完整不翻");
        assert!(!eligible("assistant", EN, false, true, "transcript"), "撤回的不翻");
        assert!(!eligible("assistant", EN, false, false, "terminal_fallback"), "備援抓的不翻");
        assert!(!eligible("assistant", "這是中文的回覆，不需要翻譯喔。", false, false, "hook"), "主要是中文不翻");
        assert!(!eligible("assistant", "   ", false, false, "hook"), "空白不翻");
        assert!(eligible("assistant", "这个问题我们需要修复一下，修复之后请再运行测试，确认没有问题了再告诉我们。", false, false, "hook"), "簡體走本地轉繁，也預先做");
    }

    #[tokio::test]
    async fn landed_simplified_reply_is_converted_locally_and_costs_nothing() {
        let _g = TEST_LOCK.lock().await;
        let (env, bot_id, f) = setup(true, 0).await;
        let simp = "这个问题我们需要修复一下，修复之后请再运行测试，确认没有问题了再告诉我们。";
        insert(&env, &bot_id, "m-pre-simp", "assistant", simp, 0, false, "transcript").await;
        pretranslate(&env.app, "m-pre-simp").await;
        assert_eq!(f.calls.load(Ordering::SeqCst), 0, "簡體不送 API");
        let shown = mt::translate(&env.app, "m-pre-simp", simp).await.unwrap();
        assert_eq!((shown["cached"].clone(), shown["translation"].clone()), (serde_json::json!(true), serde_json::json!("這個問題我們需要修復一下，修復之後請再執行測試，確認沒有問題了再告訴我們。")));
    }

    #[tokio::test]
    async fn landed_english_reply_is_translated_before_anyone_opens_the_page() {
        let _g = TEST_LOCK.lock().await;
        let (env, bot_id, f) = setup(true, 0).await;
        insert(&env, &bot_id, "m-pre-1", "assistant", EN, 0, false, "transcript").await;
        pretranslate(&env.app, "m-pre-1").await;
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "落地就翻一次");
        // 網頁之後開頁、手動按：命中快取，不再扣費。
        let shown = mt::translate(&env.app, "m-pre-1", EN).await.unwrap();
        assert_eq!((shown["cached"].clone(), shown["translation"].clone()), (serde_json::json!(true), serde_json::json!("你好世界")));
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_pretranslate_and_web_request_make_one_upstream_call() {
        let _g = TEST_LOCK.lock().await;
        let (env, bot_id, f) = setup(true, 150).await;
        insert(&env, &bot_id, "m-pre-2", "assistant", EN, 0, false, "transcript").await;
        let ((), web) = tokio::join!(pretranslate(&env.app, "m-pre-2"), mt::translate(&env.app, "m-pre-2", EN));
        let web = web.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "預先翻跟網頁同時撞上，只扣一次");
        assert_eq!(web["translation"], serde_json::json!("你好世界"), "不管誰先搶到鎖，兩邊拿到的都是同一份譯文");
    }

    #[tokio::test]
    async fn auto_off_does_nothing() {
        let _g = TEST_LOCK.lock().await;
        let (env, bot_id, f) = setup(false, 0).await;
        insert(&env, &bot_id, "m-pre-3", "assistant", EN, 0, false, "transcript").await;
        pretranslate(&env.app, "m-pre-3").await;
        assert_eq!(f.calls.load(Ordering::SeqCst), 0, "auto 關著行為不變：不翻");
    }

    #[tokio::test]
    async fn ineligible_messages_are_not_sent() {
        let _g = TEST_LOCK.lock().await;
        let (env, bot_id, f) = setup(true, 0).await;
        insert(&env, &bot_id, "m-u", "user", EN, 0, false, "web").await;
        insert(&env, &bot_id, "m-inc", "assistant", EN, 1, false, "hook").await;
        insert(&env, &bot_id, "m-rw", "assistant", EN, 0, true, "transcript").await;
        insert(&env, &bot_id, "m-fb", "assistant", EN, 0, false, "terminal_fallback").await;
        insert(&env, &bot_id, "m-zh", "assistant", "這是中文的回覆，不需要翻譯喔。", 0, false, "hook").await;
        for id in ["m-u", "m-inc", "m-rw", "m-fb", "m-zh", "m-missing"] {
            pretranslate(&env.app, id).await;
        }
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failure_pauses_pretranslate_instead_of_retrying_every_message() {
        let _g = TEST_LOCK.lock().await;
        let (env, bot_id, f) = setup(true, 0).await;
        f.fail.store(true, Ordering::SeqCst);
        insert(&env, &bot_id, "m-f1", "assistant", EN, 0, false, "transcript").await;
        insert(&env, &bot_id, "m-f2", "assistant", EN, 0, false, "transcript").await;
        pretranslate(&env.app, "m-f1").await;
        pretranslate(&env.app, "m-f2").await;
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "失敗後冷卻中，後面的落地訊息不再送");
        COOLDOWN_UNTIL_MS.store(0, Ordering::Relaxed);
        f.fail.store(false, Ordering::SeqCst);
        pretranslate(&env.app, "m-f2").await;
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "冷卻過了就照常翻");
    }
}
