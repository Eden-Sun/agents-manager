//! claude 2.1.290 的 `/` 建議清單選中列改以 `❯` 開頭（issue #875）。
//! 真畫面（2026-10-06 拋棄式目錄＋herdr shell pane 跑 2.1.290，`--source visible --ansi` 讀）：清單在輸入框**上方**，
//! 選中列是**縮排兩格**的 `  ❯ /rewind      說明`，不在第 0 欄；輸入框本身是第 0 欄的 `❯`＋U+00A0＋`/rewind`。
//! 所以第 0 欄才算的判讀（回音、`/model` 確認）碰不到它，從底部往上找的輸入框也先碰到真的框。
//! 這裡鎖住：每一種判讀的結果跟「同一張畫面把清單列抹成空白列」完全一樣。

use super::*;

const REWIND: &str = include_str!("fixtures/claude-2.1.290-slash-rewind-suggestions.ansi");
const MODEL: &str = include_str!("fixtures/claude-2.1.290-slash-model-suggestions.ansi");
const EFFORT: &str = include_str!("fixtures/claude-2.1.290-slash-effort-suggestions.ansi");
const COMPACT: &str = include_str!("fixtures/claude-2.1.290-slash-compact-suggestions.ansi");

const ECHO: &str = "Reply with PONG only";

fn plain(screen: &str) -> String {
    screen.lines().map(strip_ansi).collect::<Vec<_>>().join("\n")
}

/// 同一張畫面、沒有清單：清單那幾列（`✻ …` 回合結束列之後、輸入框框頂之前的非空列）換成空白列，列數不變。
fn without_list(screen: &str) -> String {
    let lines: Vec<&str> = screen.lines().collect();
    let text = |i: usize| strip_ansi(lines[i]);
    let done = (0..lines.len()).rposition(|i| text(i).starts_with('✻')).expect("fixture 有回合結束列");
    let composer = (0..lines.len()).rposition(|i| text(i).starts_with('❯')).expect("fixture 有輸入框");
    let top = composer - 1;
    assert!(is_rule_row(text(top).trim()), "輸入框上面是框頂");
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| if i > done && i < top { "" } else { *l })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn claude_290_suggestion_list_reads_like_the_same_screen_without_it() {
    for (cmd, raw) in [("/rewind", REWIND), ("/model", MODEL), ("/effort", EFFORT), ("/compact", COMPACT)] {
        // 額外保留舊 `5h:` 格式的回歸覆蓋；純文字真畫面則直接驗證現行 `5h left` 格式。
        let old_status = plain(raw).replace("5h left 96%", "5h:96%");
        for (form, screen) in [("ansi", raw.to_string()), ("plain", plain(raw)), ("plain-5h", old_status)] {
            let ctx = format!("{cmd} {form}");
            // fixture 形狀：選中列縮排兩格、帶說明欄，在輸入框上方；輸入框是第 0 欄 `❯`＋NBSP＋指令。
            let rows: Vec<String> = screen.lines().map(strip_ansi).collect();
            let sel = rows.iter().position(|r| r.starts_with(&format!("  ❯ {cmd} "))).unwrap_or_else(|| panic!("{ctx}: 沒有選中列"));
            let boxed = rows.iter().rposition(|r| r.trim_end() == format!("❯\u{a0}{cmd}")).unwrap_or_else(|| panic!("{ctx}: 沒有輸入框"));
            assert!(sel < boxed, "{ctx}: 清單在輸入框上方");
            assert!(rows[sel].trim_end().len() > cmd.len() + 10, "{ctx}: 選中列後面有說明欄");

            let base = without_list(&screen);
            assert!(!plain(&base).contains("  ❯ /"), "{ctx}: 基準畫面沒有清單");
            let lines: Vec<&str> = screen.lines().collect();
            let base_lines: Vec<&str> = base.lines().collect();

            // 輸入框位置與內容。
            assert_eq!(box_state("claude", &screen), box_state("claude", &base), "{ctx}: box_state");
            assert_eq!(box_state("claude", &screen), BoxState::NonEmpty, "{ctx}: 框裡有指令");
            assert_eq!(composer_text("claude", &screen), composer_text("claude", &base), "{ctx}: composer_text");
            assert_eq!(composer_text("claude", &screen).as_deref(), Some(cmd), "{ctx}: 框裡就是指令");
            assert_eq!(composer_text_whole("claude", &screen), composer_text_whole("claude", &base), "{ctx}: composer_text_whole");
            assert_eq!(composer_tail("claude", &lines), composer_tail("claude", &base_lines), "{ctx}: composer_tail");
            assert!(!pane_awaits_input("claude", &screen), "{ctx}: 框不空，不是等輸入");
            assert_eq!(composer_holds_prompt("claude", &screen, cmd), composer_holds_prompt("claude", &base, cmd), "{ctx}: composer_holds_prompt");
            assert_eq!(prompt_suggestion("claude", &screen), None, "{ctx}: 清單不是建議下一句");

            // 回音索引與送達判讀：回音是使用者上一則，不是清單選中列。
            assert_eq!(after_last_prompt_echo("claude", &lines), after_last_prompt_echo("claude", &base_lines), "{ctx}: 回音索引");
            assert_eq!(last_prompt_echo_text("claude", &screen), last_prompt_echo_text("claude", &base), "{ctx}: 回音文字");
            if form == "plain" {
                // 樣式讀的回音列前面有 ESC 碼，本來就讀不到（跟清單無關）；純文字讀要拿到使用者那句。
                assert_eq!(last_prompt_echo_text("claude", &screen).as_deref(), Some(ECHO), "{ctx}: 回音文字");
            }
            assert_eq!(echo_row_hits("claude", &screen, ECHO), echo_row_hits("claude", &base, ECHO), "{ctx}: echo_row_hits");
            assert_eq!(echo_row_hits("claude", &screen, cmd), 0, "{ctx}: 選中列不算 `{cmd}` 的回音");
            let reply = extract_reply("claude", &screen);
            assert_eq!(reply, extract_reply("claude", &base), "{ctx}: extract_reply");
            if form != "ansi" {
                assert_eq!(reply.as_deref(), Some("PONG"), "{ctx}: 輸入框與狀態列不進回覆");
            }

            // rewind／換模型的畫面判讀。
            assert_eq!(crate::claude_live::parse(&screen), crate::claude_live::parse(&base), "{ctx}: claude_live::parse");
            assert!(crate::rewind::parse_menu(&screen).is_none(), "{ctx}: 不是 rewind 選單");
            assert!(!crate::rewind::in_rewind_ui(&screen), "{ctx}: 不在 rewind 畫面");
            assert!(!crate::tui_prompts::is_switch_model_dialog(&screen), "{ctx}: 不是換模型確認框");
        }
    }
}


/// 只有整塊貼著框、帶選中列的清單才切：回覆最後一段縮排的 `/路徑` 不算清單，照樣是回覆。
#[test]
fn indented_reply_rows_above_the_box_are_not_a_suggestion_list() {
    let rule = "─".repeat(60);
    let screen = format!(
        "❯ 列出路徑\n\n● 兩個：\n    /etc/hosts  系統檔\n    /tmp/x  暫存\n{rule}\n❯\u{a0}\n{rule}\n  hunta | pt | SON5.5 H | 5h:96% | 7d:46%\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n"
    );
    assert_eq!(extract_reply("claude", &screen).as_deref(), Some("兩個：\n    /etc/hosts  系統檔\n    /tmp/x  暫存"));
}
