#!/usr/bin/env python3
"""B：畫面第二意見的題目。用法：build_cases_b.py <repo 根> > cases_b.jsonl
來源 real＝repo 的 fixture／測試常數原文；synth＝照 #227／#237／#114 描述的情境，用 repo 內容拼出來的反例。不含任何真實 bot 畫面。"""
import json, sys
FX = sys.argv[1] + "/daemon/src/lifecycle/fixtures/"
fx = lambda n: open(FX + n).read()
IDLE_CLAUDE = "────────────────────\n❯\n────────────────────\n  15m2dg | agents-manager | Opus 5 31% | 5h:96%\n  ⏵⏵ bypass permissions on\n"
IDLE_GROK = "\n  ┃ \n  ┃  Type a message…\n\n  grok-build · ~/project/agents-manager\n"
IDLE_CODEX = "\n\n› Ask Codex to do anything\n\n  gpt-5.6-luna xhigh · ~/project/agents-manager\n"
CODEX_HEAD = fx("codex-0.155-finished.txt").split("› Reply with exactly")[0]
CODEX_BANNER = "■ You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 21st, 2026 1:23 PM."
KINDS = {
 "working": "The agent is in the middle of a turn: spinner, running tool, streaming output",
 "finished_idle": "The agent finished (or has nothing to do) and an empty input box is waiting for the next prompt; earlier output may quote anything",
 "usage_limit": "The agent's own interface says the account's usage, credit or rate limit is exhausted",
 "confirmation_dialog": "A modal drawn by the interface is waiting for a key: permission to run something, trust this directory, switch model or effort",
 "login_required": "The interface is asking the user to pick a login method or sign in",
 "feedback_survey": "The interface is asking the user to rate the session",
}
def q(needle_desc):
    return {
     "is_live_ui": {"type": "noul",
       "instructions": {"question": "Is `needle` on this screen a live element drawn by the agent program's own interface that currently applies to the user?",
                        "focus": "Tell interface elements apart from content that merely contains the same words: source code, a diff, grep or cat output, test failure messages, an issue body, or the agent's own prose quoting the text. " + needle_desc},
       "criteria": {"true": "The interface itself is showing this notice, menu or dialog right now", "false": "The words appear inside printed or quoted content; the interface is not in that state"}},
     "screen_kind": {"type": "choice", "instructions": "What state is the terminal in right now? Judge by the bottom of `screen`.", "criteria": KINDS}}
cases = []
def add(i, agent, needle, screen, live, kind, src, desc="`needle` is a usage-limit notice."):
    cases.append({"id": i, "state": {"agent": agent, "needle": needle, "screen": screen[-6000:]}, "questions": q(desc),
                  "labels": {"is_live_ui": live, "screen_kind": kind}, "meta": {"source": src, "live": live}})
