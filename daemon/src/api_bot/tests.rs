use super::{
    http,
    protocol::{Message, Options, Response, ToolCall},
    runner::{self, Journal, Model, State, Tools},
};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

fn response(content: Vec<Value>, reason: &str) -> Value {
    json!({"id":"msg_fixture","model":"claude-opus-5-5","content":content,"stop_reason":reason,
        "usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30,"speed":"fast"}})
}
fn options() -> Options {
    Options {
        effort: "medium".into(),
        fast: true,
        max_tokens: 4096,
    }
}
fn initial() -> State {
    State {
        messages: vec![Message {
            role: "user".into(),
            content: json!("fixture"),
        }],
        ..Default::default()
    }
}
struct Fixture {
    endpoint: String,
    seen: Arc<Mutex<Vec<Value>>>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}
async fn fixture(status: u16, replies: Vec<Value>) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
    let route = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post({
            let seen = seen.clone();
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                let seen = seen.clone();
                let queue = queue.clone();
                async move {
                    assert_eq!(headers["x-api-key"], "fixture-not-a-real-key");
                    assert_eq!(headers["anthropic-version"], "2023-06-01");
                    assert!(!headers.contains_key("anthropic-beta"));
                    seen.lock().unwrap().push(body);
                    let value = queue.lock().unwrap().pop_front().unwrap_or(Value::Null);
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [("retry-after", "3")],
                        axum::Json(value),
                    )
                }
            }
        }),
    );
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, route)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    Fixture {
        endpoint,
        seen,
        shutdown,
        task,
    }
}
struct HttpModel<'a>(&'a Fixture);
impl Model for HttpModel<'_> {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Response, http::Error>> {
        Box::pin(async move {
            crate::claude_api::complete_full_messages(&http::FullMessagesRequest {
                endpoint:&self.0.endpoint,key:"fixture-not-a-real-key",options:&options(),
                system:&json!([{"type":"text","text":"stable rules","cache_control":{"type":"ephemeral"}}]),
                messages,timeout:Duration::from_secs(2),max_response_bytes:64*1024,
            },cancel).await
        })
    }
}
#[derive(Default)]
struct MemoryJournal {
    snapshots: Mutex<Vec<State>>,
    fail_at: Option<usize>,
}
impl Journal for MemoryJournal {
    fn save<'a>(&'a self, state: &'a State) -> BoxFuture<'a, Result<(), ()>> {
        Box::pin(async move {
            let mut saved = self.snapshots.lock().unwrap();
            if self.fail_at == Some(saved.len()) {
                return Err(());
            }
            saved.push(state.clone());
            Ok(())
        })
    }
}
#[derive(Default)]
struct FakeTools {
    count: AtomicUsize,
}
impl Tools for FakeTools {
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(format!("result:{}", call.id))
        })
    }
}
#[tokio::test]
async fn full_http_loop_preserves_thinking_and_pairs_all_tools_before_next_request() {
    let thinking = json!({"type":"thinking","thinking":"","signature":"unchanged-signature","extension":{"x":1}});
    let f=fixture(200,vec![response(vec![thinking.clone(),
        json!({"type":"tool_use","id":"t1","name":"bash","input":{"command":"pwd"}}),
        json!({"type":"tool_use","id":"t2","name":"str_replace_based_edit_tool","input":{"command":"view","path":"x"}})],"tool_use"),
        response(vec![json!({"type":"text","text":"done"})],"end_turn")]).await;
    let mut state = initial();
    let tools = FakeTools::default();
    let journal = MemoryJournal::default();
    assert_eq!(
        runner::run(
            &HttpModel(&f),
            &tools,
            &journal,
            &mut state,
            &CancellationToken::new(),
            3
        )
        .await,
        Ok(runner::Halt::EndTurn)
    );
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0]["model"], "claude-opus-5-5");
    assert_eq!(seen[0]["speed"], "fast");
    assert_eq!(seen[0]["output_config"]["effort"], "medium");
    assert!(seen[0].get("thinking").is_none());
    assert!(seen[0]["tools"][0].get("input_schema").is_none());
    assert_eq!(seen[1]["messages"][1]["content"][0], thinking);
    assert_eq!(seen[1]["messages"][2]["content"][0]["tool_use_id"], "t1");
    assert_eq!(seen[1]["messages"][2]["content"][1]["tool_use_id"], "t2");
    assert_eq!(state.responses[0].usage["speed"], "fast");
    assert_eq!(tools.count.load(Ordering::SeqCst), 2);
    assert!(journal
        .snapshots
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.tool_pending.as_deref() == Some("t1")));
    assert!(!state.api_pending && state.tool_pending.is_none());
}
#[tokio::test]
async fn pending_recovery_and_checkpoint_failure_never_execute_or_send() {
    let f = fixture(200, vec![]).await;
    let tools = FakeTools::default();
    let mut state = initial();
    state.tool_pending = Some("unknown".into());
    assert_eq!(
        runner::run(
            &HttpModel(&f),
            &tools,
            &MemoryJournal::default(),
            &mut state,
            &CancellationToken::new(),
            2
        )
        .await,
        Err(runner::Error::PendingUnknown)
    );
    state = initial();
    let journal = MemoryJournal {
        fail_at: Some(0),
        ..Default::default()
    };
    assert_eq!(
        runner::run(
            &HttpModel(&f),
            &tools,
            &journal,
            &mut state,
            &CancellationToken::new(),
            2
        )
        .await,
        Err(runner::Error::Journal)
    );
    assert!(f.seen.lock().unwrap().is_empty());
    assert_eq!(tools.count.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn journal_failure_after_tool_is_unknown_and_not_replayed() {
    let f = fixture(
        200,
        vec![response(
            vec![json!({"type":"tool_use","id":"t1","name":"bash","input":{"command":"true"}})],
            "tool_use",
        )],
    )
    .await;
    let mut state = initial();
    let tools = FakeTools::default();
    let journal = MemoryJournal {
        fail_at: Some(3),
        ..Default::default()
    };
    assert_eq!(
        runner::run(
            &HttpModel(&f),
            &tools,
            &journal,
            &mut state,
            &CancellationToken::new(),
            2
        )
        .await,
        Err(runner::Error::Journal)
    );
    assert_eq!(tools.count.load(Ordering::SeqCst), 1);
    assert_eq!(
        runner::run(
            &HttpModel(&f),
            &tools,
            &journal,
            &mut state,
            &CancellationToken::new(),
            2
        )
        .await,
        Err(runner::Error::PendingUnknown)
    );
    assert_eq!(tools.count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn refusal_token_limit_unknown_reason_and_round_budget_are_distinct() {
    for (reason, expected) in [
        ("refusal", runner::Halt::Refusal),
        ("max_tokens", runner::Halt::MaxTokens),
        ("pause_turn", runner::Halt::Other("pause_turn".into())),
    ] {
        let f = fixture(200, vec![response(vec![], reason)]).await;
        assert_eq!(
            runner::run(
                &HttpModel(&f),
                &FakeTools::default(),
                &MemoryJournal::default(),
                &mut initial(),
                &CancellationToken::new(),
                1
            )
            .await,
            Ok(expected)
        );
    }
    let f = fixture(
        200,
        vec![response(
            vec![json!({"type":"tool_use","id":"t1","name":"bash","input":{}})],
            "tool_use",
        )],
    )
    .await;
    assert_eq!(
        runner::run(
            &HttpModel(&f),
            &FakeTools::default(),
            &MemoryJournal::default(),
            &mut initial(),
            &CancellationToken::new(),
            1
        )
        .await,
        Err(runner::Error::Budget)
    );
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn duplicate_and_malformed_tools_execute_nothing() {
    let tool = json!({"type":"tool_use","id":"t1","name":"bash","input":{}});
    for content in [
        vec![tool.clone(), tool],
        vec![json!({"type":"tool_use","id":"x","name":"unknown","input":{}})],
    ] {
        let f = fixture(200, vec![response(content, "tool_use")]).await;
        let tools = FakeTools::default();
        assert!(runner::run(
            &HttpModel(&f),
            &tools,
            &MemoryJournal::default(),
            &mut initial(),
            &CancellationToken::new(),
            2
        )
        .await
        .is_err());
        assert_eq!(tools.count.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn http_rate_limit_size_errors_and_cancel_do_not_retry_or_leak_response_body() {
    for (status, expected) in [
        (
            429,
            http::Error::RateLimited {
                retry_after_secs: Some(3),
            },
        ),
        (401, http::Error::Http(401)),
        (302, http::Error::Http(302)),
    ] {
        let f = fixture(status, vec![json!({"secret":"must not escape"})]).await;
        let result = HttpModel(&f)
            .complete(&initial().messages, &CancellationToken::new())
            .await;
        assert_eq!(result.unwrap_err(), expected);
        assert_eq!(f.seen.lock().unwrap().len(), 1);
    }
    let f = fixture(
        200,
        vec![response(
            vec![json!({"type":"text","text":"x".repeat(70*1024)})],
            "end_turn",
        )],
    )
    .await;
    assert_eq!(
        HttpModel(&f)
            .complete(&initial().messages, &CancellationToken::new())
            .await
            .unwrap_err(),
        http::Error::TooLarge
    );
    let f = fixture(200, vec![]).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        HttpModel(&f)
            .complete(&initial().messages, &cancel)
            .await
            .unwrap_err(),
        http::Error::Cancelled
    );
    assert!(f.seen.lock().unwrap().is_empty());
}
#[tokio::test]
async fn test_build_refuses_all_non_loopback_endpoints() {
    let options = options();
    let system = json!("fixture");
    let state = initial();
    let req = http::FullMessagesRequest {
        endpoint: "https://api.anthropic.com/v1/messages",
        key: "fixture",
        options: &options,
        system: &system,
        messages: &state.messages,
        timeout: Duration::from_secs(1),
        max_response_bytes: 1024,
    };
    assert_eq!(
        http::complete(&req, &CancellationToken::new())
            .await
            .unwrap_err(),
        http::Error::InvalidRequest
    );
}
struct Tree(std::path::PathBuf);
impl Tree {
    fn new() -> Self {
        let p = crate::testing::track(
            std::env::temp_dir().join(format!("am-api-bot-{}", crate::db::ulid())),
        );
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn macos_local_editor_create_view_replace_insert_and_refuse_ambiguous_edits() {
    let tree = Tree::new();
    let editor = super::tools::editor::Editor::new(&tree.0).unwrap();
    editor
        .execute(&json!({"command":"create","path":"test","file_text":"one\ntwo\n"}))
        .unwrap();
    assert_eq!(
        editor
            .execute(&json!({"command":"view","path":"test","view_range":[2,-1]}))
            .unwrap(),
        "2\ttwo\n"
    );
    assert_eq!(
        editor
            .execute(&json!({"command":"view","path":"/workspace"}))
            .unwrap(),
        "test"
    );
    assert_eq!(
        editor
            .execute(&json!({"command":"view","path":"."}))
            .unwrap(),
        "test"
    );
    editor
        .execute(&json!({"command":"str_replace","path":"test","old_str":"two","new_str":"three"}))
        .unwrap();
    editor.execute(&json!({"command":"insert","path":"/workspace/test","insert_line":1,"new_str":"middle"})).unwrap();
    assert_eq!(
        std::fs::read_to_string(tree.0.join("test")).unwrap(),
        "one\nmiddle\nthree\n"
    );
    assert!(editor
        .execute(&json!({"command":"str_replace","path":"test","old_str":"e","new_str":"x"}))
        .is_err());
    assert!(editor
        .execute(&json!({"command":"create","path":"test","file_text":"overwrite"}))
        .is_err());
}
#[test]
fn macos_local_editor_denies_traversal_symlinks_hardlinks_and_oversize_without_writes() {
    let tree = Tree::new();
    let outside = Tree::new();
    std::fs::write(outside.0.join("secret"), "untouched").unwrap();
    std::os::unix::fs::symlink(&outside.0, tree.0.join("escape")).unwrap();
    std::fs::hard_link(outside.0.join("secret"), tree.0.join("hard")).unwrap();
    let editor = super::tools::editor::Editor::new(&tree.0).unwrap();
    for path in [
        "../secret".to_string(),
        outside.0.join("secret").display().to_string(),
        "escape/secret".into(),
        "hard".into(),
    ] {
        assert!(editor
            .execute(
                &json!({"command":"str_replace","path":path,"old_str":"untouched","new_str":"bad"})
            )
            .is_err());
    }
    assert!(editor
        .execute(&json!({"command":"create","path":"escape/new","file_text":"bad"}))
        .is_err());
    assert!(editor
        .execute(&json!({"command":"create","path":"new","file_text":"x".repeat(1024*1024+1)}))
        .is_err());
    assert!(!tree.0.join("new").exists());
    assert!(!outside.0.join("new").exists());
    assert_eq!(
        std::fs::read_to_string(outside.0.join("secret")).unwrap(),
        "untouched"
    );
}
#[tokio::test]
async fn macos_local_bash_rejects_unavailable_isolation_and_never_runs_host_shell() {
    let tree = Tree::new();
    let tools = super::tools::LocalTools::new(
        &tree.0,
        super::tools::Limits {
            timeout: Duration::from_secs(1),
            output_bytes: 1024,
        },
    )
    .unwrap();
    let call = ToolCall {
        id: "x".into(),
        name: "bash".into(),
        input: json!({"command":"touch should-not-exist"}),
    };
    // On Linux with a real sandbox this may succeed, but the command still cannot run outside
    // /workspace. On unsupported platforms it must refuse without creating anything.
    let result = tools.execute(&call, &CancellationToken::new()).await;
    if cfg!(target_os = "linux") && std::path::Path::new("/usr/bin/bwrap").is_file() {
        if result.is_ok() {
            assert!(tree.0.join("should-not-exist").exists());
        }
    } else {
        assert!(result.unwrap_err().contains("isolation"));
        assert!(!tree.0.join("should-not-exist").exists());
    }
}

#[tokio::test]
async fn http_malformed_reply_and_unresponsive_connection_are_explicit_failures() {
    let f = fixture(200, vec![Value::Null]).await;
    assert_eq!(
        HttpModel(&f)
            .complete(&initial().messages, &CancellationToken::new())
            .await
            .unwrap_err(),
        http::Error::InvalidResponse
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let (mut connection, _) = listener.accept().await.unwrap();
        // Drain the request, never send a reply. Client timeout closes the connection.
        let mut buf = [0; 4096];
        while connection.read(&mut buf).await.unwrap_or(0) > 0 {}
    });
    let options = options();
    let state = initial();
    let system = json!("fixture");
    let req = http::FullMessagesRequest {
        endpoint: &endpoint,
        key: "fixture",
        options: &options,
        system: &system,
        messages: &state.messages,
        timeout: Duration::from_millis(100),
        max_response_bytes: 1024,
    };
    assert_eq!(
        http::complete(&req, &CancellationToken::new())
            .await
            .unwrap_err(),
        http::Error::Timeout
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn journal_failure_on_final_response_also_poisons_in_memory_reuse() {
    let f = fixture(200, vec![response(vec![], "end_turn")]).await;
    let mut state = initial();
    let tools = FakeTools::default();
    let journal = MemoryJournal {
        fail_at: Some(1),
        ..Default::default()
    };
    assert_eq!(
        runner::run(
            &HttpModel(&f),
            &tools,
            &journal,
            &mut state,
            &CancellationToken::new(),
            1
        )
        .await,
        Err(runner::Error::Journal)
    );
    assert_eq!(
        runner::run(
            &HttpModel(&f),
            &tools,
            &journal,
            &mut state,
            &CancellationToken::new(),
            1
        )
        .await,
        Err(runner::Error::PendingUnknown)
    );
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}
