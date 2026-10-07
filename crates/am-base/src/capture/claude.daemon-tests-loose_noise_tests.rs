
    use super::*;

    /// #330：回覆裡剛好有「 · 」和 `left` 兩個字的一行（清單、進度）被當成 codex 狀態列剝掉——回覆少一行。
    #[test]
    fn a_reply_row_with_a_dot_and_the_word_left_is_not_chrome() {
        assert!(!is_noise("還剩 · 3 tasks left"));
        assert!(!is_noise("Time left · about 5 minutes"));
        let screen = "❯ 進度？\n⏺ 目前狀況：\n  build done · 3 tasks left\n  下一步跑測試\n";
        let reply = ClaudeCapture.extract_reply(screen).unwrap();
        assert!(reply.contains("3 tasks left"), "回覆中間那行不能被剝掉：{reply}");
        // 真的狀態列照舊是雜訊。
        assert!(is_noise("gpt-5.6-sol high · ~/p · Context 3% used · 5h 82% left · weekly 97% left"));
        assert!(is_noise("· 5h 82% left · weekly 97% left"));
    }

    /// #783：四種權限模式列（2.1.288 真機 shift+tab 輪一圈）都是輸入框底下的 chrome；回覆裡 `⏸` 開頭的句子不是。
    #[test]
    fn every_permission_mode_row_is_chrome_but_a_paused_reply_row_is_not() {
        for row in [
            "⏸ manual mode on · ← for agents",
            "⏸ manual mode on · ? for shortcuts",
            "⏸ plan mode on (shift+tab to cycle) · ← for agents",
            "⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
        ] {
            assert!(is_status_chrome(row) && is_noise(row), "{row}");
        }
        assert!(!is_mode_row("⏸ 暫停：等使用者決定 · mode on 的說明"));
        assert!(!is_noise("⏸ 暫停部署"));
        assert!(!is_noise("⏸ The manual mode on label is confusing here."));
        // 第一段要「以 mode on 結尾」（可再接 shift+tab），不是中間出現這幾個字。
        // `contains` 會把回覆「⏸ plan mode on the left…」整行當 chrome 剝掉。
        assert!(!is_mode_row("⏸ plan mode on the left is still default"));
        assert!(!is_noise("⏸ plan mode on the left is still default"));
        let quoted_status = "⏸ Quoted status: plan mode on";
        assert!(!is_mode_row(quoted_status), "句尾引用 mode on 也不是狀態列");
        assert!(!is_noise(quoted_status), "句尾引用 mode on 也不能當 chrome 剝除");
        let screen = "❯ 狀態？\n⏺ 畫面底下寫著：\n  ⏸ plan mode on the left is still default\n  下一步繼續\n";
        let reply = ClaudeCapture.extract_reply(screen).unwrap();
        assert!(reply.contains("plan mode on the left is still default"), "回覆提到 mode on 不能被剝：{reply}");

        let quoted_screen = "❯ 狀態？\n⏺ 引用了舊狀態列：\n  ⏸ Quoted status: plan mode on\n  下一步繼續\n";
        let quoted_reply = ClaudeCapture.extract_reply(quoted_screen).unwrap();
        assert!(quoted_reply.contains(quoted_status), "以 mode on 結尾的引用也不能被剝：{quoted_reply}");
    }

    /// #788：模式列只有 [`is_mode_row`] 一份，child_alerts 也呼叫它。四種模式（bypass／accept edits／plan／default manual）
    /// 連同各種尾巴都認；`child_alerts` 原本認的形式（沒有 `⏵⏵` 的 `bypass permissions on`、只剩 `shift+tab to cycle`
    /// 的折行、大小寫不同）不能退步。暫停中的回覆列、句子中間提到模式名稱的回覆都不是模式列。
    #[test]
    fn the_one_mode_row_check_covers_all_four_modes_and_not_a_paused_reply() {
        for row in [
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
            "⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
            "⏵⏵ bypass permissions on · 1 shell · ← for agents",
            "  ⏵⏵ bypass permissions on  ",
            "⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
            "⏵⏵ accept edits on · ? for shortcuts",
            "⏸ plan mode on (shift+tab to cycle) · ← for agents",
            "⏸ plan mode on · ? for shortcuts",
            "⏸ manual mode on · ← for agents",
            "⏸ manual mode on · ? for shortcuts",
            "⏸ manual mode on",
            // child_alerts 原本就認的：沒有箭頭的 bypass 列、窄 pane 折下來的提示、大小寫。
            "bypass permissions on · ← for agents",
            "permissions on (shift+tab to cycle)",
            "(Shift+Tab to cycle) · ← for agents",
        ] {
            assert!(is_mode_row(row), "{row}");
        }
        for reply in [
            "⏸ 暫停：等使用者決定 · mode on 的說明",
            "⏸ 暫停部署",
            "⏸ Paused: waiting for the user · manual mode on is the default",
            "⏸ The manual mode on label is confusing here.",
            "我把 bypass permissions on 這個模式關掉了",
            "manual mode on 是 2.1.288 的預設",
            "",
        ] {
            assert!(!is_mode_row(reply), "{reply}");
        }
        // 2.1.288 default 模式真畫面：最底那一行就是模式列。
        let fixture = include_str!("../../../../daemon/src/lifecycle/fixtures/claude-2.1.288-manual-mode-finished.txt");
        let last = fixture.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
        assert!(is_mode_row(last), "{last:?}");
    }

    /// #331：`*` 開頭的回覆行（markdown 項目、程式碼區塊的 ` * 註解`）被當 spinner 剝掉。
    #[test]
    fn a_reply_row_starting_with_star_or_dot_is_not_a_spinner() {
        assert!(!is_noise("* 第一點"));
        assert!(!is_noise("* Returns the count of items"));
        let screen = "❯ 寫註解\n⏺ 這樣：\n  /**\n   * Returns the count of items\n   */\n  fn count() {}\n";
        let reply = ClaudeCapture.extract_reply(screen).unwrap();
        assert!(reply.contains("* Returns the count of items"), "程式碼區塊的註解行不能被剝：{reply}");
        // 真的 spinner 行照舊是雜訊。
        assert!(is_noise("* Cooking… (3s · ↓ 1.0k tokens)"));
        assert!(is_noise("· Philosophising… (33m 33s · ↓ 94.9k tokens)"));
        assert!(is_noise("✻ Crunched for 9s · done 11:35 PM"));
    }
