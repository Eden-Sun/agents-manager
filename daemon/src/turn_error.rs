//! 「這一回合其實被 API 斷線截斷了」的偵測（見 SPEC §4.3a）。
//! 斷線時 hook 照樣送 Stop、herdr 照樣報 idle，回合會被誤記成 `completed`，所以要讀 pane 補認。

use crate::db;
use crate::lifecycle;
use crate::state::App;
use anyhow::Result;
use std::sync::Arc;

/// 錯誤行後面只會剩下狀態列與輸入框那幾行 chrome。
const TAIL_LINES: usize = 30;

/// 額度拒絕也算：claude 0 秒就 `done`，對使用者一樣是「送了沒回」（2026-09-10）。
fn is_api_error(body: &str) -> bool {
    lower_is_api_error(&body.to_ascii_lowercase())
}

fn lower_is_api_error(lower: &str) -> bool {
    // 只認 `API Error:` 與重試列；光看開頭會把回覆「API error handling 已補上」誤釘（2026-09-12 review h）。
    let banner = lower.starts_with("api error:")
        || (lower.starts_with("api error") && (lower.contains("retrying") || lower.contains("attempt ") || lower.contains("connection")));
    banner || is_quota_limit_lower(lower)
}

/// 跟斷線分開認：重送救不回來，要等重置或換模型（2026-09-12 使用者：「已用盡卻沒有正確的提示」）。
pub fn is_quota_limit(body: &str) -> bool {
    is_quota_limit_lower(&body.to_ascii_lowercase())
}

fn is_quota_limit_lower(lower: &str) -> bool {
    lower.starts_with("you've reached your") && lower.contains("limit")
}

/// 錯誤行之後出現非 chrome 行，代表錯誤已被重試蓋過。
fn is_chrome(s: &str) -> bool {
    s.is_empty()
        || s == "❯"
        || s == "›"
        || lifecycle::is_noise(s)
        || lifecycle::is_activity_shape(s)
        // claude 自動更新提示釘在輸入框上方，不是 agent 說的話（2.1.266）。
        || s.contains("Update installed")
        // claude 2.1.269 在額度拒絕下多印 `0 tokens`，沒認出來橫幅就永遠找不到（2026-09-12 使用者實測）。
        || is_token_count(s)
}

fn is_token_count(s: &str) -> bool {
    let Some(head) = s.strip_suffix(" tokens").or_else(|| s.strip_suffix(" token")) else { return false };
    !head.is_empty() && head.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.' || c == 'k' || c == 'K')
}

/// 只剝框線、保留前導記號——`is_noise` 要靠它認 spinner。
fn undecorated(line: &str) -> &str {
    line.trim().trim_matches(|c| "│┃".contains(c)).trim()
}

fn body_of(s: &str) -> &str {
    match s.chars().next() {
        Some(c) if !c.is_alphanumeric() && c != '❯' && c != '›' => s[c.len_utf8()..].trim_start(),
        _ => s,
    }
}

/// 必須是「最後一件事」：重試成功後橫幅仍留在畫面上，但下面還有回覆，不算斷線。
pub fn api_error_line(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    for line in lines[start..].iter().rev() {
        let raw = undecorated(line);
        let body = body_of(raw);
        if is_api_error(body) {
            return Some(body.to_string());
        }
        // 用帶記號的那串：`is_noise` 靠 `✻` 認出 spinner 收尾行。
        if !is_chrome(raw) {
            return None;
        }
    }
    None
}

