use super::*;

#[test]
fn kill_pid_refuses_values_that_would_wrap_onto_another_process() {
    assert_eq!(kill_pid(&json!({"pid": 59407})).unwrap(), 59407);
    for bad in [json!({"pid": 4294967396i64}), json!({"pid": 2147483648i64}), json!({"pid": 0}), json!({"pid": -1}), json!({"pid": "12"}), json!({"pid": 1.5}), json!({})] {
        assert!(matches!(kill_pid(&bad), Err(LcError::Bad(_))), "{bad} 要 400");
    }
}

/// API.md（§mem）：`signal` 只認 `TERM`／`KILL`（不分大小寫），沒帶或 `null` 就是 `TERM`；其他值（含非字串）400，什麼都不送。
/// 用不存在的主機：驗證若放在查主機之後，會得到 404 而不是 400，所以這條同時釘住「先驗 signal」的順序。
#[tokio::test]
async fn mem_kill_refuses_other_signals_and_accepts_the_documented_ones() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let kill = |body: Value| {
        let app = app.clone();
        async move { kill_mem_process(State(app), Json(body)).await }
    };
    for bad in [json!("HUP"), json!("STOP"), json!("SIGTERM"), json!(15), json!("")] {
        let err = kill(json!({"host": "no-such-host", "pid": 4242, "signal": bad.clone()}))
            .await
            .err()
            .unwrap_or_else(|| panic!("signal {bad} 應該 400，卻送出去了"));
        assert!(matches!(&err, LcError::BadValue(v) if v["error"] == "bad_signal"), "signal {bad} 要 400 bad_signal：{err:?}");
    }
    for ok in [json!("TERM"), json!("term"), json!("KILL"), json!("kill"), Value::Null] {
        let err = kill(json!({"host": "no-such-host", "pid": 4242, "signal": ok.clone()})).await.err().expect("不認得主機應該 404");
        assert!(matches!(err, LcError::NotFoundValue(_)), "signal {ok} 是合法值，應該過了驗證才查主機：{err:?}");
    }
    let err = kill(json!({"host": "no-such-host", "pid": 4242})).await.err().expect("不認得主機應該 404");
    assert!(matches!(err, LcError::NotFoundValue(_)), "沒帶 signal 要預設 TERM 並通過驗證：{err:?}");
}
