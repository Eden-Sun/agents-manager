
    use super::{split_paste, SEND_TEXT_CHUNK};

    /// #382：拆完依序接起來一定是原文，每段都不超過上限；空字串沒有片段。
    #[test]
    fn pieces_rebuild_the_text_and_respect_the_limit() {
        let mut cases = vec![String::new(), "短".into(), "a\n".into(), "\n\n\n".into(), "x".repeat(1300), "你".repeat(500)];
        cases.push((0..58).map(|i| format!("2026081{}0{:08}", i % 10, i * 7919)).collect::<Vec<_>>().join("\n") + "\n\n以上用換行號");
        cases.push(format!("{}\n{}\n{}", "a".repeat(700), "b".repeat(10), "c".repeat(511)));
        for text in cases {
            for max in [4, 7, 1000] {
                let pieces = split_paste(&text, max);
                assert_eq!(pieces.concat(), text, "max {max}");
                assert!(pieces.iter().all(|p| !p.is_empty() && p.len() <= max), "max {max}: {:?}", pieces.iter().map(|p| p.len()).collect::<Vec<_>>());
            }
        }
        assert!(split_paste("", SEND_TEXT_CHUNK).is_empty());
    }

    #[test]
    fn a_partial_paste_is_not_never_applied_even_when_herdr_refused_the_later_chunk() {
        let refused = anyhow::Error::from(super::HerdrError { code: "pane_not_found".into(), message: "gone".into() });
        assert!(super::never_applied(&refused));
        let partial = anyhow::Error::from(super::PartialPaste { sent_bytes: 1000, source: refused });
        assert!(!super::never_applied(&partial));
    }

    /// 在換行之後切：不把一行數字從中間剖開（放得進一段的行不被切）。
    #[test]
    fn a_line_that_fits_is_never_cut() {
        let text = (0..58).map(|i| format!("2026081{}0{:08}\n", i % 10, i * 7919)).collect::<String>();
        let pieces = split_paste(&text, 100);
        assert!(pieces.len() > 5);
        assert!(pieces.iter().all(|p| p.ends_with('\n') && p.len() <= 100), "{pieces:?}");
    }

    /// 一行比上限長才硬切，而且切在字元邊界（中文一個字三個位元組）。
    #[test]
    fn an_overlong_line_is_cut_on_a_char_boundary() {
        let text = "你好".repeat(10);
        let pieces = split_paste(&text, 7);
        assert_eq!(pieces.concat(), text);
        assert!(pieces.iter().all(|p| p.len() <= 7 && std::str::from_utf8(p.as_bytes()).is_ok()));
    }
