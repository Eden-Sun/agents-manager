//! 「這一回合其實被 API 斷線截斷了」的偵測（SPEC §4.3a）。
//!
//! claude 的連線在回應中途掉了會在 pane 上印
//!
//! ```text
//! ⏺ API Error: Connection lost mid-response. The response above may be incomplete.
//!
//! ✻ Baked for 5m 21s · done 12:56 AM
//! ```
//!
//! 然後就收工回到 idle。hook 照樣送 Stop、herdr 照樣報 `working -> idle`，於是這回合被記成
//! `completed`、側欄一顆綠燈——使用者以為做完了，實際上回應是斷的。
//!
//! 這支在 `working -> idle` 的終端備援掃描裡多認這一種：把那行讀出來掛到 run 上
//! （`runs.turn_error`），並在對話裡補一則 system 訊息釘在那個回合上。web 拿 `turn_error`
//! 畫紅色 badge 與「重送上一則」；下一回合一開始（`arm_progress`）就清掉。
//!
//! 判定跟 `update_notice` / codex 額度公告同一套：讀 pane、比對上次存的值、只在變了的時候寫。

use crate::db;
use crate::lifecycle;
use crate::state::App;
use anyhow::Result;
use std::sync::Arc;

/// 從螢幕底部往上找幾行。錯誤行後面只會剩下狀態列與輸入框那幾行 chrome。
const TAIL_LINES: usize = 30;

/// 這行是不是 API 錯誤橫幅（去掉裝飾字元之後以 `API error` 開頭），或是額度用盡的拒絕
/// （`You've reached your Fable limit. Run /usage-credits to continue or switch models with
/// /model.`）。後者 claude 根本沒開始回，0 秒就 `done`，pane 只剩這一行；
/// 對使用者來說一樣是「送了沒回」，一樣要釘在那個回合上（2026-09-10）。
fn is_api_error(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.starts_with("api error") || (lower.starts_with("you've reached your") && lower.contains("limit"))
}

/// 這行在錯誤行**之後**出現的話，代表 agent 後來又說了話——那次錯誤已經被重試蓋過去了。
///
/// 只有 chrome（空行、輸入框、分隔線、狀態列、spinner）不算數。`is_noise` /
/// `is_activity_shape` 是終端備援本來就在用的那組判斷，這裡直接沿用，不另外寫一套。
fn is_chrome(s: &str) -> bool {
    s.is_empty()
        || s == "❯"
        || s == "›"
        || lifecycle::is_noise(s)
        || lifecycle::is_activity_shape(s)
        // claude 自動更新的那一行釘在輸入框上方（`current: 2.1.266 · latest: 2.1.267 ✔ Update
        // installed · Restart to update`），不是 agent 說的話。
        || s.contains("Update installed")
}

/// 把一行的框線剝掉，留下 TUI 真正畫的那串（前導記號還在——`is_noise` 要靠它認 spinner）。
fn undecorated(line: &str) -> &str {
    line.trim().trim_matches(|c| "│┃".contains(c)).trim()
}

/// 再把前導記號剝掉，留下內容本身：`⏺ API Error: …` -> `API Error: …`。
fn body_of(s: &str) -> &str {
    match s.chars().next() {
        Some(c) if !c.is_alphanumeric() && c != '❯' && c != '›' => s[c.len_utf8()..].trim_start(),
        _ => s,
    }
}

/// 這張快照的**最後一件事**是不是一則 API 錯誤？是的話回傳那行原文。
///
/// 「最後一件事」是關鍵：claude 遇到暫時性錯誤會印 `API error · Retrying in 0s · attempt 1/10`
/// 然後接著把答案講完，那種橫幅留在畫面上但下面還有回覆——不算斷線。所以從底部往上掃，
/// 碰到的第一個非 chrome 行必須就是錯誤行。
pub fn api_error_line(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    for line in lines[start..].iter().rev() {
        let raw = undecorated(line);
        let body = body_of(raw);
        if is_api_error(body) {
            return Some(body.to_string());
        }
        // chrome 判斷吃**帶記號**的那串：`✻ Baked for 5m 21s · done` 的 `✻` 正是 `is_noise`
        // 用來認出它是 spinner 收尾行的依據。
        if !is_chrome(raw) {
            return None;
        }
    }
    None
}

/// 讀 pane，把「這回合被 API 截斷」記到 run 與對話上。呼叫端要持有 bot lock。
///
/// 跟 [`lifecycle::capture_codex_usage_notices`] 一樣是 best-effort、而且在回合已經被 hook
/// 收掉之後也要跑——斷線的那一回合正是 hook 會照常送 Stop 的那一種。
pub async fn capture(app: &Arc<App>, bot_id: &str, expected_run_id: &str) -> Result<()> {
    let Some(run) = db::active_run(&app.db, bot_id).await? else { return Ok(()) };
    if run.id != expected_run_id {
        return Ok(());
    }
    let Some(pane_id) = run.pane_id.as_deref() else { return Ok(()) };
    let Some(client) = app.herdr_for_run(&run).await else { return Ok(()) };
    let read = client.pane_read(pane_id, "recent_unwrapped", 200).await?;
    let Some(line) = api_error_line(&read.text) else { return Ok(()) };
    // 同一則錯誤只記一次：值沒變就是同一回合的同一行，`arm_progress` 開下一回合時會清掉。
    if run.turn_error.as_deref() == Some(line.as_str()) {
        return Ok(());
    }

    sqlx::query("UPDATE runs SET turn_error = ? WHERE id = ?")
        .bind(&line)
        .bind(&run.id)
        .execute(&app.db)
        .await?;
    tracing::warn!(bot = %bot_id, run = %run.id, error = %line, "turn cut short by an API error");

    // 釘在那一回合上：對話裡看得到是「哪一則回覆」斷的，不是一句飄在最後面的通知。
    let conversation_id = db::conversation_id(&app.db, bot_id).await?;
    let turn = last_turn(app, &run.id).await?;
    lifecycle::insert_message(
        app,
        &conversation_id,
        turn.as_ref().map(|t| t.id.as_str()),
        "system",
        &line,
        "system",
        true,
        Some(&read.text),
    )
    .await?;

    // 還在 in_flight 的話一併收掉，不然輸入框會一直鎖著（codex 額度用完走的是同一條）。
    if let Some(t) = turn {
        if t.status == "in_flight" {
            let res = sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=? AND status='in_flight'")
                .bind(db::now())
                .bind(&t.id)
                .execute(&app.db)
                .await?;
            if res.rows_affected() > 0 {
                lifecycle::emit_turn(app, &t.id).await;
            }
        }
    }
    app.emit_bot_status(bot_id).await;
    Ok(())
}

