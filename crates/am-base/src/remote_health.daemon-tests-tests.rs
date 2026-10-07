
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        crate::testing::scratch_dir(&format!("am-remote-health-{tag}"))
    }

    fn ok_at(t: &str) -> Health {
        Health { reachable: true, checked_at: t.into(), target: "me@box:22".into(), reason: None }
    }

    /// 寫進去讀得回來，而且連得上時不留 `reason`（前端不用判斷「成功但有原因」這種狀態）。
    #[test]
    fn a_recorded_result_reads_back_and_a_reachable_one_has_no_reason() {
        let d = dir("roundtrip");
        record(&d, &ok_at("2026-09-24T10:00:00.000Z")).unwrap();
        assert_eq!(read(&d), Some(ok_at("2026-09-24T10:00:00.000Z")));
        let body = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert!(!body.contains("reason"), "連得上不寫 reason：{body}");

        let bad = Health {
            reachable: false,
            checked_at: "2026-09-24T10:01:00.000Z".into(),
            target: "me@box:22".into(),
            reason: Some("ssh 回 255".into()),
        };
        record(&d, &bad).unwrap();
        assert_eq!(read(&d), Some(bad), "後面那次蓋掉前面那次");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "暫存檔沒留下來");
    }

    /// 還沒有人試過、或這份檔被誰寫壞了：都是「不知道」（`None`），不能當成「連不上」——
    /// 那會讓 UI 在外部編譯其實好好的時候報一個假的紅燈。
    #[test]
    fn no_file_or_a_garbled_one_is_unknown_not_unreachable() {
        let d = dir("garbled");
        assert_eq!(read(&d), None, "還沒有人試過");
        std::fs::write(d.join(FILE), "{not json").unwrap();
        assert_eq!(read(&d), None, "看不懂就當不知道");
        std::fs::write(d.join(FILE), r#"{"reachable":false}"#).unwrap();
        assert_eq!(read(&d), None, "少了必要欄位一樣看不懂");
    }

    /// 資料目錄不存在（設定指錯、被刪掉）時 `record` 回錯，但不 panic：呼叫端只 log，不讓編譯失敗。
    #[test]
    fn recording_into_a_missing_data_dir_fails_without_panicking() {
        let d = dir("missing").join("nope");
        assert!(record(&d, &ok_at("2026-09-24T10:00:00.000Z")).is_err());
        assert_eq!(read(&d), None);
    }
