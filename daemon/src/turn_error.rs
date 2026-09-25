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

/// `StopFailure` 帶的原因是不是「這個帳號的額度用完了」（issue #108）。比 `hookrecv::classify_failure` 的
/// rate limit 窄：那一類還包含 `overloaded`／一般 429 這種一下就好的限流，記成撞限會把排著的派工與之後的
/// 派送壓上好幾個小時。認的是跟畫面同一套的橫幅（前面可能多了 `API Error:` 之類的字）與 `usage limit`；
/// 月度花費上限那類照 [`is_quota_limit`] 的規則不算。
pub(crate) fn is_quota_exhaustion(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("usage limit")
        || lower.contains("usage_limit")
        || ["you've hit your", "you've reached your"]
            .iter()
            .any(|p| lower.find(p).is_some_and(|i| is_quota_limit_lower(&lower[i..])))
}

fn is_quota_limit_lower(lower: &str) -> bool {
    let reached = lower.starts_with("you've reached your") && lower.contains("limit");
    // 2.1.271 起速率上限也會寫成「You've hit your session／weekly／Opus limit」（CLI 的橫幅前綴清單同時有
    // hit 與 reached）。只認速率桶：`hit your monthly spend limit`、`fast limit`、團隊預算不是 5h／7d 用完，
    // 記成撞限會把量表釘成 100%（2026-09-15）。
    let hit = lower.starts_with("you've hit your") && limit_bucket(lower) != LimitBucket::Unknown;
    reached || hit
}

/// 橫幅說的是哪一桶。CLI 2.1.273 的字串表就是這幾種 rate limit：
/// `five_hour`→`session limit`、`seven_day`→`weekly limit`、`seven_day_opus`→`Opus limit`、
/// `seven_day_sonnet`→`Sonnet limit`、`seven_day_overage_included`→`Fable limit`。
/// Opus／Sonnet 跟 Fable 一樣是**模型自己的**週桶，不是整個身分的 7d（review3 c4 M1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitBucket {
    Session,
    Weekly,
    Fable,
    /// 模型專屬的週桶（`Opus limit`／`Sonnet limit`）：只擋跑那個模型的 bot。
    Model(&'static str),
    Unknown,
}

fn limit_bucket(lower: &str) -> LimitBucket {
    if lower.contains("fable") {
        LimitBucket::Fable
    } else if lower.contains("opus limit") {
        LimitBucket::Model("opus")
    } else if lower.contains("sonnet limit") {
        LimitBucket::Model("sonnet")
    } else if lower.contains("weekly") {
        LimitBucket::Weekly
    } else if lower.contains("session limit") || lower.contains("5-hour") || lower.contains("five-hour") {
        LimitBucket::Session
    } else {
        LimitBucket::Unknown
    }
}

/// 那一桶沒有讀數時，撞限要撐多久才自己過期。claude 這一側沒有 `clear_limit_hit`（Fable 用完換
/// opus 照樣能跑，成功回合不能當作解除），所以 `until=None` ＝ 永遠不過期：交辦會卡在 `quota_blocked`
/// 直到有人重啟 daemon（review 2026-09-16）。寧可保守地等一個視窗長度，也不要沒有出口。
/// 給下游用的桶名，跟 `Quota` 的欄位同名。認不出來就 `None`——不要編一個。
fn bucket_name(lower: &str) -> Option<String> {
    match limit_bucket(lower) {
        LimitBucket::Session => Some("five_hour".into()),
        LimitBucket::Weekly => Some("seven_day".into()),
        LimitBucket::Fable => Some("fable".into()),
        LimitBucket::Model(m) => Some(m.to_string()),
        LimitBucket::Unknown => None,
    }
}

/// 一段文字裡的 claude 撞限橫幅說的是哪一桶（[`bucket_name`] 的對外版本）。開機回填用它從 parked 交辦記下的
/// `error`（`帳號撞到用量上限（…）：<橫幅>`）把桶名找回來，`mission::pick` 才不必把回填的格子一律猜成週窗。
pub(crate) fn banner_bucket(text: &str) -> Option<String> {
    bucket_name(&text.to_ascii_lowercase())
}

fn fallback_until(lower: &str, at: &str) -> Option<String> {
    let hours = match limit_bucket(lower) {
        LimitBucket::Session => 5,
        LimitBucket::Weekly | LimitBucket::Fable | LimitBucket::Model(_) => 24 * 7,
        // 認不出是哪一桶：用最短的那個，寧可早一點放行讓它再撞一次。
        LimitBucket::Unknown => 5,
    };
    let base = chrono::DateTime::parse_from_rfc3339(at).ok()?.with_timezone(&chrono::Utc);
    Some(crate::db::iso_at(base + chrono::Duration::hours(hours)))
}

