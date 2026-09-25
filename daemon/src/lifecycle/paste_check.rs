//! 長段貼上（#382）：herdr 0.9.1 的 `pane.send_text` 一次超過約 1024 位元組時，**最前面的 1024 位元組會不見**，
//! 只有後面的進到 claude 的框裡（2026-09-21 實測：1048 B 只剩最後 4 行、1804 B 只剩最後 46 行）。
//! 第一層在 `HerdrClient::pane_send_text` 把它拆成小段送；這裡是第二層——按 Enter 之前再看一次框，
//! 開頭對不上就不送，不能讓 agent 收到半段。
//!
//! 但「看不到開頭」不等於缺頭（#403）：claude 不一定把分段貼上摺起來，框長到上限就只畫最後幾列。
//! 所以缺開頭時還要看框滿了沒有——滿框而且看得到的是尾段＝只是框太矮，照常按 Enter。

use super::poller::{is_rule_row, undecorate_row};

/// 開頭拿來比對的字數與下限：跟 `poller::composer_holds_prompt` 同一套。
const HEAD: usize = 12;
const HEAD_MIN: usize = 4;

/// claude 2.1.280 的輸入框最多畫幾列：終端列數的一半減 5（#403，2026-09-23 真 claude 實測
/// 20/24/26/30/40/50/55 列 → 5/7/8/10/15/20/22 列）。超過就捲動，只畫最後幾列、游標在尾端，上緣沒有任何捲動記號。
pub(crate) fn claude_box_max_rows(viewport_rows: usize) -> usize {
    (viewport_rows / 2).saturating_sub(5)
}

/// 框裡缺了開頭時的樣子：框有幾列、框裡的字是不是送出文字的**連續尾段**。
pub(crate) struct LostHead {
    rows: usize,
    tail_of_sent: bool,
}

impl LostHead {
    /// 缺開頭算不算 #382：框被高度截斷（滿框）而且看得到的是尾段，只是框太矮——交給 Enter 之後的 transcript 證據；
    /// 框沒滿卻缺開頭才是 #382。畫面列數讀不到（`None`）就不能斷定框滿了，照 #382 處理：清框、之後重試，
    /// 寧可晚一輪也不送出半段。
    pub(crate) fn truncated(&self, viewport_rows: Option<usize>) -> bool {
        let clipped = viewport_rows.is_some_and(|v| self.rows >= claude_box_max_rows(v));
        !(self.tail_of_sent && clipped)
    }
}

/// 貼完、按 Enter 之前：claude 的框裡**看不到送出文字的開頭**時回 `Some`，由 [`LostHead::truncated`] 配上終端列數下結論。
/// 看不出來（框裡有摺起來的 `[Pasted text …]`、找不到框、字太短）一律 `None`——那些情形交給 transcript 證據。
///
/// 框從最下面那個 `❯` 讀到下緣分隔線，**空白列也算**（部署交辦那種 prompt 有空行，`poller::composer_text` 碰到空行就停）；
/// 往上找 `tail` 列——剛貼的字讓框多高就找多高，不靠「`❯` 落在 24 列外就當空框」的巧合（#403）。
pub(crate) fn lost_head(kind: &str, screen: &str, sent: &str, tail: usize) -> Option<LostHead> {
    if kind != "claude" {
        return None;
    }
    let rows = box_rows(kind, screen, tail)?;
    let box_text = rows.join("\n");
    if box_text.contains("[Pasted text") {
        return None;
    }
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let (in_box, sent) = (squash(&box_text), squash(sent));
    let needle: String = sent.chars().take(HEAD).collect();
    if needle.chars().count() < HEAD_MIN || in_box.contains(&needle) {
        return None;
    }
    Some(LostHead { rows: rows.len(), tail_of_sent: !in_box.is_empty() && sent.ends_with(&in_box) })
}

