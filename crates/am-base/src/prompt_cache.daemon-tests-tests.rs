
    use super::*;
    use std::future::Future;
    use std::time::Duration;

    /// 真實格式（codex 0.15x rollout，2026-09-28 取樣）：第一筆 `info:null`、其後每筆 `last_token_usage` 是這一次請求。
    const ROLLOUT: &str = concat!(
        r#"{"timestamp":"2026-09-28T14:35:12.000Z","ordinal":3,"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#, "\n",
        r#"{"timestamp":"2026-09-28T14:35:13.100Z","ordinal":4,"type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex"}}}"#, "\n",
        r#"{"timestamp":"2026-09-28T14:35:32.249Z","ordinal":18,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":13547,"cached_input_tokens":11008,"cache_write_input_tokens":0,"output_tokens":267,"reasoning_output_tokens":126,"total_tokens":13814},"last_token_usage":{"input_tokens":13547,"cached_input_tokens":11008,"cache_write_input_tokens":0,"output_tokens":267,"reasoning_output_tokens":126,"total_tokens":13814},"model_context_window":258400},"rate_limits":{"limit_id":"codex","primary":{"used_percent":8.0,"window_minutes":300,"resets_at":1790622294}}}}"#, "\n",
        r#"{"timestamp":"2026-09-28T14:35:40.856Z","ordinal":25,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":31214,"cached_input_tokens":24064,"cache_write_input_tokens":0,"output_tokens":552,"reasoning_output_tokens":322,"total_tokens":31766},"last_token_usage":{"input_tokens":17667,"cached_input_tokens":13056,"cache_write_input_tokens":0,"output_tokens":285,"reasoning_output_tokens":196,"total_tokens":17952},"model_context_window":258400},"rate_limits":{"limit_id":"codex"}}}"#, "\n",
        r#"{"timestamp":"2026-09-28T14:35:41.000Z","ordinal":26,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"token_count 不是這行"}]}}"#, "\n",
    );

    fn ms(s: &str) -> i64 {
        db::parse_ts(s).unwrap().timestamp_millis()
    }

    #[test]
    fn the_last_token_count_in_a_real_rollout_is_read() {
        let u = last_token_count(ROLLOUT).unwrap();
        assert_eq!(u, CodexUsage { at_ms: ms("2026-09-28T14:35:40.856Z"), input: 17667, cached: 13056, window: 258400 });
        // `info:null`、其他事件、半行都不是讀數。
        assert!(last_token_count(r#"{"timestamp":"2026-09-28T14:35:13.100Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#).is_none());
        assert!(parse_token_count(r#"{"timestamp":"2026-09-28T14:35:32.249Z","type":"event_msg","payload":{"type":"token_cou"#).is_none());
        assert!(last_token_count("").is_none());
    }

    /// 使用者 2026-10-14：檔尾（256 KB）全是工具輸出、找不到 token_count 時往前多找，冷掉之後才知道冷前用了多少 context。
#[test]
fn a_token_count_older_than_the_tail_is_still_found() {
    let dir = crate::testing::scratch_dir("am-rollout-tail");
    let path = dir.join("rollout.jsonl");
    let filler = format!("{}\n", serde_json::json!({"type": "response_item", "payload": {"output": "x".repeat(4000)}}));
    let mut body = ROLLOUT.to_string();
    for _ in 0..((TAIL_BYTES as usize / filler.len()) + 10) {
        body.push_str(&filler);
    }
    std::fs::write(&path, &body).unwrap();
    let (tail, end) = read_new(&path, 0).unwrap();
    assert!(last_token_count(&tail).is_none(), "檔尾裡沒有 token_count");
    assert_eq!(last_token_count_before(&path, end), last_token_count(ROLLOUT), "往前找到最後一筆");
}

/// 保溫的熱壓門檻讀它：codex 的 context 用量＝最近一筆 `token_count` 的 input ÷ 視窗。
    #[test]
    fn codex_context_pct_is_the_last_request_over_the_window() {
        let _serial = store_serial();
        let run = "r-codex-ctx-pct";
        assert_eq!(codex_context_pct(run), None, "還沒讀到 rollout");
        let u = last_token_count(ROLLOUT).unwrap();
        store().lock().unwrap().insert(run.into(), entry_for_test(None, Some(u)));
        let pct = codex_context_pct(run).unwrap();
        assert!((pct - 17667.0 / 258400.0 * 100.0).abs() < 1e-9, "{pct}");
        store().lock().unwrap().insert(run.into(), entry_for_test(None, Some(CodexUsage { window: 0, ..u })));
        assert_eq!(codex_context_pct(run), None, "視窗大小不明不猜");
        store().lock().unwrap().remove(run);
    }

    #[test]
    fn codex_usage_becomes_an_estimate_with_context_and_hit_ratio() {
        let u = last_token_count(ROLLOUT).unwrap();
        let hot = from_codex_usage(&u, Some(3600), u.at_ms + 10 * 60_000);
        assert_eq!(hot["source"], "rollout_estimate");
        assert_eq!(hot["warm"], true);
        assert_eq!(hot["expires_at"], u.at_ms / 1000 + 3600);
        assert_eq!(hot["recache_tokens_if_cold"], 17667);
        assert_eq!(hot["hit_ratio"], 0.739);
        assert_eq!(hot["context_used_pct"], 6.8);
        assert_eq!(hot["context_used_tokens"], 17667);
        assert_eq!(hot["context_size"], 258400);
        assert_eq!(from_codex_usage(&u, Some(3600), u.at_ms + 61 * 60_000)["warm"], false);
        // 沒有 TTL（codex 現況）：閒置再久都熱，不給到期時間。
        let never = from_codex_usage(&u, None, u.at_ms + 24 * 3600_000);
        assert_eq!(never["warm"], true);
        assert!(never["expires_at"].is_null() && never["ttl_secs"].is_null());
        assert_eq!(never["hit_ratio"], 0.739);
    }

    /// claude 2.1.289 statusLine 實例（取用欄位以外的原文不外送）。
    const STATUS: &str = r#"{"version":"2.1.289","cwd":"/secret/path","transcript_path":"/x",
        "context_window":{"used_percentage":45,"context_window_size":1000000,"total_input_tokens":9999999,
          "current_usage":{"input_tokens":1000,"cache_creation_input_tokens":8000,"cache_read_input_tokens":441000}},
        "prompt_cache":{"warm":true,"expires_at":1791119812,"ttl":"1h","hit_ratio":0.994,"recache_tokens_if_cold":459258,"misses":0,
          "miss_causes":{},"last_miss_cause":null,"cache_write_tokens":434165,"requests":213,"caching_observed":true}}"#;

    #[test]
    fn claude_statusline_is_slimmed_down_without_the_raw_json() {
        let v = from_statusline(STATUS);
        assert_eq!(v["source"], "statusline");
        assert_eq!(v["warm"], true);
        assert_eq!(v["expires_at"], 1791119812);
        assert_eq!(v["ttl_secs"], 3600);
        assert_eq!(v["recache_tokens_if_cold"], 459258);
        assert_eq!(v["hit_ratio"], 0.994);
        assert_eq!(v["context_used_pct"], 45.0);
        assert_eq!(v["context_used_tokens"], 450000);
        assert_eq!(v["context_size"], 1000000);
        let s = v.to_string();
        assert!(!s.contains("/secret/path") && !s.contains("cache_write_tokens") && !s.contains("miss_causes"));
    }

    #[test]
    fn claude_without_prompt_cache_has_none() {
        assert!(from_statusline(r#"{"version":"2.1.280","context_window":{"used_percentage":5}}"#).is_null());
        assert!(from_statusline("not json").is_null());
        let mut run = json!({"id": "r-old", "status_json": r#"{"context_window":{"used_percentage":5}}"#});
        annotate(&mut run, "claude", 0);
        assert!(run["prompt_cache"].is_null());
        let mut run = json!({"id": "r-new", "status_json": STATUS});
        annotate(&mut run, "claude", 0);
        assert_eq!(run["prompt_cache"]["warm"], true);
    }

    #[test]
    fn a_ttl_that_is_not_ascii_or_overflows_is_unknown_not_a_panic() {
        // 單位若取最後一個位元組會切在字元中間（panic）；秒數乘上單位溢位（debug 直接 panic，release 繞成負數）。
        for ttl in ["5分", "分", "9000000000000000h"] {
            let v = from_statusline(&format!(r#"{{"prompt_cache":{{"ttl":"{ttl}"}}}}"#));
            assert_eq!(v["ttl_secs"], Value::Null, "{ttl}");
        }
    }

    #[test]
    fn grok_and_no_run_carry_nothing() {
        let mut run = json!({"id": "r-grok", "status_json": STATUS});
        annotate(&mut run, "grok", 0);
        assert!(run["prompt_cache"].is_null());
        let mut none = Value::Null;
        annotate(&mut none, "claude", 0);
        assert!(none.is_null());
    }

    #[test]
    fn codex_annotate_uses_the_remembered_rollout_reading() {
        let _serial = store_serial();
        let u = last_token_count(ROLLOUT).unwrap();
        store().lock().unwrap().insert("r-codex-annotate".into(), entry_for_test(Some("s"), Some(u)));
        let mut run = json!({"id": "r-codex-annotate"});
        annotate(&mut run, "codex", u.at_ms + 3 * 3600_000);
        assert_eq!(run["prompt_cache"]["source"], "rollout_estimate");
        assert_eq!(run["prompt_cache"]["warm"], true, "codex 閒置 3 小時仍熱");
        assert!(run["prompt_cache"]["expires_at"].is_null());
        // 還沒讀到任何 token_count：null。
        let mut fresh = json!({"id": "r-codex-unseen"});
        annotate(&mut fresh, "codex", 0);
        assert!(fresh["prompt_cache"].is_null());
        store().lock().unwrap().remove("r-codex-annotate");
    }

    #[test]
    fn rollout_is_read_incrementally_and_never_from_the_middle_of_a_line() {
        let dir = crate::testing::scratch_dir("am-pc");
        let path = dir.join("rollout-x.jsonl");
        let lines: Vec<&str> = ROLLOUT.lines().collect();
        // 第一輪：前三行＋第四行只寫一半。
        let half = &lines[3][..40];
        std::fs::write(&path, format!("{}\n{}\n{}\n{half}", lines[0], lines[1], lines[2])).unwrap();
        let (t1, off1) = read_new(&path, 0).unwrap();
        assert_eq!(last_token_count(&t1).unwrap().input, 13547);
        assert_eq!(off1 as usize, lines[0].len() + lines[1].len() + lines[2].len() + 3);
        // 沒新增：空字串、位移不動。
        assert_eq!(read_new(&path, off1).unwrap(), (String::new(), off1));
        // 補完那一行：只讀到新增的整行，不重讀前面。
        std::fs::write(&path, format!("{}\n{}\n{}\n{}\n", lines[0], lines[1], lines[2], lines[3])).unwrap();
        let (t2, off2) = read_new(&path, off1).unwrap();
        assert_eq!(t2.lines().count(), 1);
        assert_eq!(last_token_count(&t2).unwrap().input, 17667);
        assert_eq!(off2, std::fs::metadata(&path).unwrap().len());
        // 檔案縮小（換檔）：從頭來。
        std::fs::write(&path, format!("{}\n", lines[2])).unwrap();
        assert_eq!(last_token_count(&read_new(&path, off2).unwrap().0).unwrap().input, 13547);
        // 新增很大：只取檔尾、第一個（可能是半行的）片段丟掉。
        let mut big = String::new();
        while (big.len() as u64) < TAIL_BYTES * 2 {
            big.push_str(lines[0]);
            big.push('\n');
        }
        big.push_str(lines[3]);
        big.push('\n');
        std::fs::write(&path, &big).unwrap();
        let (t3, off3) = read_new(&path, 0).unwrap();
        assert!(t3.len() as u64 <= TAIL_BYTES);
        assert_eq!(last_token_count(&t3).unwrap().input, 17667);
        assert_eq!(off3, big.len() as u64);
    }

    #[test]
    fn retain_runs_forgets_finished_runs() {
        let _serial = store_serial();
        store().lock().unwrap().insert("r-retain-pc".into(), Entry::default());
        retain_runs(&["other".to_string()]);
        assert!(!store().lock().unwrap().contains_key("r-retain-pc"));
    }

/// `retain_runs` 會清掉整個 store：跟遠端節流測試並行會互相吃掉 entry，所以動全域 store 的測試排隊跑。
static STORE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn store_serial() -> tokio::sync::MutexGuard<'static, ()> {
    STORE_SERIAL.blocking_lock()
}

async fn store_serial_async() -> tokio::sync::MutexGuard<'static, ()> {
    STORE_SERIAL.lock().await
}

fn remote_chunk(text: &str, next: u64) -> Option<am_ports::RemoteRollout> {
    Some(am_ports::RemoteRollout { text: text.into(), next })
}

/// 遠端 codex：每 [`REMOTE_EVERY`] 最多一次、只從上次位移起讀、通道失敗保留讀數並退避；假通道（不連真主機）。
#[tokio::test]
async fn remote_codex_reads_on_a_timer_and_resumes_from_the_offset() {
    let _serial = store_serial_async().await;
    let run = "r-remote-timer";
    store().lock().unwrap().remove(run);
    let lines: Vec<&str> = ROLLOUT.lines().collect();
    let two = format!("{}\n", lines[2]); // input 13547
    let three = format!("{}\n", lines[3]); // input 17667
    let calls = std::cell::RefCell::new(Vec::<Option<u64>>::new());

    // 第一次：沒有位移，讀到第一筆讀數就變動。
    let first = refresh_remote(run, "s1", |off, _cap| {
        calls.borrow_mut().push(off);
        std::future::ready(remote_chunk(&two, two.len() as u64))
    })
    .await;
    assert!(first);
    assert_eq!(remote_state_for_test(run), Some((two.len() as u64, Some(13547))));

    // 間隔內再來：不打 host。
    assert!(!refresh_remote(run, "s1", |off, _cap| {
        calls.borrow_mut().push(off);
        std::future::ready(remote_chunk("", 0))
    })
    .await);
    assert_eq!(calls.borrow().len(), 1, "節流間隔內不讀");

    // 間隔過了、沒有新行：從上次位移讀，讀數不變。
    age_remote_for_test(run);
    let n = two.len() as u64;
    assert!(!refresh_remote(run, "s1", |off, _cap| {
        calls.borrow_mut().push(off);
        std::future::ready(remote_chunk("", n))
    })
    .await);
    assert_eq!(calls.borrow()[1], Some(n), "只從上次位移起讀");
    assert_eq!(remote_state_for_test(run), Some((n, Some(13547))));

    // 通道失敗：讀數與位移都不動，且退避到下一個間隔（不立刻再打）。
    age_remote_for_test(run);
    assert!(!refresh_remote(run, "s1", |off, _cap| {
        calls.borrow_mut().push(off);
        std::future::ready(None)
    })
    .await);
    assert_eq!(remote_state_for_test(run), Some((n, Some(13547))));
    assert!(!refresh_remote(run, "s1", |off, _cap| {
        calls.borrow_mut().push(off);
        std::future::ready(None)
    })
    .await);
    assert_eq!(calls.borrow().len(), 3, "失敗後退避中不再讀");

    // 有新行：讀數變成新的一筆，位移前進。
    age_remote_for_test(run);
    let total = n + three.len() as u64;
    assert!(refresh_remote(run, "s1", |off, _cap| {
        calls.borrow_mut().push(off);
        std::future::ready(remote_chunk(&three, total))
    })
    .await);
    assert_eq!(remote_state_for_test(run), Some((total, Some(17667))));

    // 換 session：位移與讀數作廢，從頭（None）讀。
    assert!(!refresh_remote(run, "s2", |off, _cap| {
        calls.borrow_mut().push(off);
        std::future::ready(remote_chunk("", 0))
    })
    .await);
    assert_eq!(calls.borrow().last(), Some(&None));
    assert_eq!(remote_state_for_test(run), Some((0, None)));
    store().lock().unwrap().remove(run);
}

/// 檔案被換掉（rotate）：位移由讀取端決定，新位移比舊的小也照接。
#[tokio::test]
async fn remote_codex_follows_the_offset_the_reader_returns_after_a_rotation() {
    let _serial = store_serial_async().await;
    let run = "r-remote-rotate";
    store().lock().unwrap().remove(run);
    let lines: Vec<&str> = ROLLOUT.lines().collect();
    let big = format!("{}\n{}\n", lines[2], lines[3]);
    assert!(refresh_remote(run, "s1", |_, _| std::future::ready(remote_chunk(&big, big.len() as u64))).await);
    age_remote_for_test(run);
    let short = format!("{}\n", lines[2]);
    assert!(refresh_remote(run, "s1", |_, _| std::future::ready(remote_chunk(&short, short.len() as u64))).await);
    assert_eq!(remote_state_for_test(run), Some((short.len() as u64, Some(13547))));
    store().lock().unwrap().remove(run);
}

/// 遠端讀取很慢（或卡住）時巡邏立刻返回，同一 run 同時只會有一個在飛（#1954 review）。
#[tokio::test]
async fn a_slow_remote_read_never_holds_the_patrol_and_is_never_doubled() {
    let run = "r-remote-inflight";
    let (done, gate) = tokio::sync::oneshot::channel::<()>();
    let started = std::time::Instant::now();
    assert!(spawn_remote_once(run, async move {
        let _ = gate.await;
    }));
    assert!(started.elapsed() < Duration::from_millis(100), "巡邏不等遠端讀取");
    assert!(!spawn_remote_once(run, async {}), "同一 run 還在飛就不另起");
    done.send(()).unwrap();
    for _ in 0..200 {
        if spawn_remote_once(run, async {}) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("在飛標記沒有清掉");
}

/// 第一次只讀 256 KiB（同本機 TAIL_BYTES）；檔尾沒有 `token_count`、又沒有讀數，往前補一次到 8 MiB，之後不再補。
#[tokio::test]
async fn the_first_remote_read_is_the_context_cap_and_a_missing_token_count_looks_back_once() {
    let run = "r-remote-wide";
    let _serial = store_serial_async().await;
    store().lock().unwrap().remove(run);
    let lines: Vec<&str> = ROLLOUT.lines().collect();
    let two = format!("{}\n", lines[2]); // input 13547
    let filler = "x\n".to_string(); // 檔尾全是工具輸出
    let calls = std::cell::RefCell::new(Vec::<(Option<u64>, u64)>::new());
    let read = |off: Option<u64>, cap: u64| {
        calls.borrow_mut().push((off, cap));
        let text = if cap == TAIL_BYTES { filler.clone() } else { two.clone() };
        let next = text.len() as u64;
        std::future::ready(remote_chunk(&text, next))
    };
    assert!(refresh_remote(run, "s1", read).await, "往前補到的讀數算變動");
    assert_eq!(calls.borrow().as_slice(), &[(None, TAIL_BYTES), (None, REMOTE_BACKWARD_BYTES)]);
    assert_eq!(remote_state_for_test(run), Some((two.len() as u64, Some(13547))));
    age_remote_for_test(run);
    let read = |off: Option<u64>, cap: u64| {
        calls.borrow_mut().push((off, cap));
        std::future::ready(remote_chunk(&filler, (two.len() + filler.len()) as u64))
    };
    assert!(!refresh_remote(run, "s1", read).await);
    assert_eq!(calls.borrow().len(), 3, "只往前補一次");
    assert_eq!(calls.borrow()[2], (Some(two.len() as u64), TAIL_BYTES), "之後只讀新增、仍是 256 KiB 上限");
    store().lock().unwrap().remove(run);
}

/// 超時（讀不到）不會變成每 20 秒重傳 8 MiB：只有成功讀到檔尾、又找不到讀數才會往前補。
#[tokio::test]
async fn a_timed_out_remote_read_never_escalates_to_the_wide_read() {
    let run = "r-remote-timeout";
    let _serial = store_serial_async().await;
    store().lock().unwrap().remove(run);
    let calls = std::cell::RefCell::new(Vec::<(Option<u64>, u64)>::new());
    for _ in 0..3 {
        assert!(!refresh_remote(run, "s1", |off, cap| {
            calls.borrow_mut().push((off, cap));
            std::future::ready(None)
        })
        .await);
        age_remote_for_test(run);
    }
    assert_eq!(calls.borrow().as_slice(), &[(None, TAIL_BYTES), (None, TAIL_BYTES), (None, TAIL_BYTES)]);
    store().lock().unwrap().remove(run);
}

/// 本機 codex 照舊讀本機 rollout（位移與讀數），不走遠端通道。
struct LocalRollout {
    path: PathBuf,
    remote_calls: std::sync::atomic::AtomicUsize,
}

impl am_ports::CodexRolloutAccess for LocalRollout {
    fn local_rollout_path<'a>(&'a self, _bot: &'a am_core::BotId, _session: &'a am_core::SessionId) -> impl Future<Output = Option<PathBuf>> + Send + 'a {
        std::future::ready(Some(self.path.clone()))
    }

    fn remote_rollout_read<'a>(
        &'a self,
        _run: &'a am_core::RunId,
        _bot: &'a am_core::BotId,
        _session: &'a am_core::SessionId,
        _offset: Option<u64>,
        _cap: u64,
    ) -> impl Future<Output = Option<am_ports::RemoteRollout>> + Send + 'a {
        self.remote_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(None)
    }
}

struct NoEvents;

impl am_ports::EventSink for NoEvents {
    fn emit<'a>(&'a self, _event: am_core::EventEnvelope) -> impl Future<Output = Result<am_core::EventSeq, am_core::PortError>> + Send + 'a {
        std::future::ready(Ok(0))
    }

    fn bot_status_changed<'a>(&'a self, _bot: &'a str) -> impl Future<Output = Result<(), am_core::PortError>> + Send + 'a {
        std::future::ready(Ok(()))
    }
}

#[tokio::test]
async fn a_local_codex_run_still_reads_its_own_rollout_and_never_the_remote_channel() {
    let env = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&env.app, &env.project_id, "local-codex-lock").await;
    let id = crate::testing::fake_run(&env.app, &bot.id).await;
    let mut run = db::run(&env.app.db, &id).await.unwrap().unwrap();
    run.native_session_id = Some("local-sess".into());
    let _serial = store_serial_async().await;
    store().lock().unwrap().remove(&run.id);
    let dir = crate::testing::scratch_dir("am-local-rollout");
    let path = dir.join("rollout-local-sess.jsonl");
    std::fs::write(&path, ROLLOUT).unwrap();
    let rollout = LocalRollout { path, remote_calls: std::sync::atomic::AtomicUsize::new(0) };
    refresh_codex_with_ports(&am_ports::DbContext::new(env.app.db.clone()), &rollout, &NoEvents, &run).await;
    assert_eq!(rollout.remote_calls.load(std::sync::atomic::Ordering::SeqCst), 0, "本機 run 不走遠端通道");
    assert_eq!(remote_state_for_test(&run.id), Some((ROLLOUT.len() as u64, Some(17667))));
    store().lock().unwrap().remove(&run.id);
}
