
    use super::*;

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

    /// 保溫的熱壓門檻讀它：codex 的 context 用量＝最近一筆 `token_count` 的 input ÷ 視窗。
    #[test]
    fn codex_context_pct_is_the_last_request_over_the_window() {
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
        let hot = from_codex_usage(&u, 3600, u.at_ms + 10 * 60_000);
        assert_eq!(hot["source"], "rollout_estimate");
        assert_eq!(hot["warm"], true);
        assert_eq!(hot["expires_at"], u.at_ms / 1000 + 3600);
        assert_eq!(hot["recache_tokens_if_cold"], 17667);
        assert_eq!(hot["hit_ratio"], 0.739);
        assert_eq!(hot["context_used_pct"], 6.8);
        assert_eq!(hot["context_used_tokens"], 17667);
        assert_eq!(hot["context_size"], 258400);
        assert_eq!(from_codex_usage(&u, 3600, u.at_ms + 61 * 60_000)["warm"], false);
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
        let u = last_token_count(ROLLOUT).unwrap();
        store().lock().unwrap().insert("r-codex-annotate".into(), entry_for_test(Some("s"), Some(u)));
        let mut run = json!({"id": "r-codex-annotate"});
        annotate(&mut run, "codex", u.at_ms + 60_000);
        assert_eq!(run["prompt_cache"]["source"], "rollout_estimate");
        assert_eq!(run["prompt_cache"]["warm"], true);
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
        store().lock().unwrap().insert("r-retain-pc".into(), Entry::default());
        retain_runs(&["other".to_string()]);
        assert!(!store().lock().unwrap().contains_key("r-retain-pc"));
    }
