//! 長段貼上（#382）：herdr 0.9.1 的 `pane.send_text` 一次超過約 1024 位元組時，**最前面的 1024 位元組會不見**，
//! 只有後面的進到 claude 的框裡（2026-09-21 實測：1048 B 只剩最後 4 行、1804 B 只剩最後 46 行）。
//! 第一層在 `HerdrClient::pane_send_text` 把它拆成小段送；這裡是第二層——按 Enter 之前再看一次框，
//! 開頭對不上就不送，不能讓 agent 收到半段。

use super::poller::composer_text;

/// 開頭拿來比對的字數與下限：跟 `poller::composer_holds_prompt` 同一套。
const HEAD: usize = 12;
const HEAD_MIN: usize = 4;

/// 貼完、按 Enter 之前：claude 的框裡是不是**看得出來缺了開頭**。只有確定不對才回 `true`；看不出來（框裡是摺起來的
/// `[Pasted text …]`、框太高找不到、字太短）一律回 `false`——那些情形交給 transcript 證據。
pub(crate) fn paste_truncated(kind: &str, screen: &str, sent: &str) -> bool {
    if kind != "claude" {
        return false;
    }
    let Some(box_text) = composer_text(kind, screen) else { return false };
    if box_text.starts_with("[Pasted text") {
        return false;
    }
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let needle: String = squash(sent).chars().take(HEAD).collect();
    needle.chars().count() >= HEAD_MIN && !squash(&box_text).contains(&needle)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::paste_truncated;
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

    #[test]
    fn only_a_visible_lost_head_counts_as_truncated() {
        let sent = "我了一堆給你才給我兩張\n20260813120139980\n20260813130350458";
        let screen = |rows: &[&str]| format!("⏺ 好\n\n─────\n❯\u{a0}{}\n─────\n", rows.join("\n  "));
        assert!(paste_truncated("claude", &screen(&["0139980", "20260813130350458"]), sent), "開頭不見");
        assert!(!paste_truncated("claude", &screen(&["我了一堆給你才給我兩張", "20260813120139980", "20260813130350458"]), sent), "整段都在");
        assert!(!paste_truncated("claude", &screen(&["[Pasted text #1 +2 lines]"]), sent), "摺起來看不到字");
        assert!(!paste_truncated("claude", &screen(&["ab"]), "ab"), "太短不判");
        assert!(!paste_truncated("codex", &screen(&["0139980"]), sent), "只管 claude");
        assert!(!paste_truncated("claude", "⏺ 好\n", sent), "找不到框");
    }
}