/// 下一回合開始了：把上一回合的錯誤旗標清掉（`arm_progress` 呼叫）。
pub async fn clear(app: &Arc<App>, run_id: &str, bot_id: &str) {
    let res = sqlx::query("UPDATE runs SET turn_error = NULL WHERE id = ? AND turn_error IS NOT NULL")
        .bind(run_id)
        .execute(&app.db)
        .await;
    if matches!(res, Ok(r) if r.rows_affected() > 0) {
        app.emit_bot_status(bot_id).await;
    }
}

/// 這個 run 最近開的一回合——斷線那回合可能已經被 Stop hook 收成 `completed` 了。
async fn last_turn(app: &Arc<App>, run_id: &str) -> Result<Option<db::Turn>> {
    Ok(sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE run_id = ? ORDER BY created_at DESC LIMIT 1")
        .bind(run_id)
        .fetch_optional(&app.db)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::api_error_line;

    /// pane w168:pE 的真實快照（2026-09-09）：回合被 API 斷線截斷，但收尾照樣寫 `done`。
    const CONNECTION_LOST: &str = "\
❯ 幫我把那段改掉
⏺ 我先看一下現在的實作。
  ⎿  Read daemon/src/lifecycle.rs

⏺ API Error: Connection lost mid-response. The response above may be incomplete.

✻ Baked for 5m 21s · done 12:56 AM
────────────────────────────────────────────
❯
────────────────────────────────────────────
  tony. | pt | HAI4.5 | 5h:- | 7d:-
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";

    #[test]
    fn connection_lost_is_the_last_thing_on_screen() {
        assert_eq!(
            api_error_line(CONNECTION_LOST).as_deref(),
            Some("API Error: Connection lost mid-response. The response above may be incomplete.")
        );
    }

    /// 其他 `API Error:` 變體一併吃。
    #[test]
    fn other_api_error_variants_count_too() {
        let screen = "❯ hi\n⏺ API Error: 500 Internal Server Error\n\n✻ Worked for 3s · done 1:07 AM\n❯\n";
        assert_eq!(api_error_line(screen).as_deref(), Some("API Error: 500 Internal Server Error"));
        // 重試橫幅停在最後一件事上，也是這回合斷了。
        let retry = "❯ hi\n✻ API error · Retrying in 0s · attempt 1/10\n❯\n";
        assert_eq!(api_error_line(retry).as_deref(), Some("API error · Retrying in 0s · attempt 1/10"));
    }

    /// 重試之後答案講完了：橫幅還留在畫面上，但它不是最後一件事——不能算斷線。
    #[test]
    fn a_retry_that_recovered_is_not_an_interruption() {
        let screen = "\
❯ hi
✻ API error · Retrying in 0s · attempt 1/10
⏺ 好了，改完了。
✻ Worked for 9s · done 1:07 AM
❯
";
        assert_eq!(api_error_line(screen), None);
    }

    /// 一般收工的畫面不能誤判。
    #[test]
    fn usage_limit_refusal_counts_as_an_error() {
        let screen = "\
❯ 用戶回報 cf2go
  ⎿  2 skills available
  ⎿  You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.

✻ Crunched for 0s · done 4:11 PM
                  current: 2.1.266 · latest: 2.1.267 ✔ Update installed · Restart to update
─────────────────────────────────────────────────────────────────────────────────────────────
❯
";
        assert_eq!(
            api_error_line(screen).as_deref(),
            Some("You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.")
        );
    }

    #[test]
    fn a_clean_turn_has_no_error() {
        let screen = "❯ echo 1\n⏺ PONG\n✻ Worked for 0s · done 1:07 AM\n──────\n❯\n";
        assert_eq!(api_error_line(screen), None);
        // agent 自己在講 API 錯誤這件事，不是橫幅——它不在行首。
        let prose = "❯ hi\n⏺ 這個 API Error 要自己處理\n✻ done\n❯\n";
        assert_eq!(api_error_line(prose), None);
        assert_eq!(api_error_line(""), None);
    }

    /// 窄 pane 把橫幅畫進框線裡也要認得。
    #[test]
    fn boxed_line_is_unwrapped() {
        let screen = "❯ hi\n│ ⏺ API Error: Connection lost mid-response. │\n│ ❯                                        │\n";
        assert_eq!(
            api_error_line(screen).as_deref(),
            Some("API Error: Connection lost mid-response.")
        );
    }
}
