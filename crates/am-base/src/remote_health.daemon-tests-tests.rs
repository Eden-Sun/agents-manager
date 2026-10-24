
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
        assert_eq!(read(&d, "me@box:22"), Some(ok_at("2026-09-24T10:00:00.000Z")));
        let body = std::fs::read_to_string(target_path(&d, "me@box:22")).unwrap();
        assert!(!body.contains("reason"), "連得上不寫 reason：{body}");

        let bad = Health {
            reachable: false,
            checked_at: "2026-09-24T10:01:00.000Z".into(),
            target: "me@box:22".into(),
            reason: Some("ssh 回 255".into()),
        };
        record(&d, &bad).unwrap();
        assert_eq!(read(&d, "me@box:22"), Some(bad), "後面那次蓋掉前面那次");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "暫存檔沒留下來");
    }

    /// 還沒有人試過、或這份檔被誰寫壞了：都是「不知道」（`None`），不能當成「連不上」——
    /// 那會讓 UI 在外部編譯其實好好的時候報一個假的紅燈。
    #[test]
    fn no_file_or_a_garbled_one_is_unknown_not_unreachable() {
        let d = dir("garbled");
        assert_eq!(read(&d, "me@box:22"), None, "還沒有人試過");
        std::fs::write(target_path(&d, "me@box:22"), "{not json").unwrap();
        assert_eq!(read(&d, "me@box:22"), None, "看不懂就當不知道");
        std::fs::write(target_path(&d, "me@box:22"), r#"{"reachable":false}"#).unwrap();
        assert_eq!(read(&d, "me@box:22"), None, "少了必要欄位一樣看不懂");
    }

    /// 資料目錄不存在（設定指錯、被刪掉）時 `record` 回錯，但不 panic：呼叫端只 log，不讓編譯失敗。
    #[test]
    fn recording_into_a_missing_data_dir_fails_without_panicking() {
        let d = dir("missing").join("nope");
        assert!(record(&d, &ok_at("2026-09-24T10:00:00.000Z")).is_err());
        assert_eq!(read(&d, "me@box:22"), None);
    }

    /// #1956：多台各記各的，互不蓋掉；#1956 之前的單一檔在那台自己沒有記錄時照讀，但只套用到它記的那台。
    #[test]
    fn each_build_host_keeps_its_own_record_and_the_legacy_file_still_reads() {
        let d = dir("multi");
        let local = Health { reachable: true, checked_at: "2026-10-24T10:00:00.000Z".into(), target: "local".into(), reason: None };
        let r7 = Health { reason: Some("ssh 回 255".into()), reachable: false, target: "r7@r7:22".into(), ..local.clone() };
        record(&d, &local).unwrap();
        record(&d, &r7).unwrap();
        assert_eq!(read(&d, "local"), Some(local));
        assert_eq!(read(&d, "r7@r7:22"), Some(r7));
        std::fs::write(d.join(FILE), serde_json::to_vec(&ok_at("2026-09-24T10:00:00.000Z")).unwrap()).unwrap();
        assert_eq!(read(&d, "me@box:22"), Some(ok_at("2026-09-24T10:00:00.000Z")), "舊的單一檔照讀");
        assert_eq!(read(&d, "other@box:22"), None, "舊檔記的是別台就不套用");
    }