/// 把橫幅說的那一桶標成 100%，回傳它的重置時間（撞限到那時才解除）。認不出是哪一桶時照舊先 5h 再 7d。
///
/// Opus／Sonnet 的週桶 daemon 沒有量表可放：**一格都不標**（標 7d 會讓同身分所有 bot 看起來週額度用光），
/// 重置時間借 7d 那格——`/usage` 的 `weekly_scoped` 列跟 `weekly_all` 同一個重置週期（review3 c4 M1）。
///
/// 重置時間不晚於撞的那一刻（`at`）的讀數是**上一個窗**的：閒置的 5h 窗，`/usage` 照樣回上一個重置時間
/// （2026-09-19 `claude:cc1`：0%、−0.2h）。它說不出這次撞限什麼時候解除——拿來當到期，撞限一記下就過期、被
/// `quota::set` 丟掉，派工照送（#236）。所以不拿它的時間：認不出桶名時改看下一個窗；明講的那一桶照樣標滿，
/// 過期的重置時間丟掉，到期交給保底（[`fallback_until`]）。
fn saturate_bucket(q: &mut crate::quota::Quota, lower: &str, at: &str) -> Option<String> {
    let hit_at = chrono::DateTime::parse_from_rfc3339(at).ok();
    let stale = |w: &crate::quota::Window| {
        let reset = w.resets_at.as_deref().and_then(|r| chrono::DateTime::parse_from_rfc3339(r).ok());
        matches!((reset, hit_at), (Some(r), Some(a)) if r <= a)
    };
    let current = |w: &Option<crate::quota::Window>| w.as_ref().is_some_and(|w| !stale(w));
    let full = |w: &mut Option<crate::quota::Window>| {
        w.as_mut().map(|w| {
            w.used_pct = 100.0;
            if stale(w) {
                w.resets_at = None;
            }
            w.resets_at.clone()
        })
    };
    match limit_bucket(lower) {
        LimitBucket::Fable => full(&mut q.fable).flatten(),
        LimitBucket::Model(_) => q.seven_day.as_ref().filter(|w| !stale(w)).and_then(|w| w.resets_at.clone()),
        LimitBucket::Weekly => full(&mut q.seven_day).flatten(),
        LimitBucket::Session => full(&mut q.five_hour).flatten(),
        // 5h 窗閒著（上一個窗的讀數）＝撞的不會是 5h，先看 7d。
        LimitBucket::Unknown if !current(&q.five_hour) && current(&q.seven_day) => full(&mut q.seven_day).flatten(),
        LimitBucket::Unknown => match full(&mut q.five_hour) {
            Some(until) => until,
            None => full(&mut q.seven_day).flatten(),
        },
    }
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
    // 上面那個「只記一次」的標記（`turn_error`）、釘在那一回合上的訊息、收掉在飛的回合，**同一個交易**寫；要讀的全部先讀
    // （#198 同類）。以前標記先寫、後面讀寫一失敗就回錯：下一次擷取被「只記一次」擋掉，訊息沒釘、回合不收（輸入框鎖著），
    // 再也不重來。撞限要記在哪個身分也在這裡讀。
    let Some(bot) = db::bot(&app.db, bot_id).await? else { return Ok(()) };
    let conversation_id = db::conversation_id(&app.db, bot_id).await?;
    let turn = last_turn(app, &run.id).await?;

    // 額度格標成被擋，量表與標題列才對得上。撞限是外面已經發生的事：記不進去時欠著（排著的派工照欠著的那一筆擋），
    // 下面寫不寫得進去都一樣；錯誤留到最後回給呼叫端。
    let marked = if is_quota_limit(&line) { mark_claude_limit_hit(app, &bot, &line).await } else { Ok(()) };

    let mut tx = app.db.begin().await?;
    sqlx::query("UPDATE runs SET turn_error = ? WHERE id = ?").bind(&line).bind(&run.id).execute(&mut *tx).await?;
    // 釘在那一回合上，看得出是哪一則回覆斷的。
    lifecycle::insert_message_tx(&mut tx, &conversation_id, turn.as_ref().map(|t| t.id.as_str()), "system", &line, "system", true, Some(&read.text))
        .await?;
    // 不收掉 in_flight 的話輸入框會一直鎖著。
    let mut failed = None;
    if let Some(t) = turn.as_ref().filter(|t| t.status == "in_flight") {
        let res = crate::lifecycle::turn_controller::fail_on(
            &mut tx,
            &t.id,
            crate::lifecycle::turn_controller::DeliveryOnFail::Keep,
            "CLI 回報這一回合出錯",
        )
        .await?;
        if res == crate::lifecycle::turn_controller::Outcome::Applied {
            failed = Some(t.id.clone());
        }
    }
    tx.commit().await?;
    tracing::warn!(bot = %bot_id, run = %run.id, error = %line, "turn cut short by an API error");
    if let Some(t) = failed {
        lifecycle::emit_turn(app, &t).await;
    }
    app.emit_bot_status(bot_id).await;
    marked
}

