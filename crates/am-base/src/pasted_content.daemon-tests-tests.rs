
    use super::*;

    /// 2026-09-19 zz-r3-paste（2.1.278，session `2c0bdae3…` → id `c4ab`）送出的五則與各自的回覆。
    const LOG: &str = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude_2.1.278_pasted_content.jsonl");

    fn users() -> Vec<String> {
        LOG.lines().filter_map(crate::lifecycle::transcript_user_text).collect()
    }

    #[test]
    fn the_real_wrapped_prompts_read_back_as_what_was_pasted() {
        let u = users();
        assert_eq!(u.len(), 5);
        assert!(u[0].starts_with("\n\n<pasted_content id=\"c4ab\">\n"), "{:?}", u[0]);
        assert_eq!(original(&u[0]), "請只回覆 OK 兩個字母，不要多說任何其他的話，也不要使用任何工具。");
        assert!(matches!(original(&u[1]), Cow::Borrowed("請只回覆 OK 兩個字母不要多說其他話")), "19 字沒包，原樣借出");
        assert_eq!(original(&u[2]), "請只回覆 OK 兩個字母，不要多說其他話", "20 字包起來");
        assert_eq!(original(&u[3]), "第一行：這是多行貼上測試。\n第二行：請不要使用任何工具。\n第三行：還是一樣。\n第四行：只回覆 OK 兩個字母。");
        assert!(u[4].contains("<\\pasted_content id=\"1234\">x<\\/pasted_content id=\"1234\">"), "CLI 跳脫了字面標籤：{:?}", u[4]);
        assert_eq!(original(&u[4]), "這段文字裡有字面的 <pasted_content id=\"1234\">x</pasted_content id=\"1234\"> 標籤，請只回覆 OK 兩個字母。");
    }

    #[test]
    fn only_the_prompt_that_was_sent_matches() {
        let u = users();
        assert!(is_sent(&u[0], "請只回覆 OK 兩個字母，不要多說任何其他的話，也不要使用任何工具。"));
        assert!(is_sent(&u[0], &u[0]), "原樣也算");
        assert!(!is_sent(&u[0], "請只回覆 OK 兩個字母"), "只是其中一段不算");
        assert!(!is_sent(&u[0], "請只回覆 OK 兩個字母，不要多說其他話"), "別則");
        // 沒被 CLI 改寫過的照舊逐字：頭尾空白也是內容。
        assert!(is_sent(&u[1], "請只回覆 OK 兩個字母不要多說其他話"));
        assert!(!is_sent(&u[1], "請只回覆 OK 兩個字母不要多說其他話 "));
        // 包起來的：CLI 包之前 trim 過。
        assert!(is_sent(&u[2], "請只回覆 OK 兩個字母，不要多說其他話\n"));
    }

    /// CLI 的拆法：前後文字保留、每個區塊換成內容、標籤兩側最多吃兩個換行；id 要 4 個小寫 hex、開頭後面緊接換行、
    /// 結尾 id 要一樣，不合格的一律當普通文字。
    #[test]
    fn only_well_formed_blocks_are_unwrapped() {
        let wrap = |id: &str, body: &str| format!("\n\n<pasted_content id=\"{id}\">\n{body}\n</pasted_content id=\"{id}\">\n");
        assert_eq!(original(&format!("先看這段：{}再回答", wrap("0a9f", "貼上的字"))), "先看這段：貼上的字再回答");
        assert_eq!(original(&format!("{}{}", wrap("0a9f", "一"), wrap("0a9f", "二"))), "一二");
        assert_eq!(original(&wrap("0a9f", "")), "", "空的區塊");
        assert_eq!(original(&wrap("0a9f", "a\n\nb")), "a\n\nb", "內文的換行照留");
        for bad in [
            wrap("0A9F", "大寫"),
            wrap("0a9", "三位"),
            wrap("0a9g", "不是 hex"),
            "<pasted_content id=\"0a9f\">同一行沒換行</pasted_content id=\"0a9f\">".to_string(),
            "\n\n<pasted_content id=\"0a9f\">\n沒有結尾".to_string(),
            "\n\n<pasted_content id=\"0a9f\">\n結尾 id 不同\n</pasted_content id=\"1111\">\n".to_string(),
        ] {
            assert!(matches!(original(&bad), Cow::Borrowed(_)), "{bad:?}");
        }
        // 非 ASCII 緊接在 `id="` 後面也不會切在字元中間。
        assert!(matches!(original("<pasted_content id=\"中文字\">"), Cow::Borrowed(_)));
    }
