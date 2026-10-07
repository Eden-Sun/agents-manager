
    use super::*;

    #[test]
    fn a_panes_status_line_sighting_is_forgotten_after_a_while() {
        let t0 = std::time::Instant::now();
        let mut m = std::collections::HashMap::new();
        assert_eq!(note_sighting(&mut m, "h:p1".into(), "a", t0), Sighting::New);
        assert_eq!(note_sighting(&mut m, "h:p1".into(), "a", t0 + std::time::Duration::from_secs(60)), Sighting::Same);
        assert_eq!(note_sighting(&mut m, "h:p1".into(), "b", t0 + std::time::Duration::from_secs(120)), Sighting::Changed);
        // 別的 pane 在很久以後出現：p1 沒再被讀到，格子被帶走。
        let later = t0 + SIGHTING_KEEP + std::time::Duration::from_secs(200);
        assert_eq!(note_sighting(&mut m, "h:p2".into(), "a", later), Sighting::New);
        assert!(!m.contains_key("h:p1"), "收掉的 pane 不留格子：{:?}", m.keys().collect::<Vec<_>>());
    }