/// 新回合開始：上一回合的錯誤是舊的了。寫不進去回錯（#193），呼叫端（progress poller）之後再清：留著的話，這一回合
/// 斷在同一句錯誤上會被 [`capture`] 的「同一則只記一次」吞掉——回合不收、輸入框一直鎖著。
pub async fn clear(app: &Arc<App>, run_id: &str, bot_id: &str) -> Result<()> {
    let res = sqlx::query("UPDATE runs SET turn_error = NULL WHERE id = ? AND turn_error IS NOT NULL")
        .bind(run_id)
        .execute(&app.db)
        .await;
    match res {
        Ok(r) => {
            if r.rows_affected() > 0 {
                app.emit_bot_status(bot_id).await;
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!(run = %run_id, error = %e, "the previous turn's error could not be cleared; retrying while this turn runs");
            Err(e.into())
        }
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
    use super::{api_error_line, is_quota_exhaustion, is_quota_limit};

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
    fn codex_0157_rate_limit_suggestion_is_not_an_exhausted_quota() {
        let screen = include_str!("lifecycle/fixtures/codex-0.157-rate-limit-switch.txt");
        assert_eq!(api_error_line(screen), None);
        assert!(!is_quota_limit(screen));
        assert!(!is_quota_exhaustion(screen));
        // A real exhausted-bucket banner remains classifiable despite the new recommendation.
        assert!(is_quota_limit("You've hit your weekly limit. Switch to gpt-6-luna for lower credit usage."));
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
///
/// 記不進去就回錯（#108 重開），不當作沒撞：讀不到這顆 bot 在哪台主機（不退回 `local`——那會把撞限寫進本機身分的
/// key）、那台的身分表還沒偵測完（key 算不準，[`crate::quota::resolve_quota_base`]）、排著的 prompt 身上的憑據寫不進去
/// （[`crate::lifecycle::quota_hold::stamp_queued`]）。撞限是外面已經發生的事，所以同時記成**欠著**（[`owed_limit_hit`]）：
/// 補上之前，這顆 bot 的 flush 與派送前都照欠著的那一筆擋；之後每一次問都先補一次。
pub(crate) async fn mark_claude_limit_hit(app: &Arc<App>, bot: &db::Bot, line: &str) -> Result<()> {
    if bot.kind != "claude" {
        return Ok(());
    }
    mark_limit_hit(app, bot, line, Banner::Claude).await
}

/// codex 的撞限橫幅（#198），規則同 [`mark_claude_limit_hit`]：讀不到這顆 bot 在哪台主機（以前退回 `local`——撞限寫進
/// 本機 `codex` 那一格，本機帳號被當成用盡、派工停擺，真正用盡的遠端身分反而沒擋）、那台的身分表還沒偵測完、排著的
/// prompt 蓋不上憑據，都回錯並記成欠著；補上之前 flush 與派送前照欠著的那一筆擋。
pub(crate) async fn mark_codex_limit_hit(app: &Arc<App>, bot: &db::Bot, notice: &str) -> Result<()> {
    // grok 撞週限時畫的是自己的字（`screen::grok_limit_hit_line`），走同一條畫面擷取（2026-09-19 w168:pB7：以前 grok 根本不走這條，
    // bot 一直停在 blocked、額度還顯示有餘），但撞限記在 **grok** 那一格、帶額度窗與保底到期（#222）：橫幅裡沒有時間，
    // 記成 `until: None` 就是永不過期、`/usage` 探測也校正不到，而且 `record` 對 codex 橫幅一律算 `codex` 的 key，
    // 會把 codex 標成用盡。
    match bot.kind.as_str() {
        "codex" => {
            let until = codex_banner_until(app, bot, notice).await;
            mark_limit_hit(app, bot, notice, Banner::Codex { until }).await
        }
        "grok" => {
            let (bucket, hours) = crate::lifecycle::grok_limit_window(notice);
            mark_limit_hit(app, bot, notice, Banner::Grok { bucket, hours }).await
        }
        _ => Ok(()),
    }
}

/// codex 橫幅上的時間是**那台主機**的當地時間（#239）：本機照 daemon 的時區；遠端用偵測時記下的 UTC 偏移。
/// 偏移讀不到就不猜（同 #59）：改用 app-server 讀到的重置時間，也沒有就撞限那一刻起 5 小時（最短的窗，寧可早放行再撞一次）。
async fn codex_banner_until(app: &Arc<App>, bot: &db::Bot, notice: &str) -> Option<String> {
    if !notice.to_ascii_lowercase().contains("try again at") {
        return None;
    }
    let host = db::bot_host(&app.db, &bot.id).await.ok();
    if host.as_deref() == Some(crate::config::LOCAL_HOST) {
        return crate::lifecycle::parse_codex_try_again(notice);
    }
    let offset = match &host {
        Some(h) => app.tools.lock().await.get(h).and_then(|t| t.utc_offset_secs),
        None => None,
    };
    if let Some(until) = offset.and_then(|o| crate::lifecycle::parse_codex_try_again_offset(notice, o)) {
        return Some(until);
    }
    if let Some(until) = crate::quota::next_reset_for_bot(app, bot).await {
        return Some(until);
    }
    Some(db::iso_at(chrono::Utc::now() + chrono::Duration::hours(5)))
}

async fn mark_limit_hit(app: &Arc<App>, bot: &db::Bot, line: &str, banner: Banner) -> Result<()> {
    // 撞的是 pane 裡實際的帳號＝run 起來時的身分（issue #238），不是剛改、還沒重啟生效的設定。讀不到 run 回錯（不猜）：
    // `StopFailure` 由收件匣重試；這段時間閘門同樣讀不到 run，照擋。
    let identity = crate::quota::billing_identity(app, bot).await?;
    // 同一筆撞限重來（收件匣重試 `StopFailure`、下一次讀到同一張橫幅）：撞的那一刻、橫幅上的時間都還是當初的，不往後推。
    let (banner, at) = match owed(app, &bot.id).filter(|m| m.identity == identity && m.line == line) {
        Some(m) => (m.banner, m.at),
        None => (banner, db::now()),
    };
    let mark = Mark { banner, identity, line: line.to_string(), at };
    match record(app, &bot.id, &mark).await {
        Ok(()) => {
            settled(app, &bot.id, &mark);
            Ok(())
        }
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, line, "a quota limit hit could not be recorded; it is owed and holds this bot");
            owe(app, &bot.id, mark);
            Err(e)
        }
    }
}

/// 一筆撞限：撞的那一刻、那時的身分、橫幅。欠著的時候原樣留著，補上時 `at` 不會變成補上的時間。
#[derive(Debug, Clone)]
struct Mark {
    banner: Banner,
    identity: Option<String>,
    line: String,
    at: String,
}

/// 撞限從哪一種 CLI 來：落在 `claude` 還是 `codex` 那一格，到期時間怎麼算。
#[derive(Debug, Clone, PartialEq)]
enum Banner {
    /// claude（`StopFailure`、畫面橫幅）：到期看那一桶的讀數，沒有就保底（[`fallback_until`]）。
    Claude,
    /// codex 的撞限橫幅：到期是橫幅上寫的時間，撞的當下解析（晚點補寫時裸鐘點可能已經過了）；沒寫就沒有
    /// （credits 用完，等下一個成功回合清）。
    Codex { until: Option<String> },
    /// grok 的撞額度畫面（#222）：桶名是額度窗（`seven_day`／`five_hour`，認不出就沒有），橫幅沒寫重置時間，
    /// 到期是撞的那一刻加保底時數（[`crate::lifecycle::grok_limit_window`]）。
    Grok { bucket: Option<&'static str>, hours: i64 },
}

impl Mark {
    /// 欠著的時候照這一筆擋。claude 還沒有那一桶的讀數可借，到期用保底時間。
    fn hit(&self) -> crate::quota::LimitHit {
        match &self.banner {
            Banner::Claude => {
                let lower = self.line.to_ascii_lowercase();
                crate::quota::LimitHit {
                    message: self.line.clone(),
                    until: fallback_until(&lower, &self.at),
                    at: self.at.clone(),
                    bucket: bucket_name(&lower),
                }
            }
            Banner::Codex { until } => {
                crate::quota::LimitHit { message: self.line.clone(), until: until.clone(), at: self.at.clone(), bucket: None }
            }
            Banner::Grok { bucket, hours } => {
                let until = chrono::DateTime::parse_from_rfc3339(&self.at)
                    .ok()
                    .map(|t| crate::db::iso_at(t.with_timezone(&chrono::Utc) + chrono::Duration::hours(*hours)));
                crate::quota::LimitHit { message: self.line.clone(), until, at: self.at.clone(), bucket: bucket.map(String::from) }
            }
        }
    }
}

/// 寫進那台主機、那個身分的 key，再蓋到這顆 bot 排著的每一則上。
async fn record(app: &Arc<App>, bot_id: &str, m: &Mark) -> Result<()> {
    let host = db::bot_host(&app.db, bot_id).await?;
    let hit = match &m.banner {
        Banner::Claude => record_claude(app, &host, m).await?,
        Banner::Codex { .. } => {
            let base = crate::quota::resolve_quota_base(app, &host, "codex", m.identity.as_deref()).await?;
            crate::lifecycle::apply_codex_limit_hit_quota(app, &host, &base, m.hit()).await
        }
        Banner::Grok { bucket, .. } => {
            let base = crate::quota::resolve_quota_base(app, &host, "grok", m.identity.as_deref()).await?;
            record_grok(app, &host, &base, m, *bucket).await
        }
    };
    crate::lifecycle::quota_hold::stamp_queued(app, bot_id, m.identity.as_deref(), &hit).await
}

async fn record_claude(app: &Arc<App>, host: &str, m: &Mark) -> Result<crate::quota::LimitHit> {
    // 落點只能有一份規則：手拼 `claude:{id}` 會讓「共用預設帳號」的身分（cc0）寫進一格沒有人查的
    // key，`limit_hit_for_bot` 讀的是裸 `claude`，於是撞限對 AGM 完全隱形（review 2026-09-16）。
    let base = crate::quota::resolve_quota_base(app, host, "claude", m.identity.as_deref()).await?;
    let key = crate::quota::quota_key(host, &base);
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
        account: m.identity.clone(),
        host: host.to_string(),
    });
    let lower = m.line.to_ascii_lowercase();
    // 那一桶還沒有讀數時 `saturate_bucket` 回 None，而 `None` 在 `limit_hit_expired` 是「永不過期」。
    // 給一個保底時間，撞限才有出口（review 2026-09-16）。
    let until = saturate_bucket(&mut q, &lower, &m.at).or_else(|| fallback_until(&lower, &m.at));
    let hit = crate::quota::LimitHit { message: m.line.clone(), until, at: m.at.clone(), bucket: bucket_name(&lower) };
    q.limit_hit = Some(hit.clone());
    q.updated_at = db::now();
    crate::quota::set(app, host, &base, q).await;
    Ok(hit)
}

/// grok 的一格（`grok`／`grok:<身分>`）：那個窗標成用完，撞限掛上去。窗沒有讀數時補一個 100% 的（重置時間留給
/// `/usage` 探測），額度面板才看得到「用完了」。
async fn record_grok(app: &Arc<App>, host: &str, base: &str, m: &Mark, bucket: Option<&'static str>) -> crate::quota::LimitHit {
    let key = crate::quota::quota_key(host, base);
    let prev = app.quotas.lock().await.get(&key).cloned();
    let hit = m.hit();
    // 已經記著、還沒過期的撞限：同一句再看到不是新證據（撞的那一刻不往後推）；到期比較晚的也不被較短的蓋掉
    // （同一張畫面兩句：402 credits 用完只有 5 小時的保底，`You hit your weekly limit.` 是 7 天，後到的不能把週限縮短）。
    // 已經過期的不算：過期後真的又被擋一次。
    if let Some(h) = prev.as_ref().and_then(|q| q.limit_hit.as_ref()).filter(|h| !crate::quota::limit_hit_expired(Some(h))) {
        let later = |a: &Option<String>, b: &Option<String>| match (a, b) {
            (Some(a), Some(b)) => chrono::DateTime::parse_from_rfc3339(a).ok() > chrono::DateTime::parse_from_rfc3339(b).ok(),
            (None, Some(_)) => true,
            _ => false,
        };
        if h.message == hit.message || !later(&hit.until, &h.until) {
            return h.clone();
        }
    }
    let mut q = prev.unwrap_or_else(|| crate::quota::Quota {
        five_hour: None,
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: db::now(),
        source: "grok-limit-hit".into(),
        account: m.identity.clone(),
        host: host.to_string(),
    });
    let full = |w: &mut Option<crate::quota::Window>| match w {
        Some(w) => w.used_pct = 100.0,
        None => *w = Some(crate::quota::Window { observed_at: None, used_pct: 100.0, resets_at: None }),
    };
    match bucket {
        Some("seven_day") => full(&mut q.seven_day),
        Some("five_hour") => full(&mut q.five_hour),
        _ => {}
    }
    q.limit_hit = Some(hit.clone());
    q.updated_at = db::now();
    crate::quota::set(app, host, base, q).await;
    hit
}

/// bot → 欠著的那一筆撞限（只留最新的）。只在記憶體：daemon 在補上之前重啟就沒了——`StopFailure` 那條路回錯、
/// 由 hook 收件匣（耐久）重試，那一回合也還沒收掉（在飛的回合本身就擋著 flush）；已經蓋上憑據的排隊 prompt 照
/// `quota_hold` 的規則擋。鍵帶 `boot_id`：測試裡模擬重啟的新 `App` 看不到上一個的帳，跟真的重啟一樣。
fn owed_marks() -> &'static std::sync::Mutex<std::collections::HashMap<String, Mark>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Mark>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn owed_key(app: &App, bot_id: &str) -> String {
    format!("{}\u{0}{bot_id}", app.boot_id)
}

