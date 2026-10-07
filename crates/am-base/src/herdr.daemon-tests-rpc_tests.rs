
    use super::*;

    /// accept 了但一個字都不回的假 herdr：socket 半開時最像的那種。
    fn wedged_socket(tag: &str) -> (PathBuf, tokio::task::JoinHandle<()>) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let handle = tokio::spawn(async move {
            // 接了就把連線握在手上，永遠不寫 ack。
            let mut held = Vec::new();
            while let Ok((conn, _)) = listener.accept().await {
                held.push(conn);
            }
        });
        (sock, handle)
    }

    /// issue #491：herdr 接了連線卻不回 ack 時，`subscribe` 必須在握手逾時之後回 `Err`，
    /// 不能永遠卡住——卡住的話兩個呼叫端的退避重連迴圈都不會再跑，本機的全域訂閱又沒有 watchdog，
    /// pane 事件就永遠收不到了。
    #[tokio::test(start_paused = true)]
    async fn a_socket_that_never_acks_times_out_instead_of_hanging_forever() {
        let (sock, server) = wedged_socket("noack");
        let client = HerdrClient::new(&sock);
        let started = tokio::time::Instant::now();
        let err = client.subscribe(vec![json!({"type": "pane.exited"})]).await.expect_err("不能成功");
        let msg = format!("{err:#}");
        assert!(msg.contains("握手逾時"), "要說是握手逾時：{msg}");
        // `start_paused` 下時鐘由 tokio 推進：真的等到門檻才逾時，不是立刻失敗。
        assert!(started.elapsed() >= HerdrClient::SUBSCRIBE_HANDSHAKE_TIMEOUT, "{:?}", started.elapsed());
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 同一個形狀的 socket 上，一般 RPC 本來就有逾時（對照組：#491 修的只有 subscribe 那條）。
    #[tokio::test(start_paused = true)]
    async fn an_ordinary_rpc_on_the_same_socket_already_times_out() {
        let (sock, server) = wedged_socket("rpc");
        let client = HerdrClient::new(&sock);
        let err = client.call_timeout("ping", json!({}), Duration::from_secs(15)).await.expect_err("不能成功");
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 每個 RPC 都是獨立 socket；仍須核對 response id，否則過期／錯誤回應會被當成本次成功結果。
    #[tokio::test]
    async fn an_rpc_response_with_a_missing_or_different_id_is_rejected_as_ambiguous() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-wrong-id-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let replies: &[&[u8]] = &[
                b"{\"result\":{\"pong\":true}}\n",
                b"{\"id\":\"stale-request\",\"result\":{\"pong\":true}}\n",
                b"{\"id\":\"stale-request\",\"error\":{\"code\":\"not_found\",\"message\":\"stale error\"}}\n",
            ];
            for reply in replies {
                let (conn, _) = listener.accept().await.unwrap();
                let (r, mut w) = conn.into_split();
                let mut line = String::new();
                BufReader::new(r).read_line(&mut line).await.unwrap();
                w.write_all(reply).await.unwrap();
            }
        });
        let client = HerdrClient::new(&sock);
        for case in ["missing", "different", "different error"] {
            let err = client.call_timeout("ping", json!({}), Duration::from_secs(2)).await.expect_err("response id 不符的回應不能算成功");
            let message = format!("{err:#}");
            assert!(message.contains("response id"), "{case} id 要清楚指出 protocol 不符：{message}");
            assert!(!never_applied(&err), "{case} id 對不上時，無法知道本次請求有沒有執行");
        }
        server.await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// subscribe 的握手 ack 同樣必須對上本次 request id，否則會接上一條錯誤串流。
    #[tokio::test]
    async fn a_subscribe_ack_with_a_different_id_is_rejected() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-subscribe-wrong-id-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            let (r, mut w) = conn.into_split();
            let mut line = String::new();
            BufReader::new(r).read_line(&mut line).await.unwrap();
            w.write_all(b"{\"id\":\"stale-subscribe\",\"result\":{\"type\":\"subscription_started\"}}\n").await.unwrap();
        });
        let client = HerdrClient::new(&sock);
        let err = client.subscribe(vec![json!({"type": "pane.exited"})]).await.expect_err("錯誤 ack 不能建立訂閱");
        let message = format!("{err:#}");
        assert!(message.contains("response id"), "要清楚指出 protocol id 不符：{message}");
        server.await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// JSON-RPC 回應封套必須且只能有 result/error 其中之一；畸形封套不能被當成安全可重試的 herdr 錯誤。
    #[tokio::test]
    async fn an_rpc_response_with_both_result_and_error_is_ambiguous() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-invalid-envelope-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            let (r, mut w) = conn.into_split();
            let mut line = String::new();
            BufReader::new(r).read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let reply = json!({
                "id": req["id"],
                "result": {"pong": true},
                "error": {"code": "not_found", "message": "this cannot be a valid response"}
            });
            w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
        });
        let client = HerdrClient::new(&sock);
        let err = client.call_timeout("ping", json!({}), Duration::from_secs(2)).await.expect_err("矛盾的回應不能被接受");
        assert!(format!("{err:#}").contains("invalid response"), "應回 protocol error：{err:#}");
        assert!(!never_applied(&err), "同時帶 result/error 的畸形回應不能證明請求沒執行");
        server.await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// subscribe ack 缺少 result/error 時不是成功握手，必須觸發呼叫端重訂閱。
    #[tokio::test]
    async fn a_subscribe_ack_without_result_or_error_is_rejected() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-subscribe-empty-ack-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            let (r, mut w) = conn.into_split();
            let mut line = String::new();
            BufReader::new(r).read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            w.write_all(format!("{{\"id\":{}}}\n", req["id"]).as_bytes()).await.unwrap();
        });
        let client = HerdrClient::new(&sock);
        let err = client.subscribe(vec![json!({"type": "pane.exited"})]).await.expect_err("空 ack 不能建立訂閱");
        assert!(format!("{err:#}").contains("invalid response"), "應回 protocol error：{err:#}");
        server.await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 照腳本回話的假 herdr：每條連線讀一行請求，`reply` 決定回什麼；`events.subscribe` 回 ack 之後把 `events` 一行行吐出去。
    fn scripted_socket(tag: &str, reply: impl Fn(&Value) -> Value + Send + Sync + 'static, events: Vec<Value>) -> (PathBuf, tokio::task::JoinHandle<()>) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let reply = std::sync::Arc::new(reply);
        let handle = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                let (reply, events) = (reply.clone(), events.clone());
                tokio::spawn(async move {
                    let (r, mut w) = conn.into_split();
                    let mut line = String::new();
                    BufReader::new(r).read_line(&mut line).await.unwrap();
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let mut out = vec![json!({"id": req["id"], "result": reply(&req)})];
                    if req["method"] == "events.subscribe" {
                        out.extend(events);
                    }
                    for v in out {
                        w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
                    }
                });
            }
        });
        (sock, handle)
    }

    /// herdr 0.9.2+ 讀太慢會回 `events_lost`（帶原 request id 的 JSON-RPC error）。那一行之後這條串流就缺事件了：
    /// 串流要在那裡結束（呼叫端照斷線重訂閱、對帳），不能 warn 一聲繼續讀。假 herdr 送完錯誤行**不關連線**、
    /// 還多吐一則事件，所以結束是因為認得錯誤行，不是剛好讀到 EOF。
    #[tokio::test]
    async fn an_events_lost_error_line_ends_the_stream_instead_of_being_skipped() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-lost-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            let (r, mut w) = conn.into_split();
            let mut line = String::new();
            BufReader::new(r).read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let ev = |pane: &str| json!({"event": "pane.exited", "data": {"pane_id": pane}});
            let out = [
                json!({"id": req["id"], "result": {"type": "subscription_started"}}),
                ev("w1:p1"),
                json!({"id": req["id"], "error": {"code": "events_lost",
                       "message": "event subscription fell behind retained history; resubscribe and resync with session.snapshot"}}),
                ev("w1:p2"),
            ];
            for v in out {
                w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
            }
            std::future::pending::<()>().await;
        });
        let c = HerdrClient::new(&sock);
        let mut rx = c.subscribe(vec![json!({"type": "pane.exited"})]).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().data["pane_id"], "w1:p1");
        let next = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("錯誤行之後串流要結束，不能卡著等");
        assert!(next.is_none(), "錯誤行之後的事件不能再送出來：{next:?}");
        server.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 一個 agent 帶著我們不認得的狀態值（herdr 之後新增）：不能讓整份 `agent.list`／`pane.list` 解析失敗——
    /// 對帳讀不到清單就整輪跳過，一台主機上所有 bot 都收不到對帳。不認得的當 `unknown`。
    #[test]
    fn an_unrecognised_agent_status_does_not_fail_the_whole_listing() {
        let list = json!([agent("a", "claude", "idle", false), agent("b", "claude", "waiting", false)]);
        let parsed: Vec<AgentInfo> = serde_json::from_value(list).expect("one odd status must not poison the list");
        assert_eq!(parsed[0].agent_status, AgentStatus::Idle);
        assert_eq!(parsed[1].agent_status, AgentStatus::Unknown);
        let pane: PaneInfo = serde_json::from_value(json!({"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "agent_status": "waiting"})).unwrap();
        assert_eq!(pane.agent_status, Some(AgentStatus::Unknown));
    }

    fn agent(name: &str, kind: &str, status: &str, launch_pending: bool) -> Value {
        json!({"name": name, "agent": kind, "agent_status": status, "launch_pending": launch_pending,
               "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": format!("w1:{name}"), "cwd": "/tmp"})
    }

    /// #732：herdr 0.9.2+ 對閒著的 codex 回 `unknown`，折回 0.9.1 的 `idle`；其餘一律原樣。
    #[test]
    fn only_a_launched_codex_that_herdr_cannot_classify_is_folded_to_idle() {
        use AgentStatus::*;
        assert_eq!(fold_codex_unknown(Some("codex"), Unknown, false), Idle);
        assert_eq!(fold_codex_unknown(Some("codex"), Unknown, true), Unknown, "還在啟動：不是閒著");
        for s in [Working, Blocked, Done, Idle] {
            assert_eq!(fold_codex_unknown(Some("codex"), s, false), s, "規則判得出來的照舊");
        }
        for kind in [Some("claude"), Some("grok"), None] {
            assert_eq!(fold_codex_unknown(kind, Unknown, false), Unknown, "{kind:?} 的 unknown 不動");
        }
    }

    /// `AgentInfo` has `launch_pending`; panes and events do not, so their `unknown` must stay unknown here.
    #[tokio::test]
    async fn codex_unknown_is_folded_only_when_the_source_has_launch_pending_evidence() {
        let events = vec![
            json!({"event": "pane.agent_status_changed", "data": {"agent": "codex", "agent_status": "unknown", "pane_id": "w1:p2"}}),
            json!({"event": "pane.agent_status_changed", "data": {"agent": "claude", "agent_status": "unknown", "pane_id": "w1:p1"}}),
            json!({"event": "pane.agent_status_changed", "data": {"agent": "codex", "agent_status": "working", "pane_id": "w1:p2"}}),
            json!({"event": "pane_exited", "data": {"agent": "codex", "agent_status": "unknown", "pane_id": "w1:p2"}}),
        ];
        let (sock, server) = scripted_socket(
            "fold",
            |req| match req["method"].as_str().unwrap() {
                "agent.list" => json!({"agents": [agent("cx", "codex", "unknown", false), agent("cc", "claude", "unknown", false), agent("new", "codex", "unknown", true)]}),
                "agent.get" => json!({"agent": agent("cx", "codex", "unknown", false)}),
                "pane.list" => json!({"panes": [{"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t1", "cwd": "/tmp", "agent": "codex", "agent_status": "unknown"}]}),
                "pane.get" => json!({"pane": {"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t1", "cwd": "/tmp", "agent": "codex", "agent_status": "unknown"}}),
                _ => json!({"type": "subscription_started"}),
            },
            events,
        );
        let c = HerdrClient::new(&sock);
        let got: Vec<_> = c.agent_list().await.unwrap().into_iter().map(|a| (a.name.unwrap(), a.agent_status)).collect();
        assert_eq!(got, [("cx".into(), AgentStatus::Idle), ("cc".into(), AgentStatus::Unknown), ("new".into(), AgentStatus::Unknown)]);
        assert_eq!(c.agent_get("cx").await.unwrap().unwrap().agent_status, AgentStatus::Idle);
        assert_eq!(c.pane_list(None).await.unwrap()[0].agent_status, Some(AgentStatus::Unknown));
        assert_eq!(c.pane_get("w1:p2").await.unwrap().unwrap().agent_status, Some(AgentStatus::Unknown));

        let mut rx = c.subscribe(vec![json!({"type": "pane.agent_status_changed"})]).await.unwrap();
        let mut seen = Vec::new();
        for _ in 0..4 {
            let ev = rx.recv().await.unwrap();
            seen.push((ev.event, ev.data["agent"].as_str().unwrap().to_string(), ev.data["agent_status"].as_str().unwrap().to_string()));
        }
        let want = [
            ("pane.agent_status_changed", "codex", "unknown"),
            ("pane.agent_status_changed", "claude", "unknown"),
            ("pane.agent_status_changed", "codex", "working"),
            ("pane_exited", "codex", "unknown"),
        ];
        assert_eq!(seen.iter().map(|(e, a, s)| (e.as_str(), a.as_str(), s.as_str())).collect::<Vec<_>>(), want);
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 啟動等 ready：codex 走輪詢（server 端的 `agent.wait` 等 idle 在 0.9.2+ 永遠逾時），啟動中的 `unknown` 不算 ready；
    /// 其他 kind 照舊一次 `agent.wait`。
    #[tokio::test]
    async fn a_codex_start_is_ready_once_it_leaves_launch_pending() {
        let gets = std::sync::Arc::new(AtomicU64::new(0));
        let waits = std::sync::Arc::new(AtomicU64::new(0));
        let (g, w) = (gets.clone(), waits.clone());
        let (sock, server) = scripted_socket(
            "ready",
            move |req| match req["method"].as_str().unwrap() {
                "agent.get" => {
                    let pending = g.fetch_add(1, Ordering::SeqCst) < 2;
                    json!({"agent": agent("cx", "codex", "unknown", pending)})
                }
                "agent.wait" => {
                    w.fetch_add(1, Ordering::SeqCst);
                    json!({"agent": agent("cc", "claude", "idle", false)})
                }
                m => panic!("unexpected {m}"),
            },
            vec![],
        );
        let c = HerdrClient::new(&sock);
        let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
        let info = c.agent_wait_ready("codex", "cx", &until, 10_000).await.unwrap();
        assert_eq!(info.agent_status, AgentStatus::Idle);
        assert_eq!(gets.load(Ordering::SeqCst), 3, "兩次還在啟動、第三次才算 ready");
        assert_eq!(waits.load(Ordering::SeqCst), 0, "codex 不走 server 端的 agent.wait");

        c.agent_wait_ready("claude", "cc", &until, 10_000).await.unwrap();
        assert_eq!(waits.load(Ordering::SeqCst), 1, "claude 照舊");
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 一直在啟動（或一直 working）的 codex：到期回 `Err`（呼叫端退回 `agent.get`、不關 pane），不能無限等。
    #[tokio::test]
    async fn a_codex_that_never_settles_times_out() {
        let (sock, server) = scripted_socket("never", |_| json!({"agent": agent("cx", "codex", "working", false)}), vec![]);
        let c = HerdrClient::new(&sock);
        let started = std::time::Instant::now();
        let err = c.agent_wait_ready("codex", "cx", &[AgentStatus::Idle], 1_200).await.expect_err("不能成功");
        assert!(format!("{err:#}").contains("Working"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 解析失敗的錯誤訊息不能把整份回應（尤其是畫面）原樣帶出去。
    #[test]
    fn a_parse_error_snippet_is_short_and_never_carries_screen_content() {
        let screen = "使用者的畫面內容\n".repeat(200);
        let masked = snippet("pane.read", &screen);
        assert!(!masked.contains("使用者的畫面內容"), "pane.read 的內容不進錯誤訊息：{masked}");
        assert!(masked.contains("位元組的畫面內容，不記錄"), "{masked}");
        assert_eq!(snippet("agent.read", &screen), snippet("pane.read", &screen), "agent.read 同一條規則");

        let long = "x".repeat(500);
        let cut = snippet("pane.get", &long);
        assert_eq!(cut.chars().count(), SNIPPET_CHARS + 1, "砍到 {SNIPPET_CHARS} 字再加一個省略號");
        assert!(cut.ends_with('…'));
        // 短的原樣保留（診斷還是要看得到東西）。
        assert_eq!(snippet("pane.get", "{\"oops\":1}"), "{\"oops\":1}");
    }
