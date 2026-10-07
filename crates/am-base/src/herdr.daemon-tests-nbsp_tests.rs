
    use super::*;

    /// 2026-10-01 cf-ox-2（claude 2.1.285）：回音是 `❯` 接 U+00A0。讀進來就換成空格，送達證據（回音列）才對得上。
    #[test]
    fn a_no_break_space_after_the_prompt_marker_reads_as_a_space() {
        let raw = PaneRead {
            pane_id: "p".into(),
            source: "visible".into(),
            format: "text".into(),
            text: format!("❯\u{a0}ui 審查你自己做\n\n⏺ 好\n\n{r}\n❯\u{a0}\n{r}\n  ⏵⏵ bypass permissions on\n", r = "─".repeat(40)),
            revision: 1,
            truncated: false,
        };
        let r = nbsp_to_space(raw);
        assert!(!r.text.contains('\u{a0}'));
        assert_eq!(crate::lifecycle::echo_row_hits("claude", &r.text, "ui 審查你自己做"), 1, "回音列認得出來");
    }