fn owed(app: &App, bot_id: &str) -> Option<Mark> {
    owed_marks().lock().unwrap_or_else(|e| e.into_inner()).get(&owed_key(app, bot_id)).cloned()
}

fn owe(app: &App, bot_id: &str, m: Mark) {
    owed_marks().lock().unwrap_or_else(|e| e.into_inner()).insert(owed_key(app, bot_id), m);
}

/// `recorded` 寫進去了：同一個身分、不比它新的那一筆欠帳一併結清（寫進的是同一把 key、同一刻或更晚的撞限）。
/// 補的過程中又欠了一筆更新的、或欠的是換身分之前那個身分的，留著。
fn settled(app: &App, bot_id: &str, recorded: &Mark) {
    let key = owed_key(app, bot_id);
    let mut m = owed_marks().lock().unwrap_or_else(|e| e.into_inner());
    if m.get(&key).is_some_and(|x| x.identity == recorded.identity && x.at <= recorded.at) {
        m.remove(&key);
    }
}

/// 這顆 bot 欠著、還擋著它的撞限：先補一次，補上就回 `None`（記憶體裡已經有了，照一般的查法）；補不上、而且那一筆
/// 是這顆 bot **現在**的身分撞的、沒到期、管得到它在跑的模型，就照那一筆擋。身分已經換掉的留著等補（寫回舊身分的
/// key），不擋新身分；到期的直接丟掉。`identity` 是呼叫端算好的 [`crate::quota::billing_identity`]（issue #238）。
pub(crate) async fn owed_limit_hit(app: &Arc<App>, bot: &db::Bot, identity: Option<&str>) -> Option<crate::quota::LimitHit> {
    let m = owed(app, &bot.id)?;
    let hit = m.hit();
    if crate::quota::limit_hit_expired(Some(&hit)) {
        settled(app, &bot.id, &m);
        return None;
    }
    match record(app, &bot.id, &m).await {
        Ok(()) => {
            tracing::info!(bot = %bot.id, "an owed quota limit hit is now recorded");
            settled(app, &bot.id, &m);
            return None;
        }
        Err(e) => tracing::debug!(bot = %bot.id, error = %e, "an owed quota limit hit still cannot be recorded"),
    }
    if m.identity.as_deref() != identity {
        return None;
    }
    let model = crate::quota::running_model(app, bot).await;
    crate::quota::limit_hit_blocks_model(&hit, model.as_deref()).then_some(hit)
}

