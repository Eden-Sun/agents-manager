
    use super::{fit_command_line, fold_newlines, MAX_COMMAND_BYTES};

    /// 2026-09-06: a 1256-byte launch line was cut at 1019 by the shell and never started.
    #[test]
    fn an_over_long_command_is_trimmed_rather_than_truncated_by_the_shell() {
        let long = "你是 issue #1 的 PM，暱稱 `pm`。".repeat(40); // ~1200 bytes of CJK
        let args = vec!["--dangerously-skip-permissions".to_string(), "--append-system-prompt".to_string(), long.clone()];
        let out = fit_command_line(args);

        let total: usize = out.iter().map(|s| s.len() + 3).sum();
        assert!(total <= MAX_COMMAND_BYTES, "the whole line fits, got {total} bytes");
        assert_eq!(out[0], "--dangerously-skip-permissions");
        assert_eq!(out[1], "--append-system-prompt");
        assert!(out[2].len() < long.len() && out[2].ends_with("…（後略）"), "the loss is visible");
        assert!(long.starts_with(out[2].trim_end_matches("…（後略）")), "what is kept is a real prefix");

        let short = vec!["--model".to_string(), "sonnet".to_string()];
        assert_eq!(fit_command_line(short.clone()), short);
    }

    /// Regression 2026-09-06 (11 GB of WARN): small overshoots must converge; hangs if broken.
    #[test]
    fn a_command_that_only_just_overshoots_still_converges() {
        for overshoot in [1usize, 2, 3, 4, 15, 16, 40] {
            let flag = "--append-system-prompt";
            // total() charges len + 3 per argument.
            let value_len = MAX_COMMAND_BYTES + overshoot - (flag.len() + 3) - 3;
            let args = vec![flag.to_string(), "x".repeat(value_len)];
            let out = fit_command_line(args);

            let total: usize = out.iter().map(|s| s.len() + 3).sum();
            assert!(total <= MAX_COMMAND_BYTES, "overshoot {overshoot}: still {total} bytes");
            assert_eq!(out[0], flag);
            assert!(out[1].len() < value_len, "overshoot {overshoot}: the value did shrink");
        }
    }

    #[test]
    fn protocols_verified_against_both_herdr_versions_are_supported() {
        assert!(super::protocol_supported(20), "0.8.2（正式現況）");
        assert!(super::protocol_supported(22), "0.9.1（#242 實測過）");
        assert!(!super::protocol_supported(21), "沒實測過的版本不能默默放行");
        assert!(!super::protocol_supported(23));
    }

    #[test]
    fn control_characters_are_folded() {
        assert_eq!(fold_newlines("你是 issue #1 的 PM。\n成員：`pm`、`dev-1`。\n合併由 daemon 處理。"),
                   "你是 issue #1 的 PM。 成員：`pm`、`dev-1`。 合併由 daemon 處理。");
        assert_eq!(fold_newlines("a\r\nb"), "a b");
        assert_eq!(fold_newlines("a\n\n\nb"), "a b");
        assert_eq!(fold_newlines("trailing\n"), "trailing");
        // herdr ≥0.9.0 擋的是所有 `char::is_control`，不只換行（#772）：tab、ESC、C1 一樣要清掉。
        assert_eq!(fold_newlines("a\tb"), "a b");
        assert_eq!(fold_newlines("```\n\tindented\n```"), "``` indented ```");
        for s in ["a\u{1b}[0mb", "x\u{7f}y\u{85}z\tq\n"] {
            assert!(!fold_newlines(s).chars().any(char::is_control), "{s:?} → {:?}", fold_newlines(s));
        }

        // Everything herdr accepts has to survive byte-for-byte.
        for s in ["--append-system-prompt", "with `backtick` and 'quote' and \"dq\"", "#1 中文與符號、《》", ""] {
            assert_eq!(fold_newlines(s), s, "{s}");
        }
    }
