
    use super::*;

    #[test]
    fn a_deleted_bots_probe_records_are_dropped() {
        for (bot, pane) in [("probe-gone", "w1:p1"), ("probe-gone", "w1:p2"), ("probe-kept", "w1:p3")] {
            probed().lock().unwrap().insert(probe_key(bot, pane));
        }
        retain_bots(&["probe-kept".to_string()]);
        assert!(probe_due("probe-gone", "w1:p1") && probe_due("probe-gone", "w1:p2"), "結束的 bot 不留記錄");
        assert!(!probe_due("probe-kept", "w1:p3"), "還在的 bot 的記錄不動");
        retain_bots(&[]);
    }