#[cfg(test)]
pub(crate) fn owes_limit_hit(app: &App, bot_id: &str) -> bool {
    owed(app, bot_id).is_some()
}

#[cfg(test)]
mod quota_limit_tests {
    use super::*;

    /// issue #108：`StopFailure` 的原因哪些算「帳號額度用完」。一下就好的限流不算——記成撞限會壓住派工好幾個小時。
    #[test]
    fn only_an_exhausted_account_counts_as_a_quota_stop_failure() {
        for yes in [
            "You've hit your session limit · resets 5pm",
            "API Error: You've hit your weekly limit · resets Sep 20",
            "You've reached your usage limit",
            "usage_limit_exceeded",
            "Claude usage limit reached. Your limit will reset at 5pm",
        ] {
            assert!(is_quota_exhaustion(yes), "{yes}");
        }
        for no in ["429 rate_limit", "overloaded_error", "API Error: 529 Overloaded", "You've hit your monthly spend limit", "API Error: 500"] {
            assert!(!is_quota_exhaustion(no), "{no}");
        }
    }

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

    fn window(pct: f64, resets: &str) -> Option<crate::quota::Window> {
        Some(crate::quota::Window { observed_at: None, used_pct: pct, resets_at: Some(resets.into()) })
    }

    fn quota() -> crate::quota::Quota {
        crate::quota::Quota {
            five_hour: window(40.0, "5h-reset"),
            seven_day: window(60.0, "7d-reset"),
            fable: window(10.0, "fable-reset"),
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: String::new(),
            source: String::new(),
            account: None,
            host: "local".into(),
        }
    }

    /// 那一桶還沒有讀數時（daemon 剛重啟、statusline 還沒進來），撞限一樣要有出口：
    /// claude 沒有 `clear_limit_hit`，`until=None` 等於永遠卡在 `quota_blocked`。
    #[test]
    fn a_banner_with_no_window_reading_still_expires() {
        let at = "2026-09-16T10:00:00Z";
        for (line, want) in [
            ("You've hit your session limit", "2026-09-16T15:00:00.000Z"),
            ("You've hit your weekly limit", "2026-09-23T10:00:00.000Z"),
            ("You've hit your Fable limit", "2026-09-23T10:00:00.000Z"),
            ("You've hit your limit", "2026-09-16T15:00:00.000Z"),
        ] {
            assert_eq!(fallback_until(&line.to_ascii_lowercase(), at).as_deref(), Some(want), "{line}");
        }
        assert_eq!(fallback_until("session limit", "not-a-time"), None, "讀不懂時間就不要編一個出來");

        // 有讀數時照舊用那一桶自己的 resets_at，保底不會蓋掉它。
        let mut q = crate::quota::Quota {
            five_hour: Some(crate::quota::Window { observed_at: None, used_pct: 10.0, resets_at: Some("2026-09-16T12:00:00Z".into()) }),
            seven_day: None, fable: None, reset_credits: None, limit_hit: None, plan: None,
            updated_at: at.into(), source: "test".into(), account: None, host: "local".into(),
        };
        let lower = "you've hit your session limit".to_string();
        let until = saturate_bucket(&mut q, &lower, at).or_else(|| fallback_until(&lower, at));
        assert_eq!(until.as_deref(), Some("2026-09-16T12:00:00Z"));
    }

    /// 前綴與桶名取自 2.1.273 binary 的字串表：`{five_hour:"session limit", seven_day:"weekly limit",
    /// seven_day_opus:"Opus limit", seven_day_sonnet:"Sonnet limit", seven_day_overage_included:"Fable limit"}`；
    /// `· resets …` 那段是示意，判斷不看它。以前非 Fable 一律記 5h——撞週額度卻把 5h 釘成 100%、而且等 5h 重置就當成解除了。
    #[test]
    fn each_banner_saturates_its_own_bucket() {
        for (line, bucket, until) in [
            ("You've hit your session limit · resets 4pm (Asia/Taipei)", "5h", "5h-reset"),
            ("You've hit your weekly limit · resets Sep 18", "7d", "7d-reset"),
            ("You've reached your Fable limit. Run /usage-credits to continue", "fable", "fable-reset"),
        ] {
            assert!(is_quota_limit(line), "{line}");
            let mut q = quota();
            assert_eq!(saturate_bucket(&mut q, &line.to_ascii_lowercase(), "2026-09-16T10:00:00Z").as_deref(), Some(until), "{line}");
            let pct = |w: &Option<crate::quota::Window>| w.as_ref().unwrap().used_pct;
            let got = [("5h", pct(&q.five_hour)), ("7d", pct(&q.seven_day)), ("fable", pct(&q.fable))];
            for (name, p) in got {
                assert_eq!(p == 100.0, name == bucket, "{line}: {name}={p}");
            }
        }
    }

    /// review3 c4 M1：`Opus limit`／`Sonnet limit`（CLI 的 `seven_day_opus`／`seven_day_sonnet`）是模型自己的週桶。
    /// 以前記成 `seven_day` 並把 7d 量表釘成 100%：同帳號所有 claude bot 停派到週重置，群組任務換掉整個身分。
    #[test]
    fn a_model_limit_is_its_own_bucket_and_saturates_no_bar() {
        for (line, bucket) in [
            ("You've hit your Opus limit · resets Sep 18", "opus"),
            ("You've hit your Sonnet limit · resets Sep 18", "sonnet"),
        ] {
            let lower = line.to_ascii_lowercase();
            assert!(is_quota_limit(line), "{line}");
            assert_eq!(bucket_name(&lower).as_deref(), Some(bucket), "{line}");
            let mut q = quota();
            // 重置時間借 7d 那格（`/usage` 的 weekly_scoped 與 weekly_all 同一個週期），但一格量表都不標。
            assert_eq!(saturate_bucket(&mut q, &lower, "2026-09-16T10:00:00Z").as_deref(), Some("7d-reset"), "{line}");
            let pct = |w: &Option<crate::quota::Window>| w.as_ref().unwrap().used_pct;
            assert_eq!((pct(&q.five_hour), pct(&q.seven_day), pct(&q.fable)), (40.0, 60.0, 10.0), "{line}：量表不動");
            // 那一桶連 7d 都還沒有讀數時照舊給保底（一週），不會留下 until=None。
            let mut empty = quota();
            empty.seven_day = None;
            let at = "2026-09-16T10:00:00Z";
            let until = saturate_bucket(&mut empty, &lower, at).or_else(|| fallback_until(&lower, at));
            assert_eq!(until.as_deref(), Some("2026-09-23T10:00:00.000Z"), "{line}");
        }
        // 「weekly limit」仍然是整個身分的 7d。
        assert_eq!(bucket_name("you've hit your weekly limit").as_deref(), Some("seven_day"));
    }

