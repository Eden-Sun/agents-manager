/// issue #73：設定不合法是**請求的問題**，回 400 `config_invalid` 並講明什麼都沒寫。
///
/// 混進 502（「herdr／DB 出錯」）的話，呼叫端分不出「改一下再送」跟「ssh 斷了、等一下重試」。
/// 觸發路徑是真的會發生的那條：外面把 config.toml 換成不合法的檔案，下一次寫設定時
/// `ConfigStore::update` 重讀最新版、套用、驗不過。
#[tokio::test]
async fn an_invalid_config_is_a_400_that_says_nothing_was_written() {
    let env = crate::testing::env().await;
    let app = &env.app;
    // `kind = 'nope'` 不是合法的 kind：投影一定擋。
    let broken = "[server]\nlisten = '127.0.0.1:7788'\n\n                      [[projects]]\nid = 'p1'\npath = '/tmp'\nlabel = 'demo'\nhost = 'remote'\n\n                      [[projects.bots]]\nid = 'b1'\nname = 'worker'\nkind = 'nope'\n";
    std::fs::write(&app.cfg.path, broken).unwrap();

    let err = app.cfg.update(|cfg| { cfg.projects[0].label = "renamed".into(); Ok(()) }).await.unwrap_err();
    match super::cfg_err(err) {
        crate::lifecycle::LcError::BadValue(body) => {
            assert_eq!(body["error"], "config_invalid");
            assert_eq!(body["config_written"], false, "跟 projection_refused 相反：這次什麼都沒寫");
            let msg = body["message"].as_str().unwrap();
            assert!(msg.contains("invalid bot kind"), "原因要留著：{msg}");
            assert!(msg.contains("config.toml 未變更"), "{msg}");
        }
        other => panic!("要是 400 config_invalid，不是 {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&app.cfg.path).unwrap(), broken, "檔案一個字都不該動");
}

/// 真正的上游錯誤仍然是 502：`cfg_err` 只認 `ConfigInvalid`，不是把所有錯誤都降成 400。
#[test]
fn other_errors_are_still_upstream() {
    match super::cfg_err(anyhow::anyhow!("herdr socket gone")) {
        crate::lifecycle::LcError::Upstream(m) => assert!(m.contains("herdr socket gone")),
        other => panic!("{other:?}"),
    }
}

/// issue #73 reopen：`projection_err` 把 `ProjectionRefused` 對應到 409、`config_written: false`——
/// 新的 commit boundary 下，閘門一律在落盤前就擋，不會再出現舊 `reproject`（已刪）那種
/// `config_written: true`（已經寫進 config.toml）的狀況。
#[test]
fn projection_err_says_the_change_was_not_written() {
    let refused = crate::projection::ProjectionRefused {
        detail: "test detail".into(),
        bots: vec!["b1".into()],
        projects: vec![],
        supervisor_children: vec![],
        bot_limit_exceeded: false,
    };
    match super::projection_err(anyhow::Error::new(refused)) {
        crate::lifecycle::LcError::Conflict(body) => {
            assert_eq!(body["reason"], "projection_refused");
            assert_eq!(body["config_written"], false);
            assert_eq!(body["bots"], serde_json::json!(["b1"]));
        }
        other => panic!("{other:?}"),
    }
}