/// 呼叫端要持有 bot lock。回合已被 hook 收掉後也要跑——斷線回合正是 hook 照常送 Stop 的那種。
pub async fn capture(app: &Arc<App>, bot_id: &str, expected_run_id: &str) -> Result<()> {
    let Some(run) = db::active_run(&app.db, bot_id).await? else { return Ok(()) };
    if run.id != expected_run_id {
        return Ok(());
    }
    let Some(pane_id) = run.pane_id.as_deref() else { return Ok(()) };
    let Some(client) = app.herdr_for_run(&run).await else { return Ok(()) };
    let read = client.pane_read(pane_id, "recent_unwrapped", 200).await?;
    let Some(line) = api_error_line(&read.text) else { return Ok(()) };
    // 同一則錯誤只記一次；`arm_progress` 開下一回合時清掉。
    if run.turn_error.as_deref() == Some(line.as_str()) {
        return Ok(());
    }

    sqlx::query("UPDATE runs SET turn_error = ? WHERE id = ?")
        .bind(&line)
        .bind(&run.id)
        .execute(&app.db)
        .await?;
    tracing::warn!(bot = %bot_id, run = %run.id, error = %line, "turn cut short by an API error");

    // 額度格標成被擋，量表與標題列才對得上。
    if is_quota_limit(&line) {
        mark_claude_limit_hit(app, bot_id, &line).await;
    }

    // 釘在那一回合上，看得出是哪一則回覆斷的。
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

    // 不收掉 in_flight 的話輸入框會一直鎖著。
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

pub async fn clear(app: &Arc<App>, run_id: &str, bot_id: &str) {
    let res = sqlx::query("UPDATE runs SET turn_error = NULL WHERE id = ? AND turn_error IS NOT NULL")
        .bind(run_id)
        .execute(&app.db)
        .await;
    if matches!(res, Ok(r) if r.rows_affected() > 0) {
        app.emit_bot_status(bot_id).await;
    }
}

/// 不篩 status：斷線那回合可能已被 Stop hook 收成 `completed`。
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

    #[test]
    fn other_api_error_variants_count_too() {
        let screen = "❯ hi\n⏺ API Error: 500 Internal Server Error\n\n✻ Worked for 3s · done 1:07 AM\n❯\n";
        assert_eq!(api_error_line(screen).as_deref(), Some("API Error: 500 Internal Server Error"));
        // 重試橫幅停在最後一件事上，也是這回合斷了。
        let retry = "❯ hi\n✻ API error · Retrying in 0s · attempt 1/10\n❯\n";
        assert_eq!(api_error_line(retry).as_deref(), Some("API error · Retrying in 0s · attempt 1/10"));
    }

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

    /// 2026-09-12 review h：以「API error」開頭的回覆行被誤釘成斷線。
    #[test]
    fn a_reply_line_that_merely_starts_with_api_error_is_not_a_banner() {
        let prose = "❯ 補上錯誤處理\n⏺ API error handling 已補上，測試也過了。\n✻ Worked for 9s · done 1:07 AM\n❯\n";
        assert_eq!(api_error_line(prose), None);
        let english = "❯ fix\n⏺ API errors are now retried three times.\n✻ done\n❯\n";
        assert_eq!(api_error_line(english), None);
        // 帶冒號的仍然是橫幅。
        let banner = "❯ fix\n⏺ API Error: Request timed out.\n✻ done\n❯\n";
        assert_eq!(api_error_line(banner).as_deref(), Some("API Error: Request timed out."));
    }

    #[test]
    fn boxed_line_is_unwrapped() {
        let screen = "❯ hi\n│ ⏺ API Error: Connection lost mid-response. │\n│ ❯                                        │\n";
        assert_eq!(
            api_error_line(screen).as_deref(),
            Some("API Error: Connection lost mid-response.")
        );
    }
}

/// 跟 codex 不同，**不**在下一回合成功時清掉：Fable 用盡後換 opus 照樣能跑，不代表 Fable 恢復；
/// 只靠 `until`（該桶子的 `resets_at`）到期解除。
async fn mark_claude_limit_hit(app: &Arc<App>, bot_id: &str, line: &str) {
    let Ok(Some(bot)) = db::bot(&app.db, bot_id).await else { return };
    if bot.kind != "claude" {
        return;
    }
    let host = db::bot_host(&app.db, bot_id).await.unwrap_or_else(|_| "local".to_string());
    let base = match bot.identity.as_deref() {
        Some(id) if !id.is_empty() => format!("claude:{id}"),
        _ => "claude".to_string(),
    };
    let key = crate::quota::quota_key(&host, &base);
    let prev = app.quotas.lock().await.get(&key).cloned();
    let mut q = prev.unwrap_or_else(|| crate::quota::Quota {
        five_hour: None,
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: db::now(),
        source: "claude-limit-hit".into(),
        account: bot.identity.clone(),
        host: host.clone(),
    });
    let lower = line.to_ascii_lowercase();
    let until = if lower.contains("fable") {
        match q.fable.as_mut() {
            Some(w) => {
                w.used_pct = 100.0;
                w.resets_at.clone()
            }
            None => None,
        }
    } else if let Some(w) = q.five_hour.as_mut() {
        w.used_pct = 100.0;
        w.resets_at.clone()
    } else if let Some(w) = q.seven_day.as_mut() {
        w.used_pct = 100.0;
        w.resets_at.clone()
    } else {
        None
    };
    q.limit_hit = Some(crate::quota::LimitHit { message: line.to_string(), until, at: db::now() });
    q.updated_at = db::now();
    crate::quota::set(app, &host, &base, q).await;
}

#[cfg(test)]
mod quota_limit_tests {
    use super::*;

    #[test]
    fn the_token_count_trailer_does_not_hide_the_limit_banner() {
        // 2026-09-12 實機（claude 2.1.269）：橫幅底下多一行 `0 tokens`。
        let screen = "❯ ping\n\nYou've reached your Fable limit. Run /usage-credits to continue or switch models with /model.\n\n0 tokens\n─────\n❯\n─────\n  tony. | agents-manager | Fable 5.1 | 5h:67% | F5:0%\n";
        assert_eq!(
            api_error_line(screen).as_deref(),
            Some("You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.")
        );
        assert!(is_token_count("0 tokens"));
        assert!(is_token_count("1,234 tokens"));
        assert!(!is_token_count("tokens"));
        assert!(!is_token_count("API error handling tokens"));
    }

    #[test]
    fn fable_limit_banner_is_a_quota_limit_not_a_connection_error() {
        let line = "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.";
        assert!(is_api_error(line));
        assert!(is_quota_limit(line));
        assert!(!is_quota_limit("API Error: Connection lost mid-response. The response above may be incomplete."));
    }
}