D = "`needle` is a dialog title."
g = fx("grok_limit_hit.txt")
add("grok-menu-real", "grok", "You hit your weekly limit.", g, True, "usage_limit", "real")
add("grok-402-real", "grok", "Turn failed: Request failed (402): Grok Build usage balance exhausted", g.split("Help improve Grok")[0] + IDLE_GROK, True, "usage_limit", "real")
add("codex-banner-real", "codex", CODEX_BANNER, CODEX_HEAD + "› 繼續修 #237\n\n" + CODEX_BANNER + IDLE_CODEX, True, "usage_limit", "real")
add("codex-banner-wrapped", "codex", "You've hit your usage limit.", CODEX_HEAD + "› continue\n\n■ You've hit your usage limit. Upgrade to Pro\n  (https://chatgpt.com/explore/pro), visit\n  https://chatgpt.com/codex/settings/usage to purchase\n  more credits or try again at 3:22 AM." + IDLE_CODEX, True, "usage_limit", "real")
add("claude-limit-real", "claude", "You've hit your session limit", "⏺ Bash(cargo test -p agents-managerd screen)\n  ⎿  test result: ok. 41 passed\n\n  ⎿  You've hit your session limit · resets 3pm (Asia/Taipei)\n     /upgrade to increase your usage limit.\n\n" + IDLE_CLAUDE, True, "usage_limit", "synth")
add("grok-rg-output", "grok", "you hit your weekly limit", "  ◆ Run: rg -n \"weekly limit\" daemon/src\n    daemon/src/lifecycle/screen.rs:458:    low.contains(\"you hit your weekly limit\") || low.contains(\"you've hit your weekly limit\")\n    daemon/src/lifecycle/screen.rs:1636:        assert!(lines.iter().any(|l| l.contains(\"You hit your weekly limit\")), \"{lines:?}\");\n    daemon/src/lifecycle/fixtures/grok_limit_hit.txt:20:  ┃  You hit your weekly limit.\n\n  判斷式在 screen.rs:458，只看整行含有。我接著改成要有選單佐證。\n" + IDLE_GROK, False, "finished_idle", "synth")
add("grok-diff", "grok", "you hit your weekly limit", "  ◆ Run: git diff daemon/src/lifecycle/screen.rs\n    @@ -455,7 +455,9 @@ pub(crate) fn grok_limit_hit_line(line: &str) -> Option<String> {\n    -    if low.contains(\"usage balance exhausted\") || low.contains(\"you hit your weekly limit\") {\n    +    if is_balance_line(&low) || is_limit_title(&low) {\n         return Some(body.to_string());\n\n  ⠋ Thinking… (12s)\n", False, "working", "synth")
add("grok-cat-fixture", "grok", "You hit your weekly limit.", "  ◆ Run: cat daemon/src/lifecycle/fixtures/grok_limit_hit.txt\n" + "\n".join("    " + l for l in g.splitlines()[-16:]) + "\n\n  fixture 的選單在檔尾，三個選項都在。接下來補測試。\n" + IDLE_GROK, False, "finished_idle", "synth")
add("grok-test-fail", "grok", "You hit your weekly limit.", "  ◆ Task failed in 16s: cargo test grok (exit 1)\n    ---- lifecycle::screen::extract_tests::grok_source_lines_are_not_limit_hits stdout ----\n    thread panicked at daemon/src/lifecycle/screen.rs:1671:\n    source line read as a limit hit: \"┃  You hit your weekly limit.\"\n    test result: FAILED. 40 passed; 1 failed\n\n  ⠙ Working… (48s)\n", False, "working", "synth")
add("grok-issue-body", "grok", "You hit your weekly limit.", "  ◆ Run: gh issue view 222\n    grok 撞每週額度的選單 daemon 沒認出來\n    ```\n    ┃  You hit your weekly limit.\n    ┃\n    ┃  1 (○) Upgrade tier      Upgrade to a higher tier for more usage\n    ┃  2 (○) Buy more credits  Purchase credits to keep using Grok Build\n    ┃  3 (○) Try Again         Resubmit the last prompt once you have usage again\n    ```\n    daemon 沒認出這是額度見底\n\n  讀完了，這張要讓 daemon 認得這個選單。\n" + IDLE_GROK, False, "finished_idle", "synth")
add("codex-reads-source", "codex", "You've hit your usage limit.", CODEX_HEAD + "› 看一下 screen.rs 的撞限測試\n\n• Ran sed -n '1099,1104p' daemon/src/lifecycle/screen.rs\n  └     const CODEX_LIMIT: &str = \"\\\n    ERROR: You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), \\\n    visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at 3:22 AM.\";\n\n• 測試常數在 1101 行，是整行含有的比對。\n\n  done Sep 19 at 5:12 PM\n" + IDLE_CODEX, False, "finished_idle", "synth")
add("codex-diff", "codex", "hit your usage limit", CODEX_HEAD + "• Ran git diff\n  └ -        if low.contains(\"hit your usage limit\") {\n    +        if banner_is_last_cell(lines, i) && low.contains(\"hit your usage limit\") {\n\n• Working (31s • esc to interrupt)\n", False, "working", "synth")
add("codex-panic", "codex", "ERROR: You've hit your usage limit, or try again at 10:15 PM.", CODEX_HEAD + "• Ran cargo test limit_banner\n  └ thread 'lifecycle::limit_banner::tests::sighting' panicked:\n    assertion `left == right` failed: \"ERROR: You've hit your usage limit, or try again at 10:15 PM.\"\n    test result: FAILED. 11 passed; 1 failed\n\n• 測試紅了一條，是我改的 sighting 把舊橫幅當新的。我修一下。\n\n  done Sep 19 at 5:20 PM\n" + IDLE_CODEX, False, "finished_idle", "synth")
add("codex-prose", "codex", "You've hit your usage limit", CODEX_HEAD + "› why did the turn fail?\n\n• The daemon matched the phrase \"You've hit your usage limit\" inside a diff I printed, so it closed the turn as a limit hit. The account itself still has headroom.\n\n  done Sep 19 at 5:31 PM\n" + IDLE_CODEX, False, "finished_idle", "synth")
add("claude-prose-zh", "claude", "You hit your weekly limit.", "⏺ #222 的根因：grok 的選單標題是「You hit your weekly limit.」，daemon 只認 402 那一行，所以這個畫面被當成一般 blocked。\n  - 改動：screen.rs 加 is_limit_title。\n  - 驗證：cargo test screen → 43 passed。\n" + IDLE_CLAUDE, False, "finished_idle", "synth")
# #114 的真假對話框，原文取自 tui_prompts.rs 的測試
TRUST = "  main ~/p/h/projects/rt\n\n⠀⠀⠀⠀⠀⠀⣀⣀⡀\nDo you trust the contents of this directory?\n                /Users/m4p/project/hermes-agents/projects/rt\n\nGrok Build may run or modify contents in this directory,\n              posing security risks.\n\nYes, proceed                 y\n                  No, quit                     n\n\nGrok Build  1.0.34 [stable]\n"
add("trust-real", "grok", "Do you trust the contents of this directory?", TRUST, True, "confirmation_dialog", "real", D)
add("trust-quoted", "grok", "Do you trust the contents of this directory?", "⏺ grok 的 trust 對話框長這樣，三段字缺一不可：\n  Do you trust the contents of this directory?\n  Yes, proceed\n  No, quit\n  我已經在 pane_ready_for_prompt 補上判斷，關掉之後再送 prompt。\n────────────────────\n❯\n────────────────────\n  15m2dg | agents-manager | grok | 5h:96%\n", False, "finished_idle", "real", D)
SWITCH = "  ⎿  Interrupted · What should Claude do instead?\n▔▔▔▔▔▔▔▔▔▔\n   Switch model?\n   Your next response will be slower and use more tokens\n   This conversation is cached for the current model. Switching to Haiku 4.5 means the full history gets re-read on your next message.\n   ❯ 1. Yes, switch to Haiku 4.5\n     2. No, go back\n"
EFFORT = "  ✻ Cooked for 2m 16s · done 08:17\n▔▔▔▔▔▔▔▔▔▔\n   Change effort level?\n   Your next response will be slower and use more tokens\n\nThis conversation is cached for the current effort level. Switching to low means the full history gets re-read on your next message.\n\n❯ 1. Yes, switch to low\n  2. No, go back\n"
add("switch-real", "claude", "Switch model?", SWITCH, True, "confirmation_dialog", "real", D)
add("effort-real", "claude", "Change effort level?", EFFORT, True, "confirmation_dialog", "real", D)
add("effort-scrolled", "claude", "Change effort level?", EFFORT + "⏺ Bash(cargo test)\n  ⎿  ok\n" * 8 + "─────\n❯\n─────\n  tony. | agents-manager | Opus 5 | 5h:96%\n", False, "finished_idle", "real", D)
add("effort-quoted-report", "claude", "Change effort level?", "  ⏺ 原因：claude 2.1.x 的 /effort 跳的是跟 /model 同一種確認框，但標題是「Change effort level?」。\n  - 改動：daemon/src/tui_prompts.rs 的偵測改成 \"switch model?\" || \"change effort level?\"，另兩個條件（yes, switch to / no, go back）不變；加了一條用截圖真畫面的測試。\n  - 驗證：cargo test -p agents-managerd tui_prompts → 8 passed 0 failed。\n" + IDLE_CLAUDE, False, "finished_idle", "real", D)
add("permission-real", "claude", "Do you want to proceed?", "\n ⏺ Bash(rm -rf ./target/debug)\n ╭──────────────────────────────────────╮\n │  Do you want to proceed?             │\n │  ❯ 1. Yes                            │\n │    2. No, and tell Claude what to do │\n ╰──────────────────────────────────────╯\n", True, "confirmation_dialog", "real", D)
add("login-real", "claude", "Select login method:", "Welcome to Claude Code v2.1.263\n\nSelect login\n method:\n\n❯ 1. Claude account with subscription · Pro, Max\n   2. Anthropic Console account · API usage billing\n", True, "login_required", "real", D)
add("login-quoted", "claude", "select login method", "⏺ the user asked about 'select login method' in the docs; it is the first screen of `claude auth login`.\n" + IDLE_CLAUDE, False, "finished_idle", "real", D)
add("survey-real", "claude", "How is Claude doing this session?", "\n ● How is Claude doing this session? (optional)\n   1: Bad    2: Fine   3: Good   0: Dismiss\n\n > │\n", True, "feedback_survey", "real", D)
add("survey-wrapped", "claude", "How is Claude doing this session?", "\n│ ● How is Claude doing   │\n│ this session?           │\n│ (optional)              │\n│   1: Bad    2: Fine     │\n│   3: Good   0: Dismiss  │\n│ ╭──────────────────────╮ │\n│ │ >                    │ │\n│ ╰──────────────────────╯ │\n│ claude | model | 42%      │\n│ ⏵⏵ bypass permissions on │\n", True, "feedback_survey", "real", D)
# 只問 screen_kind 的真 fixture（needle 給頁尾，is_live_ui 不計分）
for name, kind in (("codex-0.155-finished.txt", "finished_idle"), ("codex-0.155-idle.txt", "finished_idle"), ("codex-0.155-working.txt", "working"), ("codex-0.155-working-summary.txt", "working")):
    s = fx(name)
    c = {"id": name, "state": {"agent": "codex", "needle": "", "screen": s}, "questions": {"screen_kind": q("")["screen_kind"]}, "labels": {"screen_kind": kind}, "meta": {"source": "real"}}
    cases.append(c)
for c in cases:
    print(json.dumps(c, ensure_ascii=False))