    /// 花費上限、fast 上限、團隊預算不是速率桶用完，不當撞限（否則量表被釘成 100%）。
    #[test]
    fn spend_and_fast_limits_are_not_rate_limits() {
        for line in [
            "You've hit your monthly spend limit.",
            "You've hit your fast limit",
            "You've hit your team's shared budget. Switch to another model",
        ] {
            assert!(!is_quota_limit(line), "{line}");
        }
    }

    const LIMIT: &str = "You've hit your session limit · resets 5pm";

    /// 一顆 bot，對話裡排著一則。`host` 不是本機時建一個那台主機的專案。
    async fn bot_with_a_queued_prompt(env: &crate::testing::Env, host: &str, identity: Option<&str>) -> (db::Bot, String) {
        let app = env.app.clone();
        let pid = if host == crate::config::LOCAL_HOST {
            env.project_id.clone()
        } else {
            let pid = db::ulid();
            sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', ?, ?)")
                .bind(&pid)
                .bind(host)
                .bind(db::now())
                .execute(&app.db)
                .await
                .unwrap();
            pid
        };
        let bot = crate::testing::claude_bot(&app, &pid, "limited").await;
        sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(identity).bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        (bot, turn)
    }

    async fn hits(app: &Arc<App>) -> Vec<(String, String)> {
        app.quotas.lock().await.iter().filter_map(|(k, q)| q.limit_hit.as_ref().map(|h| (k.clone(), h.at.clone()))).collect()
    }

    async fn hold_on(app: &Arc<App>, turn: &str) -> Option<serde_json::Value> {
        let raw: Option<String> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap();
        raw.map(|r| serde_json::from_str(&r).unwrap())
    }