/// 框裡每一列（去掉 `❯` 與縮排），從 `❯` 那列到下緣分隔線，尾端的空白列不算。
///
/// 框頂的 `❯` 畫在第 0 欄；貼進去的每一列（含換列）claude 都縮兩格。所以只認頂格的 `❯`——
/// 貼的內容自己就有 `❯ ` 開頭的列（#562：child-blocked 通知夾著子 agent 的畫面原文）時，
/// 不能把那一列當成框頂，否則整段明明都在框裡，卻判成「看不到開頭」而清框重試到天荒地老。
fn box_rows(kind: &str, screen: &str, tail: usize) -> Option<Vec<String>> {
    let plain = super::delivery::plain_without_hints(kind, screen);
    let marker = super::screen::prompt_echo_prefix(kind)?.trim_end();
    let lines: Vec<&str> = plain.lines().collect();
    let from = lines.len().saturating_sub(tail);
    let idx = from + lines[from..].iter().rposition(|l| l.starts_with(marker))?;
    let mut rows: Vec<String> = Vec::new();
    for (n, line) in lines[idx..].iter().enumerate() {
        let row = undecorate_row(line);
        if n > 0 && is_rule_row(&row) {
            break;
        }
        rows.push(if n == 0 { row[marker.len()..].trim().to_string() } else { row });
    }
    while rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    (!rows.is_empty()).then_some(rows)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{claude_box_max_rows, lost_head};
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
        let transcript = env.dir.join(format!("session-{}.jsonl", db::ulid()));
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
        let text = include_str!("fixtures/claude-2.1.280-paste-rows26-box8-7k.sent");
        assert!(text.chars().count() > 7000);
        let out = prompt(&f.env.app, &f.bot_id, text, "crid-short-box").await.unwrap();
        assert!(matches!(out.delivery.as_str(), "ok"), "{out:?}");
        assert_eq!(received(&f), vec![text.to_string()]);
    }

    #[test]
    fn only_a_visible_lost_head_counts_as_truncated() {
        let sent = "我了一堆給你才給我兩張\n20260813120139980\n20260813130350458";
        let screen = |rows: &[&str]| format!("⏺ 好\n\n─────\n❯\u{a0}{}\n─────\n", rows.join("\n  "));
        assert!(truncated("claude", &screen(&["0139980", "20260813130350458"]), sent, Some(40)), "開頭不見、框沒滿");
        assert!(!truncated("claude", &screen(&["我了一堆給你才給我兩張", "20260813120139980", "20260813130350458"]), sent, Some(40)), "整段都在");
        assert!(!truncated("claude", &screen(&["[Pasted text #1 +2 lines]"]), sent, Some(40)), "摺起來看不到字");
        assert!(!truncated("claude", &screen(&["ab"]), "ab", Some(40)), "太短不判");
        assert!(!truncated("codex", &screen(&["0139980"]), sent, Some(40)), "只管 claude");
        assert!(!truncated("claude", "⏺ 好\n", sent, Some(40)), "找不到框");
        assert!(truncated("claude", &screen(&["別的東西別的東西", "20260813130350458"]), sent, Some(40)), "不是尾段：看得到的字跟送出的對不上");
    }

    /// 真 claude 2.1.280 的框高上限（#403，2026-09-23 獨立 herdr session 實測）。
    #[test]
    fn the_box_height_limit_matches_real_claude() {
        for (rows, max) in [(20, 5), (24, 7), (26, 8), (30, 10), (40, 15), (50, 20), (55, 22)] {
            assert_eq!(claude_box_max_rows(rows), max, "{rows} 列");
        }
    }

    /// 真畫面（`format: ansi`、`recent_unwrapped`，跟 `read_composer` 同一種讀法）：herdr 0.9.1 分段貼進真 claude 2.1.280，
    /// 沒摺起來、框長到上限、只剩最後幾列。這些都是**完整的**貼上，只是框太矮。
    macro_rules! real {
        ($name:literal) => {
            (
                include_str!(concat!("fixtures/claude-2.1.280-paste-", $name, ".ansi")),
                include_str!(concat!("fixtures/claude-2.1.280-paste-", $name, ".sent")),
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

    /// 真的 #382：一次 `pane.send_text` 1097 B，herdr 丟掉前 1024 B，框只剩 6 列、沒滿（55 列的畫面上限是 22 列）。
    #[test]
    fn a_box_that_is_not_full_and_lost_its_head_is_truncated() {
        let (screen, sent) = real!("rows55-lost-head");
        assert!(truncated("claude", screen, sent, Some(55)));
        // 同一個 8 行框，畫面其實有 55 列（上限 22）：框沒滿還看不到開頭＝真的缺頭。
        let (screen, sent) = real!("rows26-box8-46lines");
        assert!(truncated("claude", screen, sent, Some(55)));
    }

    /// #562：daemon 的 child-blocked 通知夾著子 agent 的畫面原文，裡面有 `❯ ` 開頭的列。真 claude 2.1.281
    /// 把整段都收進框（55 列的畫面），那兩列縮兩格畫在框裡——不是框頂，不能判成缺頭。
    #[test]
    fn a_pasted_row_starting_with_the_prompt_mark_is_not_the_box_top() {
        let (screen, sent) = (
            include_str!("fixtures/claude-2.1.281-paste-child-blocked-notice.ansi"),
            include_str!("fixtures/claude-2.1.281-paste-child-blocked-notice.sent"),
        );
        assert!(sent.lines().filter(|l| l.starts_with("❯ ")).count() >= 2, "fixture 要真的夾著 `❯ ` 開頭的列");
        assert!(lost_head("claude", screen, sent, 400).is_none(), "開頭就在框頂");
        assert!(!truncated("claude", screen, sent, Some(55)));
    }

    /// 同一則通知走完整的送出路：以前每次都 `paste_truncated`、清框、放回佇列；現在整段送達、只送一次。
    #[tokio::test]
    async fn a_notice_quoting_a_child_screen_reaches_the_agent() {
        let f = fixture().await;
        let text = include_str!("fixtures/claude-2.1.281-paste-child-blocked-notice.sent");
        let out = prompt(&f.env.app, &f.bot_id, text, "crid-child-blocked").await.unwrap();
        assert!(matches!(out.delivery.as_str(), "ok"), "{out:?}");
        assert_eq!(received(&f), vec![text.to_string()]);
    }

    /// 滿框但看得到的字不是送出文字的尾段（例如使用者在框裡另外打的字）：照樣算缺頭，不按 Enter。
    #[test]
    fn a_full_box_that_is_not_our_tail_is_truncated() {
        let (screen, sent) = real!("rows26-box8-46lines");
        let other: String = sent.lines().take(40).collect::<Vec<_>>().join("\n");
        assert!(truncated("claude", screen, &other, Some(26)));
    }
}
