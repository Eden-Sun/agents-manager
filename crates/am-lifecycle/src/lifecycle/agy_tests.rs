/// 守門（比照 `scratch_leak_guard`，2026-10-24 測試污染真實路徑）：測試建置的家目錄一定在暫存目錄；
/// `refuse_real_home` 對真的家目錄回錯、對暫存目錄放行。am-lifecycle 的測試行程 HOME 是真的，靠 `home::dir()` 的假目錄擋住。
#[test]
fn a_test_build_never_resolves_or_writes_the_real_home() {
    assert!(crate::home::TEST_BUILD, "這個守門只在測試建置有意義");
    let home = crate::home::dir().unwrap();
    assert!(crate::home::under_temp(&home), "{}", home.display());
    if let Some(real) = std::env::var_os("HOME").map(std::path::PathBuf::from).filter(|h| !crate::home::under_temp(h)) {
        assert!(super::agy_hook::refuse_real_home(&real).is_err(), "真的家目錄 {} 必須被拒寫", real.display());
    }
    assert!(super::agy_hook::refuse_real_home(&std::env::temp_dir().join("am-x")).is_ok()); // scratch-guard: allow 純字串斷言，不建檔
}
mod screen_reply_tests {
    use crate::lifecycle::poller::{pane_awaits_input, screen_reply};
    use crate::lifecycle::screen::{clean_screen, last_prompt_echo_text};

    const RULE: &str = "────────────────────────────────────────────────────────────";

    /// 2026-10-04 a1（子 agent，沒有 hook）真機的版面，email 換成假的：shell 的 am-env 行、啟動指令、banner（帶帳號與方案）、
    /// 使用者 prompt 的回音、工具列 `●`、思考 `▸ Thought…`＋思考內文、最後才是回答，底下是輸入列與 `? for shortcuts`。
    fn pane(with_echo: bool) -> String {
        let shell = "ubuntu@ubuntu:~/project/x$  . /tmp/am-env.AbCd12 && rm -f /tmp/am-env.AbCd12\n\
ubuntu@ubuntu:~/project/x$ agy --dangerously-skip-permissions --model gemini-3.8-flash-medium\n\n\
      ▄▀▀▄        Antigravity CLI 1.2.16\n\
     ▀▀▀▀▀▀       me@example.com (Google AI Plus)\n\
    ▀▀▀▀▀▀▀▀      Gemini 3.8 Flash (Medium)\n\
   ▄▀▀    ▀▀▄     /home/user/project/x\n\
  ▄▀▀      ▀▀▄\n\n";
        let echo = if with_echo { format!("{RULE}\n> 你是 agents-manager 的子 agent。請修好 foo\n  第二行的說明\n\n") } else { String::new() };
        format!(
            "{shell}{echo}● Read(foo.sh) (ctrl+o to expand)\n▸ Thought for 5s, 430 tokens\n  The script fails with a syntax error on line 54, matching the report.\n\n  已完成：修好了 foo。\n  測試通過。\n\n{RULE}\n>\n{RULE}\n  ● [16:37:02] bash scripts/check.sh ops running\n{RULE}\n? for shortcuts                                              Gemini 3.8 Fl\n"
        )
    }

    const SENT: &str = "你是 agents-manager 的子 agent。請修好 foo\n第二行的說明";

    fn assert_clean(reply: &str) {
        for leak in ["me@example.com", "Google AI Plus", "am-env", "agy --dangerously", "Antigravity CLI", "ubuntu@ubuntu", "for shortcuts", "Thought for", "Read(foo.sh)", "▄▀", "check.sh ops running", "子 agent。請修好"] {
            assert!(!reply.contains(leak), "回覆不能帶 `{leak}`：{reply}");
        }
    }

    #[test]
    fn the_reply_is_only_the_answer_not_the_shell_banner_echo_tools_or_thinking() {
        let reply = screen_reply("agy", &pane(true), &[SENT.to_string()]).expect("a reply");
        assert_eq!(reply, "已完成：修好了 foo。\n測試通過。");
        assert_clean(&reply);
    }

    /// 回音捲出畫面（長回答）：還是不能把 shell 行與 banner（含帳號、方案）當回覆。
    #[test]
    fn without_a_visible_echo_the_banner_and_shell_lines_still_stay_out_of_the_reply() {
        let screen = pane(false);
        let reply = screen_reply("agy", &screen, &[SENT.to_string()]).expect("a reply");
        assert_clean(&reply);
        assert!(reply.contains("已完成：修好了 foo。"), "{reply}");
        let cleaned = clean_screen("agy", &screen).unwrap();
        assert_clean(&cleaned.replace("Thought for", "").replace("Read(foo.sh)", ""));
    }

    #[test]
    fn the_prompt_echo_and_the_empty_composer_are_recognised() {
        assert_eq!(last_prompt_echo_text("agy", &pane(true)).as_deref().map(|s| s.lines().next().unwrap()), Some("你是 agents-manager 的子 agent。請修好 foo"));
        assert!(pane_awaits_input("agy", &pane(true)), "輸入列只有 `>`＝等輸入");
        let working = pane(true).replace("\n>\n", "\n> typing…\n");
        assert!(!pane_awaits_input("agy", &working));
    }

    #[test]
    fn a_screen_with_nothing_but_chrome_has_no_reply() {
        let idle = format!("ubuntu@ubuntu:~$ agy\n\n      ▄▀▀▄        Antigravity CLI 1.2.16\n     ▀▀▀▀▀▀       me@example.com (Google AI Plus)\n\n{RULE}\n>\n{RULE}\n? for shortcuts   Gemini 3.8 Fl\n");
        assert_eq!(screen_reply("agy", &idle, &[]), None);
    }
}
