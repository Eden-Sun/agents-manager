
    use super::*;

    /// 2.1.x 輸入框（`claude-2.1.281-feedback-survey.txt`）：規則線夾著空的 `❯`，底下才是狀態列。
    const COMPOSER: &str = "\
────────────────────────────────
❯
────────────────────────────────
  hunta | survey-cwd | HAI4.5 | 5h:80% | 7d:70%
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";

    /// 現行 5h left / 7d left 狀態列（issue #876）。
    const COMPOSER_MODERN: &str = "\
────────────────────────────────
❯
────────────────────────────────
  hunta | i875-throwaway | SON5.5 H ctx 4% | 5h left 96%(rst 1h 53m) | 7d left 46%(rst 3d 5h) | F5 left 95%
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";

    /// issue #876：現行 `5h left`／`7d left` 狀態列被認成 chrome，extract_reply 不收輸入框與狀態列。
    #[test]
    fn modern_5h_left_statusline_is_cut_from_reply() {
        let screen = format!(
            "❯ 請回覆測試\n⏺ 這是一則真正的回覆。\n\n回答完畢。\n{COMPOSER_MODERN}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert_eq!(reply, "這是一則真正的回覆。\n\n回答完畢。");
        assert!(!reply.contains("5h left"), "{reply}");
        assert!(!reply.contains("7d left"), "{reply}");
        assert!(!reply.contains("bypass permissions"), "{reply}");
    }

    /// #661：markdown 表格的 `│` 列與回覆裡的 `---` 不是輸入框上緣。
    #[test]
    fn a_markdown_table_and_a_horizontal_rule_stay_in_the_reply() {
        let screen = format!(
            "❯ 分支狀態？\n⏺ 以下是分支狀態：\n\n  ┌──────────┬────────┐\n  │ 分支     │ 狀態   │\n  │ feat/a   │ 過期   │\n  └──────────┴────────┘\n\n  第一部分：結論\n  ---\n  第二部分：細節很重要\n建議刪掉 feat/a。\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("│ feat/a   │ 過期   │"), "表格列被截掉：{reply}");
        assert!(reply.contains("第二部分：細節很重要"), "水平線後面被截掉：{reply}");
        assert!(reply.contains("建議刪掉 feat/a。"), "結論被截掉：{reply}");
        assert!(!reply.contains("bypass permissions"), "輸入框以下的 chrome 不能進回覆：{reply}");
        assert!(!reply.contains("5h:"), "狀態列不能進回覆：{reply}");
    }

    /// #662：回覆區塊裡的警告符號與 `Tip:` 是內容。chrome 只認輸入框底下的真狀態列。
    #[test]
    fn warning_lines_inside_the_reply_are_kept() {
        let screen = format!(
            "❯ 可以 force push 嗎？\n⏺ 可以，但注意：\n  ⚠️ 這會刪掉所有未推送的 commit\n  Tip: 先備份\n  ✗ 不要用 --force\n  ✘ 遠端也會被改寫\n  ⏵ 先看 git status\n  其餘沒問題。\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("⚠️ 這會刪掉所有未推送的 commit"), "{reply}");
        assert!(reply.contains("Tip: 先備份"), "{reply}");
        assert!(reply.contains("✗ 不要用 --force"), "{reply}");
        assert!(reply.contains("✘ 遠端也會被改寫"), "{reply}");
        assert!(reply.contains("⏵ 先看 git status"), "{reply}");
        assert!(reply.contains("其餘沒問題。"), "{reply}");
        assert!(!reply.contains("bypass permissions"), "{reply}");
    }

    /// #663：舊回覆裡的 `·`／`*` 不是 spinner。活的 spinner 只認輸入框正上方的活動列。
    #[test]
    fn an_old_dot_line_above_a_finished_reply_is_not_busy() {
        let screen = format!(
            "❯ 下載？\n⏺ 先前：\n  · 下載中… 還沒好\n  * Loading…\n  已經好了。\n{COMPOSER}"
        );
        assert!(!ClaudeCapture.still_busy(&screen), "舊回覆的項目符號被當成還在跑");
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("· 下載中… 還沒好"), "{reply}");
        assert!(reply.contains("已經好了。"), "{reply}");

        let busy = format!("❯ 下載？\n⏺ 開始了\n· Philosophising… (33m 33s · ↓ 94.9k tokens)\n{COMPOSER}");
        assert!(ClaudeCapture.still_busy(&busy), "輸入框正上方的活動列應該算還在跑");
        let baking = "❯ Reply with PONG\n✢ Baking… (3s · esc to interrupt)\n──────\n❯\n";
        assert!(ClaudeCapture.still_busy(baking));
        // codex 的中斷提示沒有經過時間，不是 claude 的活動列。
        assert!(!ClaudeCapture.still_busy("• Working (🤖 • esc to interrupt)"));
    }

    /// issue #753：回合最後一段是工具呼叫（`⏺ Bash(…)`＋它的 `⎿` 輸出）時，備援以前抓「畫面上最後一個 `⏺`」，
    /// 存成回覆的是工具呼叫原文，真正的文字回答在上面一段、整段沒進網頁。取最後一段**文字**，並收在下一個工具呼叫之前。
    #[test]
    fn the_last_text_block_wins_over_a_trailing_tool_call() {
        let screen = format!(
            "❯ 現在部 demo\n⏺ Bash(herdr agent get cf-1)\n  ⎿  status: working\n\n⏺ 已經交給 memleak 排查，\n  先看這幾點。\n\n⏺ Bash(herdr agent get cf-2 --json)\n  ⎿  {{\"status\": \"idle\"}}\n     … +3 lines (ctrl+o to expand)\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert_eq!(reply, "已經交給 memleak 排查，\n  先看這幾點。", "{reply}");
    }

    /// 最後一段就是文字時照舊（前面的工具呼叫與它們的輸出不進回覆）；收在工具呼叫之前的規則不影響它。
    #[test]
    fn a_text_block_after_tool_calls_is_still_the_reply() {
        let screen = format!(
            "❯ 看一下\n⏺ Read(src/main.rs)\n  ⎿  Read 120 lines\n\n⏺ Update(src/main.rs)\n  ⎿  Updated\n\n⏺ 改好了，三處。\n{COMPOSER}"
        );
        assert_eq!(ClaudeCapture.extract_reply(&screen).unwrap(), "改好了，三處。");
    }

    /// 這一回合只有工具呼叫（沒有任何文字段，例如被中斷）：沒有更好的可取，維持原本的行為。
    #[test]
    fn a_turn_with_only_tool_calls_keeps_the_last_one() {
        let screen = format!("❯ 跑\n⏺ Bash(sleep 60)\n  ⎿  Running…\n{COMPOSER}");
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.starts_with("Bash(sleep 60)"), "{reply}");
    }

    /// 真畫面（`claude-2.1.281-background-shell.txt`）：多行的 Bash 呼叫第一行沒有收在 `)`、下面第一行也不是 `⎿`
    /// （是命令的第二行）。這種工具呼叫要當工具呼叫，回覆仍是它上面最後一段文字。
    #[test]
    fn a_multiline_tool_call_at_the_bottom_is_still_a_tool_call() {
        let screen = format!(
            "❯ 提交\n⏺ 好，我來提交。\n\n⏺ Bash(SP=/private/tmp/claude-501/x/scratchpad\n      python3 - <<'EOF'…)\n  ⎿  daemon/src/config.rs:262: 內容\n     … +6 lines (ctrl+o to expand)\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert_eq!(reply, "好，我來提交。", "{reply}");
    }

    /// 助手畫的圓角方框圖（`╭──╮`／`╰──╯`，LLM 很愛用）是回覆內容：輸入框的位置已經由 `composer_top` 找到時，
    /// 回覆一路收到狀態列為止，不能在第一個 `╭` 就被截掉（codex 那條有同一個 fixture，`codex_reply_keeps_mermaid_box_drawing_lines`）。
    #[test]
    fn a_rounded_box_diagram_in_the_reply_does_not_end_it() {
        let screen = format!(
            "❯ 畫流程\n⏺ 流程如下：\n\n  ╭────────╮\n  │ 收到請求 │\n  ╰────┬───╯\n       │\n  ╭────▼────╮\n  │ 判斷流程 │\n  ╰─────────╯\n\n圖後文字仍屬於回覆。\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("│ 判斷流程 │"), "圖被第一個 ╭ 截掉：{reply}");
        assert!(reply.contains("圖後文字仍屬於回覆。"), "圖後面的文字被截掉：{reply}");
        assert!(!reply.contains("bypass permissions"), "輸入框以下的 chrome 不能進回覆：{reply}");
    }

    /// #762：回覆裡引了 shell 提示符行（`  ❯ npm test`，縮排在 `⏺` 區塊裡）不是使用者回音。真回音是第 0 欄的 `❯ `：
    /// 以前取「最後一個 `❯ ` 開頭的行」，起點落在引用行之後，回覆整段 `None`（備援存不到回覆）。
    #[test]
    fn a_prompt_line_quoted_inside_the_reply_is_not_the_echo() {
        let screen = format!("❯ 怎麼跑測試\n⏺ 執行：\n  ❯ npm test\n  PASS\n結論：全綠。\n{COMPOSER}");
        assert_eq!(
            ClaudeCapture.extract_reply(&screen).as_deref(),
            Some("執行：\n  ❯ npm test\n  PASS\n結論：全綠。"),
        );
        // activity 也從真回音起算：引用行之後才有的字不能讓前面的活動列消失。
        let busy = "❯ 跑\n✻ Cooking… (3s · ↓ 1 tokens)\n⏺ 結果：\n  ❯ ls\n";
        assert_eq!(ClaudeCapture.activity(busy).as_deref(), Some("Cooking… (3s · ↓ 1 tokens)"));
    }

    /// 輸入框那一行只有 `❯` 加空白（有的版本／寬度會在後面補空白）：不是回音，不能把起點推到畫面最底、
    /// 讓上面真正的回音與回覆整段消失（#762 審查；註解寫「內容要有字」，條件以前只看長度，空白也算有字）。
    #[test]
    fn a_composer_row_of_only_spaces_after_the_marker_is_not_the_echo() {
        for composer in ["❯ ", "❯   ", "❯                                        "] {
            let screen = format!("❯ 問題\n⏺ 答案\n{composer}\n");
            assert_eq!(ClaudeCapture.extract_reply(&screen).as_deref(), Some("答案"), "composer={composer:?}");
        }
    }

    /// 縮排的 `❯ ` 一律不是回音（排隊中的訊息、引用）：沒有第 0 欄回音時從頭看，最後一個 `⏺` 照舊是回覆。
    #[test]
    fn an_indented_prompt_row_never_counts_as_the_echo() {
        let screen = format!("⏺ 舊回覆\n  ❯ 排隊的新問題\n⏺ 新回覆\n{COMPOSER}");
        assert_eq!(ClaudeCapture.extract_reply(&screen).as_deref(), Some("新回覆"));
    }

    /// 回覆文字自己長得像函式呼叫（沒有 `⎿` 輸出跟在後面）不是工具呼叫。
    #[test]
    fn a_reply_that_mentions_a_call_is_not_a_tool_call() {
        let screen = format!("❯ 怎麼寫\n⏺ parse(input) 會回傳 Option，\n  記得處理 None。\n{COMPOSER}");
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("記得處理 None。"), "{reply}");
    }
