#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::*;
    use crate::lifecycle::paste_check::{claude_box_max_rows, lost_head};
    use crate::testing as tt;
    struct Fixture {
        env: tt::Env,
        bot_id: String,
        pane: String,
        transcript: std::path::PathBuf,
    }
    /// 一顆閒著的 claude pane：`pane.send_text` 一次超過 1024 B 就丟掉最前面的 1024 B（實測的行為）。
    async fn fixture() -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "long-paste").await;
        let transcript = tt::trusted_transcript(&app, &bot.id, &env.dir, &format!("session-{}.jsonl", db::ulid())).await;
        std::fs::write(&transcript, "").unwrap();
        let pane = format!("pane-{}", bot.id);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, native_session_id, transcript_path, started_at)
             VALUES (?,?,'running','idle',?,'test','sess-long',?,?)",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .bind(&pane)
        .bind(transcript.to_str().unwrap())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        env.herdr.live_pane(
            &pane,
            tt::LivePane { width: Some(120), transcript_file: Some(transcript.clone()), send_text_drops_head_over: Some(1024), ..Default::default() },
        );
        Fixture { env, bot_id: bot.id, pane, transcript }
    }
    /// 使用者截圖那一種：58 行、每行 17 位數，共約 1 KB，最後一行是一句話。
    fn long_paste() -> String {
        let mut lines: Vec<String> = vec!["我了一堆給你才給我兩張".into()];
        lines.extend((0..58).map(|i| format!("2026081{}0{:08}", i % 10, 10_000_000 + i * 7919)));
        lines.push(String::new());
        lines.push("以上用換行號".into());
        lines.join("\n")
    }
    fn received(f: &Fixture) -> Vec<String> {
        std::fs::read_to_string(&f.transcript)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v["message"]["content"].as_str().map(str::to_string))
            .collect()
    }
    /// 前後對照：以前 claude 只收到最後幾行、泡泡卻是整段；現在收到的就是使用者送出的那一段，而且只有一則。
    #[tokio::test]
    async fn a_long_multi_line_prompt_reaches_the_agent_whole_and_once() {
        let f = fixture().await;
        let text = long_paste();
        assert!(text.len() > 1024, "要超過 herdr 會丟前段的門檻");
        let out = prompt(&f.env.app, &f.bot_id, &text, "crid-long").await.unwrap();
        assert!(matches!(out.delivery.as_str(), "ok"), "{out:?}");
        assert_eq!(received(&f), vec![text], "agent 收到的必須逐字等於送出的（pane {})", f.pane);
    }
    /// `POST /api/bots/:id/text`（聊天輸入框的「併送」、終端分頁貼上）走同一條 `pane.send_text`，同樣要整段進去。
    #[tokio::test]
    async fn typing_alongside_a_long_paste_does_not_lose_its_head() {
        let f = fixture().await;
        let text = long_paste();
        send_text(&f.env.app, &f.bot_id, &text, true, None).await.unwrap();
        assert_eq!(received(&f), vec![text]);
    }
    /// 第二層：就算下層以外的原因（別的版本、別的 pane）又讓開頭掉了，框裡看得出來缺頭就不按 Enter、清框，
    /// 回「沒送出」——不能讓 agent 收到半段、泡泡卻寫已送出。
    #[tokio::test]
    async fn a_box_that_lost_the_head_is_cleared_and_never_submitted() {
        let f = fixture().await;
        // 每一段都掉前 100 B：拆小段也擋不住，只剩框裡的檢查（這一段短，框不高，看得到缺頭；使用者那次也是只剩 4 列）。
        f.env.herdr.live_pane(
            &f.pane,
            tt::LivePane { width: Some(120), transcript_file: Some(f.transcript.clone()), send_text_drops_head_over: Some(100), ..Default::default() },
        );
        let text = long_paste().lines().take(20).collect::<Vec<_>>().join("\n");
        let err = prompt(&f.env.app, &f.bot_id, &text, "crid-lost").await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)) && format!("{err:?}").contains("paste_truncated"), "{err:?}");
        assert!(received(&f).is_empty(), "半段沒有被送出去：{:?}", received(&f));
        let p = f.env.herdr.pane(&f.pane).unwrap();
        assert!(p.composer.is_empty(), "框已清空：{:?}", p.composer);
    }
    /// 判斷本身（`type_text` 那兩步合起來）：`rows` 是 `pane.get` 回的畫面列數。
    fn truncated(kind: &str, screen: &str, sent: &str, rows: Option<usize>) -> bool {
        lost_head(kind, screen, sent, 400).is_some_and(|l| l.truncated(rows))
    }
    /// #403：矮框（26 列的畫面＝8 列的框）收約 7 千字的部署交辦。框只畫得出最後 8 列，開頭永遠不在框裡——
    /// 這是框太矮，不是缺頭：要按 Enter、整段送達，不能清框重試到 blocked。
    #[tokio::test]
    async fn a_long_prompt_in_a_short_box_is_sent_whole() {
        let f = fixture().await;
        f.env.herdr.live_pane(
            &f.pane,
            tt::LivePane {
                width: Some(120),
                transcript_file: Some(f.transcript.clone()),
                send_text_drops_head_over: Some(1024),
                rows: Some(26),
                ..Default::default()
            },
        );
        let text = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.280-paste-rows26-box8-7k.sent");
        assert!(text.chars().count() > 7000);
        let out = prompt(&f.env.app, &f.bot_id, text, "crid-short-box").await.unwrap();
        assert!(matches!(out.delivery.as_str(), "ok"), "{out:?}");
        assert_eq!(received(&f), vec![text.to_string()]);
    }
    /// 真畫面（`format: ansi`、`recent_unwrapped`，跟 `read_composer` 同一種讀法）：herdr 0.9.1 分段貼進真 claude 2.1.280，
    /// 沒摺起來、框長到上限、只剩最後幾列。這些都是**完整的**貼上，只是框太矮。
    macro_rules! real {
        ($name:literal) => {
            (
                include_str!(concat!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.280-paste-", $name, ".ansi")),
                include_str!(concat!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.280-paste-", $name, ".sent")),
            )
        };
    }
    #[test]
    fn a_full_box_showing_the_tail_is_not_a_lost_head() {
        for (name, (screen, sent), rows) in [
            ("8 行框＋46 行（browser-gc 那顆）", real!("rows26-box8-46lines"), 26),
            ("20 行框＋46 行（build 那顆）", real!("rows50-box20-46lines"), 50),
            ("8 行框＋約 7 千字、有空行的部署交辦", real!("rows26-box8-7k"), 26),
            ("8 行框＋會換列的長行", real!("rows26-box8-wrapped"), 26),
        ] {
            assert!(lost_head("claude", screen, sent, 400).is_some(), "{name}：開頭確實不在框裡（不然這個 fixture 測不到什麼）");
            assert!(!truncated("claude", screen, sent, Some(rows)), "{name}：滿框的尾段要判完整");
            assert!(truncated("claude", screen, sent, None), "{name}：列數讀不到就不能斷定框滿了，寧可重試");
        }
    }
    /// #562：daemon 的 child-blocked 通知夾著子 agent 的畫面原文，裡面有 `❯ ` 開頭的列。真 claude 2.1.281
    /// 把整段都收進框（55 列的畫面），那兩列縮兩格畫在框裡——不是框頂，不能判成缺頭。
    #[test]
    fn a_pasted_row_starting_with_the_prompt_mark_is_not_the_box_top() {
        let (screen, sent) = (
            include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-paste-child-blocked-notice.ansi"),
            include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-paste-child-blocked-notice.sent"),
        );
        assert!(sent.lines().filter(|l| l.starts_with("❯ ")).count() >= 2, "fixture 要真的夾著 `❯ ` 開頭的列");
        assert!(lost_head("claude", screen, sent, 400).is_none(), "開頭就在框頂");
        assert!(!truncated("claude", screen, sent, Some(55)));
    }
    /// 同一則通知走完整的送出路：以前每次都 `paste_truncated`、清框、放回佇列；現在整段送達、只送一次。
    #[tokio::test]
    async fn a_notice_quoting_a_child_screen_reaches_the_agent() {
        let f = fixture().await;
        let text = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-paste-child-blocked-notice.sent");
        let out = prompt(&f.env.app, &f.bot_id, text, "crid-child-blocked").await.unwrap();
        assert!(matches!(out.delivery.as_str(), "ok"), "{out:?}");
        assert_eq!(received(&f), vec![text.to_string()]);
    }
}