    /// #108 重開：遠端 bot 撞限，那一刻讀不到它在哪台主機。以前退回 `local`——撞限寫進本機帳號那一格，遠端那個用盡的
    /// 身分照樣被派工。現在回錯、哪一格都不寫、記成欠著：派送前與 flush 照欠著的那一筆擋；讀得到之後補進 `remote1/claude`，
    /// 撞限時刻是當初那一刻，排著的那一則也蓋上憑據。
    #[tokio::test]
    async fn a_limit_hit_whose_host_cannot_be_read_is_owed_and_never_lands_on_the_local_key() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, queued) = bot_with_a_queued_prompt(&env, "remote1", None).await;

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        assert!(mark_claude_limit_hit(&app, &bot, LIMIT).await.is_err(), "記不進去就是錯");
        assert_eq!(hits(&app).await, vec![], "哪一格都沒寫，尤其不是本機的 `claude`");
        assert!(owes_limit_hit(&app, &bot.id));
        let owed = crate::quota::limit_hit_for_bot(&app, &bot).await.expect("欠著的那一筆照擋（派送前讀的就是這支）");
        assert_eq!(owed.bucket.as_deref(), Some("five_hour"));
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "flush 的閘門也一樣");
        assert!(hold_on(&app, &queued).await.is_none(), "還沒寫成");

        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
        let hit = crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("補上之後照一般的查法擋");
        assert!(!owes_limit_hit(&app, &bot.id), "補上了");
        assert_eq!(hits(&app).await, vec![("remote1/claude".to_string(), owed.at.clone())], "寫進遠端那一格，撞限時刻不變");
        assert_eq!(hit.at, owed.at);
        let held = hold_on(&app, &queued).await.expect("排著的那一則蓋上了憑據");
        assert_eq!((held["at"].as_str(), held["bucket"].as_str()), (Some(owed.at.as_str()), Some("five_hour")));
    }

    /// 欠著一筆（5 小時窗）的時候，同一個身分又撞了一筆更新的（週窗）而且記進去了：舊的那筆一併結清，不會在之後被補寫、
    /// 把記憶體裡較新的那筆蓋回去。
    #[tokio::test]
    async fn a_newer_hit_recorded_for_the_same_identity_settles_the_older_owed_one() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        assert!(mark_claude_limit_hit(&app, &bot, LIMIT).await.is_err());
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
        mark_claude_limit_hit(&app, &bot, "You've hit your weekly limit · resets Sep 25").await.unwrap();
        assert!(!owes_limit_hit(&app, &bot.id), "較舊的那筆一併結清");
        let hit = crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("照擋");
        assert_eq!(hit.bucket.as_deref(), Some("seven_day"), "記憶體留著較新的那筆，沒被舊的蓋回去");
    }

    /// 那一桶的讀數已經過了重置時間：閒置的 5h 窗，`/usage` 探測照樣回上一個窗的重置時間（2026-09-19 正式資料
    /// `claude:cc1`：five_hour 0%、重置 −0.2h，同時週窗 97%）。撞限的到期不能拿這個過去的時間——一記下就算過期、被
    /// `quota::set` 丟掉，等於沒撞限，排著的與派工照送、再撞一次。
    /// 認不出桶名的橫幅（`You've hit your limit`）：5h 窗閒著，撞的只可能是週窗，到期借 7d 的重置時間；
    /// 說是 session 的：5h 窗的讀數過期了，照保底（撞的那一刻 +5 小時）。
    #[tokio::test]
    async fn a_limit_hit_does_not_take_its_expiry_from_a_window_that_already_reset() {
        let hours = |h: i64| db::iso_at(chrono::Utc::now() + chrono::Duration::hours(h));
        let week_reset = hours(100);
        let reading = || crate::quota::Quota {
            five_hour: Some(crate::quota::Window { observed_at: None, used_pct: 0.0, resets_at: Some(hours(-1)) }),
            seven_day: Some(crate::quota::Window { observed_at: None, used_pct: 97.0, resets_at: Some(week_reset.clone()) }),
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: db::now(),
            source: "claude-usage".into(),
            account: None,
            host: crate::config::LOCAL_HOST.into(),
        };
        let later_than = |until: &Option<String>, h: i64| {
            let u = chrono::DateTime::parse_from_rfc3339(until.as_deref().expect("要有到期時間")).unwrap();
            u.with_timezone(&chrono::Utc) > chrono::Utc::now() + chrono::Duration::hours(h)
        };

        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        crate::quota::set(&app, crate::config::LOCAL_HOST, "claude", reading()).await;
        mark_claude_limit_hit(&app, &bot, "You've hit your limit · resets 5pm (Asia/Taipei)").await.unwrap();
        let hit = crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("撞限要擋住，不能一記下就過期");
        assert_eq!(hit.until.as_deref(), Some(week_reset.as_str()), "5h 窗閒著：到期借 7d 的重置時間");
        let held = hold_on(&app, &queued).await.expect("排著的那一則蓋上了憑據");
        assert!(later_than(&held["until"].as_str().map(String::from), 99), "憑據上的到期也不能是過去：{held}");

        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        crate::quota::set(&app, crate::config::LOCAL_HOST, "claude", reading()).await;
        mark_claude_limit_hit(&app, &bot, LIMIT).await.unwrap();
        let hit = crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("session 撞限要擋住");
        assert!(later_than(&hit.until, 4), "5h 窗的讀數過期了：保底 5 小時，不是過去的時間：{hit:?}");
    }

    /// 身分表還沒偵測完（重啟後、`tools::detect` 之前）：共用預設帳號的 cc0 會被猜成 `claude:cc0`，偵測完之後查詢端讀裸
    /// `claude`，那一格沒人看。現在不猜：欠著，偵測完的第一次查詢補進裸 `claude`。
    #[tokio::test]
    async fn a_limit_hit_before_the_identities_are_known_is_owed_until_its_key_is() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, Some("cc0")).await;
        assert!(mark_claude_limit_hit(&app, &bot, LIMIT).await.is_err());
        assert_eq!(hits(&app).await, vec![], "沒有猜一格 `claude:cc0`");
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");

        let cc0 = crate::config::IdentityCfg { name: "cc0".into(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] };
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![cc0], utc_offset_secs: None, herdr_cli: None, checked_at: db::now() },
        );
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
        assert!(!owes_limit_hit(&app, &bot.id));
        assert_eq!(hits(&app).await.into_iter().map(|(k, _)| k).collect::<Vec<_>>(), vec!["claude".to_string()], "補進裸 `claude`");
    }

    const CODEX_LIMIT: &str = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.";

    async fn codex_bot_with_a_queued_prompt(env: &crate::testing::Env, host: &str, identity: Option<&str>) -> (db::Bot, String) {
        let (bot, queued) = bot_with_a_queued_prompt(env, host, identity).await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        (db::bot(&env.app.db, &bot.id).await.unwrap().unwrap(), queued)
    }

    async fn set_remote_offset(app: &Arc<App>, host: &str, offset: Option<i32>) {
        app.tools.lock().await.insert(
            host.to_string(),
            crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![], utc_offset_secs: offset, herdr_cli: None, checked_at: db::now() },
        );
    }

    /// #239：橫幅的時間是遠端主機的當地時間。遠端 UTC−05:00、橫幅 6:43 PM → 23:43Z，不是照 daemon 的時區讀；
    /// 讀不到遠端偏移就不猜——到期是保底（約 5 小時後），不是照本機時區算出來的鐘點。
    #[tokio::test]
    async fn a_remote_codex_banner_is_read_in_the_remote_hosts_zone_and_never_guessed() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _q) = codex_bot_with_a_queued_prompt(&env, "remote1", None).await;
        set_remote_offset(&app, "remote1", Some(-5 * 3600)).await;
        mark_codex_limit_hit(&app, &bot, CODEX_LIMIT).await.unwrap();
        assert_eq!(crate::quota::limit_hit_for_bot(&app, &bot).await.and_then(|h| h.until).as_deref(), Some("2099-09-19T23:43:00.000Z"));

        let (bot2, _q) = codex_bot_with_a_queued_prompt(&env, "remote2", None).await;
        set_remote_offset(&app, "remote2", None).await;
        mark_codex_limit_hit(&app, &bot2, CODEX_LIMIT).await.unwrap();
        let until = crate::quota::limit_hit_for_bot(&app, &bot2).await.and_then(|h| h.until).expect("保底到期");
        assert!(chrono::DateTime::parse_from_rfc3339(&until).unwrap() < chrono::Utc::now() + chrono::Duration::hours(6), "保底約 5 小時，不是 2099 年的橫幅時間");
    }

    /// #198：遠端 codex bot 撞限，那一刻讀不到它在哪台主機。以前退回 `local`——撞限寫進本機 `codex` 那一格，本機帳號被當成
    /// 用盡、派工停擺，遠端那個真的用盡的身分反而照樣被派工；記不進去也沒有欠帳。現在哪一格都不寫、欠著：派送前與 flush
    /// 照欠著的那一筆擋；讀得到之後補進 `remote1/codex`，撞限時刻與橫幅上的時間不變，排著的那一則也蓋上憑據。
    #[tokio::test]
    async fn a_codex_limit_hit_whose_host_cannot_be_read_is_owed_and_never_lands_on_the_local_key() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, queued) = codex_bot_with_a_queued_prompt(&env, "remote1", None).await;

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        assert!(mark_codex_limit_hit(&app, &bot, CODEX_LIMIT).await.is_err(), "記不進去就是錯");
        assert_eq!(hits(&app).await, vec![], "哪一格都沒寫，尤其不是本機的 `codex`");
        assert!(owes_limit_hit(&app, &bot.id));
        let owed = crate::quota::limit_hit_for_bot(&app, &bot).await.expect("欠著的那一筆照擋（派送前讀的就是這支）");
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "flush 的閘門也一樣");
        let soon = chrono::Utc::now() + chrono::Duration::hours(6);
        assert!(owed.until.as_deref().is_some_and(|u| chrono::DateTime::parse_from_rfc3339(u).unwrap() < soon), "讀不到主機就不猜橫幅的時區：保底到期（#239）：{owed:?}");
        assert!(hold_on(&app, &queued).await.is_none(), "還沒寫成");

        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
        let hit = crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("補上之後照一般的查法擋");
        assert!(!owes_limit_hit(&app, &bot.id), "補上了");
        assert_eq!(hits(&app).await, vec![("remote1/codex".to_string(), owed.at.clone())], "寫進遠端那一格，撞限時刻不變");
        assert_eq!((hit.at.as_str(), hit.until.as_deref()), (owed.at.as_str(), owed.until.as_deref()));
        let held = hold_on(&app, &queued).await.expect("排著的那一則蓋上了憑據");
        assert_eq!(held["at"].as_str(), Some(owed.at.as_str()));
    }

    /// codex 也不猜 key：身分表還沒偵測完時，共用預設帳號的 `cx0` 會被算成 `codex:cx0`，偵測完之後查詢端讀裸 `codex`。
    #[tokio::test]
    async fn a_codex_limit_hit_before_the_identities_are_known_is_owed_until_its_key_is() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, _queued) = codex_bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, Some("cx0")).await;
        assert!(mark_codex_limit_hit(&app, &bot, CODEX_LIMIT).await.is_err());
        assert_eq!(hits(&app).await, vec![], "沒有猜一格 `codex:cx0`");
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");

        let cx0 = crate::config::IdentityCfg { name: "cx0".into(), kind: "codex".into(), host: None, env: Default::default(), args: vec![] };
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![cx0], utc_offset_secs: None, herdr_cli: None, checked_at: db::now() },
        );
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
        assert!(!owes_limit_hit(&app, &bot.id));
        assert_eq!(hits(&app).await.into_iter().map(|(k, _)| k).collect::<Vec<_>>(), vec!["codex".to_string()], "補進裸 `codex`");
    }

    /// #225：grok 撞週限記在 **grok** 自己那一格（6d5e74ab 讓 grok 走 `mark_codex_limit_hit`，`record` 卻把 kind 寫死成 `codex`：codex
    /// 被標成用盡、grok 那格沒事）。同一台主機上另有一顆 codex bot：它不能被擋，grok 自己要被擋；撞限當下排著的那一則蓋上憑據；
    /// `/api/quota` 的來源標 `grok-limit-hit`，不是 `codex-limit-hit`。
    #[tokio::test]
    async fn a_grok_limit_hit_lands_on_the_grok_key_not_codex() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (grok, queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&grok.id).execute(&app.db).await.unwrap();
        let grok = db::bot(&app.db, &grok.id).await.unwrap().unwrap();
        let codex = crate::testing::claude_bot(&app, &env.project_id, "cx").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&codex.id).execute(&app.db).await.unwrap();
        let codex = db::bot(&app.db, &codex.id).await.unwrap().unwrap();

        mark_codex_limit_hit(&app, &grok, "You hit your weekly limit.").await.unwrap();

        let keys: Vec<String> = hits(&app).await.into_iter().map(|(k, _)| k).collect();
        let codex_blocked = crate::quota::limit_hit_for_bot(&app, &codex).await.is_some();
        let grok_blocked = crate::quota::limit_hit_for_bot(&app, &grok).await.is_some();
        assert_eq!((keys, codex_blocked, grok_blocked), (vec!["grok".to_string()], false, true));
        assert_eq!(app.quotas.lock().await.get("grok").map(|q| q.source.clone()).as_deref(), Some("grok-limit-hit"));
        assert!(hold_on(&app, &queued).await.is_some(), "撞限當下排著的那一則蓋上憑據");
    }

    /// 同類（#198 的留言）：「同一則只記一次」的標記（`runs.turn_error`）先寫、訊息後寫的話，訊息寫不進去之後下一次擷取被
    /// 標記擋掉——錯誤沒釘上、回合不收（輸入框鎖著），再也不重來。現在標記、訊息、收回合同一個交易。
    #[tokio::test]
    async fn an_api_error_that_cannot_be_recorded_is_captured_again_next_time() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "cut").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.set_screen(&format!("pane-{}", bot.id), "❯ 幫我改\n⏺ API Error: Connection lost mid-response.\n✻ done\n❯\n");
        sqlx::query(
            "CREATE TRIGGER refuse_note BEFORE INSERT ON messages WHEN NEW.role='system'
             BEGIN SELECT RAISE(ABORT, 'injected: cannot write the error note'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        assert!(capture(&app, &bot.id, &run).await.is_err());
        let status = |app: Arc<App>, t: String| async move {
            let s: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(t).fetch_one(&app.db).await.unwrap();
            s
        };
        assert_eq!(status(app.clone(), turn.clone()).await, "in_flight");

        sqlx::query("DROP TRIGGER refuse_note").execute(&app.db).await.unwrap();
        capture(&app, &bot.id, &run).await.unwrap();
        assert_eq!(status(app.clone(), turn.clone()).await, "failed", "寫得進去了：這一次照樣記、回合收掉");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(notes, 1);
        let marked: Option<String> = sqlx::query_scalar("SELECT turn_error FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(marked.as_deref(), Some("API Error: Connection lost mid-response."));
    }

    /// 撞限記下的當下就蓋到排著的那一則上（不等 flush）；蓋不上就回錯、欠著——記憶體那一格照樣寫了（照擋），
    /// 補的時候再蓋一次。
    #[tokio::test]
    async fn a_limit_hit_stamps_the_queued_prompts_at_once_or_stays_owed() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (bot, queued) = bot_with_a_queued_prompt(&env, crate::config::LOCAL_HOST, None).await;
        sqlx::query("CREATE TRIGGER refuse_quota_hold BEFORE UPDATE OF quota_hold ON turns BEGIN SELECT RAISE(ABORT, 'injected: cannot write turns.quota_hold'); END")
            .execute(&app.db)
            .await
            .unwrap();
        assert!(mark_claude_limit_hit(&app, &bot, LIMIT).await.is_err(), "憑據沒落地不算記好了");
        assert!(owes_limit_hit(&app, &bot.id));
        assert_eq!(hits(&app).await.len(), 1, "記憶體那一格寫了：這一輪照擋");
        assert!(hold_on(&app, &queued).await.is_none());

        sqlx::query("DROP TRIGGER refuse_quota_hold").execute(&app.db).await.unwrap();
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
        assert!(!owes_limit_hit(&app, &bot.id));
        let held = hold_on(&app, &queued).await.expect("補的時候蓋上了");
        assert_eq!(held["boot"].as_str(), Some(app.boot_id.as_str()));
        assert_eq!(held["identity"], serde_json::Value::Null);
    }

    #[test]
    fn fable_limit_banner_is_a_quota_limit_not_a_connection_error() {
        let line = "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.";
        assert!(is_api_error(line));
        assert!(is_quota_limit(line));
        assert!(!is_quota_limit("API Error: Connection lost mid-response. The response above may be incomplete."));
    }
}
