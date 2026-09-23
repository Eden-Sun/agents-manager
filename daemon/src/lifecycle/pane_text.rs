//! 送進 claude pane 之前先把 CLI 會改寫的字清掉（#205）。
//!
//! claude **2.1.277** 起，prompt 裡有隱形的格式字元時，CLI 按下 Enter 不送出：把它們拿掉、清過的字留在框裡，
//! 底下顯示 `Removed N invisible character(s) · review and press Enter to send`，要再按一次 Enter 才送。
//! daemon 打字之後照 transcript 裡**逐字相同**的 user entry 認送達，所以這兩件事都會壞：框裡還有字時
//! `confirm_submitted` 會再按一次 Enter（送出去的是清過的字），transcript 跟 `turns.prompt_text` 從此對不上，
//! 送達變成 `unknown`；stall 重送又拿同一段髒字再打一次。2.1.277 之前則是另一個方向：帶終端色碼（`ESC[`）的
//! prompt 會讓整顆 TUI 崩潰（同一版 changelog 的修正）。
//!
//! 所以 daemon 這一側先清成 CLI 會原樣收下的樣子（[`for_pane`]），`prompt_text` 存清過的字，使用者的泡泡留原文。
//!
//! **清理表照 CLI 的實際程式碼，不是猜的**：2.1.278 執行檔裡的 `uqr()` 只會拿掉 [`claude_strips`] 這一組
//! （原始碼裡叫 `jn()`），換行類（CR、VT、FF、NEL、U+2028／2029）換成 `\n`、CRLF 收成 LF。CLI 另外有一批
//! **依上下文保留**的例外：夾在兩個 emoji 之間的 ZWJ（👨‍👩‍👧）、接在 emoji／keycap 後面的 FE0E／FE0F、英格蘭／蘇格蘭／
//! 威爾斯旗的 tag 序列，以及印度系、阿拉伯系、泰寮高棉緬、蒙古等文字裡的 ZWJ／ZWNJ／ZWSP／FVS。這裡**一律拿掉**，
//! 不重做那套上下文判斷：多拿掉的只會讓 agent 看到拆開的 emoji、少了變體選擇符（泡泡是原文，使用者看不到差別），
//! CLI 對清過的字一個都不會再動；少拿掉的話 CLI 還會再清、又回到要 review 的畫面。要跟著改的時候，對照新版
//! 執行檔裡的 `jn`（`strings` 找 `review and press Enter to send`，同一個模組裡）。
//!
//! 萬一還是遇到 review 畫面（清理表跟不上新版、使用者自己在終端貼的），`delivery::confirm_submitted` 認得出來
//! （[`invisible_review_notice`]），不再按 Enter。

/// 2.1.278 的 `jn()`：CLI 會從 prompt 拿掉（或把換行類換成 `\n`）的字元。`\t`、`\n` 不在裡面。
pub(crate) fn claude_strips(c: char) -> bool {
    let e = c as u32;
    if e < 160 {
        return (e < 32 && e != 9 && e != 10) || e >= 127;
    }
    if e < 8192 {
        return matches!(e, 173 | 847 | 1564 | 4447 | 4448 | 6068 | 6069) || (6155..=6159).contains(&e);
    }
    if e < 65536 {
        return (8203..=8207).contains(&e)
            || (8232..=8238).contains(&e)
            || (8288..=8303).contains(&e)
            || e == 12644
            || (65024..=65039).contains(&e)
            || e == 65279
            || e == 65440
            || (65520..=65531).contains(&e);
    }
    e == 69759
        || (78896..=78911).contains(&e)
        || e == 94180
        || (113824..=113827).contains(&e)
        || (119155..=119162).contains(&e)
        || (917504..=921599).contains(&e)
}

/// 換行類：CLI 換成 `\n`（`mi()`）。
fn line_break(c: char) -> bool {
    matches!(c, '\u{0b}' | '\u{0c}' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// 要送進 `kind` 那種 pane 的字。只有 claude 會改寫 prompt；別的照原樣。
pub(crate) fn for_pane(kind: &str, text: &str) -> String {
    if kind == "claude" {
        clean_for_claude(text)
    } else {
        text.to_string()
    }
}

/// 原本有字、清完什麼都不剩（整段都是隱形字元或控制序列）：送一個空的 Enter 進去沒有意義，呼叫端回 400。
pub(crate) fn nothing_left(original: &str, cleaned: &str) -> bool {
    !original.trim().is_empty() && cleaned.trim().is_empty()
}

/// 終端控制序列整段拿掉（`ESC[31m` 不留下 `[31m`），換行類換成 `\n`，其餘 [`claude_strips`] 的字拿掉。
fn clean_for_claude(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            // C1 的 CSI／OSC（單一字元版的 `ESC[`／`ESC]`）。
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' => skip_string(&mut chars),
            '\r' if chars.peek() == Some(&'\n') => {}
            c if line_break(c) => out.push('\n'),
            c if claude_strips(c) => {}
            c => out.push(c),
        }
    }
    out
}

fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        // OSC、DCS、SOS、PM、APC：一直到 ST（`ESC \`、U+009C）或 BEL。
        Some(']' | 'P' | 'X' | '^' | '_') => skip_string(chars),
        // 兩個字元的跳脫（`ESC 7`、`ESC c`…）：那一個字元也不是給人看的。
        _ => {}
    }
}

/// CSI：參數與中間位元組一直到收尾那一個（0x40–0x7E）。
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
    }
}

fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(c) = chars.next() {
        match c {
            '\u{07}' | '\u{9c}' => break,
            '\u{1b}' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                break;
            }
            _ => {}
        }
    }
}

/// 畫面上是 claude 清掉隱形字元之後、等人 review 的提示（2.1.278 執行檔的 `wde()`：
/// `Removed 1 invisible character · review and press Enter to send`／`Removed N invisible characters · …`）。
/// 這時框裡是**清過的**字，再按 Enter 送出去的就不是我們記下的那一段。
pub(crate) fn invisible_review_notice(screen: &str) -> bool {
    screen.lines().any(|l| l.contains("invisible character") && l.contains("review and press Enter to send"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每一類都照 2.1.278 的 `jn()`：零寬、雙向控制、tag 字元、變體選擇符、BOM、C0／C1 控制字元都拿掉。
    #[test]
    fn every_class_the_cli_strips_is_removed() {
        for (why, input) in [
            ("零寬空白 U+200B", "a\u{200b}b"),
            ("ZWNJ U+200C", "a\u{200c}b"),
            ("LRM U+200E", "a\u{200e}b"),
            ("RLO U+202E", "a\u{202e}b"),
            ("word joiner U+2060", "a\u{2060}b"),
            ("BOM U+FEFF", "\u{feff}ab"),
            ("soft hyphen U+00AD", "a\u{ad}b"),
            ("tag 字元 U+E0041", "a\u{e0041}b"),
            ("C0 控制字元 BEL", "a\u{07}b"),
            ("C1 控制字元 U+0080", "a\u{80}b"),
            ("DEL", "a\u{7f}b"),
        ] {
            assert_eq!(for_pane("claude", input), "ab", "{why}");
        }
    }

    /// 終端色碼整段拿掉（2.1.277 之前會讓 TUI 崩潰）；其他 OSC／C1 CSI 也一樣。
    #[test]
    fn terminal_escape_sequences_are_removed_whole() {
        assert_eq!(for_pane("claude", "看\u{1b}[31m紅字\u{1b}[0m完"), "看紅字完");
        assert_eq!(for_pane("claude", "\u{1b}]0;title\u{07}內容"), "內容");
        assert_eq!(for_pane("claude", "\u{1b}]8;;https://x\u{1b}\\連結\u{1b}]8;;\u{1b}\\"), "連結");
        assert_eq!(for_pane("claude", "a\u{9b}1;2Hb"), "ab");
        assert_eq!(for_pane("claude", "尾巴\u{1b}"), "尾巴", "結尾落單的 ESC");
    }

    /// 換行類照 CLI 換成 `\n`；CRLF 收成一個 LF（CLI 也是，而且不算清理）。
    #[test]
    fn line_breaks_become_newlines() {
        assert_eq!(for_pane("claude", "a\r\nb\rc\u{0b}d\u{0c}e\u{85}f\u{2028}g\u{2029}h"), "a\nb\nc\nd\ne\nf\ng\nh");
    }

    /// `\n`、`\t`、中日韓文字、一般 emoji、全形標點都不動。
    #[test]
    fn ordinary_text_is_untouched() {
        for s in ["第一行\n\t第二行", "日本語とかな、한국어", "👍 🎉 🙂", "全形：「引號」、句號。", "plain ascii ~!@#$%^&*()"] {
            assert_eq!(for_pane("claude", s), s);
        }
    }

    /// CLI 依上下文**保留**的（emoji 之間的 ZWJ、emoji 後面的 FE0F、膚色修飾接在 ZWJ 序列裡），這裡一律拿掉：
    /// 清完的字 CLI 一個都不會再動（2.1.278 `uqr()`；只讀原始碼判斷，實機驗證留到升級時，見 SPEC §6.3）。
    /// 家庭 emoji 會被拆成三個人，❤️ 變成 ❤——agent 看到的字，泡泡照原文。
    #[test]
    fn what_the_cli_keeps_only_in_context_is_removed_too() {
        assert_eq!(for_pane("claude", "👨\u{200d}👩\u{200d}👧"), "👨👩👧");
        assert_eq!(for_pane("claude", "❤\u{fe0f}"), "❤");
        assert_eq!(for_pane("claude", "1\u{fe0f}\u{20e3}"), "1\u{20e3}", "keycap 的 FE0F 拿掉、U+20E3 不在表上");
        assert_eq!(for_pane("claude", "🏴\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}"), "🏴");
        assert_eq!(for_pane("claude", "👍🏽"), "👍🏽", "膚色修飾 U+1F3FD 不在表上");
    }

    /// 清過的字再清一次不會變（CLI 對它沒有東西可清）。
    #[test]
    fn cleaning_is_a_fixed_point() {
        let dirty = "a\u{200b}\u{1b}[1mb\u{1b}[0m\r\nc\u{feff}👨\u{200d}👩";
        let once = for_pane("claude", dirty);
        assert!(!once.chars().any(|c| claude_strips(c) && c != '\n' && c != '\t'), "{once:?}");
        assert_eq!(for_pane("claude", &once), once);
    }

    /// 只有 claude 會改寫 prompt：codex／grok 照原樣。
    #[test]
    fn other_kinds_are_left_alone() {
        let s = "a\u{200b}b\u{1b}[31mc";
        assert_eq!(for_pane("codex", s), s);
        assert_eq!(for_pane("grok", s), s);
    }

    #[test]
    fn a_prompt_that_is_all_invisible_leaves_nothing() {
        assert!(nothing_left("\u{200b}\u{feff}", &for_pane("claude", "\u{200b}\u{feff}")));
        assert!(!nothing_left("", ""), "本來就是空的：照舊，不歸這條管");
        assert!(!nothing_left("a\u{200b}", "a"));
    }

    #[test]
    fn the_review_notice_is_recognised() {
        assert!(invisible_review_notice("❯ 清過的字\n────\n  Removed 1 invisible character · review and press Enter to send\n"));
        assert!(invisible_review_notice("  Removed 3 invisible characters · review and press Enter to send"));
        assert!(!invisible_review_notice("  Removed 3 invisible characters · nothing left to send"), "框是空的：沒有東西會被誤送");
        assert!(!invisible_review_notice("⏺ 我會 review and press Enter to send 那個按鈕"), "少了 invisible character 不算");
    }
}

#[cfg(test)]
mod delivery_tests {
    //! 走真的 `prompt()`＋模擬 2.1.277 的 pane（`LivePane::strips_on_enter`）。實機（真的 claude ≥ 2.1.277）驗證留到升級時，
    //! 做法寫在 SPEC §6.3。
    use super::super::*;
    use super::claude_strips;
    use crate::testing as tt;

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        pane: String,
    }

    /// 一顆閒著、會回話的 claude pane：transcript 檔就是送達證據；`strips` 是這顆 CLI 按 Enter 時會清掉的字。
    async fn fixture(strips: fn(char) -> bool) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "clean-send").await;
        let transcript = env.dir.join(format!("session-{}.jsonl", db::ulid()));
        std::fs::write(&transcript, "").unwrap();
        let pane = format!("pane-{}", bot.id);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, native_session_id, transcript_path, started_at)
             VALUES (?,?,'running','idle',?,'test','sess-clean',?,?)",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .bind(&pane)
        .bind(transcript.to_str().unwrap())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        env.herdr.live_pane(&pane, tt::LivePane { width: Some(120), transcript_file: Some(transcript), strips_on_enter: Some(strips), ..Default::default() });
        Fixture { env, bot_id: bot.id, pane }
    }

    async fn stored(f: &Fixture, turn_id: &str) -> (String, String) {
        sqlx::query_as("SELECT t.prompt_text, m.content FROM turns t JOIN messages m ON m.turn_id = t.id AND m.role = 'user' WHERE t.id = ?")
            .bind(turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap()
    }

    /// 從網頁貼來的零寬空白＋從 log 貼來的色碼：打進 pane 的、`prompt_text` 都是清過的字，泡泡是原文，
    /// CLI 一按 Enter 就收下（沒有 review），transcript 裡逐字相同——送達證據成立。
    #[tokio::test]
    async fn a_prompt_with_invisible_characters_and_colour_codes_goes_in_clean_and_is_proven() {
        let f = fixture(claude_strips).await;
        let dirty = "看這段\u{200b}：\u{1b}[31m紅字\u{1b}[0m 完\u{feff}";
        let out = prompt(&f.env.app, &f.bot_id, dirty, "crid-clean").await.unwrap();
        assert_eq!(out.delivery, "ok", "{out:?}");
        assert_eq!(stored(&f, &out.turn_id).await, ("看這段：紅字 完".to_string(), dirty.to_string()), "prompt_text 清過、泡泡原文");
        let p = f.env.herdr.pane(&f.pane).unwrap();
        assert_eq!((p.transcript.as_slice(), p.notice.as_deref()), (&["❯ 看這段：紅字 完".to_string()][..], None), "CLI 沒有要 review");
    }

    /// 保險：CLI 還會清掉一個我們表上沒有的字（這裡假設下一版把 U+2800 盲文空白也當成隱形字元）——畫面出現 review 提示，
    /// 框裡是清過的字。不再按 Enter（再按送出去的就不是記下的那一段），送達記成不明，交給使用者看。
    #[tokio::test]
    async fn a_review_prompt_from_the_cli_is_not_pressed_through() {
        fn future_cli(c: char) -> bool {
            c == '\u{2800}' || claude_strips(c)
        }
        let f = fixture(future_cli).await;
        let out = prompt(&f.env.app, &f.bot_id, "盲文空白\u{2800}在這", "crid-review").await.unwrap();
        assert_ne!(out.delivery, "ok", "{out:?}");
        let p = f.env.herdr.pane(&f.pane).unwrap();
        assert!(p.transcript.is_empty(), "沒有被按出去：{:?}", p.transcript);
        assert_eq!(p.composer, vec!["盲文空白在這".to_string()], "清過的字還在框裡");
        assert!(p.notice.as_deref().is_some_and(|n| n.contains("review and press Enter to send")));
    }

    /// 另外兩條存 `prompt_text` 的路也存清過的字：對方回合中時 AGM 的派工排進佇列（`queue_for_next_turn`，flush 與 stall
    /// 重送都讀它），以及 bot 沒在跑時送出、等它起來（`start_send`）。泡泡照樣是原文。
    #[tokio::test]
    async fn the_queued_and_the_waiting_for_start_paths_store_the_clean_text_too() {
        let f = fixture(claude_strips).await;
        let app = f.env.app.clone();
        // 對方回合中：佔住一個在飛的回合，AGM 的派工才會排進佇列。
        let run: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=?").bind(&f.bot_id).fetch_one(&app.db).await.unwrap();
        let conv = db::conversation_id(&app.db, &f.bot_id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let dirty = "派工\u{200b}內容\u{1b}[1m";
        let queued = prompt_relayed_queueable(&app, &f.bot_id, dirty, "agm-clean", Some(crate::agent_relay::DAEMON_SENDER)).await.unwrap();
        assert_eq!(queued.delivery, "queued");
        assert_eq!(stored(&f, &queued.turn_id).await, ("派工內容".to_string(), dirty.to_string()));

        let idle = tt::claude_bot(&app, &f.env.project_id, "not-running").await;
        let waiting = super::super::start_send::prompt_starting(&app, &idle.id, dirty, "crid-waiting", &[], super::super::RelaySrc::default()).await.unwrap();
        assert_eq!(stored(&f, &waiting.turn_id).await, ("派工內容".to_string(), dirty.to_string()));
    }

    /// 整段都是隱形字元：清完什麼都不剩，回 400，不打一個空的 Enter 進去。
    #[tokio::test]
    async fn a_prompt_that_is_all_invisible_is_refused() {
        let f = fixture(claude_strips).await;
        let err = prompt(&f.env.app, &f.bot_id, "\u{200b}\u{feff}", "crid-empty").await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "{err:?}");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_text"), "一個字都沒打");
    }
}
